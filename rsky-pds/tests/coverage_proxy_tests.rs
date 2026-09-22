//! Real PDS proxy/read-after-write contracts, with only remote services replaced
//! by an HTTP fixture. All accounts, repositories, signatures and SQLite reads
//! remain production components.
mod common;

use base64::Engine;
use rocket::http::{ContentType, Header, Method, Status};
use rocket::local::asynchronous::Client;
use rocket::State;
use rsky_pds::account_manager::AccountManager;
use rsky_pds::actor_store::{blobstore::BlobstoreFactory, ActorStore};
use rsky_pds::config::ServerConfig;
use rsky_pds::pipethrough::{self, OverrideOpts, ProxyRequest};
use rsky_pds::read_after_write::viewer::{LocalViewer, LocalViewerCreatorParams};
use rsky_pds::SharedIdResolver;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const CID: &str = "bafyreie5cvv4xcz3kbwdhmwbfvpvbcg6aicjkvw5pluvbhqz5dg4z5f2pa";
const APP_DID: &str = "did:plc:appviewcoveragefixture";

#[derive(Clone)]
struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
struct Received {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: String,
}

struct Upstream {
    url: String,
    replies: Arc<Mutex<BTreeMap<String, Reply>>>,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let replies = Arc::new(Mutex::new(BTreeMap::<String, Reply>::new()));
        let received = Arc::new(Mutex::new(Vec::new()));
        let (reply_map, requests, base) = (replies.clone(), received.clone(), url.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(at + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let head = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                let headers: BTreeMap<String, String> = lines
                    .filter_map(|line| {
                        line.split_once(':')
                            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    })
                    .collect();
                let length: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                requests.lock().unwrap().push(Received {
                    method,
                    target: target.clone(),
                    headers,
                    body: String::from_utf8_lossy(&bytes[header_end..]).into_owned(),
                });
                let path = target.split('?').next().unwrap();
                let reply = {
                    let replies = reply_map.lock().unwrap();
                    replies.get(&target).or_else(|| replies.get(path)).cloned()
                }.unwrap_or_else(|| {
                    let did = path.trim_start_matches('/').replace("%3A", ":").replace("%3a", ":");
                    Reply { status: 200, headers: vec![], body: json!({
                        "id": did,
                        "alsoKnownAs": ["at://foo.rsky.com"],
                        "verificationMethod": [],
                        "service": [
                            {"id":"#atproto_pds","type":"AtprotoPersonalDataServer","serviceEndpoint":base},
                            {"id":"#bsky_notif","type":"BskyNotificationService","serviceEndpoint":base},
                            {"id":"#bsky_appview","type":"BskyAppView","serviceEndpoint":base}
                        ]
                    }).to_string() }
                });
                let encoding = if reply.body.is_empty() {
                    ""
                } else {
                    "Content-Type: application/json\r\n"
                };
                let mut response = format!(
                    "HTTP/1.1 {} Fixture\r\n{encoding}Content-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (key, value) in reply.headers {
                    response.push_str(&format!("{key}: {value}\r\n"));
                }
                response.push_str("\r\n");
                response.push_str(&reply.body);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            replies,
            received,
        }
    }

    fn reply(&self, method: &str, body: Value, rev: Option<&str>) {
        self.raw_reply(method, 200, body.to_string(), rev);
    }

    fn raw_reply(&self, method: &str, status: u16, body: String, rev: Option<&str>) {
        let mut headers = vec![
            ("content-language".into(), "en".into()),
            ("x-private-upstream".into(), "secret".into()),
        ];
        if let Some(rev) = rev {
            headers.push(("atproto-repo-rev".into(), rev.into()));
        }
        self.replies.lock().unwrap().insert(
            format!("/xrpc/{method}"),
            Reply {
                status,
                body,
                headers,
            },
        );
    }

    fn last(&self, method: &str) -> Received {
        self.received
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|r| r.target.starts_with(&format!("/xrpc/{method}")))
            .unwrap()
            .clone()
    }
}

fn service_claims(request: &Received) -> Value {
    let token = request.headers["authorization"]
        .strip_prefix("Bearer ")
        .unwrap();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1).unwrap())
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn verify_service_signature(client: &Client, did: &str, request: &Received) {
    use sha2::Digest;
    let token = request.headers["authorization"]
        .strip_prefix("Bearer ")
        .unwrap();
    let (signed, encoded_signature) = token.rsplit_once('.').unwrap();
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded_signature)
        .unwrap();
    let signature = secp256k1::ecdsa::Signature::from_compact(&signature).unwrap();
    let message = secp256k1::Message::from_digest(sha2::Sha256::digest(signed.as_bytes()).into());
    let keypair = client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .keypair(did)
        .await
        .unwrap();
    signature.verify(&message, &keypair.public_key()).unwrap();
}

async fn login(client: &Client) -> (String, String) {
    common::create_account(client).await;
    let res = client
        .post("/xrpc/com.atproto.server.createSession")
        .header(ContentType::JSON)
        .body(json!({"identifier":"foo@example.com","password":"password"}).to_string())
        .dispatch()
        .await;
    assert_eq!(res.status(), Status::Ok);
    let body = res.into_json::<Value>().await.unwrap();
    let did = body["did"].as_str().unwrap().to_owned();
    // Imported-DID accounts begin deactivated. Seed an active account through
    // the production account manager before exercising repository writes.
    client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .activate_account(&did)
        .await
        .unwrap();
    (did, body["accessJwt"].as_str().unwrap().into())
}

async fn get(client: &Client, path: &str, token: Option<&str>) -> (Status, Value) {
    let mut req = client.get(path);
    if let Some(token) = token {
        req = req.header(Header::new("Authorization", format!("Bearer {token}")));
    }
    let res = req.dispatch().await;
    let status = res.status();
    let text = res.into_string().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(json!(text)))
}

async fn put_record(
    client: &Client,
    token: &str,
    did: &str,
    collection: &str,
    rkey: &str,
    record: Value,
) -> Value {
    let res = client
        .post("/xrpc/com.atproto.repo.putRecord")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .body(json!({"repo":did,"collection":collection,"rkey":rkey,"record":record}).to_string())
        .dispatch()
        .await;
    let status = res.status();
    let body = res.into_json::<Value>().await.unwrap();
    assert_eq!(status, Status::Ok, "{body}");
    body
}

async fn viewer(client: &Client, did: &str, appview: Option<&str>) -> LocalViewer {
    let store = client.rocket().state::<ActorStore>().unwrap();
    let blobs = client.rocket().state::<BlobstoreFactory>().unwrap();
    let reader = store
        .read(did.to_owned(), blobs.blobstore(did.to_owned()))
        .await
        .unwrap();
    LocalViewer::creator(LocalViewerCreatorParams {
        pds_hostname: "pds.example.test".into(),
        appview_agent: appview.map(str::to_owned),
        appview_did: appview.map(|_| APP_DID.into()),
        appview_cdn_url_pattern: Some("https://images.example.test/{}/{}/{}".into()),
    })(
        reader,
        client.rocket().state::<AccountManager>().unwrap().clone(),
    )
}

fn post_record(text: &str) -> Value {
    json!({"$type":"app.bsky.feed.post","text":text,"createdAt":"2026-01-01T00:00:00.000Z"})
}

fn profile(did: &str) -> Value {
    json!({"did":did,"handle":"foo.rsky.com","displayName":"Upstream name","description":"Upstream description","labels":[],"postsCount":3})
}

static FIXTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static UPSTREAM: std::sync::OnceLock<Upstream> = std::sync::OnceLock::new();

fn upstream_fixture() -> &'static Upstream {
    UPSTREAM.get_or_init(|| {
        let upstream = Upstream::start();
        std::env::set_var("PDS_DEV_MODE", "true");
        std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
        std::env::set_var("PDS_DID_PLC_URL", &upstream.url);
        std::env::set_var("PDS_BSKY_APP_VIEW_URL", &upstream.url);
        std::env::set_var("PDS_BSKY_APP_VIEW_DID", APP_DID);
        upstream
    })
}

#[rocket::async_test]
async fn preferences_and_generic_post_contracts() {
    let _lock = FIXTURE_LOCK.lock().await;
    let upstream = upstream_fixture();
    let (_dir, client) = common::get_client().await;
    let (did, token) = login(&client).await;
    preferences_round_trip(&client, &token).await;
    generic_post_forwarder_enforces_privilege(&client, upstream, &did, &token).await;
}

#[rocket::async_test]
async fn actor_feed_generator_and_push_routing_contracts() {
    let _lock = FIXTURE_LOCK.lock().await;
    let upstream = upstream_fixture();
    let (_dir, client) = common::get_client().await;
    let (did, token) = login(&client).await;
    proxy_routes_forward_auth_and_queries(&client, upstream, &did, &token).await;
    feed_generator_uses_record_audience(&client, upstream, &did, &token).await;
    push_registration_uses_selected_service(&client, upstream, &did, &token).await;
}

#[rocket::async_test]
async fn proxy_transport_contracts() {
    let _lock = FIXTURE_LOCK.lock().await;
    let upstream = upstream_fixture();
    let (_dir, client) = common::get_client().await;
    proxy_transport_preserves_errors_and_body(&client, upstream).await;
}

#[rocket::async_test]
async fn local_writes_and_embed_hydration_contracts() {
    let _lock = FIXTURE_LOCK.lock().await;
    let upstream = upstream_fixture();
    let (_dir, client) = common::get_client().await;
    let (did, token) = login(&client).await;
    recent_writes_replace_stale_profiles(&client, upstream, &did, &token).await;
    local_viewer_formats_embeds(&client, upstream, &did, &token).await;
    local_storage_failures_leave_upstream_readable(&client, upstream, &did, &token).await;
}

#[rocket::async_test]
async fn local_appview_routes_report_absent_configuration() {
    let _lock = FIXTURE_LOCK.lock().await;
    let upstream = upstream_fixture();
    // Complete the shared harness's one-time defaults before intentionally
    // booting an instance without an appview configuration.
    let (_bootstrap_dir, _bootstrap_client) = common::get_client().await;
    std::env::remove_var("PDS_BSKY_APP_VIEW_URL");
    let (_dir, client) = common::get_client().await;
    std::env::set_var("PDS_BSKY_APP_VIEW_URL", &upstream.url);
    assert!(client
        .rocket()
        .state::<ServerConfig>()
        .unwrap()
        .bsky_app_view
        .is_none());
    let (did, token) = login(&client).await;
    for (nsid, query, expected) in [
        (
            "app.bsky.actor.getProfile",
            format!("actor={did}"),
            Status::BadRequest,
        ),
        (
            "app.bsky.actor.getProfiles",
            format!("actors={did}"),
            Status::BadRequest,
        ),
        (
            "app.bsky.feed.getActorLikes",
            format!("actor={did}"),
            Status::InternalServerError,
        ),
        (
            "app.bsky.feed.getAuthorFeed",
            format!("actor={did}"),
            Status::BadRequest,
        ),
        (
            "app.bsky.feed.getTimeline",
            "limit=5".into(),
            Status::InternalServerError,
        ),
        (
            "app.bsky.feed.getPostThread",
            format!("uri=at://{did}/app.bsky.feed.post/one"),
            Status::InternalServerError,
        ),
    ] {
        upstream.reply(nsid, json!({"feed":[]}), None);
        let response = client
            .get(format!("/xrpc/{nsid}?{query}"))
            .header(Header::new("Authorization", format!("Bearer {token}")))
            .header(Header::new(
                "atproto-proxy",
                "did:plc:configuredproxy#bsky_appview",
            ))
            .dispatch()
            .await;
        assert_eq!(
            response.status(),
            expected,
            "{nsid}: {}",
            response.into_string().await.unwrap_or_default()
        );
    }
    for nsid in [
        "app.bsky.notification.registerPush",
        "app.bsky.notification.unregisterPush",
    ] {
        let response = client.post(format!("/xrpc/{nsid}")).header(ContentType::JSON)
            .header(Header::new("Authorization",format!("Bearer {token}")))
            .body(json!({"serviceDid":APP_DID,"token":"synthetic","platform":"web","appId":"test.example"}).to_string())
            .dispatch().await;
        assert_eq!(response.status(), Status::InternalServerError);
    }
    // The reusable inner operation can discover the notification endpoint
    // without a configured default appview. Obtain its auth through the real
    // request guard so endpoint discovery and signing retain production checks.
    use rocket::request::{FromRequest, Outcome};
    use rsky_pds::auth_verifier::scope::{RpcCall, Scoped};
    let nsid = "app.bsky.notification.unregisterPush";
    upstream.raw_reply(nsid, 200, String::new(), None);
    let request = client
        .post(format!("/xrpc/{nsid}"))
        .header(Header::new("Authorization", format!("Bearer {token}")));
    let auth = match Scoped::<RpcCall>::from_request(request.inner()).await {
        Outcome::Success(auth) => auth,
        Outcome::Error(error) => panic!("authenticated request guard failed: {error:?}"),
        Outcome::Forward(status) => panic!("authenticated request guard forwarded: {status}"),
    };
    let body = json!({"serviceDid":"did:plc:discoverednotification","token":"synthetic","platform":"web","appId":"test.example"});
    rsky_pds::apis::app::bsky::notification::unregister_push::inner_unregister_push(
        rocket::serde::json::Json(serde_json::from_value(body.clone()).unwrap()),
        auth,
        State::from(client.rocket().state::<ServerConfig>().unwrap()),
        "https://unused-appview.example.test".into(),
        State::from(client.rocket().state::<SharedIdResolver>().unwrap()),
        State::from(client.rocket().state::<ActorStore>().unwrap()),
    )
    .await
    .unwrap();
    let sent = upstream.last(nsid);
    assert_eq!(serde_json::from_str::<Value>(&sent.body).unwrap(), body);
    assert_eq!(service_claims(&sent)["iss"], did);
    assert_eq!(
        service_claims(&sent)["aud"],
        "did:plc:discoverednotification"
    );
    assert_eq!(service_claims(&sent)["lxm"], nsid);
}

async fn generic_post_forwarder_enforces_privilege(
    client: &Client,
    upstream: &Upstream,
    did: &str,
    token: &str,
) {
    let nsid = "chat.bsky.convo.sendMessage";
    upstream.reply(nsid, json!({"id":"message-1","rev":"1","text":"hello","sender":{"did":did},"sentAt":"2026-01-01T00:00:00.000Z"}), None);
    let payload = json!({"convoId":"conversation-fixture","message":{"text":"hello"}});
    let denied = client
        .post(format!("/xrpc/{nsid}"))
        .header(ContentType::JSON)
        .body(payload.to_string())
        .dispatch()
        .await;
    assert_eq!(denied.status(), Status::Unauthorized);
    assert_eq!(
        denied.into_json::<Value>().await.unwrap()["error"],
        "AuthMissing"
    );
    let before = upstream.received.lock().unwrap().len();
    for malformed in ["not JSON", "{"] {
        let response = client
            .post(format!("/xrpc/{nsid}"))
            .header(ContentType::JSON)
            .header(Header::new("Authorization", format!("Bearer {token}")))
            .body(malformed)
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::InternalServerError);
    }
    assert_eq!(upstream.received.lock().unwrap().len(), before);
    let response = client
        .post(format!("/xrpc/{nsid}"))
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .body(payload.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        response.into_json::<Value>().await.unwrap()["id"],
        "message-1"
    );
    let sent = upstream.last(nsid);
    assert_eq!(serde_json::from_str::<Value>(&sent.body).unwrap(), payload);
    assert_eq!(service_claims(&sent)["iss"], did);
    assert_eq!(service_claims(&sent)["lxm"], nsid);
    for (privileged, expected) in [(false, Status::BadRequest)] {
        let response = client
            .post("/xrpc/com.atproto.server.createAppPassword")
            .header(ContentType::JSON)
            .header(Header::new("Authorization", format!("Bearer {token}")))
            .body(
                json!({"name":format!("proxy-contract-{privileged}"),"privileged":privileged})
                    .to_string(),
            )
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
        let password = response.into_json::<Value>().await.unwrap()["password"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = client
            .post("/xrpc/com.atproto.server.createSession")
            .header(ContentType::JSON)
            .body(json!({"identifier":did,"password":password}).to_string())
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
        let session = response.into_json::<Value>().await.unwrap();
        let response = client
            .post(format!("/xrpc/{nsid}"))
            .header(ContentType::JSON)
            .header(Header::new(
                "Authorization",
                format!("Bearer {}", session["accessJwt"].as_str().unwrap()),
            ))
            .body(payload.to_string())
            .dispatch()
            .await;
        assert_eq!(response.status(), expected);
        if !privileged {
            assert_eq!(
                response.into_json::<Value>().await.unwrap()["error"],
                "InvalidToken"
            );
        }
    }
}

async fn preferences_round_trip(client: &Client, token: &str) {
    assert_eq!(
        get(client, "/xrpc/app.bsky.actor.getPreferences", Some(token)).await,
        (Status::Ok, json!({"preferences":[]}))
    );
    let prefs =
        json!({"preferences":[{"$type":"app.bsky.actor.defs#adultContentPref","enabled":true}]});
    let response = client
        .post("/xrpc/app.bsky.actor.putPreferences")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .body(prefs.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        get(client, "/xrpc/app.bsky.actor.getPreferences", Some(token))
            .await
            .1,
        prefs
    );
}

async fn proxy_routes_forward_auth_and_queries(
    client: &Client,
    upstream: &Upstream,
    did: &str,
    token: &str,
) {
    let routes = [
        (
            "app.bsky.actor.getProfile",
            format!("actor={did}"),
            profile(did),
        ),
        (
            "app.bsky.actor.getProfiles",
            format!("actors={did}"),
            json!({"profiles":[profile(did)]}),
        ),
        (
            "app.bsky.feed.getActorLikes",
            format!("actor={did}&limit=5"),
            json!({"feed":[],"cursor":"next"}),
        ),
        (
            "app.bsky.feed.getAuthorFeed",
            format!("actor={did}&limit=5"),
            json!({"feed":[]}),
        ),
        (
            "app.bsky.feed.getTimeline",
            "limit=5".into(),
            json!({"feed":[]}),
        ),
        (
            "app.bsky.feed.getPostThread",
            format!("uri=at://{did}/app.bsky.feed.post/remote"),
            json!({"thread":{"$type":"app.bsky.feed.defs#notFoundPost","uri":format!("at://{did}/app.bsky.feed.post/remote"),"notFound":true}}),
        ),
    ];
    for (nsid, query, body) in &routes {
        upstream.reply(nsid, body.clone(), None);
        let path = format!("/xrpc/{nsid}?{query}");
        let (status, value) = get(client, &path, Some(token)).await;
        assert_eq!(status, Status::Ok, "{nsid}: {value}");
        assert_eq!(&value, body);
        let request = upstream.last(nsid);
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, path);
        verify_service_signature(client, did, &request).await;
        let claims = service_claims(&request);
        assert_eq!(claims["iss"], did);
        assert_eq!(claims["aud"], APP_DID);
        assert_eq!(claims["lxm"], *nsid);
        if *nsid != "app.bsky.feed.getPostThread" {
            let (status, error) = get(client, &path, None).await;
            assert_eq!(status, Status::Unauthorized);
            assert_eq!(error["error"], "AuthMissing");
        }
    }
    upstream.reply("app.bsky.actor.getProfile", profile(did), None);
    let response = client
        .get(format!("/xrpc/app.bsky.actor.getProfile?actor={did}"))
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .header(Header::new("accept-language", "fr"))
        .header(Header::new(
            "atproto-accept-labelers",
            "did:plc:labelerfixture",
        ))
        .header(Header::new("x-bsky-topics", "art"))
        .header(Header::new("x-private-client", "do-not-forward"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.headers().get_one("content-language"), Some("en"));
    assert_eq!(response.headers().get_one("x-private-upstream"), None);
    let sent = upstream.last("app.bsky.actor.getProfile");
    assert_eq!(sent.headers["accept-language"], "fr");
    assert_eq!(sent.headers["x-bsky-topics"], "art");
    assert_eq!(
        sent.headers["atproto-accept-labelers"],
        "did:plc:labelerfixture"
    );
    assert!(!sent.headers.contains_key("x-private-client"));
    upstream.raw_reply(
        "app.bsky.actor.getProfile",
        429,
        json!({"error":"RateLimitExceeded","message":"retry later"}).to_string(),
        None,
    );
    let (status, error) = get(
        client,
        &format!("/xrpc/app.bsky.actor.getProfile?actor={did}"),
        Some(token),
    )
    .await;
    assert_eq!(status, Status::TooManyRequests);
    assert_eq!(error["error"], "RateLimitExceeded");
    assert_eq!(error["message"], "retry later");
    upstream.raw_reply(
        "app.bsky.feed.getPostThread",
        403,
        json!({"error":"BlockedActor","message":"thread blocked"}).to_string(),
        None,
    );
    let (status, error) = get(
        client,
        &format!("/xrpc/app.bsky.feed.getPostThread?uri=at://{did}/app.bsky.feed.post/remote"),
        Some(token),
    )
    .await;
    assert_eq!(status, Status::BadRequest);
    assert_eq!(error["message"], "thread blocked");
    let (status, _) = get(
        client,
        &format!("/xrpc/app.bsky.feed.getPostThread?uri=at://{did}/app.bsky.feed.post/remote"),
        None,
    )
    .await;
    assert_eq!(status, Status::Unauthorized);
}

async fn feed_generator_uses_record_audience(
    client: &Client,
    upstream: &Upstream,
    did: &str,
    token: &str,
) {
    let feed = format!("at://{did}/app.bsky.feed.generator/art");
    upstream.reply(
        "com.atproto.repo.getRecord",
        json!({"uri":feed,"cid":CID,"value":{"did":"did:plc:generatorcoverage"}}),
        None,
    );
    upstream.reply(
        "app.bsky.feed.getFeed",
        json!({"feed":[],"cursor":"generator-page-2"}),
        None,
    );
    let path = format!("/xrpc/app.bsky.feed.getFeed?feed={feed}&limit=7&cursor=start");
    assert_eq!(
        get(client, &path, Some(token)).await,
        (Status::Ok, json!({"feed":[],"cursor":"generator-page-2"}))
    );
    let lookup = upstream.last("com.atproto.repo.getRecord");
    let query: BTreeMap<String, String> =
        url::form_urlencoded::parse(lookup.target.split_once('?').unwrap().1.as_bytes())
            .into_owned()
            .collect();
    assert_eq!(query["repo"], did);
    assert_eq!(query["collection"], "app.bsky.feed.generator");
    assert_eq!(query["rkey"], "art");
    assert_eq!(service_claims(&lookup)["aud"], APP_DID);
    let sent = upstream.last("app.bsky.feed.getFeed");
    verify_service_signature(client, did, &sent).await;
    assert_eq!(sent.target, path);
    assert_eq!(service_claims(&sent)["aud"], "did:plc:generatorcoverage");
    assert_eq!(
        service_claims(&sent)["lxm"],
        "app.bsky.feed.getFeedSkeleton"
    );
    for body in ["not-json".to_owned(), json!({"value":{}}).to_string()] {
        upstream.raw_reply("com.atproto.repo.getRecord", 200, body, None);
        assert_eq!(get(client, &path, Some(token)).await.0, Status::BadRequest);
    }
    upstream.raw_reply(
        "com.atproto.repo.getRecord",
        502,
        json!({"error":"UpstreamFailure","message":"lookup unavailable"}).to_string(),
        None,
    );
    assert_eq!(get(client, &path, Some(token)).await.0, Status::BadRequest);
    upstream.reply(
        "com.atproto.repo.getRecord",
        json!({"value":{"did":"did:plc:generatorcoverage"}}),
        None,
    );
    upstream.raw_reply(
        "app.bsky.feed.getFeed",
        503,
        json!({"error":"Unavailable","message":"generator offline"}).to_string(),
        None,
    );
    assert_eq!(get(client, &path, Some(token)).await.0, Status::BadRequest);
    upstream.reply("app.bsky.feed.getFeed", json!({"feed":[]}), None);
    let before = upstream.received.lock().unwrap().len();
    for invalid in ["abc", "256", "-1"] {
        let (status, error) = get(
            client,
            &format!("/xrpc/app.bsky.feed.getFeed?feed={feed}&limit={invalid}"),
            Some(token),
        )
        .await;
        assert_eq!(status, Status::BadRequest, "limit={invalid}: {error}");
    }
    assert_eq!(
        get(client, "/xrpc/app.bsky.feed.getFeed?limit=5", Some(token))
            .await
            .0,
        Status::BadRequest
    );
    assert_eq!(
        get(
            client,
            &format!("/xrpc/app.bsky.feed.getFeed?feed={feed}&limit=101"),
            Some(token)
        )
        .await
        .0,
        Status::BadRequest
    );
    let (status, error) = get(client, "/xrpc/app.bsky.feed.getFeed?feed=%20", Some(token)).await;
    assert_eq!(status, Status::BadRequest);
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("`feed` is invalid"));
    assert_eq!(upstream.received.lock().unwrap().len(), before);
    assert_eq!(
        get(client, &path, Some("invalid.jwt")).await.0,
        Status::BadRequest
    );
}

async fn push_registration_uses_selected_service(
    client: &Client,
    upstream: &Upstream,
    did: &str,
    token: &str,
) {
    for service in [APP_DID, "did:plc:notificationcoverage"] {
        for nsid in [
            "app.bsky.notification.registerPush",
            "app.bsky.notification.unregisterPush",
        ] {
            upstream.raw_reply(nsid, 200, String::new(), None);
            let body = json!({"serviceDid":service,"token":"synthetic-device-token","platform":"web","appId":"test.example"});
            let response = client
                .post(format!("/xrpc/{nsid}"))
                .header(ContentType::JSON)
                .header(Header::new("Authorization", format!("Bearer {token}")))
                .body(body.to_string())
                .dispatch()
                .await;
            let status = response.status();
            assert_eq!(
                status,
                Status::Ok,
                "{}",
                response.into_string().await.unwrap_or_default()
            );
            let sent = upstream.last(nsid);
            assert_eq!(serde_json::from_str::<Value>(&sent.body).unwrap(), body);
            assert_eq!(service_claims(&sent)["iss"], did);
            assert_eq!(service_claims(&sent)["aud"], service);
            assert_eq!(service_claims(&sent)["lxm"], nsid);
        }
    }
    upstream.replies.lock().unwrap().insert(
        "/did:plc:nopushservice".into(),
        Reply {
            status: 200,
            headers: vec![],
            body: json!({"id":"did:plc:nopushservice","service":[]}).to_string(),
        },
    );
    for nsid in [
        "app.bsky.notification.registerPush",
        "app.bsky.notification.unregisterPush",
    ] {
        let response = client.post(format!("/xrpc/{nsid}")).header(ContentType::JSON)
            .header(Header::new("Authorization",format!("Bearer {token}")))
            .body(json!({"serviceDid":"did:plc:nopushservice","token":"synthetic","platform":"web","appId":"test.example"}).to_string()).dispatch().await;
        assert_eq!(response.status(), Status::InternalServerError);
    }
    for nsid in [
        "app.bsky.notification.registerPush",
        "app.bsky.notification.unregisterPush",
    ] {
        upstream.raw_reply(
            nsid,
            500,
            json!({"error":"InternalServerError"}).to_string(),
            None,
        );
        let response = client.post(format!("/xrpc/{nsid}")).header(ContentType::JSON)
            .header(Header::new("Authorization", format!("Bearer {token}")))
            .body(json!({"serviceDid":APP_DID,"token":"synthetic","platform":"ios","appId":"test.example"}).to_string()).dispatch().await;
        assert_eq!(response.status(), Status::InternalServerError);
    }
}

async fn proxy_transport_preserves_errors_and_body(client: &Client, upstream: &Upstream) {
    use rocket::request::{FromRequest, Outcome};
    upstream.reply(
        "app.bsky.actor.getProfile",
        profile("did:plc:guardfixture"),
        None,
    );
    let unauthenticated = client.get("/xrpc/app.bsky.actor.getProfile?actor=did:plc:guardfixture");
    let outcome =
        rsky_pds::xrpc_server::types::HandlerPipeThrough::from_request(unauthenticated.inner())
            .await;
    assert!(matches!(outcome, Outcome::Error((status, _)) if status == Status::BadRequest));
    let invalid = client
        .get("/xrpc/app.bsky.actor.getProfile?actor=did:plc:guardfixture")
        .header(Header::new("Authorization", "Bearer invalid.jwt"));
    let outcome =
        rsky_pds::xrpc_server::types::HandlerPipeThrough::from_request(invalid.inner()).await;
    assert!(matches!(outcome, Outcome::Error((status, _)) if status == Status::BadRequest));
    let cfg = client.rocket().state::<ServerConfig>().unwrap();
    let mut req = ProxyRequest {
        headers: BTreeMap::new(),
        query: Some("limit=2".into()),
        path: "/xrpc/chat.bsky.convo.sendMessage".into(),
        method: Method::Post,
        id_resolver: State::from(client.rocket().state::<SharedIdResolver>().unwrap()),
        cfg: State::from(cfg),
        actor_store: State::from(client.rocket().state::<ActorStore>().unwrap()),
    };
    upstream.reply(
        "chat.bsky.convo.sendMessage",
        json!({"id":"message-1"}),
        None,
    );
    let response = pipethrough::pipethrough_procedure(&req, None, Some(json!({"text":"hello"})))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&response.buffer).unwrap(),
        json!({"id":"message-1"})
    );
    assert_eq!(
        upstream.last("chat.bsky.convo.sendMessage").body,
        "{\"text\":\"hello\"}"
    );
    pipethrough::pipethrough_procedure::<Value>(&req, None, None)
        .await
        .unwrap();
    assert!(upstream.last("chat.bsky.convo.sendMessage").body.is_empty());
    let response = pipethrough::pipethrough_procedure_post(&req, None, None)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&response.buffer).unwrap()["id"],
        "message-1"
    );
    assert!(upstream.last("chat.bsky.convo.sendMessage").body.is_empty());
    req.headers.insert(
        "atproto-proxy".into(),
        "did:plc:proxycoverage#bsky_appview".into(),
    );
    for _ in 0..2 {
        let resolved = pipethrough::format_url_and_aud(&req, None).await.unwrap();
        assert_eq!(resolved.aud, "did:plc:proxycoverage");
        assert_eq!(
            resolved.url.as_str(),
            format!("{}/xrpc/chat.bsky.convo.sendMessage?limit=2", upstream.url)
        );
    }
    for header in [
        "did:plc:missingservice",
        "did:plc:proxycoverage#service#extra",
        "did:plc:unknownservice#missing",
    ] {
        req.headers.insert("atproto-proxy".into(), header.into());
        assert!(pipethrough::format_url_and_aud(&req, None).await.is_err());
    }
    for (did, status, body) in [
        ("did:plc:missingproxy", 404, json!({"error":"NotFound"})),
        ("did:plc:invalidproxy", 200, json!({"id":17})),
    ] {
        upstream.replies.lock().unwrap().insert(
            format!("/{did}"),
            Reply {
                status,
                body: body.to_string(),
                headers: vec![],
            },
        );
        req.headers
            .insert("atproto-proxy".into(), format!("{did}#bsky_appview"));
        assert!(pipethrough::parse_proxy_header(&req).await.is_err());
        assert!(
            rsky_pds::apis::app::bsky::util::get_did_doc(req.id_resolver, &did.to_string())
                .await
                .is_err()
        );
    }
    req.headers.clear();
    req.headers
        .insert("accept-language".into(), "en\r\nX-Injected: yes".into());
    assert!(pipethrough::format_headers(
        &req,
        APP_DID.into(),
        "app.bsky.actor.getProfile".into(),
        None
    )
    .await
    .is_err());
    let error = pipethrough::pipethrough_procedure_post(&req, None, None)
        .await
        .unwrap_err();
    assert!(matches!(error, rsky_pds::apis::ApiError::InvalidRequest(_)));
    req.headers.clear();
    req.path.push('/');
    assert_eq!(
        pipethrough::parse_req_nsid(&req),
        "chat.bsky.convo.sendMessage"
    );
    req.path.pop();
    req.method = Method::Put;
    assert!(pipethrough::format_req_init(
        &req,
        upstream.url.parse().unwrap(),
        Default::default(),
        None
    )
    .is_err());
    req.method = Method::Head;
    assert_eq!(
        pipethrough::format_req_init_with_value(
            &req,
            upstream.url.parse().unwrap(),
            Default::default(),
            Some(json!({"ignored":true}))
        )
        .unwrap()
        .build()
        .unwrap()
        .method(),
        reqwest::Method::HEAD
    );
    req.method = Method::Post;
    let built = pipethrough::format_req_init_with_value(
        &req,
        upstream.url.parse().unwrap(),
        Default::default(),
        Some(json!({"text":"hello"})),
    )
    .unwrap()
    .build()
    .unwrap();
    assert_eq!(
        built.body().unwrap().as_bytes().unwrap(),
        b"{\"text\":\"hello\"}"
    );
    assert_eq!(built.headers()["content-type"], "application/json");
    upstream.raw_reply(
        "chat.bsky.convo.sendMessage",
        429,
        json!({"error":"RateLimitExceeded","message":"retry later"}).to_string(),
        None,
    );
    let error = pipethrough::pipethrough(
        &req,
        None,
        OverrideOpts {
            aud: None,
            lxm: None,
        },
    )
    .await
    .unwrap_err();
    match pipethrough::pipethrough_error(&error) {
        rsky_pds::apis::ApiError::UpstreamResponse(status, error, message) => {
            assert_eq!(status, 429);
            assert_eq!(error, "RateLimitExceeded");
            assert_eq!(message, "retry later");
        }
        other => panic!("wrong error: {other:?}"),
    }
    match pipethrough::pipethrough_procedure_post(&req, None, None)
        .await
        .unwrap_err()
    {
        rsky_pds::apis::ApiError::UpstreamResponse(status, error, message) => {
            assert_eq!(status, 429);
            assert_eq!(error, "RateLimitExceeded");
            assert_eq!(message, "retry later");
        }
        other => panic!("wrong procedure error: {other:?}"),
    }
    let mut absent_cfg = cfg.clone();
    absent_cfg.bsky_app_view = None;
    req.cfg = State::from(&absent_cfg);
    assert!(pipethrough::format_url_and_aud(&req, None).await.is_err());
    let error = pipethrough::pipethrough_procedure_post(&req, None, None)
        .await
        .unwrap_err();
    assert!(matches!(error, rsky_pds::apis::ApiError::InvalidRequest(_)));
}

async fn recent_writes_replace_stale_profiles(
    client: &Client,
    upstream: &Upstream,
    did: &str,
    token: &str,
) {
    put_record(client, token, did, "app.bsky.graph.follow", "baseline", json!({"$type":"app.bsky.graph.follow","subject":"did:plc:followedfixture","createdAt":"2026-01-01T00:00:00.000Z"})).await;
    let location = client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .get_location(did)
        .unwrap();
    let rev: String = rusqlite::Connection::open(location.db_location)
        .unwrap()
        .query_row(
            "SELECT \"repoRev\" FROM record WHERE rkey = 'baseline'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let post = put_record(
        client,
        token,
        did,
        "app.bsky.feed.post",
        "local",
        post_record("local-first post"),
    )
    .await;
    put_record(client, token, did, "app.bsky.actor.profile", "self", json!({"$type":"app.bsky.actor.profile","displayName":"Locally updated","description":"Local description"})).await;
    let local = viewer(client, did, None)
        .await
        .get_records_since_rev(rev.clone())
        .await
        .unwrap();
    assert_eq!(local.count, 2);
    assert_eq!(local.posts[0].record.text, "local-first post");
    assert_eq!(
        local.profile.unwrap().record.display_name.as_deref(),
        Some("Locally updated")
    );
    upstream.reply("app.bsky.actor.getProfile", profile(did), Some(&rev));
    let response = client
        .get(format!("/xrpc/app.bsky.actor.getProfile?actor={did}"))
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert!(response
        .headers()
        .get_one("Atproto-Upstream-Lag")
        .unwrap()
        .parse::<usize>()
        .is_ok());
    let body = response.into_json::<Value>().await.unwrap();
    assert_eq!(body["displayName"], "Locally updated");
    assert_eq!(body["description"], "Local description");
    assert_eq!(body["postsCount"], 4);
    assert!(body.get("body").is_none());
    upstream.reply(
        "app.bsky.actor.getProfiles",
        json!({"profiles":[profile(did),profile("did:plc:otheractor")]}),
        Some(&rev),
    );
    let (_, body) = get(
        client,
        &format!("/xrpc/app.bsky.actor.getProfiles?actors={did}"),
        Some(token),
    )
    .await;
    assert_eq!(body["profiles"][0]["displayName"], "Locally updated");
    assert_eq!(body["profiles"][1]["displayName"], "Upstream name");
    for nsid in [
        "app.bsky.feed.getTimeline",
        "app.bsky.feed.getAuthorFeed",
        "app.bsky.feed.getActorLikes",
    ] {
        let feed = if nsid == "app.bsky.feed.getAuthorFeed" {
            json!([{"post":{"uri":format!("at://{did}/app.bsky.feed.post/older"),"cid":CID,"author":{"did":did,"handle":"foo.rsky.com"},"record":post_record("older"),"indexedAt":"2025-01-01T00:00:00.000Z"}}])
        } else {
            json!([])
        };
        upstream.reply(nsid, json!({"feed":feed}), Some(&rev));
        let (status, body) = get(client, &format!("/xrpc/{nsid}?actor={did}"), Some(token)).await;
        assert_eq!(status, Status::Ok, "{body}");
        if nsid != "app.bsky.feed.getActorLikes" {
            assert_eq!(
                body["feed"][0]["post"]["record"]["text"],
                "local-first post"
            );
        }
    }
    upstream.raw_reply(
        "app.bsky.feed.getPostThread",
        404,
        json!({"error":"NotFound","message":"not indexed"}).to_string(),
        Some(&rev),
    );
    let (status, body) = get(
        client,
        &format!(
            "/xrpc/app.bsky.feed.getPostThread?uri={}",
            post["uri"].as_str().unwrap()
        ),
        Some(token),
    )
    .await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_eq!(body["thread"]["post"]["record"]["text"], "local-first post");
    upstream.reply(
        "app.bsky.actor.getProfile",
        profile(did),
        Some("zzzzzzzzzzzzz"),
    );
    assert_eq!(
        get(
            client,
            &format!("/xrpc/app.bsky.actor.getProfile?actor={did}"),
            Some(token)
        )
        .await
        .1["displayName"],
        "Upstream name"
    );
    upstream.raw_reply(
        "app.bsky.actor.getProfile",
        200,
        "upstream bytes without JSON".into(),
        Some(&rev),
    );
    assert_eq!(
        get(
            client,
            &format!("/xrpc/app.bsky.actor.getProfile?actor={did}"),
            Some(token)
        )
        .await,
        (Status::Ok, json!("upstream bytes without JSON"))
    );
    let parent_uri = format!("at://{did}/app.bsky.feed.post/remote-parent");
    let mut reply = post_record("new reply");
    reply["reply"] =
        json!({"root":{"uri":parent_uri,"cid":CID},"parent":{"uri":parent_uri,"cid":CID}});
    let reply_record = put_record(client, token, did, "app.bsky.feed.post", "reply", reply).await;
    let parent_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("depth", "0")
        .append_pair("parentHeight", "80")
        .append_pair("uri", &parent_uri)
        .finish();
    upstream.replies.lock().unwrap().insert(format!("/xrpc/app.bsky.feed.getPostThread?{parent_query}"), Reply {
        status:200, headers:vec![], body:json!({"thread":{"$type":"app.bsky.feed.defs#threadViewPost","post":{"uri":parent_uri,"cid":CID,"author":{"did":did,"handle":"foo.rsky.com"},"record":post_record("upstream parent"),"indexedAt":"2026-01-01T00:00:00.000Z"}}}).to_string(),
    });
    let (status, body) = get(
        client,
        &format!(
            "/xrpc/app.bsky.feed.getPostThread?uri={}",
            reply_record["uri"].as_str().unwrap()
        ),
        Some(token),
    )
    .await;
    assert_eq!(status, Status::Ok, "{body}");
    assert_eq!(body["thread"]["post"]["record"]["text"], "new reply");
    assert_eq!(
        body["thread"]["parent"]["post"]["record"]["text"],
        "upstream parent"
    );
}

async fn local_viewer_formats_embeds(client: &Client, upstream: &Upstream, did: &str, token: &str) {
    let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==").unwrap();
    let response = client
        .post("/xrpc/com.atproto.repo.uploadBlob")
        .header(ContentType::PNG)
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .body(png)
        .dispatch()
        .await;
    let status = response.status();
    let upload = response.into_json::<Value>().await.unwrap();
    assert_eq!(status, Status::Ok, "{upload}");
    put_record(client, token, did, "app.bsky.actor.profile", "self", json!({"$type":"app.bsky.actor.profile","displayName":"With avatar","avatar":upload["blob"]})).await;
    let viewer = viewer(client, did, Some(&upstream.url)).await;
    let basic = viewer.get_profile_basic().await.unwrap().unwrap();
    assert_eq!(basic.display_name.as_deref(), Some("With avatar"));
    assert_eq!(
        basic.avatar.as_deref(),
        Some(
            format!(
                "https://images.example.test/avatar/{did}/{}",
                upload["blob"]["ref"]["$link"].as_str().unwrap()
            )
            .as_str()
        )
    );
    assert!(viewer
        .service_auth_headers("did:plc:someoneelse", "app.bsky.feed.getPosts")
        .await
        .is_err());
    let record = |collection: &str| {
        serde_json::from_value(json!({"$type":"app.bsky.embed.record","record":{"uri":format!("at://{did}/{collection}/one"),"cid":CID}})).unwrap()
    };
    upstream.reply("app.bsky.feed.getPosts", json!({"posts":[]}), None);
    let missing = viewer
        .format_record_embed(record("app.bsky.feed.post"))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(missing).unwrap()["record"]["notFound"],
        true
    );
    let remote_post = json!({"uri":format!("at://{did}/app.bsky.feed.post/one"),"cid":CID,"author":{"did":did,"handle":"foo.rsky.com"},"record":post_record("remote quote"),"indexedAt":"2026-01-01T00:00:00.000Z"});
    upstream.reply(
        "app.bsky.feed.getPosts",
        json!({"posts":[remote_post]}),
        None,
    );
    let quote = serde_json::to_value(
        viewer
            .format_record_embed(record("app.bsky.feed.post"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(quote["record"]["value"]["text"], "remote quote");
    assert_eq!(
        service_claims(&upstream.last("app.bsky.feed.getPosts"))["lxm"],
        "app.bsky.feed.getPosts"
    );
    let generator = json!({"$type":"app.bsky.feed.defs#generatorView","uri":format!("at://{did}/app.bsky.feed.generator/one"),"cid":CID,"did":did,"creator":{"did":did,"handle":"foo.rsky.com","labels":[]},"displayName":"Art feed","indexedAt":"2026-01-01T00:00:00.000Z"});
    upstream.reply(
        "app.bsky.feed.getFeedGenerator",
        json!({"view":generator,"isOnline":true,"isValid":true}),
        None,
    );
    let generated = serde_json::to_value(
        viewer
            .format_record_embed(record("app.bsky.feed.generator"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(generated["record"]["displayName"], "Art feed");
    assert_eq!(
        service_claims(&upstream.last("app.bsky.feed.getFeedGenerator"))["lxm"],
        "app.bsky.feed.getFeedGenerator"
    );
    let list = json!({"$type":"app.bsky.graph.defs#listView","uri":format!("at://{did}/app.bsky.graph.list/one"),"cid":CID,"creator":{"did":did,"handle":"foo.rsky.com","labels":[]},"name":"Artists","purpose":"app.bsky.graph.defs#curatelist","indexedAt":"2026-01-01T00:00:00.000Z"});
    upstream.reply(
        "app.bsky.graph.getList",
        json!({"list":list,"items":[]}),
        None,
    );
    let listed = serde_json::to_value(
        viewer
            .format_record_embed(record("app.bsky.graph.list"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed["record"]["name"], "Artists");
    assert_eq!(
        service_claims(&upstream.last("app.bsky.graph.getList"))["lxm"],
        "app.bsky.graph.getList"
    );
    assert!(viewer
        .format_record_embed_internal(record("app.bsky.graph.follow"))
        .await
        .unwrap()
        .is_none());
    let media = json!({"$type":"app.bsky.embed.external","external":{"uri":"https://example.test/article","title":"Article","description":"Description"}});
    let mut embedded = post_record("external");
    embedded["embed"] = media.clone();
    let view = viewer
        .format_post_embed(serde_json::from_value(embedded).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(view).unwrap()["external"]["title"],
        "Article"
    );
    let combined = json!({"$type":"app.bsky.embed.recordWithMedia","record":{"$type":"app.bsky.embed.record","record":{"uri":format!("at://{did}/app.bsky.graph.follow/one"),"cid":CID}},"media":media});
    let mut embedded = post_record("quote with media");
    embedded["embed"] = combined;
    let view = serde_json::to_value(
        viewer
            .format_post_embed(serde_json::from_value(embedded).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(view["record"]["record"]["notFound"], true);
    assert_eq!(
        view["media"]["external"]["uri"],
        "https://example.test/article"
    );
    let blob = json!({"$type":"blob","ref":{"$link":CID},"mimeType":"image/jpeg","size":12});
    let mut image_post = post_record("image");
    image_post["embed"] = json!({"$type":"app.bsky.embed.images","images":[{"image":blob,"alt":"Hand drawn","aspectRatio":{"width":4,"height":3}}]});
    let images = serde_json::to_value(
        viewer
            .format_post_embed(serde_json::from_value(image_post).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(images["images"][0]["alt"], "Hand drawn");
    assert_eq!(
        images["images"][0]["thumb"],
        format!("https://images.example.test/feed_thumbnail/{did}/{CID}")
    );
    assert_eq!(
        images["images"][0]["fullsize"],
        format!("https://images.example.test/feed_fullsize/{did}/{CID}")
    );
    let combined = json!({"$type":"app.bsky.embed.recordWithMedia","record":{"$type":"app.bsky.embed.record","record":{"uri":format!("at://{did}/app.bsky.feed.post/one"),"cid":CID}},"media":{"$type":"app.bsky.embed.images","images":[{"image":blob,"alt":"Quoted drawing","aspectRatio":{"width":4,"height":3}}]}});
    let view = serde_json::to_value(
        viewer
            .format_record_with_media_embed(serde_json::from_value(combined).unwrap())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(view["record"]["record"]["value"]["text"], "remote quote");
    assert_eq!(view["media"]["images"][0]["alt"], "Quoted drawing");
    assert_eq!(
        view["media"]["images"][0]["fullsize"],
        format!("https://images.example.test/feed_fullsize/{did}/{CID}")
    );
    let updated = viewer.update_profile_detailed(
        serde_json::from_value(profile(did)).unwrap(),
        serde_json::from_value(
            json!({"$type":"app.bsky.actor.profile","avatar":blob,"banner":blob}),
        )
        .unwrap(),
        2,
    );
    assert_eq!(
        updated.avatar.as_deref(),
        Some(format!("https://images.example.test/avatar/{did}/{CID}").as_str())
    );
    assert_eq!(
        updated.banner.as_deref(),
        Some(format!("https://images.example.test/banner/{did}/{CID}").as_str())
    );
    assert_eq!(updated.posts_count, Some(5));
    let video = json!({"$type":"app.bsky.embed.video","video":{"$type":"blob","ref":{"$link":CID},"mimeType":"video/mp4","size":12}});
    let mut video_post = post_record("video awaiting appview hydration");
    video_post["embed"] = video.clone();
    assert!(viewer
        .format_post_embed(serde_json::from_value(video_post).unwrap())
        .await
        .unwrap()
        .is_none());
    let combined = json!({"$type":"app.bsky.embed.recordWithMedia","record":{"$type":"app.bsky.embed.record","record":{"uri":format!("at://{did}/app.bsky.feed.post/one"),"cid":CID}},"media":video});
    assert!(viewer
        .format_record_with_media_embed(serde_json::from_value(combined).unwrap())
        .await
        .unwrap()
        .is_none());
    let broken = LocalViewer::new(
        viewer.actor_store,
        viewer.account_manager,
        viewer.pds_hostname,
        viewer.appview_agent,
        None,
        viewer.appview_did,
        None,
    );
    assert!(broken
        .format_record_embed(record("app.bsky.feed.post"))
        .await
        .unwrap_err()
        .to_string()
        .contains("no appview url"));
    let offline = self::viewer(client, did, None).await;
    assert!(offline
        .service_auth_headers(did, "app.bsky.feed.getPosts")
        .await
        .is_err());
}

async fn local_storage_failures_leave_upstream_readable(
    client: &Client,
    upstream: &Upstream,
    did: &str,
    token: &str,
) {
    let location = client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .get_location(did)
        .unwrap();
    let db = rusqlite::Connection::open(location.db_location).unwrap();
    db.execute_batch("DROP TABLE account_pref").unwrap();
    assert_eq!(
        get(client, "/xrpc/app.bsky.actor.getPreferences", Some(token))
            .await
            .0,
        Status::InternalServerError
    );
    let response = client
        .post("/xrpc/app.bsky.actor.putPreferences")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {token}")))
        .body("{\"preferences\":[]}")
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::InternalServerError);
    db.execute_batch("DROP TABLE record").unwrap();
    upstream.reply("app.bsky.actor.getProfile", profile(did), Some("0"));
    assert_eq!(
        get(
            client,
            &format!("/xrpc/app.bsky.actor.getProfile?actor={did}"),
            Some(token)
        )
        .await,
        (Status::Ok, profile(did))
    );
}
