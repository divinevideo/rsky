use super::*;
use crate::oauth_scope::{AccountAction, RepoAction};

const POST: &str = "app.bsky.feed.post";

fn creds(granted: &[&str]) -> Option<Credentials> {
    Some(Credentials {
        r#type: "oauth".to_string(),
        did: Some("did:plc:test".to_string()),
        scope: Some(AuthScope::AppPass),
        granted_scopes: Some(granted.iter().map(|s| s.to_string()).collect()),
        audience: None,
        token_id: None,
        aud: None,
        iss: None,
        is_privileged: None,
    })
}

/// The four synchronous resource checks, plus the gate + `allows_rpc`
/// pair that `pipethrough::assert_rpc_scope` performs once it has
/// resolved an audience (that helper needs a live `ProxyRequest`, so its
/// scope decision is exercised here rather than through the route).
fn denials(credentials: &Option<Credentials>) -> [bool; 5] {
    let rpc_denied = match scoped_session(
        credentials.as_ref().and_then(|c| c.granted_scopes.as_ref()),
        true,
    ) {
        Some(scopes) => !scopes.allows_rpc("app.bsky.feed.getTimeline", "did:web:api.example"),
        None => false,
    };
    [
        assert_repo_scope(credentials, POST, RepoAction::Create).is_err(),
        assert_blob_scope(credentials, "image/png").is_err(),
        rpc_denied,
        assert_identity_scope(credentials, "handle").is_err(),
        assert_account_scope(credentials, "email", AccountAction::Manage).is_err(),
    ]
}

#[test]
fn blob_scope_allows_and_denies_by_mime() {
    let allowed = creds(&["atproto", "blob:image/*"]);
    assert!(assert_blob_scope(&allowed, "image/png").is_ok());
    assert!(matches!(
        assert_blob_scope(&allowed, "video/mp4"),
        Err(ApiError::InsufficientScope(_))
    ));

    // No `blob:` grant at all. Enforcement is gated on the auth source,
    // not on whether the session happens to hold a grant of this kind, so
    // absence of the grant is a denial.
    let no_blob_grant = creds(&["atproto", "repo:app.bsky.feed.post"]);
    assert!(matches!(
        assert_blob_scope(&no_blob_grant, "video/mp4"),
        Err(ApiError::InsufficientScope(_))
    ));

    // Legacy app-password sessions carry no `granted_scopes` at all.
    assert!(assert_blob_scope(&None, "video/mp4").is_ok());
}

#[test]
fn identity_scope_allows_and_denies_by_attribute() {
    let allowed = creds(&["atproto", "identity:handle"]);
    assert!(assert_identity_scope(&allowed, "handle").is_ok());

    let wrong_attr = creds(&["atproto", "identity:invalid"]);
    assert!(matches!(
        assert_identity_scope(&wrong_attr, "handle"),
        Err(ApiError::InsufficientScope(_))
    ));

    let no_identity_grant = creds(&["atproto", "repo:app.bsky.feed.post"]);
    assert!(matches!(
        assert_identity_scope(&no_identity_grant, "handle"),
        Err(ApiError::InsufficientScope(_))
    ));
}

#[test]
fn account_scope_allows_and_denies_by_attribute_and_action() {
    let manage = creds(&["atproto", "account:email?action=manage"]);
    assert!(assert_account_scope(&manage, "email", AccountAction::Manage).is_ok());

    let read_only = creds(&["atproto", "account:email"]);
    assert!(matches!(
        assert_account_scope(&read_only, "email", AccountAction::Manage),
        Err(ApiError::InsufficientScope(_))
    ));

    let wrong_attr = creds(&["atproto", "account:status?action=manage"]);
    assert!(matches!(
        assert_account_scope(&wrong_attr, "email", AccountAction::Manage),
        Err(ApiError::InsufficientScope(_))
    ));

    let no_account_grant = creds(&["atproto", "repo:app.bsky.feed.post"]);
    assert!(matches!(
        assert_account_scope(&no_account_grant, "email", AccountAction::Manage),
        Err(ApiError::InsufficientScope(_))
    ));
}

/// The original defect: a granular session holding one `repo:` grant and
/// nothing else got *unrestricted* access to every other resource,
/// because each check gated on whether the session held a grant of that
/// same kind.
#[test]
fn a_repo_only_grant_confers_nothing_on_other_resources() {
    let repo_only = creds(&["atproto", "repo:app.bsky.feed.post"]);
    assert!(assert_repo_scope(&repo_only, POST, RepoAction::Create).is_ok());
    assert_eq!(denials(&repo_only), [false, true, true, true, true]);
}

/// A session granted the base scope and nothing else can do nothing.
#[test]
fn an_empty_scope_set_is_denied_every_resource() {
    assert_eq!(denials(&creds(&["atproto"])), [true; 5]);
}

/// `include:` scopes reach these helpers already expanded
/// (`permission_set::expand_includes`). One that resolved to nothing --
/// an unreachable authority, or a set naming no permissions -- must leave
/// the session with no grants, not with every grant.
#[test]
fn an_include_that_resolved_to_nothing_is_denied_every_resource() {
    assert_eq!(
        denials(&creds(&["atproto", "include:app.example.nothing"])),
        [true; 5]
    );
}

/// An unparseable scope string is inert: it can never be the reason a
/// session gets access it was not granted.
#[test]
fn an_unparseable_scope_widens_nothing() {
    assert_eq!(
        denials(&creds(&["atproto", "not-a-scope-this-server-knows"])),
        [true; 5]
    );
}

/// Backward compatibility for legacy `transition:generic` sessions,
/// following upstream's `ScopePermissionsTransition`: it overrides
/// `allowsRepo`, `allowsBlob` and `allowsRpc`, but not `allowsIdentity`,
/// so a transition session still needs an explicit `identity:` grant to
/// change its handle.
#[test]
fn a_transition_generic_session_keeps_its_legacy_reach() {
    // Repo writes, blob uploads and service proxying, per the OAuth spec's
    // definition of the scope -- and no account management: not the handle,
    // not the email, not deactivation, not migration.
    let transition = creds(&["atproto", "transition:generic"]);
    assert_eq!(denials(&transition), [false, false, false, true, true]);

    // Explicit grants alongside it still work.
    let with_identity = creds(&["atproto", "transition:generic", "identity:handle"]);
    assert!(assert_identity_scope(&with_identity, "handle").is_ok());
    let with_account = creds(&[
        "atproto",
        "transition:generic",
        "account:email?action=manage",
    ]);
    assert!(assert_account_scope(&with_account, "email", AccountAction::Manage).is_ok());
}

/// `registerPush` and `unregisterPush` mint service auth and call the
/// notification service, so they need the `rpc:` grant that governs
/// reaching outward -- against the audience the request body names.
#[test]
fn an_rpc_target_is_checked_against_the_audience_the_body_names() {
    let lxm = "app.bsky.notification.registerPush";
    let aud = "did:web:notif.example.com";

    // The matching grant permits it; a wildcard audience does too.
    let exact = creds(&["atproto", &format!("rpc:{lxm}?aud={aud}")]);
    assert!(assert_rpc_target(&exact, lxm, aud).is_ok());
    let wildcard = creds(&["atproto", &format!("rpc:{lxm}?aud=*")]);
    assert!(assert_rpc_target(&wildcard, lxm, aud).is_ok());

    // A grant for a different audience does not.
    let other_aud = creds(&[
        "atproto",
        &format!("rpc:{lxm}?aud=did:web:someone-else.example.com"),
    ]);
    assert!(assert_rpc_target(&other_aud, lxm, aud).is_err());

    // Nor does a grant for a different method, nor no rpc grant at all.
    let other_lxm = creds(&[
        "atproto",
        &format!("rpc:app.bsky.feed.getTimeline?aud={aud}"),
    ]);
    assert!(assert_rpc_target(&other_lxm, lxm, aud).is_err());
    assert!(assert_rpc_target(&creds(&["atproto", "repo:app.bsky.feed.post"]), lxm, aud).is_err());

    // Legacy sessions are unaffected, as everywhere else.
    assert!(assert_rpc_target(&creds(&["atproto", "transition:generic"]), lxm, aud).is_ok());
    assert!(assert_rpc_target(&None, lxm, aud).is_ok());
}

/// The OAuth spec confers the address through `transition:email` or an
/// explicit `account:email` grant, and through nothing else.
#[test]
fn email_is_visible_only_to_a_session_granted_it() {
    // No granted scopes at all: app password / legacy token, unaffected.
    assert!(allows_email_read(&None));

    // The two grants the spec names.
    assert!(allows_email_read(&creds(&["atproto", "transition:email"])));
    assert!(allows_email_read(&creds(&["atproto", "account:email"])));
    assert!(allows_email_read(&creds(&[
        "atproto",
        "account:email?action=read"
    ])));
    // `manage` subsumes `read`.
    assert!(allows_email_read(&creds(&[
        "atproto",
        "account:email?action=manage"
    ])));

    // Everything else is withheld, transition:generic included.
    assert!(!allows_email_read(&creds(&["atproto"])));
    assert!(!allows_email_read(&creds(&[
        "atproto",
        "transition:generic"
    ])));
    assert!(!allows_email_read(&creds(&[
        "atproto",
        "repo:app.bsky.feed.post"
    ])));
    assert!(!allows_email_read(&creds(&["atproto", "account:status"])));
    assert!(!allows_email_read(&creds(&[
        "atproto",
        "include:app.example.set"
    ])));
}

/// App passwords and legacy access tokens carry no `granted_scopes`;
/// nothing here applies to them.
#[test]
fn a_session_without_granted_scopes_is_unaffected() {
    assert_eq!(denials(&None), [false; 5]);
}

#[rocket::get("/xrpc/com.example.thing?<count>")]
fn thing(count: u8) -> String {
    count.to_string()
}

#[rocket::get("/xrpc/<_nsid>?<_query..>", rank = 2)]
fn proxied(_nsid: &str, _query: Option<&str>, _local: NotServedLocally) -> &'static str {
    "proxied"
}

async fn answer(client: &rocket::local::asynchronous::Client, path: &str) -> (Status, String) {
    let response = client.get(path).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

#[tokio::test]
async fn only_methods_served_elsewhere_reach_the_proxy() {
    let rocket = rocket::build()
        .mount("/", rocket::routes![thing, proxied])
        .register("/", rocket::catchers![crate::default_catcher]);
    let client = rocket::local::asynchronous::Client::untracked(rocket)
        .await
        .unwrap();

    let (status, text) = answer(&client, "/xrpc/com.example.other?x=1").await;
    assert_eq!((status, text.as_str()), (Status::Ok, "proxied"));
    let (status, text) = answer(&client, "/xrpc/com.example.thing?count=7").await;
    assert_eq!((status, text.as_str()), (Status::Ok, "7"));

    let (status, text) = answer(&client, "/xrpc/com.example.thing").await;
    assert_eq!(status, Status::BadRequest);
    assert!(
        text.contains("Params must have the property \\\"count\\\""),
        "{text}"
    );
    let (status, text) = answer(&client, "/xrpc/com.example.thing?count=many").await;
    assert_eq!(status, Status::BadRequest);
    assert!(text.contains("Invalid request parameters"), "{text}");
}
