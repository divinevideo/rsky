mod common;

use rocket::http::{ContentType, Header, Status};
use rocket::request::{FromRequest, Outcome};
use rsky_pds::account_manager::AccountManager;
use rsky_pds::actor_store::ActorStore;
use rsky_pds::auth_verifier::{AuthError, ModService};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

// This binary owns its process environment: the PLC failure fixture must not
// replace the working directory used by other account integration tests.
#[tokio::test]
async fn rejected_plc_publication_rolls_back_the_new_account_repository() {
    std::env::remove_var("PDS_MOD_SERVICE_DID");
    let bare = rocket::local::asynchronous::Client::tracked(rocket::build())
        .await
        .unwrap();
    let request = bare.get("/xrpc/com.atproto.admin.getAccountInfo");
    assert!(matches!(
        ModService::from_request(request.inner()).await,
        Outcome::Error((status, AuthError::UntrustedIss(_))) if status == Status::BadRequest
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "PDS_DID_PLC_URL",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let (published, received) = tokio::sync::oneshot::channel();
    let directory = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_fixture_request(&mut stream).await;
        let mut words = request.split_whitespace();
        assert_eq!(words.next(), Some("POST"));
        let did = urlencoding::decode(words.next().unwrap().trim_start_matches('/'))
            .unwrap()
            .into_owned();
        published.send(did).unwrap();
        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let (_dir, client) = common::get_client().await;
    let admin = common::get_admin_token();
    let response = client
        .post("/xrpc/com.atproto.server.createInviteCode")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", admin.clone()))
        .body(json!({"useCount": 1}).to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let invite: Value = response.into_json().await.unwrap();
    let domain = &client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0];
    let response = client.post("/xrpc/com.atproto.server.createAccount")
        .header(ContentType::JSON).header(Header::new("Authorization", admin))
        .body(json!({"handle": format!("publication{domain}"), "email": "publication@example.com", "password": "password", "inviteCode": invite["code"]}).to_string())
        .dispatch().await;
    assert_eq!(response.status(), Status::InternalServerError);
    let body: Value = response.into_json().await.unwrap();
    assert!(body.get("accessJwt").is_none());
    let did = received.await.unwrap();
    assert!(did.starts_with("did:plc:"));
    assert_eq!(did.len(), 32);
    assert!(!client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .exists(&did)
        .await
        .unwrap());
    assert!(client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .get_account(&did, None)
        .await
        .unwrap()
        .is_none());
    directory.await.unwrap();
}

/// Read complete HTTP headers and any declared body, even when TCP fragments
/// the request. These local fixtures accept at most 16 KiB headers/64 KiB body.
async fn read_fixture_request(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let headers = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let header_end = end + 4;
                if header_end > 16 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "fixture HTTP headers exceed 16 KiB",
                    ));
                }
                let headers = std::str::from_utf8(&bytes[..end])
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                let body_len = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse::<usize>())
                    .transpose()
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
                    .unwrap_or(0);
                if body_len > 64 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "fixture HTTP body exceeds 64 KiB",
                    ));
                }
                if bytes.len() >= header_end + body_len {
                    bytes.truncate(header_end);
                    return String::from_utf8(bytes).map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    });
                }
            } else if bytes.len() > 16 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "fixture HTTP headers exceed 16 KiB",
                ));
            }
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).await?;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "fixture client disconnected before completing its request",
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
    })
    .await
    .expect("fixture HTTP request timed out")
    .expect("fixture HTTP request was incomplete, oversized, or invalid");
    headers
}
