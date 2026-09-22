use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::Client;
use serde_json::Value;
use std::ffi::OsString;

mod common;

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

async fn assert_metadata(client: &Client, authorization_server: &str) {
    let resource = client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .service
        .public_url
        .clone();
    let response = client
        .get("/.well-known/oauth-protected-resource")
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some(ContentType::JSON.to_string().as_str())
    );
    assert!(response.headers().get_one("Location").is_none());

    let body = response.into_json::<Value>().await.expect("json body");
    assert_eq!(body["resource"], resource);
    assert_eq!(
        body["authorization_servers"],
        serde_json::json!([authorization_server])
    );
    assert_eq!(body["scopes_supported"], serde_json::json!([]));
    assert_eq!(
        body["bearer_methods_supported"],
        serde_json::json!(["header"])
    );
    assert_eq!(body["resource_documentation"], "https://atproto.com");
}

#[tokio::test]
async fn oauth_metadata_keeps_default_and_explicit_entryway_overrides() {
    let _entryway_guard = EnvVarGuard::unset("PDS_ENTRYWAY_URL");
    let _authorization_guard = EnvVarGuard::unset("PDS_OAUTH_AUTHORIZATION_SERVER");

    let (_default_dir, default_client) = common::get_client().await;
    assert_metadata(&default_client, rsky_pds::config::DEFAULT_ENTRYWAY_URL).await;

    let configured_authorization_server = "https://oauth.example.invalid";
    std::env::set_var(
        "PDS_OAUTH_AUTHORIZATION_SERVER",
        configured_authorization_server,
    );
    let (_configured_dir, configured_client) = common::get_client().await;
    assert_metadata(&configured_client, configured_authorization_server).await;

    let configured_entryway = "https://entryway.example.invalid";
    std::env::set_var("PDS_ENTRYWAY_URL", configured_entryway);
    let (_entryway_dir, entryway_client) = common::get_client().await;
    assert_metadata(&entryway_client, configured_entryway).await;
}
