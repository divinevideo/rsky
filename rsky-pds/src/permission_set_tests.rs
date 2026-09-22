use super::*;

/// Published permission-set schema, vendored to catch interoperability
/// regressions that round trips through our own serializer would miss.
const BULLETED_SPACE_ACCESS: &str = r#"{
  "$type": "com.atproto.lexicon.schema",
  "lexicon": 1,
  "id": "app.bulleted.spaceAccess",
  "defs": {
    "main": {
      "type": "permission-set",
      "title": "Bulleted spaces",
      "detail": "Read the outlines shared in your Bulleted spaces, and write your own bullets in them.",
      "permissions": [
        {
          "type": "permission",
          "resource": "space",
          "spaceType": "app.bulleted.space",
          "authority": "*",
          "collection": [
            "app.bulleted.node",
            "app.bulleted.note",
            "app.bulleted.outline",
            "app.bulleted.mirror",
            "app.bulleted.comment",
            "app.bulleted.commentPolicy"
          ],
          "action": ["read", "create", "update", "delete"]
        }
      ]
    }
  }
}"#;

fn bulleted_scopes() -> Vec<String> {
    let record: SchemaRecord = serde_json::from_str(BULLETED_SPACE_ACCESS).unwrap();
    resource_scopes_from_record(&record)
}

struct DirectoryFixture {
    resolver: PermissionSetResolver,
    http_requests: Arc<std::sync::Mutex<Vec<String>>>,
    service_available: Arc<std::sync::atomic::AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for DirectoryFixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn directory_fixture(txt: bool, service: bool, record_status: u16) -> DirectoryFixture {
    use hickory_resolver::config::{NameServerConfig, Protocol};
    use hickory_resolver::proto::op::{Message, MessageType};
    use hickory_resolver::proto::rr::{rdata::TXT, RData, Record};
    use tokio::io::AsyncWriteExt;
    const DID: &str = "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dns_address = socket.local_addr().unwrap();
    let http_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = http_requests.clone();
    let http_endpoint = endpoint.clone();
    let service_available = Arc::new(std::sync::atomic::AtomicBool::new(service));
    let published_service = service_available.clone();
    let http = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let request = read_fixture_request(&mut stream).await;
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            seen.lock().unwrap().push(path.clone());
            let (status, body) = if path == format!("/{DID}") {
                (
                    200,
                    serde_json::json!({"id": DID, "service": if published_service.load(std::sync::atomic::Ordering::SeqCst) {
                    vec![serde_json::json!({"id": format!("{DID}#atproto_pds"), "type": "AtprotoPersonalDataServer", "serviceEndpoint": http_endpoint})]
                } else { vec![] }}),
                )
            } else if path.starts_with("/xrpc/com.atproto.repo.getRecord?") {
                let url = url::Url::parse(&format!("{http_endpoint}{path}")).unwrap();
                let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
                assert_eq!(params.get("repo").map(String::as_str), Some(DID));
                assert_eq!(
                    params.get("collection").map(String::as_str),
                    Some("com.atproto.lexicon.schema")
                );
                assert_eq!(
                    params.get("rkey").map(String::as_str),
                    Some("app.example.permissions")
                );
                (
                    record_status,
                    serde_json::json!({"value": {
                        "$type": "com.atproto.lexicon.schema", "lexicon": 1, "id": "app.example.permissions",
                        "defs": {"main": {"type": "permission-set", "permissions": [
                            {"resource": "repo", "collection": ["app.example.note"], "action": ["create"]},
                            {"resource": "rpc", "lxm": ["app.example.read"], "inheritAud": true}
                        ]}}
                    }}),
                )
            } else {
                (404, serde_json::json!({"error": "NotFound"}))
            };
            let body = body.to_string();
            let response = format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let dns = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        while let Ok((count, address)) = socket.recv_from(&mut buffer).await {
            let query = Message::from_vec(&buffer[..count]).unwrap();
            let mut response = Message::new();
            response
                .set_id(query.id())
                .set_message_type(MessageType::Response)
                .set_recursion_desired(true)
                .set_recursion_available(true);
            for question in query.queries() {
                assert_eq!(question.name().to_utf8(), "_lexicon.example.app.");
                response.add_query(question.clone());
                response.add_answer(Record::from_rdata(
                    question.name().clone(),
                    60,
                    RData::TXT(TXT::new(vec![if txt {
                        format!("did={DID}")
                    } else {
                        "unrelated=fixture".into()
                    }])),
                ));
            }
            socket
                .send_to(&response.to_vec().unwrap(), address)
                .await
                .unwrap();
        }
    });
    let dns = (
        dns,
        TokioAsyncResolver::tokio(
            ResolverConfig::from_parts(
                None,
                vec![],
                vec![NameServerConfig::new(dns_address, Protocol::Udp)],
            ),
            ResolverOpts::default(),
        ),
    );
    let identity = DidResolver::new(DidResolverOpts {
        timeout: Some(Duration::from_secs(1)),
        plc_url: Some(endpoint),
        did_cache: Arc::new(MemoryCache::new(None, None)),
    });
    DirectoryFixture {
        resolver: PermissionSetResolver::with_resolvers(
            dns.1,
            identity,
            SafeClient::new(
                rsky_identity::safe_fetch::NetworkPolicy::PERMISSIVE,
                Duration::from_secs(1),
            )
            .unwrap(),
        ),
        http_requests,
        service_available,
        tasks: vec![http, dns.0],
    }
}

#[tokio::test]
async fn permission_sets_resolve_through_dns_and_plc_and_keep_audiences_separate_in_cache() {
    let fixture = directory_fixture(true, true, 200).await;
    let first = fixture
        .resolver
        .try_resolved_scopes("app.example.permissions?aud=did:web:first.example")
        .await
        .unwrap();
    let granted = crate::oauth_scope::GrantedScopes::parse(&first);
    assert!(granted.allows_repo("app.example.note", crate::oauth_scope::RepoAction::Create));
    assert!(!granted.allows_repo("app.example.note", crate::oauth_scope::RepoAction::Delete));
    assert!(!granted.allows_repo("app.example.other", crate::oauth_scope::RepoAction::Create));
    assert!(granted.allows_rpc("app.example.read", "did:web:first.example"));
    assert!(!granted.allows_rpc("app.example.read", "did:web:second.example"));
    let second = fixture
        .resolver
        .try_resolved_scopes("app.example.permissions?aud=did:web:second.example")
        .await
        .unwrap();
    let granted = crate::oauth_scope::GrantedScopes::parse(&second);
    assert!(granted.allows_rpc("app.example.read", "did:web:second.example"));
    assert!(!granted.allows_rpc("app.example.read", "did:web:first.example"));
    assert_eq!(
        fixture.http_requests.lock().unwrap().len(),
        2,
        "one DID lookup and one record fetch; cached grants retain per-request audiences"
    );
}

#[tokio::test]
async fn expired_permission_grants_refresh_the_publishers_service_document() {
    let fixture = directory_fixture(true, true, 200).await;
    assert!(!fixture
        .resolver
        .resolved_scopes("app.example.permissions")
        .await
        .is_empty());
    fixture
        .service_available
        .store(false, std::sync::atomic::Ordering::SeqCst);
    fixture
        .resolver
        .cache
        .write()
        .await
        .get_mut("app.example.permissions")
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);

    let error = fixture
        .resolver
        .try_resolved_scopes("app.example.permissions")
        .await
        .unwrap_err();
    assert!(error.reason.contains("no #atproto_pds service"), "{error}");
    assert!(fixture
        .resolver
        .resolved_scopes("app.example.permissions")
        .await
        .is_empty());
}

#[tokio::test]
async fn permission_sets_fail_closed_when_the_authority_document_or_record_is_missing() {
    for (txt, service, status, reason, requests) in [
        (false, true, 200, "no did= TXT record", 0),
        (true, false, 200, "no #atproto_pds service", 1),
        (true, true, 503, "returned 503", 2),
    ] {
        let fixture = directory_fixture(txt, service, status).await;
        let error = fixture
            .resolver
            .try_resolved_scopes("app.example.permissions")
            .await
            .unwrap_err();
        assert!(error.reason.contains(reason), "{error}");
        assert!(fixture
            .resolver
            .resolved_scopes("app.example.permissions")
            .await
            .is_empty());
        assert_eq!(fixture.http_requests.lock().unwrap().len(), requests);
    }
}

#[test]
fn incomplete_permissions_cannot_widen_access() {
    for resource in ["repo", "blob", "rpc", "identity", "account"] {
        assert!(Permission {
            resource: resource.into(),
            ..Default::default()
        }
        .to_scope_string()
        .is_none());
    }
    let permission = Permission {
        resource: "account".into(),
        attr: Some("email".into()),
        ..Default::default()
    };
    let granted =
        crate::oauth_scope::GrantedScopes::parse(&[permission.to_scope_string().unwrap()]);
    assert!(granted.allows_account("email", crate::oauth_scope::AccountAction::Read));
    assert!(!granted.allows_account("email", crate::oauth_scope::AccountAction::Manage));
}

#[tokio::test]
async fn expanding_includes_keeps_inline_scopes_and_restricts_resolved_grants() {
    let resolver = PermissionSetResolver::new();
    resolver
        .prime(
            "app.example.notes",
            vec![repo_permission("app.example.note")],
        )
        .await;
    let expanded = expand_includes(
        &resolver,
        &[
            "atproto".into(),
            "include:app.example.notes".into(),
            "blob:image/png".into(),
        ],
    )
    .await;
    let granted = crate::oauth_scope::GrantedScopes::parse(&expanded);
    assert!(granted.allows_repo("app.example.note", crate::oauth_scope::RepoAction::Create));
    assert!(!granted.allows_repo("app.example.other", crate::oauth_scope::RepoAction::Create));
    assert!(granted.allows_blob("image/png"));
    assert!(!granted.allows_blob("text/plain"));
}

#[test]
fn a_published_set_becomes_scope_strings_the_normal_parser_accepts() {
    let scopes = bulleted_scopes();
    assert_eq!(scopes.len(), 1);
    let scope = &scopes[0];
    assert!(scope.starts_with("space:app.bulleted.space?"), "{scope}");
    assert!(scope.contains("authority=*"), "{scope}");
    assert!(scope.contains("collection=app.bulleted.node"), "{scope}");
    assert!(scope.contains("action=create"), "{scope}");
    // The whole point: it parses as a grant, not just as a string.
    crate::space_scope::SpaceScope::parse(scope).expect("resolved scope must parse");
}

#[test]
fn entries_that_are_not_space_grants_are_skipped() {
    let repo_grant = Permission {
        resource: "repo".to_string(),
        collection: vec!["app.bulleted.node".to_string()],
        action: vec!["create".to_string()],
        ..Default::default()
    };
    assert!(repo_grant.to_space_scope().is_none());
    // A space entry with no space type names no spaces, so it grants none.
    let untyped = Permission {
        resource: "space".to_string(),
        space_type: None,
        ..repo_grant.clone()
    };
    assert!(untyped.to_space_scope().is_none());
}

#[test]
fn a_bare_space_grant_needs_no_query_string() {
    let bare = Permission {
        resource: "space".to_string(),
        space_type: Some("app.bulleted.space".to_string()),
        ..Default::default()
    };
    assert_eq!(bare.to_space_scope().unwrap(), "space:app.bulleted.space");
}

#[test]
fn manage_ops_survive_the_round_trip() {
    let managing = Permission {
        resource: "space".to_string(),
        space_type: Some("app.bulleted.space".to_string()),
        authority: Some("self".to_string()),
        skey: Some("main".to_string()),
        manage: vec!["create".to_string(), "delete".to_string()],
        ..Default::default()
    };
    let scope = managing.to_space_scope().unwrap();
    assert_eq!(
        scope,
        "space:app.bulleted.space?authority=self&skey=main&manage=create&manage=delete"
    );
    crate::space_scope::SpaceScope::parse(&scope).unwrap();
}

#[test]
fn to_scope_string_expands_repo_blob_rpc_identity_and_account() {
    let repo = Permission {
        resource: "repo".to_string(),
        collection: vec!["app.bulleted.node".to_string()],
        action: vec!["create".to_string()],
        ..Default::default()
    };
    let scope = repo.to_scope_string().unwrap();
    assert!(scope.starts_with("repo:?"), "{scope}");
    assert!(scope.contains("collection=app.bulleted.node"), "{scope}");
    assert!(scope.contains("action=create"), "{scope}");
    assert!(crate::oauth_scope::GrantedScopes::parse(&[scope])
        .allows_repo("app.bulleted.node", crate::oauth_scope::RepoAction::Create));

    let blob = Permission {
        resource: "blob".to_string(),
        accept: vec!["image/*".to_string()],
        ..Default::default()
    };
    assert_eq!(blob.to_scope_string().unwrap(), "blob:?accept=image/*");

    let rpc = Permission {
        resource: "rpc".to_string(),
        lxm: vec!["com.example.method".to_string()],
        aud: Some("did:web:example.com".to_string()),
        ..Default::default()
    };
    let scope = rpc.to_scope_string().unwrap();
    assert!(scope.starts_with("rpc:?"), "{scope}");
    assert!(scope.contains("lxm=com.example.method"), "{scope}");
    assert!(scope.contains("aud=did:web:example.com"), "{scope}");

    let identity = Permission {
        resource: "identity".to_string(),
        attr: Some("handle".to_string()),
        ..Default::default()
    };
    assert_eq!(identity.to_scope_string().unwrap(), "identity:handle");

    let account = Permission {
        resource: "account".to_string(),
        attr: Some("email".to_string()),
        action: vec!["manage".to_string()],
        ..Default::default()
    };
    assert_eq!(
        account.to_scope_string().unwrap(),
        "account:email?action=manage"
    );

    // Missing the field its kind requires: no grant.
    assert!(Permission {
        resource: "rpc".to_string(),
        lxm: vec!["com.example.method".to_string()],
        ..Default::default()
    }
    .to_scope_string()
    .is_none());
    // An unrecognised resource confers nothing.
    assert!(Permission {
        resource: "unknown-future-resource".to_string(),
        ..Default::default()
    }
    .to_scope_string()
    .is_none());
}

/// The bug this whole file exists to fix: a permission set naming only a
/// `repo:` grant (no `space:`) used to expand into nothing, so a session
/// that granted only `include:<nsid>` ended up with no `repo:` scope at
/// all. Expanding non-space entries is what gives such a session the
/// grants it was actually issued.
#[tokio::test]
async fn a_permission_set_only_grant_restricts_rather_than_failing_open() {
    const REPO_ONLY_SET: &str = r#"{
      "$type": "com.atproto.lexicon.schema",
      "lexicon": 1,
      "id": "app.example.repoAccess",
      "defs": {
        "main": {
          "type": "permission-set",
          "permissions": [
            {
              "type": "permission",
              "resource": "repo",
              "action": ["create", "update", "delete"],
              "collection": ["app.bsky.feed.post"]
            }
          ]
        }
      }
    }"#;
    let record: SchemaRecord = serde_json::from_str(REPO_ONLY_SET).unwrap();
    let scopes = resource_scopes_from_record(&record);
    assert_eq!(scopes.len(), 1);

    // Simulate what `expand_includes` produces for a session that
    // granted only the permission set: `atproto` plus the resolved
    // `repo:` scope, no bare `repo:` of its own.
    let mut granted = vec!["atproto".to_string()];
    granted.extend(scopes);
    let granted_scopes = crate::oauth_scope::GrantedScopes::parse(&granted);
    assert!(
        granted_scopes.allows_repo("app.bsky.feed.post", crate::oauth_scope::RepoAction::Create)
    );
    assert!(
        !granted_scopes.allows_repo("app.bsky.feed.like", crate::oauth_scope::RepoAction::Create)
    );
}

#[tokio::test]
async fn an_unresolvable_set_confers_nothing_and_is_not_retried_immediately() {
    let fixture = directory_fixture(true, true, 503).await;
    let resolver = &fixture.resolver;
    // A transient publisher failure contributes no permission grants.
    let first = resolver.resolved_scopes("app.example.permissions").await;
    assert!(first.is_empty());
    // The failure is remembered, so the next call is a cache hit rather
    // than another DNS lookup, and it still reports as a failure.
    {
        let cached = resolver.cache.read().await;
        assert!(cached["app.example.permissions"].failed);
    }
    let err = resolver
        .try_resolved_scopes("app.example.permissions")
        .await
        .unwrap_err();
    assert_eq!(err.nsid, "app.example.permissions");
    assert_eq!(err.reason, "recent fetch failed");
    assert!(err.to_string().contains("app.example.permissions"));
}

#[tokio::test]
async fn a_cached_set_is_served_as_resolved() {
    let fixture = directory_fixture(true, true, 503).await;
    let resolver = &fixture.resolver;
    resolver
        .prime(
            "app.example.cached",
            vec![repo_permission("app.example.record")],
        )
        .await;
    assert_eq!(
        resolver
            .try_resolved_scopes("app.example.cached")
            .await
            .unwrap(),
        vec!["repo:?collection=app.example.record".to_string()]
    );
    // an expired entry is fetched again
    resolver.cache.write().await.insert(
        "app.example.permissions".to_string(),
        CacheEntry {
            permissions: vec![repo_permission("app.example.record")],
            expires: Instant::now() - Duration::from_secs(1),
            failed: false,
        },
    );
    assert!(resolver
        .try_resolved_scopes("app.example.permissions")
        .await
        .is_err());
}

#[test]
fn include_scopes_carry_an_optional_audience() {
    assert_eq!(
        IncludeScope::parse("app.bsky.authViewAll").unwrap(),
        IncludeScope {
            nsid: "app.bsky.authViewAll".into(),
            aud: None
        }
    );
    assert_eq!(
        IncludeScope::parse("app.bsky.authViewAll?aud=did:web:api.bsky.app%23bsky_appview")
            .unwrap(),
        IncludeScope {
            nsid: "app.bsky.authViewAll".into(),
            aud: Some("did:web:api.bsky.app#bsky_appview".into())
        }
    );
    let error = IncludeScope::parse("app.bsky.authViewAll?aud=").unwrap_err();
    assert_eq!(error.reason, "unexpected include parameter: aud=");
    let error = IncludeScope::parse("app.bsky.authViewAll?lxm=x").unwrap_err();
    assert_eq!(error.reason, "unexpected include parameter: lxm=x");
    let error = IncludeScope::parse("app.bsky.authViewAll?aud=a&aud=b").unwrap_err();
    assert!(error.reason.contains("aud=b"));
    let error = IncludeScope::parse("not an nsid").unwrap_err();
    assert!(error.reason.starts_with("invalid nsid"), "{error}");
}

/// The reference's rule: an `rpc` permission that inherits its audience
/// takes the one the `include:` names; without one it keeps this
/// server's any-audience stand-in.
#[tokio::test]
async fn an_inherited_audience_comes_from_the_include() {
    let resolver = PermissionSetResolver::new();
    resolver
        .prime(
            "app.example.viewAll",
            vec![
                Permission {
                    resource: "rpc".into(),
                    lxm: vec!["app.example.getThing".into()],
                    inherit_aud: true,
                    ..Default::default()
                },
                Permission {
                    resource: "rpc".into(),
                    lxm: vec!["app.example.getOther".into()],
                    aud: Some("*".into()),
                    ..Default::default()
                },
            ],
        )
        .await;
    assert_eq!(
        resolver
            .try_resolved_scopes("app.example.viewAll?aud=did:web:api.example.com%23svc")
            .await
            .unwrap(),
        [
            "rpc:?lxm=app.example.getThing&aud=did:web:api.example.com#svc",
            "rpc:?lxm=app.example.getOther&aud=*",
        ]
    );
    assert_eq!(
        resolver.resolved_scopes("app.example.viewAll").await,
        [
            "rpc:?lxm=app.example.getThing&aud=*",
            "rpc:?lxm=app.example.getOther&aud=*",
        ]
    );
    assert!(resolver
        .resolved_scopes("app.example.viewAll?nope=1")
        .await
        .is_empty());
}

#[tokio::test]
async fn expansion_leaves_scopes_that_are_not_includes_alone() {
    let resolver = PermissionSetResolver::new();
    let granted = vec!["atproto".to_string(), "blob:image/*".to_string()];
    assert_eq!(expand_includes(&resolver, &granted).await, granted);
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
