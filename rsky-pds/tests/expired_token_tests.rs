//! End-to-end coverage for expired-token error propagation.
//!
//! rsky historically collapsed every auth failure to
//! `400 {"error":"InvalidRequest","message":"BadJwt: ..."}`. Clients (our
//! atbridge and `@atproto/api`) key on the `ExpiredToken` code to decide when to
//! call `refreshSession`, so an expired token that looks identical to a
//! malformed one leaves them unable to recover. The reference Bluesky PDS
//! answers an expired token with `400 {"error":"ExpiredToken","message":"Token
//! is expired"}`; these tests drive the real HTTP endpoints against a real
//! Postgres and assert that exact contract.
//!
//! Three layers had to be fixed for this to work, so these tests fail on the
//! pre-fix code:
//!   1. `validate_bearer_token` masked expiry with a repo-key fallback error.
//!   2. `access_check` collapsed everything to `BadJwt`.
//!   3. Every guard hardcoded `ApiError::InvalidRequest` when rendering.

use diesel::prelude::*;
use rocket::http::{Header, Status};
use rocket::local::asynchronous::Client;
use serde_json::Value;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;

mod common;

/// Clear `actor.deactivatedAt` for `did` straight in Postgres.
///
/// `create_account` supplies an explicit DID, which makes the server create the
/// account *deactivated* (see `create_account.rs`: an explicit did forces
/// `deactivated = true`). `getSession`'s default account lookup excludes
/// deactivated accounts, so the happy-path assertions need the account marked
/// active. We do this at the DB layer to avoid `activateAccount`'s network DID
/// resolution and S3/sequencer side effects.
async fn activate_account_in_db(postgres: &ContainerAsync<Postgres>, did: &str) {
    use rsky_pds::schema::pds::actor::dsl as ActorSchema;

    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    let mut conn = common::establish_connection(&url).expect("connection established");

    diesel::update(ActorSchema::actor.filter(ActorSchema::did.eq(did)))
        .set(ActorSchema::deactivatedAt.eq::<Option<String>>(None))
        .execute(&mut conn)
        .expect("clear deactivatedAt");
}

/// GET `com.atproto.server.getSession` bearing `token`.
async fn get_session(client: &Client, token: &str) -> (Status, Value) {
    let response = client
        .get("/xrpc/com.atproto.server.getSession")
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .dispatch()
        .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    (status, json)
}

/// POST `com.atproto.server.refreshSession` bearing `token`.
async fn refresh_session(client: &Client, token: &str) -> (Status, Value) {
    let response = client
        .post("/xrpc/com.atproto.server.refreshSession")
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .dispatch()
        .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    (status, json)
}

/// DoD #1: an expired ACCESS token on an access-guarded endpoint must yield the
/// byte-for-byte `ExpiredToken` contract.
#[tokio::test]
async fn expired_access_token_yields_expired_token_error() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    let (username, password) = common::create_account(&client).await;
    let did = common::login_did(&client, &username, &password).await;

    let (status, body) = get_session(&client, &common::expired_access_token(&did)).await;

    assert_eq!(status, Status::BadRequest, "got {status}: {body}");
    assert_eq!(body["error"], "ExpiredToken", "body was {body}");
    assert_eq!(body["message"], "Token is expired", "body was {body}");
}

/// DoD #2: an expired REFRESH token on refreshSession must also yield
/// `ExpiredToken` (the refresh guard bypasses `access_check`).
#[tokio::test]
async fn expired_refresh_token_on_refresh_session_yields_expired_token_error() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    let (username, password) = common::create_account(&client).await;
    let did = common::login_did(&client, &username, &password).await;

    let (status, body) = refresh_session(&client, &common::expired_refresh_token(&did)).await;

    assert_eq!(status, Status::BadRequest, "got {status}: {body}");
    assert_eq!(body["error"], "ExpiredToken", "body was {body}");
    assert_eq!(body["message"], "Token is expired", "body was {body}");
}

/// DoD #3: a malformed/garbage token must NOT be reported as expired -- it stays
/// an InvalidRequest/BadJwt so clients don't loop calling refreshSession.
#[tokio::test]
async fn malformed_token_is_not_reported_as_expired() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    common::create_account(&client).await;

    let (status, body) = get_session(&client, "not-a-real-jwt.garbage.value").await;

    assert_eq!(status, Status::BadRequest, "got {status}: {body}");
    assert_ne!(
        body["error"], "ExpiredToken",
        "a malformed token must not masquerade as expired: {body}"
    );
    assert_eq!(
        body["error"], "InvalidRequest",
        "malformed token should stay InvalidRequest: {body}"
    );
}

/// DoD #4: a missing Authorization header is unchanged -- still an
/// InvalidRequest 400, never ExpiredToken.
#[tokio::test]
async fn missing_auth_header_is_unchanged() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    common::create_account(&client).await;

    let response = client
        .get("/xrpc/com.atproto.server.getSession")
        .dispatch()
        .await;
    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);

    assert_eq!(status, Status::BadRequest, "got {status}: {body}");
    assert_ne!(json["error"], "ExpiredToken", "body was {json}");
    assert_eq!(json["error"], "InvalidRequest", "body was {json}");
}

/// DoD #5: a valid access token still succeeds -- no regression from the expiry
/// handling.
#[tokio::test]
async fn valid_access_token_yields_200() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    let (username, password) = common::create_account(&client).await;
    let did = common::login_did(&client, &username, &password).await;
    activate_account_in_db(&postgres, &did).await;

    let (status, body) = get_session(&client, &common::valid_access_token(&did)).await;

    assert_eq!(status, Status::Ok, "got {status}: {body}");
    assert_eq!(body["did"], did, "body was {body}");
}

/// DoD #6 (regression): a repo-key-signed service-auth token (as video.bsky.app
/// sends) must still verify via the repo-key fallback. This locks in the safety
/// property that "don't fall back on expiry" cannot break the video path --
/// jwt-simple checks the signature before claims, so a repo-key token fails the
/// JWT-key attempt at *signature*, never at expiry, and still reaches fallback.
#[tokio::test]
async fn repo_key_signed_service_token_still_verifies() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    let (username, password) = common::create_account(&client).await;
    let did = common::login_did(&client, &username, &password).await;
    activate_account_in_db(&postgres, &did).await;

    let (status, body) = get_session(&client, &common::repo_key_signed_access_token(&did)).await;

    assert_eq!(
        status,
        Status::Ok,
        "repo-key-signed token must still verify via fallback, got {status}: {body}"
    );
    assert_eq!(body["did"], did, "body was {body}");
}
