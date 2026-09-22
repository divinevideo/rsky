use super::*;
use crate::read_after_write::types::RecordDescript;
use rocket::local::asynchronous::Client;
use serde_json::{json, Value};
use std::str::FromStr;

#[rocket::get("/munged")]
fn munged() -> ReadAfterWriteResponse<Value> {
    ReadAfterWriteResponse::HandlerResponse(
        format_munged_response(json!({"displayName":"Just written"}), Some(42)).unwrap(),
    )
}

#[rocket::get("/passthrough")]
fn passthrough() -> ReadAfterWriteResponse<Value> {
    ReadAfterWriteResponse::HandlerPipeThrough(HandlerPipeThrough {
        encoding: "application/octet-stream".into(),
        buffer: vec![0, 255, 42],
        headers: Some(BTreeMap::from([("retry-after".into(), "7".into())])),
    })
}

#[rocket::async_test]
async fn responders_preserve_raw_bytes_and_publish_munged_body_and_lag() {
    let client =
        Client::untracked(rocket::build().mount("/", rocket::routes![munged, passthrough]))
            .await
            .unwrap();
    let response = client.get("/munged").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        response.headers().get_one("Atproto-Upstream-Lag"),
        Some("42")
    );
    assert_eq!(
        response.into_json::<Value>().await.unwrap(),
        json!({"displayName":"Just written"})
    );
    let response = client.get("/passthrough").dispatch().await;
    assert_eq!(response.headers().get_one("retry-after"), Some("7"));
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some("application/octet-stream")
    );
    assert_eq!(response.into_bytes().await.unwrap(), vec![0, 255, 42]);
}

#[test]
fn lag_uses_oldest_profile_or_post_and_rejects_invalid_time() {
    let mut local = LocalRecords {
        count: 0,
        profile: None,
        posts: vec![],
    };
    assert_eq!(get_local_lag(&local).unwrap(), None);
    let cid =
        lexicon_cid::Cid::from_str("bafyreie5cvv4xcz3kbwdhmwbfvpvbcg6aicjkvw5pluvbhqz5dg4z5f2pa")
            .unwrap();
    let profile_at = chrono::Utc::now() - chrono::Duration::seconds(10);
    local.profile = Some(RecordDescript {
        uri: rsky_syntax::aturi::AtUri::new(
            "at://did:plc:lagfixture/app.bsky.actor.profile/self".into(),
            None,
        )
        .unwrap(),
        cid,
        indexed_at: profile_at.to_rfc3339(),
        record: serde_json::from_value(json!({"$type":"app.bsky.actor.profile"})).unwrap(),
    });
    for seconds in [20, 5] {
        local.posts.push(RecordDescript {
            uri: rsky_syntax::aturi::AtUri::new(format!("at://did:plc:lagfixture/app.bsky.feed.post/{seconds}"), None).unwrap(),
            cid, indexed_at: (chrono::Utc::now()-chrono::Duration::seconds(seconds)).to_rfc3339(),
            record: serde_json::from_value(json!({"$type":"app.bsky.feed.post","text":"lag test","createdAt":"2026-01-01T00:00:00.000Z"})).unwrap(),
        });
    }
    let lag = get_local_lag(&local).unwrap().unwrap();
    assert!((20_000..25_000).contains(&lag), "lag was {lag}");
    local.profile = None;
    assert!(get_local_lag(&local).unwrap().unwrap() >= 20_000);
    local.posts[0].indexed_at = "!invalid time".into();
    assert!(get_local_lag(&local).is_err());
    assert!(format_munged_response(json!({}), None)
        .unwrap()
        .headers
        .is_none());
}
