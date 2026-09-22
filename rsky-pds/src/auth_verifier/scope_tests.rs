use super::*;
use crate::auth_verifier::AuthScope;
use rocket::http::ContentType;
use rocket::local::asynchronous::Client;

fn session(granted_scopes: Option<Vec<String>>) -> Option<Credentials> {
    Some(Credentials {
        r#type: "test".to_string(),
        did: Some("did:plc:aaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
        scope: Some(AuthScope::AppPass),
        granted_scopes,
        audience: None,
        token_id: None,
        aud: None,
        iss: None,
        is_privileged: None,
    })
}

fn oauth_session(granted: &[&str]) -> Option<Credentials> {
    session(Some(granted.iter().map(|s| s.to_string()).collect()))
}

/// App passwords and legacy `createSession` tokens carry no grants.
fn legacy_session() -> Option<Credentials> {
    session(None)
}

async fn client() -> Client {
    Client::untracked(rocket::build())
        .await
        .expect("local client")
}

#[tokio::test]
async fn copied_scope_proofs_preserve_checks_and_anonymous_is_not_a_subject() {
    let authenticated = Scoped::<RepoWrite> {
        access: AccessOutput {
            credentials: oauth_session(&["atproto", "repo:app.example.note?action=create"]),
            artifacts: None,
        },
        _decl: PhantomData,
    };
    let copied = authenticated.clone();
    assert_eq!(
        copied
            .did_for(&vec![RepoTarget::new(
                "app.example.note",
                RepoAction::Create
            )])
            .await
            .unwrap(),
        "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert!(matches!(
        copied
            .did_for(&vec![RepoTarget::new(
                "app.example.note",
                RepoAction::Delete
            )])
            .await,
        Err(ApiError::InsufficientScope(_))
    ));
    let anonymous = Scoped::<NoScopeRequired> {
        access: AccessOutput {
            credentials: None,
            artifacts: None,
        },
        _decl: PhantomData,
    };
    assert!(anonymous.did_opt().await.unwrap().is_none());
    assert!(matches!(
        anonymous.did().await,
        Err(ApiError::AuthRequiredError(_))
    ));
    assert!(AccountStatus::check(&legacy_session(), &()).await.is_ok());
}

#[tokio::test]
async fn no_scope_required_permits_every_session() {
    let client = client().await;
    let request = client.get("/");
    let req = request.inner();
    assert!(NoScopeRequired::precheck(
        req,
        &oauth_session(&["atproto", "repo:app.bsky.feed.post"])
    )
    .await
    .is_ok());
    assert!(NoScopeRequired::precheck(req, &legacy_session())
        .await
        .is_ok());
    assert!(NoScopeRequired::precheck(req, &None).await.is_ok());
}

#[tokio::test]
async fn oauth_forbidden_rejects_an_oauth_session_and_permits_an_app_password() {
    let client = client().await;
    let request = client.get("/");
    let req = request.inner();
    assert!(matches!(
        OAuthForbidden::precheck(req, &oauth_session(&["atproto", "transition:generic"])).await,
        Err(ApiError::Forbidden(_))
    ));
    assert!(matches!(
        OAuthForbiddenEmail::precheck(req, &oauth_session(&["atproto", "transition:email"]))
            .await,
        Err(ApiError::Forbidden(message)) if message.starts_with("Use the account manager")
    ));
    assert!(OAuthForbidden::precheck(req, &legacy_session())
        .await
        .is_ok());
}

#[tokio::test]
async fn blob_upload_reads_the_mime_off_the_request() {
    let client = client().await;
    let request = client.post("/").header(ContentType::PNG);
    assert!(BlobUpload::precheck(
        request.inner(),
        &oauth_session(&["atproto", "blob:image/*"])
    )
    .await
    .is_ok());
    let request = client.post("/").header(ContentType::Plain);
    assert!(matches!(
        BlobUpload::precheck(
            request.inner(),
            &oauth_session(&["atproto", "blob:image/*"])
        )
        .await,
        Err(ApiError::InsufficientScope(_))
    ));
}

#[tokio::test]
async fn identity_and_account_declarations_narrow_their_attribute() {
    let client = client().await;
    let request = client.post("/");
    let req = request.inner();
    assert!(
        IdentityHandle::precheck(req, &oauth_session(&["atproto", "identity:handle"]))
            .await
            .is_ok()
    );
    assert!(matches!(
        IdentityHandle::precheck(req, &oauth_session(&["atproto", "identity:invalid"])).await,
        Err(ApiError::InsufficientScope(_))
    ));
    assert!(AccountEmail::precheck(
        req,
        &oauth_session(&["atproto", "account:email?action=manage"])
    )
    .await
    .is_ok());
    assert!(matches!(
        AccountEmail::precheck(req, &oauth_session(&["atproto", "account:email"])).await,
        Err(ApiError::InsufficientScope(_))
    ));
}

#[tokio::test]
async fn repo_write_checks_every_declared_target() {
    let granted = oauth_session(&["atproto", "repo:app.bsky.feed.post"]);
    assert!(RepoWrite::check(
        &granted,
        &vec![RepoTarget::new("app.bsky.feed.post", RepoAction::Create)]
    )
    .await
    .is_ok());
    // One uncovered collection in a batch denies the whole batch.
    assert!(matches!(
        RepoWrite::check(
            &granted,
            &vec![
                RepoTarget::new("app.bsky.feed.post", RepoAction::Create),
                RepoTarget::new("app.bsky.feed.like", RepoAction::Create),
            ]
        )
        .await,
        Err(ApiError::InsufficientScope(_))
    ));
}

#[tokio::test]
async fn repo_write_refuses_an_empty_declaration() {
    assert!(matches!(
        RepoWrite::check(&oauth_session(&["atproto", "repo:*"]), &vec![]).await,
        Err(ApiError::InsufficientScope(_))
    ));
}

#[tokio::test]
async fn legacy_sessions_clear_every_declaration() {
    let client = client().await;
    let request = client.post("/").header(ContentType::Plain);
    let req = request.inner();
    let legacy = legacy_session();
    assert!(BlobUpload::precheck(req, &legacy).await.is_ok());
    assert!(IdentityHandle::precheck(req, &legacy).await.is_ok());
    assert!(AccountEmail::precheck(req, &legacy).await.is_ok());
    assert!(AccountStatus::precheck(req, &legacy).await.is_ok());
    assert!(AccountRepo::precheck(req, &legacy).await.is_ok());
    assert!(RepoWrite::check(
        &legacy,
        &vec![RepoTarget::new("app.bsky.feed.post", RepoAction::Create)]
    )
    .await
    .is_ok());
}
