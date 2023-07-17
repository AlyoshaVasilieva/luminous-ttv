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
    extract::{Path, Query, State},
    headers::UserAgent,
    response::IntoResponse,
    routing::get,
    BoxError, Json, Router, TypedHeader,
};
use cfg_if::cfg_if;
use clap::Parser;
use extend::ext;
use http::{
    header::{CACHE_CONTROL, USER_AGENT},
    HeaderValue, Response, StatusCode,
};
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

const ID_PARAM: &str = "id";
const VOD_ENDPOINT: &str = const_format::concatcp!("/vod/:", ID_PARAM);
const LIVE_ENDPOINT: &str = const_format::concatcp!("/live/:", ID_PARAM);
/// TTV-LOL emulation
const LIVE_TTVLOL_ENDPOINT: &str = const_format::concatcp!("/playlist/:", ID_PARAM);
// for Firefox only
const STATUS_ENDPOINT: &str = "/stat/";
const STATUS_TTVLOL_ENDPOINT: &str = "/ping"; // no trailing slash
const CONCURRENCY_LIMIT: usize = 64;

#[cfg(feature = "true-status")]
pub(crate) static PROXY: once_cell::sync::OnceCell<Option<Proxy>> =
    once_cell::sync::OnceCell::new();

#[derive(Parser, Debug)]
#[clap(version, about)]
pub(crate) struct Opts {
    /// Address for this server to listen on.
    #[arg(short, long, default_value = "127.0.0.1", env = "LUMINOUS_TTV_ADDR")]
    address: IpAddr,
    /// Port for this server to listen on.
    #[arg(short, long, default_value = "9595", env = "LUMINOUS_TTV_PORT")]
    server_port: u16,
    /// Connect directly to Twitch, without a proxy. Useful when running this server remotely
    /// in a country where Twitch doesn't serve ads.
    #[cfg_attr(feature = "hola", arg(long, conflicts_with_all(&["proxy", "country"])))]
    #[cfg_attr(not(feature = "hola"), arg(long, conflicts_with = "proxy"))]
    no_proxy: bool,
    /// Custom proxy to use, instead of Hola. Takes the form of 'scheme://host:port',
    /// where scheme is one of: http/https/socks5/socks5h.
    /// Must be in a country where Twitch doesn't serve ads for this system to work.
    #[cfg_attr(feature = "hola", arg(short, long))]
    #[cfg_attr(not(feature = "hola"), arg(short, long, required_unless_present = "no_proxy"))]
    proxy: Option<Url>,
    /// Country to request a proxy in. See https://client.hola.org/client_cgi/vpn_countries.json.
    #[cfg(feature = "hola")]
    #[arg(short, long, conflicts_with = "proxy", value_parser = parse_country, default_value = "ru")]
    country: String,
    /// Don't save Hola credentials.
    #[cfg(feature = "hola")]
    #[arg(short, long, conflicts_with = "proxy")]
    discard_creds: bool,
    /// Regenerate Hola credentials (don't load them).
    #[cfg(feature = "hola")]
    #[arg(short, long, conflicts_with = "proxy")]
    regen_creds: bool,
    /// List Hola's available countries, for use with --country
    #[cfg(feature = "hola")]
    #[arg(long)]
    list_countries: bool,
    /// Private key for TLS. Enables TLS if specified.
    #[cfg(feature = "tls")]
    #[arg(long, requires = "tls_cert", display_order = 4800)]
    tls_key: Option<PathBuf>,
    /// Server certificate for TLS.
    #[cfg(feature = "tls")]
    #[arg(long, display_order = 4801)]
    tls_cert: Option<PathBuf>,
    #[cfg(feature = "true-status")]
    #[arg(long, env = "LUMINOUS_TTV_STATUS_SECRET")]
    /// Secret for deep status endpoint, at /truestat/SECRET
    status_secret: String,
    /// Debug logging.
    #[arg(long, display_order = 5000, env = "LUMINOUS_TTV_DEBUG")]
    debug: bool,
    /// Twitch client ID used to access the API. Default is the ID of the website.
    #[arg(long, default_value = "kimne78kx3ncx6brgo4mv6wki5h1ko", env = "LUMINOUS_TTV_CLIENT_ID")]
    twitch_client_id: String,
    /// User-Agent header value to use with all requests. Currently not necessary or useful,
    /// provided because it seems potentially useful from comments I've seen. Overrides the normal
    /// copy-or-default system.
    #[arg(short, long, env = "LUMINOUS_TTV_USER_AGENT")]
    user_agent: Option<HeaderValue>,
}

// The "kimne..." client ID is shown in the clear if you load the main page.
// Try `curl -s https://www.twitch.tv | tidy -q | grep 'clientId='`.

#[cfg(feature = "hola")]
fn parse_country(input: &str) -> Result<String> {
    if input.len() != 2 {
        anyhow::bail!("Country argument invalid, must be 2 letters: {}", input);
    } // better to actually validate from the API, too lazy
    Ok(input.to_ascii_lowercase())
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts: Opts = Opts::parse();
    #[cfg(windows)]
    if let Err(code) = nu_ansi_term::enable_ansi_support() {
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
    let state = LState {
        client,
        twitch_client_id: Box::leak(opts.twitch_client_id.into_boxed_str()),
        user_agent: opts.user_agent,
    };

    #[allow(unused_mut)] // feature-gated
    let mut router = Router::new()
        .route(VOD_ENDPOINT, get(process_vod))
        .route(LIVE_ENDPOINT, get(process_live))
        .route(LIVE_TTVLOL_ENDPOINT, get(process_live))
        .route(STATUS_ENDPOINT, get(status))
        .route(STATUS_TTVLOL_ENDPOINT, get(status)); // all TTV-LOL cares about is HTTP 200
    #[cfg(feature = "true-status")]
    {
        router =
            router.route(&format!("/truestat/{}", opts.status_secret), get(status::deep_status));
    }
    let mut router = router.with_state(state);
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
    info!("About to start listening on {addr}");
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

#[derive(Clone, Debug)]
struct LState {
    client: Client,
    twitch_client_id: &'static str,
    // changing CID during operation isn't supported, so just leak it as a pointless optimization
    user_agent: Option<HeaderValue>,
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
    let mut cb = ClientBuilder::new().timeout(Duration::from_secs(20));
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

// in Chrome-like browsers the extension can download the M3U8, and if that succeeds redirect
// to it in Base64 form. In Firefox that isn't permitted. Checking if the server is online before
// redirecting to it reduces the chance of the extension breaking Twitch.
cfg_if! {
    if #[cfg(feature = "true-status")] {
        async fn status() -> Response<BoxBody> {
            let online = status::STATUS.load(std::sync::atomic::Ordering::Acquire);
            if online {
                (StatusCode::OK, Json(Status { online })).into_response()
            } else {
                (StatusCode::SERVICE_UNAVAILABLE, Json(Status { online })).into_response()
            }
        }
    } else {
        async fn status() -> Json<Status> {
            Json(Status { online: true })
        }
    }
}

pub(crate) struct ProcessData {
    sid: StreamID,
    query: HashMap<String, String>,
    user_agent: UserAgent,
}

impl ProcessData {
    fn build<F: FnOnce(String) -> StreamID>(
        id: String,
        query: HashMap<String, String>,
        ua: Option<TypedHeader<UserAgent>>,
        ua_override: Option<&HeaderValue>,
        enum_type: F,
    ) -> AppResult<Self> {
        let (id, query) = if let Some((id, query)) = id.split_once(".m3u8?") {
            // TTV-LOL encodes the query string into the path for some bizarre reason,
            // so this ignores the empty query map, splits out the query string, then
            // deserializes it to a map
            // (axum already did the first percent-decoding step)
            let query: HashMap<String, String> =
                serde_urlencoded::from_str(query).map_err(|e| AppError::Anyhow(e.into()))?;
            (id.to_ascii_lowercase(), query)
        } else {
            // normal path
            (id.into_ascii_lowercase(), query)
        };
        let user_agent = common::get_user_agent(ua, ua_override)?;
        Ok(Self { sid: enum_type(id), query, user_agent })
    }
}

// the User-Agent header is copied from the user if present by default
// when using this locally it's basically pointless, but for a remote server handling many users
// it should make it less detectable on Twitch's end (it'll look like more like a VPN endpoint or
// similar rather than an automated system)
// UAs shouldn't be individually identifiable in any remotely normal browser

type QueryMap = Query<HashMap<String, String>>;

async fn process_live(
    Path(id): Path<String>,
    Query(query): QueryMap,
    ua: Option<TypedHeader<UserAgent>>,
    State(state): State<LState>,
) -> Response<BoxBody> {
    let pd = match ProcessData::build(id, query, ua, state.user_agent.as_ref(), StreamID::Live) {
        Ok(pd) => pd,
        Err(e) => return e.into_response(),
    };
    process(pd, &state).await.into_response()
}

async fn process_vod(
    Path(id): Path<String>,
    Query(query): QueryMap,
    ua: Option<TypedHeader<UserAgent>>,
    State(state): State<LState>,
) -> Response<BoxBody> {
    let pd = match ProcessData::build(id, query, ua, state.user_agent.as_ref(), StreamID::VOD) {
        Ok(pd) => pd,
        Err(e) => return e.into_response(),
    };
    if let StreamID::VOD(s) = &pd.sid {
        if s.parse::<u64>().is_err() {
            return StatusCode::BAD_REQUEST.into_response();
        } // can't validate up front (which is cleaner) due to TTV-LOL emulation
    }
    process(pd, &state).await.into_response()
}

pub(crate) async fn process(pd: ProcessData, state: &LState) -> AppResult<Response<BoxBody>> {
    let token = get_token(state, &pd).await?;
    let m3u8 = get_m3u8(&state.client, &pd, token.data.playback_access_token).await?;
    Ok(([("Content-Type", "application/vnd.apple.mpegurl")], m3u8).into_response())
}

async fn get_m3u8(client: &Client, pd: &ProcessData, token: PlaybackAccessToken) -> Result<String> {
    const PERMITTED_INCOMING_KEYS: [&str; 11] = [
        "player_backend",             // mediaplayer
        "playlist_include_framerate", // true
        "reassignments_supported",    // true
        "supported_codecs",           // avc1, usually. sometimes vp09,avc1
        "cdm",                        // wv
        "player_version",             // 1.20.0
        "fast_bread",                 // true; related to low latency mode
        "allow_source",               // true
        "allow_audio_only",           // true
        "warp",                       // true; https://github.com/kixelated/warp-draft
        "transcode_mode",             // cbr_v1
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
        .append_pair("sig", &token.signature)
        .append_pair("acmb", "e30=");
    // "acmb" appears to be a tracking param, copy not permitted; value of e30= is empty object
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
async fn get_token(state: &LState, pd: &ProcessData) -> Result<AccessTokenResponse> {
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
    //  2023-06-02: it's definitely back
    Ok(state
        .client
        .post("https://gql.twitch.tv/gql")
        .header("Client-ID", state.twitch_client_id)
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

#[derive(Debug)]
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
            AppError::Anyhow(mut e) => {
                if let Some(e) = e.downcast_mut::<reqwest::Error>() {
                    if let Some(url) = e.url_mut() {
                        // vaporize query string since the token has the IP that twitch sees
                        // TODO: for a fix which preserves more info, copy the query string
                        //  except for p, play_session_id, token, sig, acmb
                        url.set_query(None);
                    }
                };
                let message = format!("{e:?}");
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
        match &self {
            Self::Live(channel) => format!("{BASE}api/channel/hls/{channel}.m3u8"),
            Self::VOD(id) => format!("{BASE}vod/{id}.m3u8"),
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

    (StatusCode::INTERNAL_SERVER_ERROR, Cow::from(format!("Unhandled internal error: {error}")))
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
