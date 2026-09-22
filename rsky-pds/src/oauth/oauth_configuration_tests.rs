use super::*;

const K256_HEX: &str = "9d5907143471e8f0e8df0f8b9512a8c5377878ee767f18fcf961055ecfc071cd";

#[tokio::test]
async fn include_expander_replaces_sets_and_fails_closed() {
    let expander = IncludeExpander::default();
    assert_eq!(
        expander.expand("atproto blob:image/*").await.unwrap(),
        "atproto blob:image/*"
    );
    expander
        .resolver
        .prime(
            "app.example.set",
            vec![crate::permission_set::repo_permission("app.example.record")],
        )
        .await;
    assert_eq!(
        expander
            .expand("atproto include:app.example.set transition:generic")
            .await
            .unwrap(),
        "atproto repo:?collection=app.example.record transition:generic"
    );
    // an audience on the include is not part of the set's name
    assert_eq!(
        expander
            .expand("include:app.example.set?aud=did:web:x%23y")
            .await
            .unwrap(),
        "repo:?collection=app.example.record"
    );
    // `.invalid` never resolves, so the set cannot be established
    let err = expander
        .expand("atproto include:invalid.example.nothing")
        .await
        .unwrap_err();
    assert!(err.0.contains("invalid.example.nothing"));
}

#[test]
fn device_cookie_is_persistent_site_wide_and_secure_on_https() {
    let cookie = device_cookie("dev-1", "ses-1", true);
    assert_eq!(cookie.name(), DEVICE_COOKIE);
    assert_eq!(cookie.value(), "dev-1.ses-1");
    assert_eq!(cookie.path(), Some("/"));
    assert_eq!(cookie.http_only(), Some(true));
    assert_eq!(cookie.secure(), Some(true));
    assert_eq!(cookie.same_site(), Some(SameSite::Lax));
    assert_eq!(cookie.max_age(), Some(DEVICE_COOKIE_MAX_AGE));
    assert_eq!(device_cookie("dev-1", "ses-1", false).secure(), Some(false));
    let probe = cookie_test_cookie(false);
    assert_eq!(probe.name(), COOKIE_TEST);
    assert_eq!(probe.path(), Some("/"));
    assert_eq!(probe.max_age(), None);
    assert_eq!(
        legacy_device_cookie_removal(),
        "device-id=; Path=/oauth; Max-Age=0; HttpOnly; SameSite=Lax"
    );
}

#[test]
fn signing_key_prefers_the_shared_secret() {
    assert!(matches!(
        signing_key_for(Some("secret".to_owned()), Some(K256_HEX.to_owned())),
        SigningKey::Symmetric(_)
    ));
    assert!(matches!(
        signing_key_for(None, Some(K256_HEX.to_owned())),
        SigningKey::Ec(_)
    ));
}

#[test]
#[should_panic(expected = "must be set")]
fn signing_key_requires_a_key() {
    signing_key_for(None, None);
}

#[tokio::test]
async fn replay_store_is_in_memory_without_redis() {
    let store = replay_store_for(None, None).await;
    assert!(store.unique("DPoP", "a", 1000).await.unwrap());
    assert!(!store.unique("DPoP", "a", 1000).await.unwrap());
}

/// Runs against the redis named by `TEST_REDIS_URL`, skipped without one.
#[tokio::test]
async fn replay_store_uses_redis_when_configured() {
    let Ok(url) = std::env::var("TEST_REDIS_URL") else {
        return;
    };
    let address = url.trim_start_matches("redis://").to_owned();
    let store = replay_store_for(Some(address), Some(String::new())).await;
    let nonce = format!("jti-{}", rsky_common::get_random_str());
    assert!(store.unique("DPoP", &nonce, 60_000).await.unwrap());
    assert!(!store.unique("DPoP", &nonce, 60_000).await.unwrap());
}

#[tokio::test]
#[should_panic(expected = "redis is unreachable")]
async fn replay_store_refuses_to_start_without_its_redis() {
    replay_store_for(Some("127.0.0.1:1".to_owned()), None).await;
}
