use anyhow::{Context, Result};
use axum::headers::{Header, UserAgent};
use axum::TypedHeader;
use http::HeaderValue;
use rand::Rng;

// use ESR user-agent if we don't have anything else
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:109.0) \
Gecko/20100101 Firefox/115.0";

pub(crate) fn get_rng() -> impl Rng {
    rand::thread_rng()
}

pub(crate) fn get_user_agent(
    ua: Option<TypedHeader<UserAgent>>, // inbound user UA
    ua_override: Option<&HeaderValue>,
) -> Result<UserAgent> {
    Ok(if let Some(ua) = ua_override {
        UserAgent::decode(&mut std::iter::once(ua)).context("decoding User-Agent")?
    } else {
        ua.map(|ua| ua.0).unwrap_or_else(|| UserAgent::from_static(USER_AGENT))
    })
}
