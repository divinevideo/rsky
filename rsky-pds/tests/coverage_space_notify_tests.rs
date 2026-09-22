//! Notification delivery against real local HTTP subscribers in supported development mode.
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rsky_pds::actor_store::space::Subscriber;
use rsky_pds::apis::com::atproto::space::deliver_notifications;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;

const WRITER: &str = "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa";
const AUTHORITY: &str = "did:plc:bbbbbbbbbbbbbbbbbbbbbbbb";
const SYNCER: &str = "did:plc:cccccccccccccccccccccccc#atproto_space_syncer";
const METHOD: &str = "com.atproto.space.notifyWrite";

fn development_transport() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| std::env::set_var("PDS_DEV_MODE", "true"));
}

type Received = (String, String, Value);

async fn subscriber(statuses: Vec<u16>) -> (String, tokio::task::JoinHandle<Vec<Received>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let receiver = tokio::spawn(async move {
        let mut received = Vec::new();
        for status in statuses {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "subscriber received an incomplete request");
                bytes.extend_from_slice(&chunk[..count]);
                assert!(bytes.len() <= 65536);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let content_length: usize = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .unwrap()
                .1
                .trim()
                .parse()
                .unwrap();
            assert!(content_length <= 65536);
            while bytes.len() < header_end + content_length {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let body: Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
            let authorization = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .unwrap()
                .1
                .trim()
                .to_owned();
            received.push((
                headers.lines().next().unwrap().to_owned(),
                authorization,
                body,
            ));
            let response = format!(
                "HTTP/1.1 {status} fixture\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        received
    });
    (endpoint, receiver)
}

#[tokio::test]
async fn signed_notifications_keep_subscriber_audiences_and_continue_after_rejection() {
    development_transport();
    let (endpoint, receiver) = subscriber(vec![503, 200]).await;
    let key = secp256k1::Keypair::from_secret_key(
        &secp256k1::Secp256k1::new(),
        &secp256k1::SecretKey::from_slice(&[53; 32]).unwrap(),
    );
    let body = json!({"space":format!("at://{AUTHORITY}/space/com.example.room/main"),"repo":WRITER,"rev":"3knotification"});
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    deliver_notifications(
        &key,
        WRITER,
        AUTHORITY,
        METHOD,
        &[
            Subscriber {
                endpoint: format!("{endpoint}/rejected"),
                service: Some(SYNCER.into()),
            },
            Subscriber {
                endpoint: format!("{endpoint}/accepted/"),
                service: None,
            },
        ],
        &body,
    )
    .await;
    let received = tokio::time::timeout(Duration::from_secs(30), receiver)
        .await
        .expect("notification receiver timed out")
        .unwrap();
    assert_eq!(received.len(), 2);
    let public_key = rsky_crypto::utils::encode_did_key(&key.public_key());
    let mut ids = Vec::new();
    for ((request, authorization, actual_body), (route, audience)) in received
        .iter()
        .zip([("rejected", SYNCER), ("accepted", AUTHORITY)])
    {
        assert_eq!(request, &format!("POST /{route}/xrpc/{METHOD} HTTP/1.1"));
        assert_eq!(actual_body, &body);
        let token = authorization.strip_prefix("Bearer ").unwrap();
        let parts: Vec<_> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["alg"], "ES256K");
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], WRITER);
        assert_eq!(claims["aud"], audience);
        assert_eq!(claims["lxm"], METHOD);
        let issued = claims["iat"].as_u64().unwrap();
        assert!(issued >= before);
        assert_eq!(claims["exp"].as_u64().unwrap() - issued, 60);
        let id = claims["jti"].as_str().unwrap().to_owned();
        assert!(!id.is_empty());
        ids.push(id);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert!(rsky_crypto::verify::verify_signature_digest(
            &public_key,
            &Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes()),
            &signature,
            None
        )
        .unwrap());
    }
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn inbound_host_notification_delivers_to_the_registered_syncer() {
    use rocket::http::{ContentType, Header, Status};
    use rsky_pds::account_manager::AccountManager;
    use rsky_pds::actor_store::{blobstore::BlobstoreFactory, ActorStore};
    const HOST: &str = "did:plc:khvyd3oiw46vif5gm7hijslk";
    development_transport();
    let (_dir, client) = common::oauth::get_oauth_client().await;
    common::oauth::create_active_account(&client).await;
    let accounts = client.rocket().state::<AccountManager>().unwrap();
    let (access, _) = accounts
        .create_session(HOST.into(), None, false)
        .await
        .unwrap();
    let domain = &client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0];
    let writer = client.post("/xrpc/com.atproto.server.createAccount")
        .header(ContentType::JSON).header(Header::new("Authorization", common::get_admin_token()))
        .body(json!({"did": WRITER,"handle":format!("writer{domain}"),"email":"writer@example.com","password":"password"}).to_string())
        .dispatch().await;
    assert_eq!(writer.status(), Status::Ok);
    accounts.activate_account(WRITER).await.unwrap();
    let created = client
        .post("/xrpc/com.atproto.simplespace.createSpace")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {access}")))
        .body(json!({"type":"com.example.room","skey":"notifications"}).to_string())
        .dispatch()
        .await;
    assert_eq!(created.status(), Status::Ok);
    let created: Value = created.into_json().await.unwrap();
    let space = created["uri"].as_str().unwrap();
    let member = client
        .post("/xrpc/com.atproto.simplespace.addMember")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {access}")))
        .body(json!({"space":space,"did":WRITER}).to_string())
        .dispatch()
        .await;
    assert_eq!(member.status(), Status::Ok);

    let (endpoint, receiver) = subscriber(vec![200]).await;
    let actors = client.rocket().state::<ActorStore>().unwrap();
    let factory = client.rocket().state::<BlobstoreFactory>().unwrap();
    let reader = actors
        .read(HOST.into(), factory.blobstore(HOST.into()))
        .await
        .unwrap();
    reader
        .space
        .register_host_notify(
            space,
            &Subscriber {
                endpoint: format!("{endpoint}/forwarded"),
                service: Some(SYNCER.into()),
            },
            &rsky_pds::apis::com::atproto::space::format_expiry(
                &rsky_pds::apis::com::atproto::space::notify_expiry(),
            ),
        )
        .await
        .unwrap();
    let writer_key = actors.keypair(WRITER).await.unwrap();
    let incoming =
        rsky_pds::space_auth::mint_space_service_token(&writer_key, WRITER, HOST, METHOD);
    let body = json!({"space":space,"repo":WRITER,"rev":"3kroute-notification"});
    let response = client
        .post(format!("/xrpc/{METHOD}"))
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {incoming}")))
        .body(body.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    actors.background_queue.process_all().await;
    let received = tokio::time::timeout(Duration::from_secs(30), receiver)
        .await
        .expect("route failed to deliver notification")
        .unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].0,
        format!("POST /forwarded/xrpc/{METHOD} HTTP/1.1")
    );
    assert_eq!(received[0].2, body);
    let parts: Vec<_> = received[0]
        .1
        .strip_prefix("Bearer ")
        .unwrap()
        .split('.')
        .collect();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], HOST);
    assert_eq!(claims["aud"], SYNCER);
    assert_eq!(claims["lxm"], METHOD);
    let host_key = actors.keypair(HOST).await.unwrap();
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    assert!(rsky_crypto::verify::verify_signature_digest(
        &rsky_crypto::utils::encode_did_key(&host_key.public_key()),
        &Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes()),
        &signature,
        None
    )
    .unwrap());
    let writers = reader.space.list_writers(space, 10, None).await.unwrap();
    assert!(writers
        .iter()
        .any(|writer| writer.did == WRITER && writer.rev == "3kroute-notification"));

    // Actor writes have their own fan-out entry point; exercise it with two
    // distinct subscribers, then verify deletion reaches each exactly once.
    reader
        .space
        .unregister_host_notify(space, &format!("{endpoint}/forwarded"))
        .await
        .unwrap();
    let (endpoint, receiver) = subscriber(vec![200; 4]).await;
    for route in ["two", "one"] {
        reader
            .space
            .register_host_notify(
                space,
                &Subscriber {
                    endpoint: format!("{endpoint}/{route}"),
                    service: Some(SYNCER.into()),
                },
                &rsky_pds::apis::com::atproto::space::format_expiry(
                    &rsky_pds::apis::com::atproto::space::notify_expiry(),
                ),
            )
            .await
            .unwrap();
    }
    let (writer_access, _) = accounts
        .create_session(WRITER.into(), None, false)
        .await
        .unwrap();
    let written = client
        .post("/xrpc/com.atproto.space.createRecord")
        .header(ContentType::JSON)
        .header(Header::new(
            "Authorization",
            format!("Bearer {writer_access}"),
        ))
        .body(
            json!({"space":space,"repo":WRITER,"collection":"com.example.post",
            "rkey":"3kactor-write","record":{"text":"actor write"}})
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(written.status(), Status::Ok);
    let written: Value = written.into_json().await.unwrap();
    let revision = written["commit"]["rev"].as_str().unwrap();
    actors.background_queue.process_all().await;
    let deleted = client
        .post("/xrpc/com.atproto.simplespace.deleteSpace")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", format!("Bearer {access}")))
        .body(json!({"space":space}).to_string())
        .dispatch()
        .await;
    assert_eq!(deleted.status(), Status::Ok);
    actors.background_queue.process_all().await;
    let received = tokio::time::timeout(Duration::from_secs(30), receiver)
        .await
        .expect("actor write and deletion notifications were not delivered")
        .unwrap();
    assert_eq!(received.len(), 4);
    let deleted_method = rsky_pds::space_auth::NOTIFY_SPACE_DELETED_LXM;
    let mut requests = std::collections::HashSet::new();
    for (request, authorization, body) in received {
        assert!(
            requests.insert(request.clone()),
            "duplicate notification: {request}"
        );
        let method = if request.contains(METHOD) {
            METHOD
        } else {
            deleted_method
        };
        if method == METHOD {
            assert_eq!(body, json!({"space":space,"repo":WRITER,"rev":revision}));
        } else {
            assert_eq!(body, json!({"space":space}));
        }
        let parts: Vec<_> = authorization
            .strip_prefix("Bearer ")
            .unwrap()
            .split('.')
            .collect();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], HOST);
        assert_eq!(claims["aud"], SYNCER);
        assert_eq!(claims["lxm"], method);
        assert!(rsky_crypto::verify::verify_signature_digest(
            &rsky_crypto::utils::encode_did_key(&host_key.public_key()),
            &Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes()),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
            None,
        )
        .unwrap());
    }
    for route in ["one", "two"] {
        for method in [METHOD, deleted_method] {
            assert!(requests.contains(&format!("POST /{route}/xrpc/{method} HTTP/1.1")));
        }
    }
}
