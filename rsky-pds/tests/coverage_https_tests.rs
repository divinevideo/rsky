use rocket::config::TlsConfig;
use rocket::http::{ContentType, Header, Status};
use rocket::response::{Responder, Response};
use rocket::Request;
use rsky_identity::safe_fetch::{NetworkPolicy, SafeClient, SystemLookup};
use rsky_oauth::client::ClientMetadataFetcher;
use rsky_pds::oauth::fetcher::HttpClientMetadataFetcher;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

struct Document(String);

fn signing_key() -> String {
    let secret = secp256k1::SecretKey::from_slice(&[0x11; 32]).unwrap();
    rsky_crypto::utils::encode_did_key(&secret.public_key(&secp256k1::Secp256k1::new()))
}

impl<'r> Responder<'r, 'static> for Document {
    fn respond_to(self, _: &'r Request<'_>) -> rocket::response::Result<'static> {
        let (status, content_type, body) = match self.0.as_str() {
            "did" | "missing-service" | "missing-key" => {
                let did = "did:web:identity.example.test";
                let methods = if self.0 == "missing-key" {
                    vec![]
                } else {
                    vec![json!({
                        "id":format!("{did}#atproto"), "controller":did, "type":"Multikey",
                        "publicKeyMultibase":signing_key().strip_prefix("did:key:").unwrap()
                    })]
                };
                let services = if self.0 == "missing-service" {
                    vec![]
                } else {
                    vec![json!({
                        "id":"#atproto_pds", "type":"AtprotoPersonalDataServer", "serviceEndpoint":"https://pds.example.test"
                    })]
                };
                (
                    Status::Ok,
                    ContentType::JSON,
                    json!({"id":did,"verificationMethod":methods,"service":services}).to_string(),
                )
            }
            "status" => (
                Status::ServiceUnavailable,
                ContentType::JSON,
                "{}".to_owned(),
            ),
            "redirect" => (Status::Found, ContentType::JSON, "{}".to_owned()),
            "type" => (Status::Ok, ContentType::Plain, "{}".to_owned()),
            "large" => (Status::Ok, ContentType::JSON, " ".repeat(512 * 1024 + 1)),
            "malformed" => (Status::Ok, ContentType::JSON, "{".to_owned()),
            "jwks" => (
                Status::Ok,
                ContentType::JSON,
                json!({"keys":[]}).to_string(),
            ),
            _ => (
                Status::Ok,
                ContentType::JSON,
                json!({
                    "client_id":"https://client.example.test/metadata",
                    "redirect_uris":["https://client.example.test/callback"],
                    "token_endpoint_auth_method":"none",
                    "grant_types":["authorization_code","refresh_token"],
                    "response_types":["code"],
                    "application_type":"web",
                    "dpop_bound_access_tokens":true
                })
                .to_string(),
            ),
        };
        Response::build()
            .status(status)
            .header(content_type)
            .header(Header::new(
                "Location",
                "https://redirect-must-not-be-followed.invalid/",
            ))
            .sized_body(body.len(), std::io::Cursor::new(body))
            .ok()
    }
}

#[rocket::get("/<path>")]
fn document(path: &str) -> Document {
    Document(path.to_owned())
}

#[tokio::test]
async fn metadata_fetches_validate_https_and_reject_bad_responses() {
    std::env::set_var("PDS_HOSTNAME", "pds.example.test");
    std::env::set_var(
        "PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX",
        "1111111111111111111111111111111111111111111111111111111111111111",
    );
    let dir = tempfile::tempdir().unwrap();
    let certificate = dir.path().join("certificate.pem");
    let key = dir.path().join("key.pem");
    // A fresh private CA signs a separate leaf; trust stays scoped to this client.
    let openssl = |args: &[&str]| {
        let output = std::process::Command::new("openssl")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "1",
        "-subj",
        "/CN=Coverage Test CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
        "-keyout",
        "ca-key.pem",
        "-out",
        "ca.pem",
    ]);
    openssl(&[
        "req",
        "-new",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-subj",
        "/CN=localhost",
        "-keyout",
        "key.pem",
        "-out",
        "server.csr",
    ]);
    std::fs::write(dir.path().join("extensions.cnf"), "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n").unwrap();
    openssl(&[
        "x509",
        "-req",
        "-in",
        "server.csr",
        "-CA",
        "ca.pem",
        "-CAkey",
        "ca-key.pem",
        "-CAcreateserial",
        "-days",
        "1",
        "-extfile",
        "extensions.cnf",
        "-out",
        "certificate.pem",
    ]);
    let root_pem = std::fs::read(dir.path().join("ca.pem")).unwrap();
    let pem = std::fs::read(&certificate).unwrap();
    let private = std::fs::read(&key).unwrap();
    let (notify, ready) = tokio::sync::oneshot::channel();
    let server = rocket::custom(rocket::Config {
        address: "127.0.0.1".parse().unwrap(),
        port: 0,
        tls: Some(TlsConfig::from_bytes(&pem, &private)),
        log_level: rocket::config::LogLevel::Off,
        ..rocket::Config::debug_default()
    })
    .mount("/", rocket::routes![document])
    .attach(rocket::fairing::AdHoc::on_liftoff("ready", move |rocket| {
        Box::pin(async move {
            notify.send(rocket.config().port).unwrap();
        })
    }))
    .ignite()
    .await
    .unwrap();
    let shutdown = server.shutdown();
    let task = tokio::spawn(server.launch());
    let port = tokio::time::timeout(Duration::from_secs(10), ready)
        .await
        .unwrap()
        .unwrap();
    let roots = vec![reqwest::Certificate::from_pem(&root_pem).unwrap()];
    let client = SafeClient::with_lookup_and_roots(
        NetworkPolicy::PERMISSIVE,
        Duration::from_secs(5),
        Arc::new(SystemLookup::new()),
        roots,
    )
    .unwrap();
    // Derived transports retain the same explicit trust roots.
    let url = format!("https://127.0.0.1:{port}");
    assert_eq!(
        client
            .builder()
            .build()
            .unwrap()
            .get(format!("{url}/jwks"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let fetcher = HttpClientMetadataFetcher::from_client(client.clone());
    let metadata = fetcher
        .fetch_client_metadata(&format!("{url}/metadata"))
        .await
        .unwrap();
    assert_eq!(metadata.client_id, "https://client.example.test/metadata");
    assert!(fetcher
        .fetch_jwks(&format!("{url}/jwks"))
        .await
        .unwrap()
        .keys
        .is_empty());
    for (path, expected) in [
        ("status", "unexpected status 503"),
        ("redirect", "unexpected status 302"),
        ("type", "unexpected content-type"),
        ("large", "exceeds"),
        ("malformed", "invalid JWKS document"),
    ] {
        let error = fetcher
            .fetch_jwks(&format!("{url}/{path}"))
            .await
            .unwrap_err();
        assert!(error.error_description().contains(expected), "{error}");
    }
    let error = fetcher
        .fetch_client_metadata(&format!("{url}/malformed"))
        .await
        .unwrap_err();
    assert!(error
        .error_description()
        .contains("invalid client metadata document"));
    // Permitting loopback does not implicitly trust its certificate.
    let untrusted = HttpClientMetadataFetcher::new(NetworkPolicy::PERMISSIVE);
    assert!(untrusted.fetch_jwks(&format!("{url}/jwks")).await.is_err());
    let public = SafeClient::with_lookup_and_roots(
        NetworkPolicy::PUBLIC,
        Duration::from_secs(5),
        Arc::new(SystemLookup::new()),
        vec![reqwest::Certificate::from_pem(&root_pem).unwrap()],
    )
    .unwrap();
    let error = HttpClientMetadataFetcher::from_client(public)
        .fetch_jwks(&format!("{url}/jwks"))
        .await
        .unwrap_err();
    assert!(error.error_description().contains("loopback"));
    use rsky_pds::apis::com::atproto::server::assert_valid_web_did_document;
    assert_valid_web_did_document(
        &client,
        format!("{url}/did").parse().unwrap(),
        &signing_key(),
    )
    .await
    .unwrap();
    for (path, expected) in [
        ("status", "answered 503"),
        ("missing-service", "service endpoint does not match"),
        ("missing-key", "verification method does not match"),
    ] {
        let error = assert_valid_web_did_document(
            &client,
            format!("{url}/{path}").parse().unwrap(),
            &signing_key(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert!(assert_valid_web_did_document(
        &client,
        format!("{url}/malformed").parse().unwrap(),
        &signing_key()
    )
    .await
    .is_err());
    shutdown.notify();
    task.await.unwrap().unwrap();
}
