use super::{
    cached_proxy_target, remember_proxy_target, PROXY_TARGETS, PROXY_TARGET_CAPACITY,
    PROXY_TARGET_TTL,
};

#[test]
fn proxy_targets_are_remembered_until_they_expire() {
    let header = "did:web:cache.test#bsky_appview";
    assert_eq!(cached_proxy_target(header), None);
    remember_proxy_target(header, "https://cache.test");
    assert_eq!(
        cached_proxy_target(header).as_deref(),
        Some("https://cache.test")
    );
    remember_proxy_target(header, "https://cache.test/again");
    assert_eq!(
        cached_proxy_target(header).as_deref(),
        Some("https://cache.test/again")
    );
    {
        let mut targets = PROXY_TARGETS.write().unwrap();
        let expired = std::time::Instant::now() - PROXY_TARGET_TTL * 2;
        targets.insert(
            header.to_owned(),
            ("https://cache.test".to_owned(), expired),
        );
        for i in 0..PROXY_TARGET_CAPACITY {
            targets.insert(
                format!("did:web:full{i}#svc"),
                ("https://full.test".to_owned(), expired),
            );
        }
    }
    assert_eq!(cached_proxy_target(header), None);
    remember_proxy_target(header, "https://cache.test/fresh");
    assert_eq!(
        cached_proxy_target(header).as_deref(),
        Some("https://cache.test/fresh")
    );
    assert!(PROXY_TARGETS.read().unwrap().len() <= 2);
    {
        let mut targets = PROXY_TARGETS.write().unwrap();
        for i in 0..PROXY_TARGET_CAPACITY {
            targets.insert(
                format!("did:web:recent{i}#svc"),
                ("https://recent.test".to_owned(), std::time::Instant::now()),
            );
        }
    }
    remember_proxy_target("did:web:overflow.test#svc", "https://overflow.test");
    assert_eq!(cached_proxy_target(header), None);
    assert_eq!(
        cached_proxy_target("did:web:overflow.test#svc").as_deref(),
        Some("https://overflow.test")
    );
}
