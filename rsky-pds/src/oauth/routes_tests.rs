use super::*;

#[test]
fn authorize_hrefs_encode_their_parameters() {
    assert_eq!(
        authorize_href("http://localhost?x=1", "urn:ietf:params:oauth:request_uri:r", None),
        "/oauth/authorize?client_id=http%3A%2F%2Flocalhost%3Fx%3D1&request_uri=urn%3Aietf%3Aparams%3Aoauth%3Arequest_uri%3Ar"
    );
    assert!(authorize_href("c", "r", Some("sign-in")).ends_with("&view=sign-in"));
}

#[test]
fn granted_scope_drops_email_only_when_offered_and_unchecked() {
    let mut form = ConsentFormData {
        request_uri: "r".into(),
        client_id: "c".into(),
        csrf: "t".into(),
        did: None,
        scope: None,
        email_optional: None,
        allow_email: None,
        session_token: None,
    };
    assert_eq!(form.granted_scope(), None);
    form.scope = Some("atproto account:email?action=manage repo:a.b.c".into());
    assert_eq!(
        form.granted_scope().as_deref(),
        Some("atproto account:email?action=manage repo:a.b.c")
    );
    form.email_optional = Some("1".into());
    assert_eq!(form.granted_scope().as_deref(), Some("atproto repo:a.b.c"));
    form.allow_email = Some("on".into());
    assert_eq!(
        form.granted_scope().as_deref(),
        Some("atproto account:email?action=manage repo:a.b.c")
    );
    form.scope = Some("atproto account:email".into());
    form.allow_email = None;
    assert_eq!(form.granted_scope().as_deref(), Some("atproto"));
}

#[tokio::test]
async fn include_sets_resolve_once_per_set_and_fail_closed() {
    let sets = SharedPermissionSets::default();
    sets.resolver
        .prime(
            "app.example.set",
            vec![crate::permission_set::repo_permission("app.example.record")],
        )
        .await;
    let views = include_sets(
        &sets,
        &[
            "atproto".into(),
            "include:app.example.set".into(),
            "include:app.example.set".into(),
            "include:app.example.set?aud=did:web:x%23y".into(),
        ],
    )
    .await
    .unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(
        views["app.example.set"].scopes,
        ["repo:?collection=app.example.record"]
    );
    assert_eq!(
        views["app.example.set?aud=did:web:x%23y"].scopes,
        ["repo:?collection=app.example.record"]
    );
    assert!(views["app.example.set"].title.is_none());
    let error = include_sets(&sets, &["include:invalid.example.nothing".into()])
        .await
        .unwrap_err();
    assert_eq!(error, PERMISSION_SETS_UNAVAILABLE);
}

#[test]
fn cross_site_posts_are_told_apart_by_fetch_metadata_and_origin() {
    let own = "https://pds.test";
    assert!(!is_cross_site(None, None, own));
    assert!(!is_cross_site(Some("same-origin"), None, own));
    assert!(!is_cross_site(Some("none"), Some("https://pds.test"), own));
    assert!(is_cross_site(Some("cross-site"), None, own));
    assert!(is_cross_site(
        Some("Cross-Site"),
        Some("https://pds.test"),
        own
    ));
    assert!(!is_cross_site(None, Some("https://pds.test"), own));
    assert!(!is_cross_site(None, Some("HTTPS://PDS.TEST/"), own));
    assert!(is_cross_site(None, Some("https://evil.test"), own));
    assert!(is_cross_site(None, Some("null"), own));
    assert!(is_cross_site(None, Some("https://pds.test"), "not a url"));
    assert!(is_ios(Some(
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)"
    )));
    assert!(is_ios(Some(
        "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)"
    )));
    assert!(!is_ios(Some(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)"
    )));
    assert!(!is_ios(None));
}

#[test]
fn account_proofs_prefer_the_ephemeral_token() {
    let session = DeviceSession {
        device_id: "dev-1".into(),
        session_id: "ses-1".into(),
        csrf: "c".into(),
    };
    assert_eq!(
        account_proof(&session, Some("tok")),
        AccountProof::Ephemeral("tok")
    );
    assert_eq!(
        account_proof(&session, None),
        AccountProof::Device {
            session_id: "ses-1"
        }
    );
}

#[test]
fn is_new_oauth_session_only_true_for_authorization_code() {
    assert!(is_new_oauth_session(
        rsky_oauth::types::GRANT_AUTHORIZATION_CODE
    ));
    assert!(!is_new_oauth_session(
        rsky_oauth::types::GRANT_REFRESH_TOKEN
    ));
    assert!(!is_new_oauth_session("something_else"));
}
