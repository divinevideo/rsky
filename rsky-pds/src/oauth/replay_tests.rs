use super::*;

/// Runs against the redis named by `TEST_REDIS_URL`; the store is
/// exercised for real, so the test is skipped without one.
#[tokio::test]
async fn redis_store_tracks_nonces_per_namespace() {
    let Ok(url) = std::env::var("TEST_REDIS_URL") else {
        return;
    };
    let store = RedisReplayStore::connect(&url).await.unwrap();
    let nonce = format!("jti-{}", rsky_common::get_random_str());
    assert!(store.unique("DPoP", &nonce, 60_000).await.unwrap());
    assert!(!store.unique("DPoP", &nonce, 60_000).await.unwrap());
    assert!(store.unique("DPoP@client", &nonce, 60_000).await.unwrap());
    assert!(store
        .unique("DPoP", &format!("{nonce}-short"), 1)
        .await
        .unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(store
        .unique("DPoP", &format!("{nonce}-short"), 1)
        .await
        .unwrap());
    assert_eq!(RedisReplayStore::key("DPoP", "x"), "nonces:DPoP:x");
    let broken = RedisReplayStore::connect("redis://127.0.0.1:1").await;
    assert!(broken.is_err());
}
