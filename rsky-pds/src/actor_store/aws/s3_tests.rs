use super::*;
use aws_sdk_s3::config::BehaviorVersion;
use rsky_common::ipld::sha256_to_cid;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct FakeS3 {
    objects: BTreeMap<String, Vec<u8>>,
    requests: Vec<String>,
    fail_next: bool,
}

fn decode_aws_chunks(body: &[u8]) -> Vec<u8> {
    let mut position = 0;
    let mut decoded = Vec::new();
    loop {
        let line_end = body[position..]
            .windows(2)
            .position(|bytes| bytes == b"\r\n")
            .expect("chunk size line");
        let line = std::str::from_utf8(&body[position..position + line_end]).unwrap();
        let size = usize::from_str_radix(line.split(';').next().unwrap(), 16).unwrap();
        position += line_end + 2;
        if size == 0 {
            break;
        }
        decoded.extend_from_slice(&body[position..position + size]);
        position += size;
        assert_eq!(&body[position..position + 2], b"\r\n");
        position += 2;
    }
    decoded
}

async fn fake_s3() -> (String, Arc<Mutex<FakeS3>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(FakeS3::default()));
    let shared = state.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 8192];
                    let size = stream.read(&mut chunk).await.unwrap();
                    if size == 0 {
                        return;
                    }
                    data.extend_from_slice(&chunk[..size]);
                    if let Some(end) = data.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&data[..header_end]).to_string();
                let request = headers.lines().next().unwrap();
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap().to_owned();
                let target = parts.next().unwrap().to_owned();
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while data.len() - header_end < content_len {
                    let mut chunk = [0u8; 8192];
                    let size = stream.read(&mut chunk).await.unwrap();
                    if size == 0 {
                        return;
                    }
                    data.extend_from_slice(&chunk[..size]);
                }
                let mut body = data[header_end..header_end + content_len].to_vec();
                if headers.lines().any(|line| {
                    line.split_once(':').is_some_and(|(name, value)| {
                        name.eq_ignore_ascii_case("content-encoding")
                            && value
                                .trim()
                                .split(',')
                                .any(|part| part.trim() == "aws-chunked")
                    })
                }) {
                    body = decode_aws_chunks(&body);
                }
                let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
                let key = urlencoding::decode(
                    url.path()
                        .trim_start_matches('/')
                        .trim_start_matches("bucket/"),
                )
                .unwrap()
                .into_owned();
                let query = url.query().unwrap_or("");
                let copy_source = headers.lines().find_map(|line| {
                    line.to_ascii_lowercase()
                        .starts_with("x-amz-copy-source:")
                        .then(|| line.split_once(':').unwrap().1.trim().to_owned())
                });
                let (status, response) = {
                    let mut state = shared.lock().unwrap();
                    state.requests.push(format!("{method} {target}"));
                    if state.fail_next {
                        state.fail_next = false;
                        (
                            "403 Forbidden",
                            b"<Error><Code>AccessDenied</Code></Error>".to_vec(),
                        )
                    } else if method == "GET" && query.contains("list-type=2") {
                        let prefix = url
                            .query_pairs()
                            .find(|(name, _)| name == "prefix")
                            .map(|(_, value)| value.into_owned())
                            .unwrap_or_default();
                        let offset = url
                            .query_pairs()
                            .find(|(name, _)| name == "continuation-token")
                            .and_then(|(_, value)| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        let keys: Vec<_> = state
                            .objects
                            .keys()
                            .filter(|key| key.starts_with(&prefix))
                            .collect();
                        let item = keys
                            .get(offset)
                            .map(|key| format!("<Contents><Key>{key}</Key></Contents>"));
                        let more = offset + 1 < keys.len();
                        let continuation = if more {
                            format!(
                                "<NextContinuationToken>{}</NextContinuationToken>",
                                offset + 1
                            )
                        } else {
                            String::new()
                        };
                        (
                            "200 OK",
                            format!(
                                "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsTruncated>{more}</IsTruncated>{continuation}{}</ListBucketResult>",
                                item.unwrap_or_default()
                            )
                            .into_bytes(),
                        )
                    } else if method == "PUT" {
                        if let Some(source) = copy_source {
                            let source = urlencoding::decode(
                                source.trim_start_matches('/').trim_start_matches("bucket/"),
                            )
                            .unwrap()
                            .into_owned();
                            match state.objects.get(&source).cloned() {
                                Some(bytes) => {
                                    state.objects.insert(key, bytes);
                                    ("200 OK", b"<CopyObjectResult><LastModified>2026-01-01T00:00:00.000Z</LastModified><ETag>\"test\"</ETag></CopyObjectResult>".to_vec())
                                }
                                None => (
                                    "404 Not Found",
                                    b"<Error><Code>NoSuchKey</Code></Error>".to_vec(),
                                ),
                            }
                        } else {
                            state.objects.insert(key, body);
                            ("200 OK", Vec::new())
                        }
                    } else if method == "HEAD" {
                        if state.objects.contains_key(&key) {
                            ("200 OK", Vec::new())
                        } else {
                            ("404 Not Found", Vec::new())
                        }
                    } else if method == "GET" {
                        match state.objects.get(&key) {
                            Some(bytes) => ("200 OK", bytes.clone()),
                            None => (
                                "404 Not Found",
                                b"<Error><Code>NoSuchKey</Code></Error>".to_vec(),
                            ),
                        }
                    } else if method == "DELETE" {
                        state.objects.remove(&key);
                        ("204 No Content", Vec::new())
                    } else if method == "POST" && query.contains("delete") {
                        let xml = String::from_utf8_lossy(&body);
                        for value in xml.split("<Key>").skip(1) {
                            if let Some((key, _)) = value.split_once("</Key>") {
                                state.objects.remove(key);
                            }
                        }
                        (
                            "200 OK",
                            b"<DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"/>"
                                .to_vec(),
                        )
                    } else {
                        ("400 Bad Request", Vec::new())
                    }
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                if method != "HEAD" {
                    stream.write_all(&response).await.unwrap();
                }
            });
        }
    });
    (endpoint, state, server)
}

fn local_sdk(endpoint: &str) -> SdkConfig {
    SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(endpoint)
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(
            aws_sdk_s3::config::Credentials::new("key", "secret", None, None, "test"),
        ))
        .build()
}

fn sdk_config(endpoint: Option<&str>) -> SdkConfig {
    let builder = SdkConfig::builder().behavior_version(BehaviorVersion::latest());
    match endpoint {
        Some(endpoint) => builder.endpoint_url(endpoint).build(),
        None => builder.build(),
    }
}

#[test]
fn did_prefixed_key_layout() {
    let cfg = sdk_config(None);
    let store = S3BlobStore::new(
        "did:example:alice".to_owned(),
        &cfg,
        Some("shared-bucket".to_owned()),
        true,
    );
    assert!(store.path_style());
    assert_eq!(store.bucket, "shared-bucket");
    assert_eq!(store.get_tmp_path("key"), "tmp/did:example:alice/key");
    let cid = sha256_to_cid(Sha256::digest(b"layout").to_vec());
    assert_eq!(
        store.get_stored_path(cid),
        format!("blocks/did:example:alice/{cid}")
    );
    assert_eq!(
        store.get_quarantined_path(cid),
        format!("quarantine/did:example:alice/{cid}")
    );
    let key = store.gen_key();
    assert_eq!(key.len(), 32);
}

#[test]
fn legacy_fallback_uses_did_as_bucket() {
    let cfg = sdk_config(Some("https://nyc3.digitaloceanspaces.com"));
    let store = S3BlobStore::new("did:example:alice".to_owned(), &cfg, None, false);
    assert_eq!(store.bucket, "did:example:alice");
    assert!(!store.path_style());
    assert!(store.retries_disabled());
    assert!(format!("{store:?}").contains("did:example:alice"));
}

/// A request against an endpoint that does not answer is journaled as
/// one failed attempt, never retried behind the journal's back.
#[tokio::test]
async fn every_request_is_one_journaled_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let journal = AttemptJournal::open(dir.path().join("attempts.sqlite"), true)
        .await
        .unwrap();
    let cfg = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url("http://127.0.0.1:1")
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(
            aws_sdk_s3::config::Credentials::new("k", "s", None, None, "test"),
        ))
        .build();
    let store = S3BlobStore::new(
        "did:example:alice".to_owned(),
        &cfg,
        Some("bucket".to_owned()),
        false,
    )
    .with_attempts(journal.clone());
    let cid = sha256_to_cid(Sha256::digest(b"unreachable").to_vec());
    assert!(store
        .put_permanent(cid, b"unreachable".to_vec())
        .await
        .is_err());
    assert!(store.put_temp(b"unreachable".to_vec()).await.is_err());
    assert!(store.delete(cid).await.is_err());
    assert!(store.delete_many(vec![cid]).await.is_err());
    assert!(store.restore_copy_only(cid).await.is_err());
    let stored = store.get_stored_path(cid);
    let attempts = journal
        .attempts("did:example:alice", &stored)
        .await
        .unwrap();
    assert_eq!(
        attempts
            .iter()
            .map(|attempt| attempt.operation.as_str())
            .collect::<Vec<_>>(),
        ["put", "delete", "delete", "copy"]
    );
    // an endpoint that never answers leaves every attempt unconfirmed:
    // the request may still take effect
    assert!(attempts.iter().all(|attempt| attempt
        .outcome
        .as_deref()
        .unwrap_or("")
        .starts_with("ambiguous")));
    let unresolved = journal.unresolved("did:example:alice").await.unwrap();
    assert_eq!(unresolved.len(), 5, "{unresolved:?}");
    assert!(unresolved.iter().all(|attempt| attempt.is_unresolved()));
    assert_eq!(
        unresolved
            .iter()
            .filter(|attempt| attempt.is_write())
            .count(),
        3
    );
    // the physical-key operations the collector uses
    assert_eq!(
        store.namespace_prefixes(),
        [
            "blocks/did:example:alice/",
            "tmp/did:example:alice/",
            "quarantine/did:example:alice/"
        ]
    );
    assert_eq!(
        store.object_key(ObjectKind::Permanent, "bafy", 1),
        "blocks/did:example:alice/bafy.g1"
    );
    assert_eq!(
        store.object_key(ObjectKind::Temp, "t", 0),
        "tmp/did:example:alice/t"
    );
    assert_eq!(
        store.object_key(ObjectKind::Quarantine, "q", 0),
        "quarantine/did:example:alice/q"
    );
    assert!(store.list_objects("blocks/".to_owned()).await.is_err());
    assert!(matches!(
        store.delete_object(stored.clone()).await,
        Err(DeleteError::Ambiguous(_))
    ));
    assert!(!store.object_exists(stored.clone()).await.unwrap());
    assert!(BlobStore::get_object(&store, stored.clone()).await.is_err());
    assert!(store
        .put_object(stored.clone(), b"x".to_vec())
        .await
        .is_err());
    let latest = journal
        .latest("did:example:alice", &stored)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.operation, "put-object");
    assert!(latest.is_unresolved());
    // without a journal the bulk delete goes out as one request
    let bare = S3BlobStore::new(
        "did:example:alice".to_owned(),
        &cfg,
        Some("b".to_owned()),
        false,
    );
    assert!(bare.delete_many(vec![cid]).await.is_err());
    assert!(bare.make_permanent("k".to_owned(), cid).await.is_err());
    assert!(bare.quarantine(cid).await.is_err());
}

/// The adapter must preserve bytes across copy, quarantine, restore and
/// paginated listing; a definitive service refusal must not be journaled
/// as an ambiguous write.
#[tokio::test]
async fn s3_operations_preserve_bytes_and_journal_confirmed_outcomes() {
    let (endpoint, state, server) = fake_s3().await;
    let sdk = local_sdk(&endpoint);
    let dir = tempfile::tempdir().unwrap();
    let journal = AttemptJournal::open(dir.path().join("attempts.sqlite"), true)
        .await
        .unwrap();
    let did = "did:example:storage";
    let store = S3BlobStore::new(did.to_owned(), &sdk, Some("bucket".to_owned()), true)
        .with_attempts(journal.clone());
    let first = sha256_to_cid(Sha256::digest(b"first").to_vec());
    let second = sha256_to_cid(Sha256::digest(b"second").to_vec());
    let third = sha256_to_cid(Sha256::digest(b"third").to_vec());
    let fourth = sha256_to_cid(Sha256::digest(b"fourth").to_vec());

    let temporary = BlobStore::put_temp(&store, b"first".to_vec())
        .await
        .unwrap();
    assert!(BlobStore::has_temp(&store, temporary.clone())
        .await
        .unwrap());
    BlobStore::make_permanent(&store, temporary.clone(), first)
        .await
        .unwrap();
    assert!(!BlobStore::has_temp(&store, temporary).await.unwrap());
    assert!(BlobStore::has_stored(&store, first).await.unwrap());
    assert_eq!(
        BlobStore::get_bytes(&store, first).await.unwrap(),
        b"first".to_vec()
    );
    assert_eq!(
        BlobStore::get_stream(&store, first)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap()
            .into_bytes()
            .to_vec(),
        b"first".to_vec()
    );

    let file = dir.path().join("second.bin");
    std::fs::write(&file, b"second").unwrap();
    let temporary = BlobStore::put_temp_from_path(&store, file).await.unwrap();
    BlobStore::make_permanent_copy_only(&store, temporary.clone(), second)
        .await
        .unwrap();
    BlobStore::make_permanent_copy_only(&store, temporary.clone(), second)
        .await
        .unwrap();
    assert!(BlobStore::has_temp(&store, temporary.clone())
        .await
        .unwrap());
    assert_eq!(
        BlobStore::get_bytes(&store, second).await.unwrap(),
        b"second".to_vec()
    );
    BlobStore::delete_object(&store, store.get_tmp_path(&temporary))
        .await
        .unwrap();

    BlobStore::quarantine(&store, second).await.unwrap();
    assert!(!BlobStore::has_stored(&store, second).await.unwrap());
    assert!(BlobStore::has_quarantined(&store, second).await.unwrap());
    BlobStore::restore_copy_only(&store, second).await.unwrap();
    BlobStore::restore_copy_only(&store, second).await.unwrap();
    assert_eq!(
        BlobStore::get_bytes(&store, second).await.unwrap(),
        b"second".to_vec()
    );
    BlobStore::unquarantine(&store, second).await.unwrap();
    assert!(!BlobStore::has_quarantined(&store, second).await.unwrap());

    BlobStore::put_permanent(&store, third, b"third".to_vec())
        .await
        .unwrap();
    let mut listed = BlobStore::list_objects(&store, format!("blocks/{did}/"))
        .await
        .unwrap();
    listed.sort();
    let mut expected = vec![
        store.get_stored_path(first),
        store.get_stored_path(second),
        store.get_stored_path(third),
    ];
    expected.sort();
    assert_eq!(listed, expected);
    assert!(
        state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|request| { request.contains("list-type=2") })
            .count()
            >= 3
    );

    let duplicate = BlobStore::put_temp(&store, b"obsolete".to_vec())
        .await
        .unwrap();
    BlobStore::make_permanent(&store, duplicate.clone(), first)
        .await
        .unwrap();
    assert!(!BlobStore::has_temp(&store, duplicate).await.unwrap());
    assert_eq!(
        BlobStore::get_bytes(&store, first).await.unwrap(),
        b"first".to_vec()
    );

    BlobStore::delete_many(&store, vec![first, second])
        .await
        .unwrap();
    assert!(!BlobStore::has_stored(&store, first).await.unwrap());
    assert!(!BlobStore::has_stored(&store, second).await.unwrap());
    let deletion = journal
        .latest(did, &store.get_stored_path(first))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deletion.operation, "delete");
    assert_eq!(deletion.outcome.as_deref(), Some("succeeded"));

    let bare = S3BlobStore::new(did.to_owned(), &sdk, Some("bucket".to_owned()), true);
    BlobStore::delete_many(&bare, vec![third]).await.unwrap();
    assert!(
        !BlobStore::object_exists(&bare, store.get_stored_path(third))
            .await
            .unwrap()
    );
    BlobStore::put_permanent(&bare, fourth, b"fourth".to_vec())
        .await
        .unwrap();
    BlobStore::delete(&bare, fourth).await.unwrap();
    assert!(!BlobStore::has_stored(&bare, fourth).await.unwrap());

    let key = format!("blocks/{did}/physical");
    BlobStore::put_object(&store, key.clone(), b"physical".to_vec())
        .await
        .unwrap();
    assert_eq!(
        BlobStore::get_object(&store, key.clone()).await.unwrap(),
        b"physical".to_vec()
    );
    assert!(BlobStore::object_exists(&store, key.clone()).await.unwrap());
    state.lock().unwrap().fail_next = true;
    assert!(matches!(
        BlobStore::delete_object(&store, key.clone()).await,
        Err(DeleteError::Definitive(_))
    ));
    assert!(BlobStore::object_exists(&store, key.clone()).await.unwrap());
    state.lock().unwrap().fail_next = true;
    assert!(
        BlobStore::put_object(&store, key.clone(), b"rejected".to_vec())
            .await
            .is_err()
    );
    let failed = journal.latest(did, &key).await.unwrap().unwrap();
    assert_eq!(failed.operation, "put-object");
    assert!(failed.outcome.unwrap().starts_with("failed:"));
    assert_eq!(
        BlobStore::get_object(&store, key.clone()).await.unwrap(),
        b"physical".to_vec()
    );
    BlobStore::delete_object(&store, key).await.unwrap();
    server.abort();
}
