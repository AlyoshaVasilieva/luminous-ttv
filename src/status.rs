use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Context, Result};
use axum::{extract::State, http::StatusCode};
use axum_extra::headers::UserAgent;
use http::header::USER_AGENT;
use rand::prelude::IteratorRandom;
use rand::rng;
use serde::Deserialize;
use serde_json::json;

use crate::{common, create_client, generate_id, AppError, LState, ProcessData, StreamID};

pub(crate) static STATUS: AtomicBool = AtomicBool::new(true);

/// Point something like UptimeRobot/Caddy at this endpoint, it needs to be routinely hit
pub(crate) async fn deep_status(State(mut state): State<LState>) -> StatusCode {
    let client = create_client(state.proxy.clone()).unwrap();
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
    query.insert("supported_codecs", "av1,h264");
    query.insert("cdm", "wv");
    query.insert("player_version", "1.46.0-rc.3");
    query.insert("allow_source", "true");
    query.insert("fast_bread", "true");
    query.insert("playlist_include_framerate", "true");
    query.insert("reassignments_supported", "true");
    query.insert("transcode_mode", "cbr_v1");
    query.insert("enable_score", "true");
    query.insert("multigroup_video", "false");
    query.insert("platform", "web");
    query.insert("include_unavailable", "false");
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
                "sha256Hash": "14fee5369fafaecbb6203b941c4b1bdf73f9782274f965f618f28e34f6bb5537"
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
    Ok(get_broadcaster_login_from_streams(res)?)
}

fn get_broadcaster_login_from_streams(gqlr: GQLResponse) -> Result<String> {
    let broadcaster = gqlr
        .data
        .featured_streams
        .iter()
        .filter_map(|s| s.stream.as_ref())
        .filter(|s| s.stream_type.eq_ignore_ascii_case("live"))
        // for some reason broadcaster can be null
        .filter_map(|s| s.broadcaster.clone())
        .filter(|broadcaster| !broadcaster.login.starts_with("prime"))
        .choose(&mut rng())
        .ok_or_else(|| anyhow!("no streams available"))?;
    // streams named "prime*" are removed because they're Prime Video
    Ok(broadcaster.login.clone())
}

#[derive(Clone, Debug, Deserialize)]
struct GQLResponse {
    pub(crate) data: Data,
}

#[derive(Clone, Debug, Deserialize)]
struct Data {
    #[serde(rename = "featuredStreams")]
    pub(crate) featured_streams: Vec<FeaturedStream>,
}

#[derive(Clone, Debug, Deserialize)]
struct FeaturedStream {
    pub(crate) stream: Option<Stream>,
}

#[derive(Clone, Debug, Deserialize)]
struct Stream {
    pub(crate) broadcaster: Option<Broadcaster>,
    #[serde(rename = "type")]
    pub(crate) stream_type: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Broadcaster {
    pub(crate) login: String,
}

#[cfg(test)]
mod test {
    use crate::status::get_broadcaster_login_from_streams;
    use crate::status::GQLResponse;
    #[test]
    fn getting_broadcaster_works() {
        // XXX: update this every once in a while
        let input = r#"{"data":{"featuredStreams":[{"itemTrackingID":"fd53e3f3-c3ce-450d-a0dd-293483ea87b1","isScheduled":true,"isSponsored":false,"priorityLevel":5,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":{"displayName":"NoodleBeefNoodle","id":"98200969","profileImageURL":"https://static-cdn.jtvnw.net/jtv_user_pictures/d631278c-aafc-484f-bb06-e6bcd8dcdfdf-profile_image-150x150.png","login":"noodlebeefnoodle","__typename":"User"},"game":{"id":"27284","slug":"retro","name":"Retro","displayName":"Retro","__typename":"Game"},"id":"314983180497","type":"live","viewersCount":2317,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_noodlebeefnoodle-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"},{"itemTrackingID":"3115dd1b-d283-4b78-af1f-4b93d3793b29","isScheduled":true,"isSponsored":false,"priorityLevel":5,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":{"displayName":"CliffTerios","id":"57102629","profileImageURL":"https://static-cdn.jtvnw.net/jtv_user_pictures/06b41d73-9282-4df1-8f68-2c5c76b7d34c-profile_image-150x150.png","login":"cliffterios","__typename":"User"},"game":{"id":"490377","slug":"sea-of-thieves","name":"Sea of Thieves","displayName":"Sea of Thieves","__typename":"Game"},"id":"315703774566","type":"live","viewersCount":164,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_cliffterios-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"},{"itemTrackingID":"f16c4b2b-f6de-452a-bc48-0578fffaeb40","isScheduled":true,"isSponsored":false,"priorityLevel":5,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":{"displayName":"Spammals","id":"98058108","profileImageURL":"https://static-cdn.jtvnw.net/jtv_user_pictures/8ebc056c-8c89-4892-994e-0d5ca2d50622-profile_image-150x150.png","login":"spammals","__typename":"User"},"game":{"id":"490377","slug":"sea-of-thieves","name":"Sea of Thieves","displayName":"Sea of Thieves","__typename":"Game"},"id":"314983504465","type":"live","viewersCount":28,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_spammals-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"},{"itemTrackingID":"c516883e-613a-4b55-8989-ae5459364505","isScheduled":true,"isSponsored":false,"priorityLevel":5,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":{"displayName":"poopernoodle","id":"179065334","profileImageURL":"https://static-cdn.jtvnw.net/jtv_user_pictures/e76dc2c3-3a06-48d0-99d2-690d9aa128e3-profile_image-150x150.png","login":"poopernoodle","__typename":"User"},"game":{"id":"1680364352","slug":"light-up-the-town","name":"Light Up the Town","displayName":"Light Up the Town","__typename":"Game"},"id":"316689189211","type":"live","viewersCount":2510,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_poopernoodle-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"},{"itemTrackingID":"3b58f8e6-760f-46b0-9555-8f80ddad400b","isScheduled":true,"isSponsored":false,"priorityLevel":5,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":{"displayName":"ebacon1_","id":"237604958","profileImageURL":"https://static-cdn.jtvnw.net/jtv_user_pictures/d9781769-cbb1-4244-8678-098f5d039f97-profile_image-150x150.png","login":"ebacon1_","__typename":"User"},"game":{"id":"490377","slug":"sea-of-thieves","name":"Sea of Thieves","displayName":"Sea of Thieves","__typename":"Game"},"id":"315419111908","type":"live","viewersCount":43,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_ebacon1_-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"},{"itemTrackingID":"0f2ebe93-9595-4f0b-9084-e7c3b7299ded","isScheduled":true,"isSponsored":false,"priorityLevel":5,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":{"displayName":"xMyPetCactusx","id":"48528373","profileImageURL":"https://static-cdn.jtvnw.net/jtv_user_pictures/2093936f-c3f9-4152-9669-dada070645b8-profile_image-150x150.png","login":"xmypetcactusx","__typename":"User"},"game":{"id":"511391","slug":"hollow-knight-silksong","name":"Hollow Knight: Silksong","displayName":"Hollow Knight: Silksong","__typename":"Game"},"id":"316509930585","type":"live","viewersCount":2353,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_xmypetcactusx-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"},{"itemTrackingID":"5482e18d-d1d5-4711-b0ed-8487c3d26b6f","isScheduled":false,"isSponsored":false,"priorityLevel":6,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":{"displayName":"ironmouse","id":"175831187","profileImageURL":"https://static-cdn.jtvnw.net/jtv_user_pictures/c2aca19b-b318-4398-99a5-fb6a597536fd-profile_image-150x150.png","login":"ironmouse","__typename":"User"},"game":{"id":"1680364352","slug":"light-up-the-town","name":"Light Up the Town","displayName":"Light Up the Town","__typename":"Game"},"id":"316278917852","type":"live","viewersCount":10362,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_ironmouse-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"},{"itemTrackingID":"2539c72b-bb6d-49fa-9200-de98b2b06028","isScheduled":true,"isSponsored":false,"priorityLevel":6,"sourceType":"PROMOTION","description":"","stream":{"broadcaster":null,"game":null,"id":"315461715943","type":"live","viewersCount":1,"previewImageURL":"https://static-cdn.jtvnw.net/previews-ttv/live_user_dharmannstudioslive-320x180.jpg","__typename":"Stream"},"title":"","version":2,"__typename":"FeaturedStream"}]},"extensions":{"durationMilliseconds":56,"operationName":"FeaturedContentCarouselStreams","requestID":"12345"}}"#;
        let gqlr: GQLResponse = serde_json::from_str(input).unwrap();
        get_broadcaster_login_from_streams(gqlr).unwrap();
    }
}
