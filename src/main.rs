use std::time::Duration;

use clap::{AppSettings, Clap};
use extend::ext;
use once_cell::sync::OnceCell;
use rand::{distributions::Alphanumeric, seq::SliceRandom, Rng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tide::security::{CorsMiddleware, Origin};
use tide::{log, Request, Response, Result, Status, StatusCode};
use ureq::{Agent, AgentBuilder, Proxy};
use url::{form_urlencoded::Parse, Url};
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

static AGENT: OnceCell<Agent> = OnceCell::new();
// using ureq because it supports a wide variety of proxies, unlike surf

#[derive(Clap)]
#[clap(version, about)]
#[clap(setting = AppSettings::ColoredHelp)]
struct Opts {
    /// Port for this server to listen on.
    #[clap(short, long, default_value = "9595")]
    server_port: u16,
    /// Custom proxy to use, instead of Hola. Takes the form of 'scheme://user:password@host:port',
    /// where scheme is one of: http/https/socks4/socks4a/socks5.
    /// Must be in a country where Twitch doesn't serve ads for this system to work.
    #[clap(short, long)]
    proxy: Option<String>,
    /// Don't save Hola credentials.
    #[clap(short, long)]
    discard_creds: bool,
    /// Regenerate Hola credentials (don't load them).
    #[clap(short, long)]
    regen_creds: bool,
    /// Debug logging.
    #[clap(long)]
    debug: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Config {
    uuid: Option<Uuid>,
}

#[async_std::main]
async fn main() -> Result<()> {
    let crate_name = clap::crate_name!();
    let opts = Opts::parse();
    log::with_level(if opts.debug { log::LevelFilter::Debug } else { log::LevelFilter::Info });
    let mut config: Config = confy::load(crate_name, None)?;
    let mut ab =
        AgentBuilder::new().user_agent(common::USER_AGENT).timeout(Duration::from_secs(20));
    if let Some(proxy) = opts.proxy {
        ab = ab.proxy(Proxy::new(proxy)?);
    } else {
        ab = setup_hola(&mut config, &opts, ab).await?;
        if !opts.discard_creds {
            log::info!(
                "Saving Hola credentials to {}",
                confy::get_configuration_file_path(crate_name, None)?.display()
            );
            confy::store(crate_name, None, &config)?;
        }
    };
    let agent = ab.build();
    AGENT.set(agent).unwrap();
    let mut app = tide::new();
    app.at(VOD_ENDPOINT).get(process_vod);
    app.at(LIVE_ENDPOINT).get(process_live);
    app.with(CorsMiddleware::new().allow_origin(Origin::Any));
    app.listen(("127.0.0.1", opts.server_port)).await?;
    Ok(())
}

/// Connect to Hola, retrieve tunnels, set the AgentBuilder to use one of the proxies. Updates
/// stored UUID in the config if we regenerated our creds.
async fn setup_hola(config: &mut Config, opts: &Opts, ab: AgentBuilder) -> Result<AgentBuilder> {
    let uuid = if !opts.regen_creds { config.uuid } else { None };
    let (bg, uuid) = hello::background_init(uuid)?;
    config.uuid = Some(uuid);
    if bg.blocked.unwrap_or_default() || bg.permanent.unwrap_or_default() {
        panic!("Blocked by Hola: {:?}", bg);
    }
    let proxy_type = ProxyType::Direct;
    let tunnels = hello::get_tunnels(&uuid, bg.key, "ru", proxy_type, 3)?;
    log::debug!("{:?}", tunnels);
    let login = hello::uuid_to_login(&uuid);
    let password = tunnels.agent_key;
    log::debug!("login: {}", login);
    log::debug!("password: {}", password);
    let (hostname, ip) =
        tunnels.ip_list.choose(&mut common::get_rng()).expect("no tunnels found in hola response");
    let port = proxy_type.get_port(&tunnels.port);
    let proxy = if !hostname.is_empty() {
        // https://github.com/algesten/ureq/issues/420
        format!("http://{}:{}@{}:{}", login, password, hostname, port)
    } else {
        format!("http://{}:{}@{}:{}", login, password, ip, port)
    }; // does this check actually need to exist?
    Ok(ab.proxy(Proxy::new(proxy)?))
}

async fn process_live(req: Request<()>) -> Result<Response> {
    let id = req.param(ID_PARAM).unwrap();
    let sid = StreamID::Live(id.to_lowercase());
    process(sid, req.url().query_pairs()).await
}

async fn process_vod(req: Request<()>) -> Result<Response> {
    let id = req.param(ID_PARAM).unwrap();
    let sid = StreamID::VOD(id.to_string());
    process(sid, req.url().query_pairs()).await
}

async fn process(sid: StreamID, query: Parse<'_>) -> Result<Response> {
    let token = get_token(&sid).await?;
    let m3u8 = get_m3u8(&sid.get_url(), token.data.playback_access_token, query).await?;
    Ok(Response::builder(StatusCode::Ok)
        .content_type("application/vnd.apple.mpegurl")
        .body(m3u8)
        .build())
}

async fn get_m3u8(url: &str, token: PlaybackAccessToken, query: Parse<'_>) -> Result<String> {
    const PERMITTED_INCOMING_KEYS: [&str; 9] = [
        "player_backend",             // mediaplayer
        "playlist_include_framerate", // true
        "reassignments_supported",    // true
        "supported_codecs",           // avc1, usually. sometimes vp09,avc1
        "cdm",                        // wv
        "player_version",             // 1.4.0 or 1.5.0 (being A/B tested?)
        "fast_bread",                 // true; related to low latency mode
        "allow_source",               // true
        "warp",                       // true; I have no idea what this is
    ];
    let mut url = Url::parse(url)?;
    // set query string automatically using non-identifying parameters
    url.query_pairs_mut()
        .extend_pairs(query.filter(|(k, _)| PERMITTED_INCOMING_KEYS.contains(&k.as_ref())));
    // add our fake ID
    url.query_pairs_mut()
        .append_pair("p", &common::get_rng().gen_range(0..=9_999_999).to_string())
        .append_pair("play_session_id", &generate_id().into_ascii_lowercase())
        .append_pair("token", &token.value)
        .append_pair("sig", &token.signature);
    let m3u = async_std::task::spawn_blocking::<_, Result<_>>(move || {
        Ok(AGENT.get().unwrap().get(url.as_str()).call().into_tide()?.into_string()?)
    })
    .await?;

    const UC_START: &str = "USER-COUNTRY=\"";
    if let Some(country) = m3u.lines().find_map(|line| line.substring_between(UC_START, "\"")) {
        log::info!("Twitch states that the proxy is in {}", country);
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
    async_std::task::spawn_blocking(|| {
        Ok(AGENT
            .get()
            .unwrap()
            .post("https://gql.twitch.tv/gql")
            .set("Client-ID", TWITCH_CLIENT)
            .set("Device-ID", &generate_id())
            .send_json(request)
            .into_tide()?
            .into_json()?)
    })
    .await
}

#[ext]
impl<R> std::result::Result<R, ureq::Error> {
    /// Attach a status code to a ureq response if it is an error. This lets Tide send
    /// a 404 if ureq got a 404, instead of 404 becoming 500.
    fn into_tide(self) -> tide::Result<R> {
        match self {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(code, _)) => self.status(code),
            Err(e) => Err(e.into()),
        }
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
