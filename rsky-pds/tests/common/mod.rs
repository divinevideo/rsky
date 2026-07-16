#![allow(dead_code)]

use anyhow::Result;
use diesel::{Connection, PgConnection};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use http_auth_basic::Credentials;
use jwt_simple::prelude::*;
use rocket::http::{ContentType, Header};
use rocket::local::asynchronous::Client;
use rocket::serde::json::json;
use rsky_common::env::env_str;
use rsky_lexicon::com::atproto::server::CreateInviteCodeOutput;
use rsky_pds::config::ServerConfig;
use rsky_pds::{build_rocket, RocketConfig};
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres;
use testcontainers_modules::postgres::Postgres;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();
const TEST_ADMIN_PASSWORD: &str = "test-admin-password";
const TEST_REPO_SIGNING_KEY_HEX: &str =
    "4f3edf983ac636a65a842ce7c78d9aa706d3b113bce036f4aeb4f7f7a5c5f3cf";
const TEST_PLC_ROTATION_KEY_HEX: &str =
    "6c3699283bda56ad74f6b855546325b68d482e983852a5b0d1f5b0f8d7e79b4f";
const TEST_JWT_KEY_HEX: &str = "8f2a55949068468ad5d670dfd0c0a33d5b9e7e1a2c0d2059f0f8f8779d4d078d";

/**
    Establish connection to the testcontainer postgres
*/
#[tracing::instrument(skip_all)]
pub fn establish_connection(database_url: &str) -> Result<PgConnection> {
    tracing::debug!("Establishing database connection");
    let result = PgConnection::establish(database_url).map_err(|error| {
        let context = format!("Error connecting to {database_url:?}");
        anyhow::Error::new(error).context(context)
    })?;

    Ok(result)
}

/**
    Fetch the configured admin password to be used for creating initial accounts
*/
pub fn get_admin_token() -> String {
    let admin_password = env_str("PDS_ADMIN_PASSWORD")
        .or_else(|| env_str("PDS_ADMIN_PASS"))
        .unwrap_or_else(|| TEST_ADMIN_PASSWORD.to_string());
    let credentials = Credentials::new("admin", admin_password.as_str());
    credentials.as_http_header()
}

/**
    Starts a testcontainer for a postgres instance, and runs migrations from rsky-pds
*/
pub async fn get_postgres() -> ContainerAsync<Postgres> {
    let postgres = postgres::Postgres::default()
        .start()
        .await
        .expect("Valid postgres instance");
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let connection_string = format!("postgres://postgres:postgres@localhost:{port}/postgres",);
    let mut conn =
        establish_connection(connection_string.as_str()).expect("Connection  Established");
    conn.run_pending_migrations(MIGRATIONS).unwrap();
    postgres
}

/**
    Start Client for the RSky-PDS and have it use the provided postgres container
*/
pub async fn get_client(postgres: &ContainerAsync<Postgres>) -> Client {
    unsafe {
        std::env::set_var("PDS_ADMIN_PASSWORD", TEST_ADMIN_PASSWORD);
        std::env::set_var("PDS_ADMIN_PASS", TEST_ADMIN_PASSWORD);
        std::env::set_var("PDS_HOSTNAME", "localhost");
        std::env::set_var("PDS_SERVICE_DID", "did:web:localhost");
        std::env::set_var("PDS_SERVICE_HANDLE_DOMAINS", ".test");
        std::env::set_var("PDS_JWT_KEY_K256_PRIVATE_KEY_HEX", TEST_JWT_KEY_HEX);
        std::env::set_var(
            "PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX",
            TEST_REPO_SIGNING_KEY_HEX,
        );
        std::env::set_var(
            "PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX",
            TEST_PLC_ROTATION_KEY_HEX,
        );
    }
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let connection_string = format!("postgres://postgres:postgres@localhost:{port}/postgres",);
    Client::untracked(
        build_rocket(Some(RocketConfig {
            db_url: String::from(connection_string),
        }))
        .await,
    )
    .await
    .expect("Valid Rocket instance")
}

/**
    Creates a mock account for testing purposes
*/
pub async fn create_account(client: &Client) -> (String, String) {
    let domain = client
        .rocket()
        .state::<ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains
        .first()
        .unwrap();
    let input = json!({
        "useCount": 1
    });

    let response = client
        .post("/xrpc/com.atproto.server.createInviteCode")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", get_admin_token()))
        .body(input.to_string())
        .dispatch()
        .await;
    let invite_code = response
        .into_json::<CreateInviteCodeOutput>()
        .await
        .unwrap()
        .code;

    let account_input = json!({
        "did": "did:plc:khvyd3oiw46vif5gm7hijslk",
        "email": "foo@example.com",
        "handle": format!("foo{domain}"),
        "password": "password",
        "inviteCode": invite_code
    });

    client
        .post("/xrpc/com.atproto.server.createAccount")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", get_admin_token()))
        .body(account_input.to_string())
        .dispatch()
        .await;

    ("foo@example.com".to_string(), "password".to_string())
}

/// The `PDS_SERVICE_DID` configured by [`get_client`]; every session/refresh
/// token must carry this as its audience.
pub const TEST_SERVICE_DID: &str = "did:web:localhost";

/// Log in with `username`/`password` and return the account's real DID.
///
/// `createAccount` mints its own DID rather than honouring the one supplied, so
/// tokens whose `sub` must match a real account have to use this value.
pub async fn login_did(client: &Client, username: &str, password: &str) -> String {
    let response = client
        .post("/xrpc/com.atproto.server.createSession")
        .header(ContentType::JSON)
        .body(json!({ "identifier": username, "password": password }).to_string())
        .dispatch()
        .await;
    let session: serde_json::Value =
        serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    session["did"].as_str().expect("session did").to_string()
}

/// The custom claim payload rsky embeds in session tokens. Only `scope` matters
/// for the auth path; `lxm` defaults to absent on deserialization.
#[derive(serde::Serialize, serde::Deserialize)]
struct TestScopeClaim {
    scope: String,
}

/// Forge a session-style ES256K JWT with full control over its validity window,
/// signed by the raw 32-byte secp256k1 secret in `key_hex`.
///
/// `issued_ago_secs` / `expires_in_secs` are offsets from *now* (seconds).
/// A negative `expires_in_secs` produces a token whose `exp` is in the past.
/// Because jwt-simple's default clock tolerance is 900s, callers wanting a
/// genuinely-expired token must backdate `exp` by MORE than 15 minutes.
#[allow(clippy::too_many_arguments)]
pub fn sign_test_token(
    key_hex: &str,
    scope: &str,
    sub: &str,
    aud: &str,
    jti: Option<String>,
    issued_ago_secs: u64,
    expires_in_secs: i64,
) -> String {
    let key = ES256kKeyPair::from_bytes(&hex::decode(key_hex).expect("valid hex key"))
        .expect("valid ES256K key");
    let now = Clock::now_since_epoch();
    let issued_at = now - Duration::from_secs(issued_ago_secs);
    let expires_at = if expires_in_secs >= 0 {
        now + Duration::from_secs(expires_in_secs as u64)
    } else {
        now - Duration::from_secs((-expires_in_secs) as u64)
    };
    // `valid_for` here is overwritten below; jwt-simple just needs a seed value.
    let mut claims = Claims::with_custom_claims(
        TestScopeClaim {
            scope: scope.to_string(),
        },
        Duration::from_hours(2),
    )
    .with_audience(aud.to_string())
    .with_subject(sub.to_string());
    claims.issued_at = Some(issued_at);
    claims.invalid_before = Some(issued_at);
    claims.expires_at = Some(expires_at);
    if let Some(jti) = jti {
        claims = claims.with_jwt_id(jti);
    }
    key.sign(claims).expect("sign test token")
}

/// A genuinely-expired access token (exp 1h in the past), signed by the JWT key.
pub fn expired_access_token(did: &str) -> String {
    sign_test_token(
        TEST_JWT_KEY_HEX,
        "com.atproto.access",
        did,
        TEST_SERVICE_DID,
        None,
        10_800,
        -3_600,
    )
}

/// A genuinely-expired refresh token (exp 1h in the past), signed by the JWT
/// key and carrying a `jti` (required by the refresh guard).
pub fn expired_refresh_token(did: &str) -> String {
    sign_test_token(
        TEST_JWT_KEY_HEX,
        "com.atproto.refresh",
        did,
        TEST_SERVICE_DID,
        Some("expired-refresh-jti".to_string()),
        10_800,
        -3_600,
    )
}

/// A currently-valid access token (exp 2h in the future), signed by the JWT key.
pub fn valid_access_token(did: &str) -> String {
    sign_test_token(
        TEST_JWT_KEY_HEX,
        "com.atproto.access",
        did,
        TEST_SERVICE_DID,
        None,
        0,
        7_200,
    )
}

/// A currently-valid access token signed by the REPO signing key (as an
/// external service such as video.bsky.app sends). Exercises the repo-key
/// fallback in `validate_bearer_token`.
pub fn repo_key_signed_access_token(did: &str) -> String {
    sign_test_token(
        TEST_REPO_SIGNING_KEY_HEX,
        "com.atproto.access",
        did,
        TEST_SERVICE_DID,
        None,
        0,
        7_200,
    )
}
