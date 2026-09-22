use super::*;

#[test]
fn named_resource_parameters_preserve_their_grants_and_boundaries() {
    let granted = GrantedScopes::parse(&[
        "atproto".into(),
        "repo?collection=app.example.note&action=*&ignored=value".into(),
        "identity?attr=handle".into(),
        "account?attr=email&action=read".into(),
        "rpc?lxm=app.example.read&aud=did%3Aweb%3Aapi.example&ignored=value".into(),
        "space:app.example.room?action=read".into(),
    ]);
    for action in [RepoAction::Create, RepoAction::Update, RepoAction::Delete] {
        assert!(granted.allows_repo("app.example.note", action));
        assert!(!granted.allows_repo("app.example.other", action));
    }
    assert!(granted.allows_identity("handle"));
    assert!(granted.allows_account("email", AccountAction::Read));
    assert!(!granted.allows_account("email", AccountAction::Manage));
    assert!(granted.allows_rpc("app.example.read", "did:web:api.example"));
    assert!(!granted.allows_rpc("app.example.read", "did:web:other.example"));
    assert_eq!(
        granted.space_grants(),
        ["space:app.example.room?action=read"]
    );
    assert_eq!(granted.iter().count(), 6);
    assert_eq!(percent_decoded("%FF"), "%FF");
    let malformed = GrantedScopes::parse(&[
        "identity?attr=handle&action=manage".into(),
        "account?attr=email&action=unknown".into(),
        "rpc?aud=did:web:api.example".into(),
    ]);
    assert!(!malformed.allows_identity("handle"));
    assert!(!malformed.allows_account("email", AccountAction::Read));
    assert!(!malformed.allows_rpc("app.example.read", "did:web:api.example"));
    assert!(
        GrantedScopes::parse(&["repo".into()]).allows_repo("app.example.note", RepoAction::Delete)
    );
}

#[test]
fn parses_each_scope_form() {
    assert_eq!(OAuthScope::parse("atproto"), OAuthScope::Atproto);
    assert_eq!(
        OAuthScope::parse("transition:generic"),
        OAuthScope::Transition("generic".into())
    );
    assert_eq!(
        OAuthScope::parse("repo:app.bsky.feed.post"),
        OAuthScope::Repo("app.bsky.feed.post".into())
    );
    assert_eq!(
        OAuthScope::parse("blob:image/*"),
        OAuthScope::Blob("image/*".into())
    );
    assert_eq!(
        OAuthScope::parse("rpc:com.example.method"),
        OAuthScope::Rpc("com.example.method".into())
    );
    assert_eq!(
        OAuthScope::parse("identity:handle"),
        OAuthScope::Identity("handle".into())
    );
    assert_eq!(
        OAuthScope::parse("account:email?action=manage"),
        OAuthScope::Account("email?action=manage".into())
    );
    assert_eq!(
        OAuthScope::parse("include:app.bulleted.authFull"),
        OAuthScope::Include("app.bulleted.authFull".into())
    );
    assert_eq!(
        OAuthScope::parse("space:app.bulleted.space?action=read"),
        OAuthScope::Space("app.bulleted.space?action=read".into())
    );
    assert_eq!(
        OAuthScope::parse("something-else"),
        OAuthScope::Unknown("something-else".into())
    );
}

#[test]
fn round_trips_through_display() {
    for token in [
        "atproto",
        "transition:generic",
        "repo:app.bsky.feed.post",
        "blob:image/*",
        "rpc:com.example.method",
        "identity:handle",
        "identity:*",
        "account:email",
        "account:email?action=manage",
        "include:app.bulleted.authFull",
        "space:app.bulleted.space?action=read",
        "something-else",
    ] {
        assert_eq!(OAuthScope::parse(token).to_string(), token);
    }
}

#[test]
fn classifies_permission_grants() {
    assert!(!OAuthScope::parse("atproto").is_permission_grant());
    assert!(!OAuthScope::parse("transition:generic").is_permission_grant());
    assert!(OAuthScope::parse("repo:app.bsky.feed.post").is_permission_grant());
    assert!(OAuthScope::parse("blob:image/*").is_permission_grant());
    assert!(OAuthScope::parse("rpc:com.example.method").is_permission_grant());
    assert!(OAuthScope::parse("identity:handle").is_permission_grant());
    assert!(OAuthScope::parse("account:email").is_permission_grant());
    assert!(OAuthScope::parse("include:app.bulleted.authFull").is_permission_grant());
    assert!(OAuthScope::parse("space:app.bulleted.space").is_permission_grant());
    assert!(!OAuthScope::parse("nonsense").is_permission_grant());
}

#[test]
fn granted_scopes_answers_session_questions() {
    // The exact base scope bulleted-app declares.
    let granted: Vec<String> = "atproto include:app.bulleted.authFull blob:image/*"
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    let scopes = GrantedScopes::parse(&granted);
    assert!(scopes.has_atproto());
    assert!(!scopes.has_transition("generic"));
    assert!(scopes.has_permission_grant());
    assert!(scopes.space_grants().is_empty());
}

#[test]
fn collects_space_grants_with_prefix_intact() {
    let granted: Vec<String> = vec![
        "atproto".into(),
        "space:app.bulleted.space?manage=create&action=read_self".into(),
    ];
    let scopes = GrantedScopes::parse(&granted);
    assert_eq!(
        scopes.space_grants(),
        vec!["space:app.bulleted.space?manage=create&action=read_self".to_string()]
    );
}

#[test]
fn repo_scope_confines_collections_and_actions() {
    let post = "app.bsky.feed.post";
    let like = "app.bsky.feed.like";

    // A collection-limited grant permits only that collection.
    let g = GrantedScopes::parse(&["atproto".into(), format!("repo:{post}")]);
    assert!(g.allows_repo(post, RepoAction::Create));
    assert!(g.allows_repo(post, RepoAction::Delete));
    assert!(!g.allows_repo(like, RepoAction::Create));

    // An action-limited grant permits only that action.
    let g = GrantedScopes::parse(&["atproto".into(), format!("repo:{post}?action=create")]);
    assert!(g.allows_repo(post, RepoAction::Create));
    assert!(!g.allows_repo(post, RepoAction::Delete));

    // `repo:*` covers every collection.
    let g = GrantedScopes::parse(&["atproto".into(), "repo:*".into()]);
    assert!(g.allows_repo(like, RepoAction::Update));

    // A legacy transition session names no `repo:` grant of its own; the
    // enforcement helper exempts it rather than this parser.
    let g = GrantedScopes::parse(&["atproto".into(), "transition:generic".into()]);
    assert!(!g.allows_repo(post, RepoAction::Create));

    // Multi-valued collections in query form.
    let g = GrantedScopes::parse(&[
        "atproto".into(),
        format!("repo?collection={post}&collection={like}&action=create"),
    ]);
    assert!(g.allows_repo(post, RepoAction::Create));
    assert!(g.allows_repo(like, RepoAction::Create));
    assert!(!g.allows_repo("app.bsky.feed.repost", RepoAction::Create));
}

#[test]
fn legacy_only_session_has_no_permission_grant() {
    let granted: Vec<String> = vec!["atproto".into(), "transition:generic".into()];
    let scopes = GrantedScopes::parse(&granted);
    assert!(scopes.has_atproto());
    assert!(scopes.has_transition("generic"));
    assert!(!scopes.has_permission_grant());
}

#[test]
fn identity_scope_parses_valid_and_rejects_invalid_forms() {
    // Positional shorthand, the primary form.
    assert_eq!(parse_identity_scope("handle"), Some("handle".to_string()));
    assert_eq!(parse_identity_scope("*"), Some("*".to_string()));
    // Named query form is equivalent.
    assert_eq!(
        parse_identity_scope("?attr=handle"),
        Some("handle".to_string())
    );
    // Unrecognised attribute, empty suffix, and a stray parameter
    // (this resource takes no `action`) all deny rather than default.
    assert_eq!(parse_identity_scope("invalid"), None);
    assert_eq!(parse_identity_scope(""), None);
    assert_eq!(parse_identity_scope("handle?action=manage"), None);
}

#[test]
fn account_scope_parses_valid_and_rejects_invalid_forms() {
    assert_eq!(
        parse_account_scope("email"),
        Some(("email".to_string(), vec![AccountAction::Read]))
    );
    assert_eq!(
        parse_account_scope("email?action=manage"),
        Some(("email".to_string(), vec![AccountAction::Manage]))
    );
    assert_eq!(
        parse_account_scope("repo?action=manage"),
        Some(("repo".to_string(), vec![AccountAction::Manage]))
    );
    assert_eq!(
        parse_account_scope("?attr=status&action=manage"),
        Some(("status".to_string(), vec![AccountAction::Manage]))
    );
    // Unrecognised attribute or action, and a bare/empty suffix, deny.
    assert_eq!(parse_account_scope("invalid"), None);
    assert_eq!(parse_account_scope("email?action=invalid"), None);
    assert_eq!(parse_account_scope(""), None);
    assert_eq!(parse_account_scope("?action=manage"), None);
}

#[test]
fn identity_scope_enforces_the_granted_attribute() {
    let g = GrantedScopes::parse(&["atproto".into(), "identity:handle".into()]);
    assert!(g.allows_identity("handle"));

    // Wildcard covers every attribute.
    let g = GrantedScopes::parse(&["atproto".into(), "identity:*".into()]);
    assert!(g.allows_identity("handle"));

    // A session holding some other resource's grant permits no identity
    // attribute.
    let g = GrantedScopes::parse(&["atproto".into(), "repo:app.bsky.feed.post".into()]);
    assert!(!g.allows_identity("handle"));

    // An invalid `identity:` grant permits nothing.
    let g = GrantedScopes::parse(&["atproto".into(), "identity:invalid".into()]);
    assert!(!g.allows_identity("handle"));
}

#[test]
fn account_scope_enforces_attribute_and_action() {
    let g = GrantedScopes::parse(&["atproto".into(), "account:email?action=manage".into()]);
    assert!(g.allows_account("email", AccountAction::Manage));
    assert!(g.allows_account("email", AccountAction::Read)); // manage implies read
    assert!(!g.allows_account("status", AccountAction::Manage));

    // Default action is `read`; it does not satisfy a `manage` check.
    let g = GrantedScopes::parse(&["atproto".into(), "account:email".into()]);
    assert!(g.allows_account("email", AccountAction::Read));
    assert!(!g.allows_account("email", AccountAction::Manage));
}

#[test]
fn blob_scope_enforces_the_granted_mime_pattern() {
    let g = GrantedScopes::parse(&["atproto".into(), "blob:image/*".into()]);
    assert!(g.allows_blob("image/png"));
    assert!(!g.allows_blob("video/mp4"));

    let g = GrantedScopes::parse(&["atproto".into(), "blob:*/*".into()]);
    assert!(g.allows_blob("video/mp4"));

    // Multiple accepted patterns via the query form.
    let g = GrantedScopes::parse(&[
        "atproto".into(),
        "blob:?accept=image/png&accept=video/mp4".into(),
    ]);
    assert!(g.allows_blob("image/png"));
    assert!(g.allows_blob("video/mp4"));
    assert!(!g.allows_blob("text/plain"));
}

#[test]
fn rpc_scope_enforces_method_and_audience() {
    let g = GrantedScopes::parse(&[
        "atproto".into(),
        "rpc:com.example.method?aud=did:web:example.com".into(),
    ]);
    assert!(g.allows_rpc("com.example.method", "did:web:example.com"));
    assert!(!g.allows_rpc("com.example.method", "did:web:other.com"));
    assert!(!g.allows_rpc("com.example.other", "did:web:example.com"));

    // No `aud` named: proposal 0011 requires one, so the grant matches
    // nothing rather than defaulting to "any service".
    let g = GrantedScopes::parse(&["atproto".into(), "rpc:com.example.method".into()]);
    assert!(!g.allows_rpc("com.example.method", "did:web:example.com"));

    // `rpc:*?aud=*` is forbidden outright.
    let g = GrantedScopes::parse(&["atproto".into(), "rpc:*?aud=*".into()]);
    assert!(!g.allows_rpc("com.example.method", "did:web:example.com"));
}

/// Regression coverage for the fail-open shape described on
/// [`OAuthScope::Unknown`]: a scope string this parser doesn't recognise
/// must never be the reason a session ends up with more access than its
/// recognised scopes alone would grant.
#[test]
fn unrecognised_scope_never_widens_access() {
    let post = "app.bsky.feed.post";
    let with_junk = GrantedScopes::parse(&[
        "atproto".into(),
        format!("repo:{post}"),
        "totally-unrecognised-scope-string".into(),
    ]);
    let without_junk = GrantedScopes::parse(&["atproto".into(), format!("repo:{post}")]);
    // Identical grants with or without the unrecognised scope.
    assert_eq!(
        with_junk.allows_repo(post, RepoAction::Create),
        without_junk.allows_repo(post, RepoAction::Create)
    );
    assert_eq!(
        with_junk.allows_repo("app.bsky.feed.like", RepoAction::Create),
        without_junk.allows_repo("app.bsky.feed.like", RepoAction::Create)
    );
    // It confers nothing on any *other* resource either. Under the
    // auth-source gate these are outright denials, not the unrestricted
    // pass-through the old `is_granular_*_session` shape gave them.
    assert!(!with_junk.allows_blob("image/png"));
    assert!(!with_junk.allows_rpc("com.example.method", "did:web:example.com"));
    assert!(!with_junk.allows_identity("handle"));
    assert!(!with_junk.allows_account("email", AccountAction::Manage));

    // A session carrying only unrecognised scopes (plus the mandatory
    // base scope) is not a valid modern grant -- it is refused entirely by
    // `oauth_scopes_to_auth_scope` before it ever reaches these checks --
    // and confers nothing if it somehow did.
    let junk_only =
        GrantedScopes::parse(&["atproto".into(), "totally-unrecognised-scope-string".into()]);
    assert!(!junk_only.has_permission_grant());
    assert!(!junk_only.allows_repo(post, RepoAction::Create));
    assert!(!junk_only.allows_blob("image/png"));
    assert!(!junk_only.allows_rpc("com.example.method", "did:web:example.com"));
    assert!(!junk_only.allows_identity("handle"));
    assert!(!junk_only.allows_account("email", AccountAction::Manage));
}

/// The empty grant: a session holding only the mandatory base scope
/// permits nothing at all on any resource.
#[test]
fn base_scope_alone_confers_no_grant() {
    let g = GrantedScopes::parse(&["atproto".into()]);
    assert!(!g.allows_repo("app.bsky.feed.post", RepoAction::Create));
    assert!(!g.allows_blob("image/png"));
    assert!(!g.allows_rpc("com.example.method", "did:web:example.com"));
    assert!(!g.allows_identity("handle"));
    assert!(!g.allows_account("email", AccountAction::Manage));
}

#[test]
fn unknown_repo_actions_cannot_expand_an_explicit_create_grant() {
    let grants =
        GrantedScopes::parse(&["repo:app.example.note?action=create&action=unknown".into()]);
    assert!(grants.allows_repo("app.example.note", RepoAction::Create));
    assert!(!grants.allows_repo("app.example.note", RepoAction::Delete));
    assert!(!grants.allows_repo("app.example.note", RepoAction::Update));
    let invalid_account =
        GrantedScopes::parse(&["account:email?action=read&unknown=manage".into()]);
    assert!(!invalid_account.allows_account("email", AccountAction::Read));
    assert!(!invalid_account.allows_account("email", AccountAction::Manage));
}
