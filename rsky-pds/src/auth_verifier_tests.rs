use super::*;

// base64("admin:password")
const CREDS: &str = "YWRtaW46cGFzc3dvcmQ=";

fn jwt_with_payload(payload: &str) -> String {
    format!("eyJhbGciOiJFUzI1NksifQ.{}.sig", base64url.encode(payload))
}

#[tokio::test]
async fn dpop_authentication_requires_a_configured_provider() {
    let client = rocket::local::asynchronous::Client::untracked(rocket::build())
        .await
        .unwrap();
    let request =
        client
            .get("/xrpc/com.atproto.server.getSession")
            .header(rocket::http::Header::new(
                "Authorization",
                "DPoP synthetic-token",
            ));
    let error = validate_access_token(request.inner(), vec![AuthScope::AppPass], None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "OAuth provider is not configured");
}

#[test]
fn only_a_payload_naming_a_method_is_service_auth() {
    assert!(jwt_names_method(&jwt_with_payload(
        r#"{"iss":"did:web:a.invalid","lxm":"com.atproto.repo.uploadBlob"}"#
    )));
    assert!(!jwt_names_method(&jwt_with_payload(r#"{"lxm":null}"#)));
    assert!(!jwt_names_method(&jwt_with_payload(
        r#"{"sub":"did:web:a.invalid","scope":"com.atproto.access"}"#
    )));
    // standard base64 payloads from older builds are still decoded
    assert!(jwt_names_method(&format!(
        "h.{}.s",
        base64pad.encode(r#"{"lxm":"com.example.a"}"#)
    )));
    assert!(!jwt_names_method(&jwt_with_payload("not json")));
    assert!(!jwt_names_method("h.%%%.s"));
    assert!(!jwt_names_method("no-dots"));
}

#[test]
fn auth_scopes_round_trip_through_their_wire_names() {
    for scope in [
        AuthScope::Access,
        AuthScope::Refresh,
        AuthScope::AppPass,
        AuthScope::AppPassPrivileged,
        AuthScope::SignupQueued,
        AuthScope::Takendown,
    ] {
        assert_eq!(AuthScope::from_str(scope.as_str()).unwrap(), scope);
    }
    assert!(AuthScope::from_str("com.atproto.nope").is_err());
    assert!(!AuthScope::Takendown.is_privileged());
}

#[test]
fn oauth_scope_mapping_follows_transition_semantics() {
    let scopes = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    assert_eq!(
        oauth_scopes_to_auth_scope(&scopes(&["atproto", "transition:generic"])).unwrap(),
        AuthScope::AppPass
    );
    assert_eq!(
        oauth_scopes_to_auth_scope(&scopes(&[
            "atproto",
            "transition:generic",
            "transition:chat.bsky"
        ]))
        .unwrap(),
        AuthScope::AppPassPrivileged
    );
    // chat access alone still maps to privileged app-password access
    assert_eq!(
        oauth_scopes_to_auth_scope(&scopes(&["atproto", "transition:chat.bsky"])).unwrap(),
        AuthScope::AppPassPrivileged
    );
    // atproto alone grants no legacy access level
    assert!(oauth_scopes_to_auth_scope(&scopes(&["atproto"])).is_err());
    // missing the mandatory atproto scope is rejected outright
    assert!(oauth_scopes_to_auth_scope(&scopes(&["transition:generic"])).is_err());
    assert!(oauth_scopes_to_auth_scope(&[]).is_err());
}

#[test]
fn oauth_scope_mapping_accepts_permission_set_sessions() {
    let scopes = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    // The exact base scope a permission-set client declares: no
    // transition grant, so this was refused before the modern forms
    // were recognised.
    assert_eq!(
        oauth_scopes_to_auth_scope(&scopes(&[
            "atproto",
            "include:app.bulleted.authFull",
            "blob:image/*"
        ]))
        .unwrap(),
        AuthScope::AppPass
    );
    // each permission-grant form on its own is sufficient
    for grant in [
        "repo:app.bsky.feed.post",
        "blob:image/*",
        "rpc:com.example.method",
        "include:app.example.set",
        "space:app.bulleted.space?action=read",
    ] {
        assert_eq!(
            oauth_scopes_to_auth_scope(&scopes(&["atproto", grant])).unwrap(),
            AuthScope::AppPass,
            "{grant} should map to app-password level"
        );
    }
    // a chat transition still wins over a permission grant
    assert_eq!(
        oauth_scopes_to_auth_scope(&scopes(&[
            "atproto",
            "include:app.bulleted.authFull",
            "transition:chat.bsky"
        ]))
        .unwrap(),
        AuthScope::AppPassPrivileged
    );
    // an unrecognised token is not a permission grant
    assert!(oauth_scopes_to_auth_scope(&scopes(&["atproto", "nonsense"])).is_err());
}

fn assert_admin(parsed: Option<BasicAuth>) {
    let parsed = parsed.expect("expected successful parse");
    assert_eq!(parsed.username, "admin");
    assert_eq!(parsed.password, "password");
}

#[test]
fn parses_normal_basic_auth() {
    assert_admin(parse_basic_auth(&format!("Basic {CREDS}")));
}

#[test]
fn tolerates_extra_whitespace() {
    assert_admin(parse_basic_auth(&format!("Basic  {CREDS}")));
    assert_admin(parse_basic_auth(&format!("Basic \t {CREDS}")));
}

#[test]
fn tolerates_trailing_whitespace() {
    assert_admin(parse_basic_auth(&format!("Basic {CREDS} ")));
    assert_admin(parse_basic_auth(&format!("  Basic {CREDS}  ")));
}

#[test]
fn preserves_colons_in_password() {
    // base64("admin:pass:word")
    let parsed = parse_basic_auth("Basic YWRtaW46cGFzczp3b3Jk").expect("expected parse");
    assert_eq!(parsed.username, "admin");
    assert_eq!(parsed.password, "pass:word");
}

// Single test: sequential env mutation stays deterministic across threads
#[test]
fn admin_password_prefers_upstream_env_name() {
    env::remove_var("PDS_ADMIN_PASSWORD");
    env::remove_var("PDS_ADMIN_PASS");
    assert_eq!(admin_password_from_env(), None);

    env::set_var("PDS_ADMIN_PASS", "legacy");
    assert_eq!(admin_password_from_env(), Some("legacy".to_string()));

    env::set_var("PDS_ADMIN_PASSWORD", "standard");
    assert_eq!(admin_password_from_env(), Some("standard".to_string()));

    env::remove_var("PDS_ADMIN_PASSWORD");
    env::remove_var("PDS_ADMIN_PASS");
}

#[test]
fn rejects_garbage() {
    assert!(parse_basic_auth("").is_none());
    assert!(parse_basic_auth("Basic").is_none());
    assert!(parse_basic_auth("Basic not-base64!").is_none());
    assert!(parse_basic_auth(&format!("Bearer {CREDS}")).is_none());
    assert!(parse_basic_auth(&format!("Basic {CREDS} extra")).is_none());
    // base64("no-colon")
    assert!(parse_basic_auth("Basic bm8tY29sb24=").is_none());
}
