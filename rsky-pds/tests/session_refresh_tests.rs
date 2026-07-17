//! End-to-end coverage for `com.atproto.server.refreshSession`.
//!
//! Before the time-unit fix, `rotate_refresh_token` reached `from_micros_to_str`, which
//! handed a MICROsecond timestamp to `NaiveDateTime::from_timestamp` (which takes
//! SECONDS). chrono panicked with "invalid or out-of-range datetime" and Rocket turned
//! that into a 500 -- on EVERY refreshSession call, for every account. These tests drive
//! the real HTTP endpoint against a real Postgres, so they fail on the pre-fix code.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{Duration, Utc};
use diesel::prelude::*;
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::Client;
use rsky_common::time::from_str_to_utc;
use serde_json::json;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;

mod common;

/// Log in and return `(accessJwt, refreshJwt)`.
async fn create_session(client: &Client) -> (String, String) {
    let (username, password) = common::create_account(client).await;

    let response = client
        .post("/xrpc/com.atproto.server.createSession")
        .header(ContentType::JSON)
        .body(json!({ "identifier": username, "password": password }).to_string())
        .dispatch()
        .await;

    let status = response.status();
    let body = response.into_string().await.unwrap_or_default();
    assert_eq!(status, Status::Ok, "createSession failed: {body}");

    let session: serde_json::Value = serde_json::from_str(&body).unwrap();
    (
        session["accessJwt"].as_str().unwrap().to_string(),
        session["refreshJwt"].as_str().unwrap().to_string(),
    )
}

/// POST refreshSession bearing `refresh_jwt`.
async fn refresh_session(client: &Client, refresh_jwt: &str) -> (Status, String) {
    let response = client
        .post("/xrpc/com.atproto.server.refreshSession")
        .header(Header::new(
            "Authorization",
            format!("Bearer {refresh_jwt}"),
        ))
        .dispatch()
        .await;

    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

/// The `jti` of a refresh token is its primary key in `pds.refresh_token`.
fn jti_of(jwt: &str) -> String {
    let payload = jwt.split('.').nth(1).expect("jwt payload segment");
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    claims["jti"].as_str().expect("jti claim").to_string()
}

/// Read `pds.refresh_token.expiresAt` for a token id, straight out of Postgres.
async fn stored_expiry(postgres: &ContainerAsync<Postgres>, token_id: &str) -> String {
    use rsky_pds::schema::pds::refresh_token::dsl as RefreshTokenSchema;

    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    let mut conn = common::establish_connection(&url).expect("connection established");

    RefreshTokenSchema::refresh_token
        .filter(RefreshTokenSchema::id.eq(token_id))
        .select(RefreshTokenSchema::expiresAt)
        .first::<String>(&mut conn)
        .expect("refresh token row should exist")
}

/// The headline regression: refreshSession 500s on every call in the pre-fix code.
#[tokio::test]
async fn refresh_session_returns_200_and_rotates_the_tokens() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    let (_access_jwt, refresh_jwt) = create_session(&client).await;

    let (status, body) = refresh_session(&client, &refresh_jwt).await;

    assert_eq!(
        status,
        Status::Ok,
        "refreshSession should return 200, got {status}: {body}"
    );
    let refreshed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let new_refresh_jwt = refreshed["refreshJwt"].as_str().expect("refreshJwt");
    assert!(
        refreshed["accessJwt"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "refreshSession must return a new accessJwt"
    );
    assert_ne!(
        new_refresh_jwt, refresh_jwt,
        "the refresh token must actually rotate"
    );
    // A rotated token is a *different* row, keyed by a fresh jti.
    assert_ne!(jti_of(new_refresh_jwt), jti_of(&refresh_jwt));
}

/// Regression: `rotate_refresh_token` added a 2-hour grace period expressed in
/// MILLIseconds (`2 * HOUR`) to a MICROsecond timestamp. 7_200_000 microseconds is
/// 7.2 seconds -- so the window in which a client could safely retry with its old
/// refresh token (a flaky network, a racing tab) collapsed from two hours to seven
/// seconds. Assert the stored grace expiry is ~2h out; the tolerance below is three
/// orders of magnitude away from 7.2s, so the old math cannot sneak through.
#[tokio::test]
async fn refresh_session_sets_a_two_hour_reuse_grace_window() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    let (_access_jwt, refresh_jwt) = create_session(&client).await;
    let old_jti = jti_of(&refresh_jwt);

    let (status, body) = refresh_session(&client, &refresh_jwt).await;
    assert_eq!(status, Status::Ok, "refreshSession failed: {body}");

    // Rotation shortens the OLD token's life from its full 90 days down to the
    // revocation grace period.
    let grace_expiry = from_str_to_utc(&stored_expiry(&postgres, &old_jti).await);
    let remaining = grace_expiry - Utc::now();

    assert!(
        remaining > Duration::minutes(110) && remaining < Duration::minutes(130),
        "expected a ~2h reuse grace window, got {remaining} (grace expiry {grace_expiry})"
    );
}

/// Guard for the seconds/micros mixup in `store_refresh_token`: that bug and the
/// `from_micros_to_utc` bug cancelled out, so rows already on disk have correct
/// wall-clock expiries. The fix corrects both sides at once and MUST leave the stored
/// value unchanged -- a 90-day refresh token stays a 90-day refresh token.
#[tokio::test]
async fn create_session_stores_a_ninety_day_refresh_token_expiry() {
    let postgres = common::get_postgres().await;
    let client = common::get_client(&postgres).await;
    let (_access_jwt, refresh_jwt) = create_session(&client).await;

    let expiry = from_str_to_utc(&stored_expiry(&postgres, &jti_of(&refresh_jwt)).await);
    let remaining = expiry - Utc::now();

    assert!(
        remaining > Duration::days(89) && remaining < Duration::days(91),
        "expected a ~90d refresh token expiry, got {remaining} (expiry {expiry})"
    );
}
