use super::{did_doc_for_session, validate_handle};
use crate::SharedIdResolver;
use rsky_identity::types::IdentityResolverOpts;
use rsky_identity::IdResolver;
use std::io::{Read, Write};

/// Serves one DID document for every request.
fn serve_document(body: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn resolver(plc_url: String) -> SharedIdResolver {
    SharedIdResolver {
        id_resolver: tokio::sync::RwLock::new(IdResolver::new(IdentityResolverOpts {
            timeout: Some(std::time::Duration::from_millis(500)),
            plc_url: Some(plc_url),
            did_cache: None,
            backup_nameservers: None,
        })),
    }
}

#[tokio::test]
async fn session_did_doc_is_optional_and_never_fails_the_session() {
    let did = "did:plc:sessiondoc";
    let good = resolver(serve_document(
        r#"{"id":"did:plc:sessiondoc","alsoKnownAs":["at://doc.test"],"verificationMethod":[],"service":[]}"#,
    ));
    assert_eq!(did_doc_for_session(false, &good, did).await, None);
    let doc = did_doc_for_session(true, &good, did).await.unwrap();
    assert_eq!(doc["id"], did);
    let unreachable = resolver("http://127.0.0.1:1".to_owned());
    assert_eq!(did_doc_for_session(true, &unreachable, did).await, None);
}

fn domains() -> Vec<String> {
    vec![
        ".pds.example.com".to_string(),
        "alt.example.net".to_string(),
    ]
}

#[test]
fn accepts_direct_child_of_service_domain() {
    assert!(validate_handle("alice.pds.example.com", &domains()));
}

#[test]
fn accepts_direct_child_of_secondary_domain() {
    assert!(validate_handle("bob.alt.example.net", &domains()));
}

#[test]
fn rejects_evil_suffix_domain() {
    assert!(!validate_handle("alice.evilpds.example.com", &domains()));
    assert!(!validate_handle("evilpds.example.com", &domains()));
}

#[test]
fn rejects_multi_label_handles() {
    assert!(!validate_handle("a.b.pds.example.com", &domains()));
}

#[test]
fn rejects_bare_service_domain() {
    assert!(!validate_handle("pds.example.com", &domains()));
    assert!(!validate_handle("alt.example.net", &domains()));
}
