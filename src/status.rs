use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Result};
use axum::extract::Path;
use axum::headers::UserAgent;
use axum::http::StatusCode;
use rand::prelude::IteratorRandom;
use reqwest_middleware::ClientWithMiddleware as Client;
use serde::Deserialize;
use serde_json::json;

use crate::{common, create_client, generate_id, AppError, ProcessData, StreamID, TWITCH_CLIENT};

/// To be proper this should be read at runtime, but really who cares
const SECRET_KEY: &str = env!("LUMINOUS_TTV_STATUS_SECRET");

pub(crate) static STATUS: AtomicBool = AtomicBool::new(true);

/// Point something like UptimeRobot at this endpoint, it needs to be routinely hit
pub(crate) async fn deep_status(Path(secret): Path<String>) -> StatusCode {
    if secret != SECRET_KEY {
        return StatusCode::FORBIDDEN;
    }

    // purposefully not reusing client
    let client = create_client(crate::PROXY.get().unwrap().clone()).unwrap();
    match test_random_stream(&client).await {
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

async fn test_random_stream(client: &Client) -> Result<()> {
    let login = find_random_stream(client).await?;
    let mut query = HashMap::with_capacity(8);
    query.insert("player_backend", "mediaplayer");
    query.insert("supported_codecs", "avc1");
    query.insert("cdm", "wv");
    query.insert("player_version", "1.13.0");
    query.insert("allow_source", "true");
    query.insert("fast_bread", "true");
    query.insert("playlist_include_framerate", "true");
    query.insert("reassignments_supported", "true");
    let pd = ProcessData {
        sid: StreamID::Live(login),
        query: query.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
        user_agent: UserAgent::from_static(common::USER_AGENT),
    };
    match crate::process(pd, client).await {
        Ok(_) => Ok(()),
        Err(AppError::Anyhow(e)) => Err(e),
    }
}

async fn find_random_stream(client: &Client) -> Result<String> {
    let req = json!({
        "operationName": "FeaturedContentCarouselStreams",
        "variables": {
            "language": "en",
            "first": 8,
            "acceptedMature": true,
            "freeformTagsEnabled": false
        },
        "extensions": {
            "persistedQuery": {
                "version": 1,
                "sha256Hash": "9c7fbd1422b1ea5c2eab3d0fd496b686ac66649e9023e649947e757204dff00e"
            }
        }
    });
    let res: GQLResponse = client
        .post("https://gql.twitch.tv/gql")
        .header("Client-ID", TWITCH_CLIENT)
        .header("Device-ID", &generate_id())
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
        .choose(&mut common::get_rng())
        .ok_or_else(|| anyhow!("no streams available"))?;
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
