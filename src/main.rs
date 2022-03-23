use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use axum::{
    body::BoxBody,
    extract::{Path, Query},
    http::{Response, StatusCode},
    response::{Headers, IntoResponse},
    routing::get,
    Json, Router,
};
use clap::Parser;
use extend::ext;
use once_cell::sync::OnceCell;
use rand::{distributions::Alphanumeric, seq::SliceRandom, Rng};
use reqwest::{Client, ClientBuilder, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};
#[allow(unused)]
use tracing::{debug, error, info, warn, Level};
use url::Url;
use uuid::Uuid;

use crate::hello::ProxyType;

mod common;
mod hello;

/// Client-ID of Twitch's web player. Shown in the clear if you load the main page.
/// Try `curl -s https://www.twitch.tv | tidy -q | grep '"Client-ID":"'`.
const TWITCH_CLIENT: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const ID_PARAM: &str = "id";
const VOD_ENDPOINT: &str = const_format::concatcp!("/vod/:", ID_PARAM);
const LIVE_ENDPOINT: &str = const_format::concatcp!("/live/:", ID_PARAM);

static CLIENT: OnceCell<Client> = OnceCell::new();

#[derive(Parser, Debug)]
#[clap(version, about)]
struct Opts {
    /// Port for this server to listen on.
    #[clap(short, long, default_value = "9595")]
    server_port: u16,
    /// Custom proxy to use, instead of Hola. Takes the form of 'scheme://host:port',
    /// where scheme is one of: http/https/socks5/socks5h.
    /// Must be in a country where Twitch doesn't serve ads for this system to work.
    #[clap(short, long)]
    proxy: Option<String>,
    /// Country to request a proxy in. See https://client.hola.org/client_cgi/vpn_countries.json.
    #[clap(short, long, conflicts_with = "proxy", parse(try_from_str = parse_country), default_value = "ru")]
    country: String,
    /// Don't save Hola credentials.
    #[clap(short, long, conflicts_with = "proxy")]
    discard_creds: bool,
    /// Regenerate Hola credentials (don't load them).
    #[clap(short, long, conflicts_with = "proxy")]
    regen_creds: bool,
    /// Debug logging.
    #[clap(long)]
    debug: bool,
}

fn parse_country(input: &str) -> anyhow::Result<String> {
    if input.len() != 2 {
        anyhow::bail!("Country argument invalid, must be 2 letters: {}", input);
    } // better to actually validate from the API, too lazy
    Ok(input.to_lowercase())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Config {
    uuid: Option<Uuid>,
}

const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    #[cfg(windows)]
    if let Err(code) = ansi_term::enable_ansi_support() {
        error!("failed to enable ANSI support, error code {}", code);
    }
    tracing_subscriber::fmt()
        .with_max_level(if opts.debug { Level::DEBUG } else { Level::INFO })
        .init();
    let mut config: Config = confy::load(CRATE_NAME, None)?;
    // TODO: SOCKS4 for reqwest
    let mut cb =
        ClientBuilder::new().user_agent(common::USER_AGENT).timeout(Duration::from_secs(20));
    if let Some(proxy) = opts.proxy {
        cb = cb.proxy(Proxy::all(proxy)?);
    } else {
        cb = setup_hola(&mut config, &opts, cb).await?;
        if !opts.discard_creds {
            info!(
                "Saving Hola credentials to {}",
                confy::get_configuration_file_path(CRATE_NAME, None)?.display()
            );
            confy::store(CRATE_NAME, None, &config)?;
        }
    };
    let client = cb.build()?;
    CLIENT.set(client).unwrap();
    let app = Router::new()
        .route(VOD_ENDPOINT, get(process_vod))
        .route(LIVE_ENDPOINT, get(process_live))
        .layer(CorsLayer::new().allow_origin(Any));
    let addr = SocketAddr::from(([127, 0, 0, 1], opts.server_port));
    axum::Server::bind(&addr).serve(app.into_make_service()).await?;
    Ok(())
}

/// Connect to Hola, retrieve tunnels, set the ClientBuilder to use one of the proxies. Updates
/// stored UUID in the config if we regenerated our creds.
async fn setup_hola(
    config: &mut Config,
    opts: &Opts,
    cb: ClientBuilder,
) -> anyhow::Result<ClientBuilder> {
    let uuid = if !opts.regen_creds { config.uuid } else { None };
    let (bg, uuid) = hello::background_init(uuid).await?;
    config.uuid = Some(uuid);
    if bg.blocked || bg.permanent {
        panic!("Blocked by Hola: {:?}", bg);
    }
    let proxy_type = ProxyType::Direct;
    let tunnels = hello::get_tunnels(&uuid, bg.key, &opts.country, proxy_type, 3).await?;
    debug!("{:?}", tunnels);
    let login = hello::uuid_to_login(&uuid);
    let password = tunnels.agent_key;
    debug!("login: {}", login);
    debug!("password: {}", password);
    let (hostname, ip) =
        tunnels.ip_list.choose(&mut common::get_rng()).expect("no tunnels found in hola response");
    let port = proxy_type.get_port(&tunnels.port);
    let proxy = if !hostname.is_empty() {
        format!("https://{}:{}", hostname, port)
    } else {
        format!("http://{}:{}", ip, port)
    }; // does this check actually need to exist?
    Ok(cb.proxy(Proxy::all(proxy)?.basic_auth(&login, &password)))
}

type QueryMap = Query<HashMap<String, String>>;

async fn process_live(Path(id): Path<String>, Query(query): QueryMap) -> Response<BoxBody> {
    let sid = StreamID::Live(id.to_lowercase());
    process(sid, query).await.into_response()
}

async fn process_vod(Path(id): Path<u64>, Query(query): QueryMap) -> Response<BoxBody> {
    let sid = StreamID::VOD(id.to_string());
    process(sid, query).await.into_response()
}

async fn process(sid: StreamID, query: HashMap<String, String>) -> AppResult<Response<BoxBody>> {
    let token = get_token(&sid).await?;
    let m3u8 = get_m3u8(&sid.get_url(), token.data.playback_access_token, query).await?;
    Ok((Headers([("Content-Type", "application/vnd.apple.mpegurl")]), m3u8).into_response())
}

async fn get_m3u8(
    url: &str,
    token: PlaybackAccessToken,
    query: HashMap<String, String>,
) -> Result<String> {
    const PERMITTED_INCOMING_KEYS: [&str; 9] = [
        "player_backend",             // mediaplayer
        "playlist_include_framerate", // true
        "reassignments_supported",    // true
        "supported_codecs",           // avc1, usually. sometimes vp09,avc1
        "cdm",                        // wv
        "player_version",             // 1.9.0
        "fast_bread",                 // true; related to low latency mode
        "allow_source",               // true
        "warp",                       // true; I have no idea what this is; no longer present
    ];
    let mut url = Url::parse(url)?;
    // set query string automatically using non-identifying parameters
    url.query_pairs_mut()
        .extend_pairs(query.iter().filter(|(k, _)| PERMITTED_INCOMING_KEYS.contains(&k.as_ref())));
    // add our fake ID
    url.query_pairs_mut()
        .append_pair("p", &common::get_rng().gen_range(0..=9_999_999).to_string())
        .append_pair("play_session_id", &generate_id().into_ascii_lowercase())
        .append_pair("token", &token.value)
        .append_pair("sig", &token.signature);
    let m3u =
        CLIENT.get().unwrap().get(url.as_str()).send().await?.error_for_status()?.text().await?;

    const UC_START: &str = "USER-COUNTRY=\"";
    if let Some(country) = m3u.lines().find_map(|line| line.substring_between(UC_START, "\"")) {
        info!("Twitch states that the proxy is in {}", country);
    }

    Ok(m3u)
}

/// Get an access token for the given stream.
async fn get_token(sid: &StreamID) -> Result<AccessTokenResponse> {
    let request = json!({
        "operationName": "PlaybackAccessToken",
        "extensions": {
            "persistedQuery": {
                "version": 1,
                "sha256Hash": "0828119ded1c13477966434e15800ff57ddacf13ba1911c129dc2200705b0712",
            },
        },
        "variables": {
            "isLive": matches!(sid, StreamID::Live(_)),
            "login": if matches!(sid, StreamID::Live(_)) { sid.data() } else { "" },
            "isVod": matches!(sid, StreamID::VOD(_)),
            "vodID": if matches!(sid, StreamID::VOD(_)) { sid.data() } else { "" },
            "playerType": "site", // "embed" may also be valid
        },
    });
    // XXX: I've seen a different method of doing this that involves X-Device-Id (frontpage only?)
    Ok(CLIENT
        .get()
        .unwrap()
        .post("https://gql.twitch.tv/gql")
        .header("Client-ID", TWITCH_CLIENT)
        .header("Device-ID", &generate_id())
        .json(&request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

type AppResult<T> = std::result::Result<T, AppError>;

enum AppError {
    Anyhow(anyhow::Error),
}

// TODO: thiserror?

impl From<anyhow::Error> for AppError {
    fn from(inner: anyhow::Error) -> Self {
        AppError::Anyhow(inner)
    }
}

// errors are first mapped to anyhow, then to AppError

impl IntoResponse for AppError {
    fn into_response(self) -> Response<BoxBody> {
        let (status, error_message) = match self {
            AppError::Anyhow(e) => {
                let message = format!("{:?}", e);
                let status = e
                    .downcast_ref::<reqwest::Error>()
                    .and_then(|e| e.status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, message)
            }
        };
        let body = Json(json!({
            "code": status.as_u16(),
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

// make a pointless optimization expressible in one line at the cost of 7 lines
#[ext]
impl String {
    fn into_ascii_lowercase(mut self) -> String {
        self.make_ascii_lowercase();
        self
    }
}

#[ext]
impl str {
    fn substring_between(&self, start: &str, end: &str) -> Option<&str> {
        let start_idx = self.find(start)?;
        let s = &self[start_idx + start.len()..];
        let end_idx = s.find(end)?;
        Some(&s[..end_idx])
    }
}

/// Generate an ID suitable for use both as a Device-ID and a play_session_id.
/// The latter must be lowercased, as this function returns a mixed-case string.
fn generate_id() -> String {
    let mut rng = common::get_rng();
    std::iter::repeat(()).map(|_| rng.sample(Alphanumeric)).map(char::from).take(32).collect()
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct AccessTokenResponse {
    pub(crate) data: Data,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Data {
    /// The signed access token itself.
    ///
    /// Can in fact be `null`, for example if the VOD ID is wrong or pointing to a deleted VOD.
    /// Not modeled since we want to error out anyway. TODO: Model it so we can make a nicer error?
    // Name depends on whether it's a livestream or a VOD.
    #[serde(rename = "streamPlaybackAccessToken", alias = "videoPlaybackAccessToken")]
    pub(crate) playback_access_token: PlaybackAccessToken,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PlaybackAccessToken {
    pub(crate) value: String,
    pub(crate) signature: String,
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StreamID {
    Live(String),
    VOD(String),
}

impl StreamID {
    pub(crate) fn get_url(&self) -> String {
        const BASE: &str = "https://usher.ttvnw.net/";
        let endpoint = match &self {
            Self::Live(channel) => format!("api/channel/hls/{}.m3u8", channel),
            Self::VOD(id) => format!("vod/{}.m3u8", id),
        };
        format!("{}{}", BASE, endpoint)
    }
    pub(crate) fn data(&self) -> &str {
        match self {
            Self::Live(d) | Self::VOD(d) => d.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::strExt;

    #[test]
    fn substring() {
        let input = r#"se",USER-COUNTRY="RU",MANI"#;
        assert_eq!(input.substring_between("USER-COUNTRY=\"", "\""), Some("RU"));
    }
}
