mod common;

use rocket::http::{ContentType, Header, Status};
use rsky_pds::account_manager::AccountManager;
use rsky_pds::config::ServerConfig;
use rsky_pds::crawlers::Crawlers;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

#[tokio::test]
async fn crawler_notifications_advertise_the_host_and_throttle_even_on_failure() {
    for status in [200, 503] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut buf = [0; 1024];
                let count = stream.read(&mut buf).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buf[..count]);
                if let Some(at) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            assert!(headers.starts_with("POST /xrpc/com.atproto.sync.requestCrawl HTTP/1.1\r\n"));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + length {
                let mut buf = [0; 1024];
                let count = stream.read(&mut buf).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buf[..count]);
            }
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes[header_end..]).unwrap(),
                json!({"hostname":"pds.example.test"})
            );
            write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            listener
        });
        let mut crawlers = Crawlers::new(
            "pds.example.test".into(),
            vec![String::new(), format!("http://{address}")],
        );
        crawlers.notify_of_update().await.unwrap();
        let listener = server.join().unwrap();
        assert_ne!(crawlers.last_notified, 0);
        let last = crawlers.last_notified;
        crawlers.notify_of_update().await.unwrap();
        assert_eq!(crawlers.last_notified, last);
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut crawlers = Crawlers::new("pds.example.test".into(), vec![format!("http://{address}")]);
    crawlers.notify_of_update().await.unwrap();
    assert_ne!(crawlers.last_notified, 0);
}

#[tokio::test]
async fn well_known_documents_identify_only_hosted_accounts_and_the_configured_service() {
    let (_dir, client) = common::get_client().await;
    common::create_account(&client).await;
    let did = "did:plc:khvyd3oiw46vif5gm7hijslk";
    client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .activate_account(did)
        .await
        .unwrap();
    let config = client.rocket().state::<ServerConfig>().unwrap();
    for (host, expected) in [
        ("external.example.test".to_owned(), Status::NotFound),
        (
            format!("absent{}", config.identity.service_handle_domains[0]),
            Status::NotFound,
        ),
        (
            format!("foo{}", config.identity.service_handle_domains[0]),
            Status::Ok,
        ),
    ] {
        let response = client
            .get("/.well-known/atproto-did")
            .header(Header::new("Host", host))
            .dispatch()
            .await;
        assert_eq!(response.status(), expected);
        if expected == Status::Ok {
            assert_eq!(response.into_string().await.unwrap(), did);
        }
    }
    let response = client.get("/.well-known/did.json").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let document: Value = response.into_json().await.unwrap();
    assert_eq!(
        document["id"],
        format!("did:web:{}", config.service.hostname)
    );
    assert_eq!(
        document["service"][0]["serviceEndpoint"],
        config.service.public_url
    );
    assert_eq!(
        document["verificationMethod"][0]["controller"],
        document["id"]
    );
    let key = document["verificationMethod"][0]["publicKeyMultibase"]
        .as_str()
        .unwrap();
    let (_, bytes) = multibase::decode(key).unwrap();
    assert_eq!(&bytes[..2], &[0xe7, 0x01]);
    assert_eq!(bytes.len(), 35);
}

#[tokio::test]
async fn deprecated_sync_endpoints_allow_owner_access_to_deactivated_repos() {
    let (_dir, client) = common::get_client().await;
    let (identifier, password) = common::create_account(&client).await;
    let did = "did:plc:khvyd3oiw46vif5gm7hijslk";
    let session: Value = client
        .post("/xrpc/com.atproto.server.createSession")
        .header(ContentType::JSON)
        .body(json!({"identifier":identifier,"password":password}).to_string())
        .dispatch()
        .await
        .into_json()
        .await
        .unwrap();
    let token = session["accessJwt"].as_str().unwrap();
    for method in ["getHead", "getCheckout"] {
        let response = client
            .get(format!("/xrpc/com.atproto.sync.{method}?did={did}"))
            .header(Header::new("Authorization", format!("Bearer {token}")))
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
        assert!(!response.into_bytes().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn validated_record_creation_replaces_conflicting_backlinks() {
    let (_dir, client) = common::get_client().await;
    common::oauth::create_active_account(&client).await;
    let session: Value = client
        .post("/xrpc/com.atproto.server.createSession")
        .header(ContentType::JSON)
        .body(json!({"identifier":"foo@example.com","password":"password"}).to_string())
        .dispatch()
        .await
        .into_json()
        .await
        .unwrap();
    let token = session["accessJwt"].as_str().unwrap();
    let did = "did:plc:khvyd3oiw46vif5gm7hijslk";
    let subject = "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa";
    for key in ["first", "second"] {
        let response = client.post("/xrpc/com.atproto.repo.createRecord").header(ContentType::JSON)
            .header(Header::new("Authorization", format!("Bearer {token}")))
            .body(json!({"repo":did,"collection":"app.bsky.graph.follow","rkey":key,"validate":true,
                "record":{"$type":"app.bsky.graph.follow","subject":subject,"createdAt":"2026-01-01T00:00:00Z"}}).to_string()).dispatch().await;
        assert_eq!(response.status(), Status::Ok);
    }
    let records: Value = client
        .get(format!(
            "/xrpc/com.atproto.repo.listRecords?repo={did}&collection=app.bsky.graph.follow"
        ))
        .dispatch()
        .await
        .into_json()
        .await
        .unwrap();
    assert_eq!(records["records"].as_array().unwrap().len(), 1);
    assert_eq!(
        records["records"][0]["uri"],
        format!("at://{did}/app.bsky.graph.follow/second")
    );
}

#[test]
fn record_preparation_rejects_legacy_blobs_and_preserves_custom_blob_freedom() {
    use rsky_pds::repo::prepare::{blobs_for_write, set_collection_name};
    use rsky_repo::storage::Ipld;
    use rsky_repo::types::{Lex, RepoRecord};
    let cid = "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku";
    let mut legacy: RepoRecord = serde_json::from_value(json!({
        "$type":"com.example.media", "file":{"cid":cid,"mimeType":"image/png"}
    }))
    .unwrap();
    // Explicit typed lex values preserve the distinction from raw JSON.
    legacy.insert(
        "file".into(),
        Lex::Blob(serde_json::from_value(json!({"cid":cid,"mimeType":"image/png"})).unwrap()),
    );
    assert!(blobs_for_write(legacy.clone(), true)
        .unwrap_err()
        .to_string()
        .contains("Legacy blob ref"));
    assert_eq!(blobs_for_write(legacy, false).unwrap().len(), 1);
    let mut custom: RepoRecord = serde_json::from_value(json!({"file":{
        "$type":"blob", "ref":{"$link":cid},"mimeType":"image/png","size":123
    }}))
    .unwrap();
    custom.insert(
        "$type".into(),
        Lex::Ipld(Ipld::String("com.example.media".into())),
    );
    let prepared = blobs_for_write(custom, true).unwrap();
    assert_eq!(prepared.len(), 1);
    assert!(prepared[0].constraints.max_size.is_none());
    assert!(prepared[0].constraints.accept.is_none());
    let record = RepoRecord::from([(
        "$type".into(),
        Lex::Ipld(Ipld::Json(json!("com.example.wrong"))),
    )]);
    assert!(
        set_collection_name(&"com.example.record".into(), record.clone(), true)
            .unwrap_err()
            .to_string()
            .contains("Invalid $type")
    );
    assert!(set_collection_name(&"com.example.record".into(), record, false).is_ok());
}

#[tokio::test]
async fn storage_errors_are_rendered_with_stable_public_protocol_codes() {
    use rocket::response::Responder;
    use rsky_pds::actor_store::blob::BlobMismatch;
    use rsky_pds::admission::NotAdmitted;
    use rsky_pds::apis::ApiError;
    let client = rocket::local::asynchronous::Client::untracked(rocket::build())
        .await
        .unwrap();
    let request = client.get("/");
    let cases = [
        (
            ApiError::RecordNotFoundUri("at://did:example:actor/com.example.post/missing".into()),
            400,
            "RecordNotFound",
        ),
        (
            ApiError::from(anyhow::Error::new(rsky_pds::lifecycle::AccountDeleting(
                "did:example:deleting".into(),
            ))),
            400,
            "InvalidRequest",
        ),
        (
            ApiError::from(&rsky_pds::auth_verifier::AuthError::Forbidden(
                "account is not permitted".into(),
            )),
            403,
            "Forbidden",
        ),
        (
            ApiError::Overloaded("writer queue full".into()),
            503,
            "ServiceUnavailable",
        ),
        (
            ApiError::InsufficientScope("repo write required".into()),
            403,
            "InsufficientScope",
        ),
        (
            ApiError::from(anyhow::Error::new(BlobMismatch::MimeType {
                expected: "image/png".into(),
                got: "image/jpeg".into(),
            })),
            400,
            "InvalidRequest",
        ),
        (
            ApiError::from(anyhow::Error::new(NotAdmitted {
                did: "did:plc:actor".into(),
                state: "maintenance".into(),
            })),
            503,
            "NotAdmitted",
        ),
        (
            rsky_pds::apis::com::atproto::space::space_error(anyhow::anyhow!("sqlite disk error")),
            500,
            "InternalServerError",
        ),
    ];
    for (error, code, name) in cases {
        assert!(!error.to_string().is_empty());
        let mut response = error.respond_to(request.inner()).unwrap();
        assert_eq!(response.status().code, code);
        if name == "ServiceUnavailable" {
            assert_eq!(response.headers().get_one("Retry-After"), Some("5"));
        }
        let body: Value =
            serde_json::from_str(&response.body_mut().to_string().await.unwrap()).unwrap();
        assert_eq!(body["error"], name);
    }
}

#[tokio::test]
async fn unpublished_actor_store_has_no_account_root_to_reconcile() {
    let (_dir, client) = common::get_client().await;
    let temp = tempfile::tempdir().unwrap();
    let db = rsky_pds::actor_store::db::get_migrated_db(temp.path().join("empty.sqlite"))
        .await
        .unwrap();
    let reconciled = rsky_pds::publication::sync_account_root(
        &db,
        client.rocket().state::<AccountManager>().unwrap(),
        "did:plc:empty",
    )
    .await
    .unwrap();
    assert!(!reconciled);
    let seq = rsky_pds::sequencer::db::get_db(temp.path().join("seq.sqlite")).unwrap();
    assert_eq!(
        seq.run(|conn| Ok(conn.query_row("SELECT 1", [], |row| row.get::<_, i32>(0))?))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn sync_routes_report_missing_roots_and_missing_actor_databases() {
    let (_dir, client) = common::get_client().await;
    common::create_account(&client).await;
    let did = "did:plc:khvyd3oiw46vif5gm7hijslk";
    client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .activate_account(did)
        .await
        .unwrap();
    let location = client
        .rocket()
        .state::<rsky_pds::actor_store::ActorStore>()
        .unwrap()
        .get_location(did)
        .unwrap();
    let db = rusqlite::Connection::open(&location.db_location).unwrap();
    db.execute("DELETE FROM repo_root", []).unwrap();
    let response = client
        .get(format!("/xrpc/com.atproto.sync.getHead?did={did}"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
    assert_eq!(
        response.into_json::<Value>().await.unwrap()["error"],
        "HeadNotFound"
    );
    let response = client
        .get(format!("/xrpc/com.atproto.sync.getCheckout?did={did}"))
        .dispatch()
        .await;
    assert_ne!(response.status(), Status::Ok);
    drop(db);
    // The account can outlive a lost actor directory; stale connections must
    // not make the sync API pretend that the repository is available.
    client
        .rocket()
        .state::<rsky_pds::actor_store::ActorStore>()
        .unwrap()
        .unlink(did)
        .await
        .unwrap();
    let response = client
        .get(format!("/xrpc/com.atproto.sync.getHead?did={did}"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::InternalServerError);
}

async fn plc_response(body: Value) -> (rsky_pds::plc::Client, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = rsky_pds::plc::Client::new(format!("http://{}", listener.local_addr().unwrap()));
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        stream.read(&mut bytes).await.unwrap();
        let body = body.to_string();
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    (client, task)
}

#[tokio::test]
async fn plc_updates_require_an_active_operation_and_report_invalid_documents() {
    let did = "did:plc:operationfixture".to_owned();
    let (client, task) =
        plc_response(json!({"type":"plc_tombstone","prev":"previous","sig":"c2ln"})).await;
    assert!(client
        .ensure_last_op(&did)
        .await
        .unwrap_err()
        .to_string()
        .contains("Cannot apply op to tombstone"));
    task.await.unwrap();
    let (client, task) = plc_response(json!({"type":"create","signingKey":"did:key:signing","recoveryKey":"did:key:recovery","handle":"alice.example.test","service":"https://pds.example.test","prev":null})).await;
    let operation = client.ensure_last_op(&did).await.unwrap();
    assert_eq!(
        serde_json::to_value(operation).unwrap()["handle"],
        "alice.example.test"
    );
    task.await.unwrap();
    let (client, task) = plc_response(json!({"invalid":"document"})).await;
    assert!(client.get_document_data(&did).await.is_err());
    task.await.unwrap();
}

#[tokio::test]
async fn sequencer_retries_an_unreadable_event_without_advancing_its_cursor() {
    use rsky_pds::sequencer::Sequencer;
    let dir = tempfile::tempdir().unwrap();
    let db = rsky_pds::sequencer::db::get_migrated_db(dir.path().join("seq.sqlite"))
        .await
        .unwrap();
    let mut sequencer = Sequencer::new(
        db.clone(),
        Crawlers::new("pds.example.test".into(), vec![]),
        None,
    );
    let first = sequencer
        .sequence_identity_evt("did:plc:sequencer".into(), None)
        .await
        .unwrap();
    let mut receiver = sequencer.subscribe();
    let mut worker = sequencer.clone();
    let task = tokio::spawn(async move { worker.start().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while sequencer.last_seen() != first {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let valid = rsky_pds::sequencer::events::format_seq_identity_evt(
        "did:plc:sequencer".into(),
        Some("alice.example.test".into()),
    )
    .await
    .unwrap();
    let mut broken = valid.clone();
    broken.event = vec![0xff];
    let next = sequencer.sequence_evt(broken).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), receiver.recv())
            .await
            .is_err()
    );
    assert_eq!(sequencer.last_seen(), first);
    db.run(move |conn| {
        conn.execute(
            "UPDATE repo_seq SET event = ?1 WHERE seq = ?2",
            rusqlite::params![valid.event, next],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq(), next);
    assert_eq!(sequencer.last_seen(), next);
    sequencer.destroy().await;
    task.await.unwrap().unwrap();
}
