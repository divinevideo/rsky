use anyhow::Result;
use diesel::{Connection, PgConnection};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use http_auth_basic::Credentials;
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
const TEST_JWT_KEY_HEX: &str =
    "8f2a55949068468ad5d670dfd0c0a33d5b9e7e1a2c0d2059f0f8f8779d4d078d";

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
