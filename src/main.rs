use std::borrow::Cow;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
#[cfg(feature = "tls")]
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use axum::{
    body::BoxBody,
    error_handling::HandleErrorLayer,
    extract::{Path, Query},
    headers::UserAgent,
    http::{
        header::{CACHE_CONTROL, USER_AGENT},
        HeaderValue, Response, StatusCode,
    },
    response::IntoResponse,
    routing::get,
    BoxError, Extension, Json, Router, TypedHeader,
};
use clap::Parser;
use extend::ext;
use rand::{distributions::Alphanumeric, Rng};
use reqwest::{ClientBuilder, Proxy};
use reqwest_middleware::ClientWithMiddleware as Client;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower::ServiceBuilder;
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
#[allow(unused)]
use tracing::{debug, error, info, warn, Level};
use url::Url;

mod common;
#[cfg(feature = "hola")]
mod hello;
#[cfg(feature = "hola")]
mod hello_config;
#[cfg(feature = "true-status")]
mod status;

/// Client-ID of Twitch's web player. Shown in the clear if you load the main page.
/// Try `curl -s https://www.twitch.tv | tidy -q | grep 'clientId='`.
pub(crate) const TWITCH_CLIENT: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const ID_PARAM: &str = "id";
const VOD_ENDPOINT: &str = const_format::concatcp!("/vod/:", ID_PARAM);
const LIVE_ENDPOINT: &str = const_format::concatcp!("/live/:", ID_PARAM);
// for Firefox only
const STATUS_ENDPOINT: &str = "/stat/";
// for uptime monitoring
#[cfg(feature = "true-status")]
const TRUE_STATUS_ENDPOINT: &str = "/truestat/:secret";
const CONCURRENCY_LIMIT: usize = 64;

#[cfg(feature = "true-status")]
pub(crate) static PROXY: once_cell::sync::OnceCell<Option<Proxy>> =
    once_cell::sync::OnceCell::new();

#[derive(Parser, Debug)]
#[clap(version, about)]
pub(crate) struct Opts {
    /// Address for this server to listen on.
    #[clap(short, long, default_value = "127.0.0.1")]
    address: IpAddr,
    /// Port for this server to listen on.
    #[clap(short, long, default_value = "9595")]
    server_port: u16,
    /// Connect directly to Twitch, without a proxy. Useful when running this server remotely
    /// in a country where Twitch doesn't serve ads.
    #[cfg_attr(feature = "hola", clap(long, conflicts_with_all(&["proxy", "country"])))]
    #[cfg_attr(not(feature = "hola"), clap(long, conflicts_with_all(&["proxy"])))]
    no_proxy: bool,
    /// Custom proxy to use, instead of Hola. Takes the form of 'scheme://host:port',
    /// where scheme is one of: http/https/socks5/socks5h.
    /// Must be in a country where Twitch doesn't serve ads for this system to work.
    #[cfg_attr(feature = "hola", clap(short, long))]
    #[cfg_attr(not(feature = "hola"), clap(short, long, required_unless_present = "no-proxy"))]
    proxy: Option<Url>,
    /// Country to request a proxy in. See https://client.hola.org/client_cgi/vpn_countries.json.
    #[cfg(feature = "hola")]
    #[clap(short, long, conflicts_with = "proxy", parse(try_from_str = parse_country), default_value = "ru")]
    country: String,
    /// Don't save Hola credentials.
    #[cfg(feature = "hola")]
    #[clap(short, long, conflicts_with = "proxy")]
    discard_creds: bool,
    /// Regenerate Hola credentials (don't load them).
    #[cfg(feature = "hola")]
    #[clap(short, long, conflicts_with = "proxy")]
    regen_creds: bool,
    /// List Hola's available countries, for use with --country
    #[cfg(feature = "hola")]
    #[clap(long)]
    list_countries: bool,
    /// Private key for TLS. Enables TLS if specified.
    #[cfg(feature = "tls")]
    #[clap(long, requires = "tls-cert", display_order = 4800)]
    tls_key: Option<PathBuf>,
    /// Server certificate for TLS.
    #[cfg(feature = "tls")]
    #[clap(long, display_order = 4801)]
    tls_cert: Option<PathBuf>,
    /// Debug logging.
    #[clap(long, display_order = 5000)]
    debug: bool,
}

#[cfg(feature = "hola")]
fn parse_country(input: &str) -> Result<String> {
    if input.len() != 2 {
        anyhow::bail!("Country argument invalid, must be 2 letters: {}", input);
    } // better to actually validate from the API, too lazy
    Ok(input.to_ascii_lowercase())
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    #[cfg(windows)]
    if let Err(code) = ansi_term::enable_ansi_support() {
        error!("failed to enable ANSI support, error code {}", code);
    }
    tracing_subscriber::fmt()
        .with_max_level(if opts.debug { Level::DEBUG } else { Level::INFO })
        .init();
    #[cfg(feature = "hola")]
    if opts.list_countries {
        return hello::list_countries().await;
    }
    // TODO: SOCKS4 for reqwest
    let proxy = if let Some(proxy) = opts.proxy {
        let proxy = Proxy::all(proxy)?;
        Some(proxy)
    } else if opts.no_proxy {
        None
    } else {
        hola_proxy(&opts).await?
    };
    #[cfg(feature = "true-status")]
    PROXY.set(proxy.clone()).unwrap();
    let client = create_client(proxy)?;

    let mut router = Router::new()
        .route(VOD_ENDPOINT, get(process_vod))
        .route(LIVE_ENDPOINT, get(process_live))
        .route(STATUS_ENDPOINT, get(status))
        .layer(Extension(client));
    #[cfg(feature = "true-status")]
    {
        router = router.route(TRUE_STATUS_ENDPOINT, get(status::deep_status));
    }
    #[cfg(feature = "gzip")]
    {
        router = router.layer(tower_http::compression::CompressionLayer::new());
    }
    router = router
        .layer(CorsLayer::new().allow_origin(Any))
        .layer(SetResponseHeaderLayer::overriding(
            CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-store"),
        ))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_error))
                .load_shed()
                .concurrency_limit(CONCURRENCY_LIMIT)
                .timeout(Duration::from_secs(40))
                .into_inner(),
        ); // rudimentary global rate-limiting, plus failsafe timeout
    let addr = SocketAddr::new(opts.address, opts.server_port);
    #[cfg(feature = "tls")]
    if let (Some(key), Some(cert)) = (opts.tls_key, opts.tls_cert) {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await?;
        tokio::spawn(reload(config.clone(), key, cert));
        return Ok(axum_server::bind_rustls(addr, config)
            .serve(router.into_make_service())
            .await?);
    }
    axum::Server::bind(&addr).serve(router.into_make_service()).await?;
    Ok(())
}

#[cfg(feature = "hola")]
async fn hola_proxy(opts: &Opts) -> Result<Option<Proxy>> {
    let proxy = hello_config::setup_hola(opts).await?;
    Ok(Some(proxy))
}

#[cfg(not(feature = "hola"))]
async fn hola_proxy(_opts: &Opts) -> Result<Option<Proxy>> {
    unreachable!("how'd you get here") // checked earlier by clap in arg parsing
}

pub(crate) fn create_client(proxy: Option<Proxy>) -> Result<Client> {
    let mut cb =
        ClientBuilder::new().user_agent(common::USER_AGENT).timeout(Duration::from_secs(20));
    if let Some(proxy) = proxy {
        cb = cb.proxy(proxy);
    } else {
        cb = cb.no_proxy();
    }
    let client = cb.build()?;
    let backoff = ExponentialBackoff::builder()
        .retry_bounds(Duration::from_millis(1), Duration::from_secs(2))
        .build_with_total_retry_duration(Duration::from_secs(15));
    let client = reqwest_middleware::ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(backoff))
        .build();
    // network errors can happen on occasion, this should avoid causing an annoying error for a user
    Ok(client)
}

/// Endlessly loops, reloading the TLS certificate and key every 24 hours.
#[cfg(feature = "tls")]
async fn reload(config: axum_server::tls_rustls::RustlsConfig, key: PathBuf, cert: PathBuf) {
    loop {
        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
        match config.reload_from_pem_file(&cert, &key).await {
            Ok(_) => info!("reloaded TLS key/cert"),
            Err(e) => error!("failed to reload TLS key/cert: {}", e),
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize)]
struct Status {
    online: bool,
}

async fn status() -> Json<Status> {
    // in Chrome-like browsers the extension can download the M3U8, and if that succeeds redirect
    // to it in Base64 form. In Firefox that isn't permitted. Checking if the server is online before
    // redirecting to it reduces the chance of the extension breaking Twitch.
    #[cfg(not(feature = "true-status"))]
    return Json(Status { online: true });
    #[cfg(feature = "true-status")]
    return Json(Status { online: status::STATUS.load(std::sync::atomic::Ordering::Acquire) });
}

pub(crate) struct ProcessData {
    sid: StreamID,
    query: HashMap<String, String>,
    user_agent: UserAgent,
}

// the User-Agent header is copied from the user if present
// when using this locally it's basically pointless, but for a remote server handling many users
// it should make it less detectable on Twitch's end (it'll look like more like a VPN endpoint or
// similar rather than an automated system)
// UAs shouldn't be individually identifiable in any remotely normal browser

type QueryMap = Query<HashMap<String, String>>;

async fn process_live(
    Path(id): Path<String>,
    Query(query): QueryMap,
    user_agent: Option<TypedHeader<UserAgent>>,
    Extension(client): Extension<Client>,
) -> Response<BoxBody> {
    let pd = ProcessData {
        sid: StreamID::Live(id.into_ascii_lowercase()),
        query,
        user_agent: user_agent.unwrap_or_common(),
    };
    process(pd, &client).await.into_response()
}

async fn process_vod(
    Path(id): Path<u64>,
    Query(query): QueryMap,
    user_agent: Option<TypedHeader<UserAgent>>,
    Extension(client): Extension<Client>,
) -> Response<BoxBody> {
    let pd = ProcessData {
        sid: StreamID::VOD(id.to_string()),
        query,
        user_agent: user_agent.unwrap_or_common(),
    };
    process(pd, &client).await.into_response()
}

pub(crate) async fn process(pd: ProcessData, client: &Client) -> AppResult<Response<BoxBody>> {
    let token = get_token(client, &pd).await?;
    let m3u8 = get_m3u8(client, &pd, token.data.playback_access_token).await?;
    Ok(([("Content-Type", "application/vnd.apple.mpegurl")], m3u8).into_response())
}

async fn get_m3u8(client: &Client, pd: &ProcessData, token: PlaybackAccessToken) -> Result<String> {
    const PERMITTED_INCOMING_KEYS: [&str; 9] = [
        "player_backend",             // mediaplayer
        "playlist_include_framerate", // true
        "reassignments_supported",    // true
        "supported_codecs",           // avc1, usually. sometimes vp09,avc1
        "cdm",                        // wv
        "player_version",             // 1.11.0
        "fast_bread",                 // true; related to low latency mode
        "allow_source",               // true
        "warp",                       // true; https://www.ietf.org/id/draft-lcurley-warp-01.html
    ];
    let mut url = Url::parse(&pd.sid.get_url())?;
    // set query string automatically using non-identifying parameters
    url.query_pairs_mut().extend_pairs(
        pd.query.iter().filter(|(k, _)| PERMITTED_INCOMING_KEYS.contains(&k.as_ref())),
    );
    // add our fake ID
    url.query_pairs_mut()
        .append_pair("p", &common::get_rng().gen_range(0..=9_999_999).to_string())
        .append_pair("play_session_id", &generate_id().into_ascii_lowercase())
        .append_pair("token", &token.value)
        .append_pair("sig", &token.signature);
    let m3u = client
        .get(url.as_str())
        .header(USER_AGENT, pd.user_agent.as_str())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    const UC_START: &str = "USER-COUNTRY=\"";
    if let Some(country) = m3u.lines().find_map(|line| line.substring_between(UC_START, "\"")) {
        info!("Twitch states that the proxy is in {}", country);
    }

    #[cfg(feature = "redact-ip")]
    {
        // if the server is behind Cloudflare or similar, the playlist exposes the real IP, which
        // removes all the DDoS protection
        let user_ip = lazy_regex::regex!(r#"USER-IP="(([[:digit:]]{1,3}\.){3}[[:digit:]]{1,3})""#);
        return Ok(user_ip.replace(&m3u, r#"USER-IP="1.1.1.1""#).into_owned());
    }
    #[cfg(not(feature = "redact-ip"))]
    Ok(m3u)
}

/// Get an access token for the given stream.
async fn get_token(client: &Client, pd: &ProcessData) -> Result<AccessTokenResponse> {
    let sid = &pd.sid;
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
    //  2022-04-16: No longer seeing it
    Ok(client
        .post("https://gql.twitch.tv/gql")
        .header("Client-ID", TWITCH_CLIENT)
        .header("Device-ID", &generate_id())
        .header(USER_AGENT, pd.user_agent.as_str())
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

#[ext]
impl Option<TypedHeader<UserAgent>> {
    /// Returns the header value or the common User-Agent if not present.
    fn unwrap_or_common(self) -> UserAgent {
        self.map(|ua| ua.0).unwrap_or_else(|| UserAgent::from_static(common::USER_AGENT))
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
        match &self {
            Self::Live(channel) => format!("{}api/channel/hls/{}.m3u8", BASE, channel),
            Self::VOD(id) => format!("{}vod/{}.m3u8", BASE, id),
        }
    }
    pub(crate) fn data(&self) -> &str {
        match self {
            Self::Live(d) | Self::VOD(d) => d.as_str(),
        }
    }
}

async fn handle_error(error: BoxError) -> impl IntoResponse {
    if error.is::<tower::timeout::error::Elapsed>() {
        return (StatusCode::GATEWAY_TIMEOUT, Cow::from("timeout"));
    }
    if error.is::<tower::load_shed::error::Overloaded>() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Cow::from("service is overloaded, try again later"),
        );
    }

    (StatusCode::INTERNAL_SERVER_ERROR, Cow::from(format!("Unhandled internal error: {}", error)))
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
