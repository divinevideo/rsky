use super::*;
use crate::actor_store::db::get_migrated_db;

#[tokio::test]
async fn unregister_removes_only_the_matching_repo_notification() {
    let dir = tempfile::tempdir().unwrap();
    let db = get_migrated_db(dir.path().join("actor.sqlite"))
        .await
        .unwrap();
    let store = SpaceStore::new("did:example:author".to_string(), db);
    let first_space = SpaceId::new("did:example:owner", "com.example.forum", "first").uri();
    let second_space = SpaceId::new("did:example:owner", "com.example.forum", "second").uri();
    let first = Subscriber {
        endpoint: "https://first.example/notify".to_string(),
        service: None,
    };
    let second = Subscriber {
        endpoint: "https://second.example/notify".to_string(),
        service: None,
    };
    let expires = "2999-01-01T00:00:00.000Z";
    store
        .register_repo_notify(&first_space, &first, expires)
        .await
        .unwrap();
    store
        .register_repo_notify(&first_space, &second, expires)
        .await
        .unwrap();
    store
        .register_repo_notify(&second_space, &first, expires)
        .await
        .unwrap();

    store
        .unregister_repo_notify(&first_space, &first.endpoint)
        .await
        .unwrap();

    assert_eq!(
        store
            .repo_notify_endpoints(&first_space, "2026-01-01T00:00:00.000Z")
            .await
            .unwrap(),
        vec![second]
    );
    assert_eq!(
        store
            .repo_notify_endpoints(&second_space, "2026-01-01T00:00:00.000Z")
            .await
            .unwrap(),
        vec![first]
    );
}
