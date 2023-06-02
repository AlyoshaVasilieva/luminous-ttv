use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Context, Result};
use axum::{extract::State, headers::UserAgent, http::StatusCode};
use http::header::USER_AGENT;
use rand::prelude::IteratorRandom;
use serde::Deserialize;
use serde_json::json;

use crate::{common, create_client, generate_id, AppError, LState, ProcessData, StreamID};

pub(crate) static STATUS: AtomicBool = AtomicBool::new(true);

/// Point something like UptimeRobot/Caddy at this endpoint, it needs to be routinely hit
pub(crate) async fn deep_status(State(mut state): State<LState>) -> StatusCode {
    let client = create_client(crate::PROXY.get().unwrap().clone()).unwrap();
    state.client = client;
    // purposefully not reusing client
    match test_random_stream(&state).await {
        Ok(_) => {
            STATUS.store(true, Ordering::Release);
            StatusCode::OK
        }
        Err(e) => {
            tracing::error!("Status check failed: {} / {:?}", e, e);
            STATUS.store(false, Ordering::Release);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn test_random_stream(state: &LState) -> Result<()> {
    let user_agent = common::get_user_agent(None, state.user_agent.as_ref())?;
    let login = find_random_stream(state, &user_agent).await.context("find_random_stream")?;
    let mut query = HashMap::with_capacity(9);
    query.insert("player_backend", "mediaplayer");
    query.insert("supported_codecs", "avc1");
    query.insert("cdm", "wv");
    query.insert("player_version", "1.18.0");
    query.insert("allow_source", "true");
    query.insert("fast_bread", "true");
    query.insert("playlist_include_framerate", "true");
    query.insert("reassignments_supported", "true");
    query.insert("transcode_mode", "cbr_v1");
    let pd = ProcessData {
        sid: StreamID::Live(login),
        query: query.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
        user_agent,
    };
    match crate::process(pd, state).await {
        Ok(_) => Ok(()),
        Err(AppError::Anyhow(e)) => Err(e),
    }
    .context("process")
}

async fn find_random_stream(state: &LState, ua: &UserAgent) -> Result<String> {
    let req = json!({
        "operationName": "FeaturedContentCarouselStreams",
        "variables": {
            "language": "en",
            "first": 8,
            "acceptedMature": true,
        },
        "extensions": {
            "persistedQuery": {
                "version": 1,
                "sha256Hash": "1fc22cf18e3afe09cb56e10181ff25073818b80f07dfca546c8aa3bc1ad15f76"
            }
        }
    });
    let res: GQLResponse = state
        .client
        .post("https://gql.twitch.tv/gql")
        .header("Client-ID", state.twitch_client_id)
        .header("Device-ID", &generate_id())
        .header(USER_AGENT, ua.as_str())
        .json(&req)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let stream = res
        .data
        .featured_streams
        .iter()
        .filter_map(|s| s.stream.as_ref())
        .filter(|s| s.stream_type.eq_ignore_ascii_case("live"))
        .filter(|s| !s.broadcaster.login.starts_with("prime"))
        .choose(&mut common::get_rng())
        .ok_or_else(|| anyhow!("no streams available"))?;
    // streams named "prime*" are removed because they're Prime Video
    Ok(stream.broadcaster.login.clone())
}

#[derive(Debug, Deserialize)]
struct GQLResponse {
    pub(crate) data: Data,
}

#[derive(Debug, Deserialize)]
struct Data {
    #[serde(rename = "featuredStreams")]
    pub(crate) featured_streams: Vec<FeaturedStream>,
}

#[derive(Debug, Deserialize)]
struct FeaturedStream {
    pub(crate) stream: Option<Stream>,
}

#[derive(Debug, Deserialize)]
struct Stream {
    pub(crate) broadcaster: Broadcaster,
    #[serde(rename = "type")]
    pub(crate) stream_type: String,
}

#[derive(Debug, Deserialize)]
struct Broadcaster {
    #[serde(rename = "login")]
    pub(crate) login: String,
}
