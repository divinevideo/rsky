//! Account and authorization contracts exercised against the real SQLite PDS.
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::{Client, LocalResponse};
use rocket::request::FromRequest;
use rsky_pds::account_manager::AccountManager;
use rsky_pds::auth_verifier::{validate_bearer_access_token, AuthScope, UserDidAuth};
use serde_json::{json, Value};

mod common;

const DID: &str = "did:plc:khvyd3oiw46vif5gm7hijslk";

async fn test_client() -> (tempfile::TempDir, Client) {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // Scope denials happen before any outbound request. Keep the configured
        // destination valid under the public policy so the grant is decisive.
        std::env::set_var("PDS_BSKY_APP_VIEW_URL", "https://appview.example");
        std::env::set_var("PDS_BSKY_APP_VIEW_DID", "did:web:appview.example");
    });
    common::oauth::get_oauth_client().await
}

async fn post<'a>(client: &'a Client, method: &str, auth: &str, body: Value) -> LocalResponse<'a> {
    client
        .post(format!("/xrpc/{method}"))
        .header(ContentType::JSON)
        .header(Header::new("Authorization", auth.to_owned()))
        .body(body.to_string())
        .dispatch()
        .await
}

async fn login(client: &Client) -> String {
    let response = post(
        client,
        "com.atproto.server.createSession",
        "",
        json!({
            "identifier": "foo@example.com", "password": "password"
        }),
    )
    .await;
    assert_eq!(response.status(), Status::Ok);
    response.into_json::<Value>().await.unwrap()["accessJwt"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn moderation_tracks_account_and_record_status_without_losing_the_subject() {
    let (_dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    let auth = format!("Bearer {}", login(&client).await);
    let admin = common::get_admin_token();
    let info = client
        .get(format!("/xrpc/com.atproto.admin.getAccountInfo?did={DID}"))
        .header(Header::new("Authorization", admin.clone()))
        .dispatch()
        .await;
    assert_eq!(info.status(), Status::Ok);
    let info: Value = info.into_json().await.unwrap();
    assert_eq!(info["did"], DID);
    assert_eq!(info["email"], "foo@example.com");

    let created = post(&client, "com.atproto.repo.createRecord", &auth, json!({
        "repo": DID, "collection": "app.bsky.feed.post", "rkey": "moderation-test",
        "record": {"$type": "app.bsky.feed.post", "text": "Synthetic moderation fixture", "createdAt": "2026-01-01T00:00:00.000Z"}
    })).await;
    assert_eq!(created.status(), Status::Ok);
    let record: Value = created.into_json().await.unwrap();
    let subject =
        json!({"$type": "com.atproto.repo.strongRef", "uri": record["uri"], "cid": record["cid"]});
    for applied in [true, false] {
        let updated = post(
            &client,
            "com.atproto.admin.updateSubjectStatus",
            &admin,
            json!({
                "subject": subject, "takedown": {"applied": applied, "ref": "synthetic-review"}
            }),
        )
        .await;
        assert_eq!(updated.status(), Status::Ok);
        let status = client
            .get(format!(
                "/xrpc/com.atproto.admin.getSubjectStatus?uri={}",
                urlencoding::encode(record["uri"].as_str().unwrap())
            ))
            .header(Header::new("Authorization", admin.clone()))
            .dispatch()
            .await;
        assert_eq!(status.status(), Status::Ok);
        let status: Value = status.into_json().await.unwrap();
        assert_eq!(status["subject"]["uri"], record["uri"]);
        assert_eq!(status["takedown"]["applied"], applied);
    }
    for applied in [true, false] {
        let updated = post(
            &client,
            "com.atproto.admin.updateSubjectStatus",
            &admin,
            json!({
                "subject": {"$type": "com.atproto.admin.defs#repoRef", "did": DID},
                "takedown": {"applied": applied, "ref": "synthetic-review"}
            }),
        )
        .await;
        assert_eq!(updated.status(), Status::Ok);
        let status = client
            .get(format!(
                "/xrpc/com.atproto.admin.getSubjectStatus?did={DID}"
            ))
            .header(Header::new("Authorization", admin.clone()))
            .dispatch()
            .await;
        assert_eq!(status.status(), Status::Ok);
        assert_eq!(
            status.into_json::<Value>().await.unwrap()["takedown"]["applied"],
            applied
        );
    }
    let missing_blob = client.get(format!("/xrpc/com.atproto.admin.getSubjectStatus?did={DID}&blob=bafkreihl6t3dlil5cdlowrv2nafxafbedgvdeihsfaabua3ngencf3u5fi"))
        .header(Header::new("Authorization", admin)).dispatch().await;
    assert_eq!(missing_blob.status(), Status::InternalServerError);
}

#[tokio::test]
async fn account_tools_keep_private_keys_private_and_enforce_service_token_lifetimes() {
    let (dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    // The embedded-provider handler advertises this PDS as its own issuer;
    // the separately mounted well-known handler can advertise an entryway.
    let shared = client
        .rocket()
        .state::<rsky_pds::oauth::SharedOAuthProvider>()
        .unwrap();
    let metadata =
        rsky_pds::oauth::routes::oauth_protected_resource_metadata(rocket::State::from(shared))
            .await
            .into_inner();
    let resource = &client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .service
        .public_url;
    assert_eq!(metadata["resource"], *resource);
    assert_eq!(metadata["authorization_servers"], json!([resource]));
    assert_eq!(metadata["bearer_methods_supported"], json!(["header"]));
    let token = login(&client).await;
    let auth = format!("Bearer {token}");
    let reserved = post(
        &client,
        "com.atproto.server.reserveSigningKey",
        "",
        json!({}),
    )
    .await;
    assert_eq!(reserved.status(), Status::Ok);
    let reserved: Value = reserved.into_json().await.unwrap();
    assert!(reserved["signingKey"]
        .as_str()
        .unwrap()
        .starts_with("did:key:z"));
    assert_eq!(reserved.as_object().unwrap().len(), 1);
    let invites = client
        .get(
            "/xrpc/com.atproto.server.getAccountInviteCodes?includeUsed=true&createAvailable=false",
        )
        .header(Header::new("Authorization", auth.clone()))
        .dispatch()
        .await;
    assert_eq!(invites.status(), Status::Ok);
    assert!(invites.into_json::<Value>().await.unwrap()["codes"].is_array());
    let password = post(
        &client,
        "com.atproto.server.createAppPassword",
        &auth,
        json!({"name": "revocable"}),
    )
    .await;
    assert_eq!(password.status(), Status::Ok);
    let password: Value = password.into_json().await.unwrap();
    let revoked = post(
        &client,
        "com.atproto.server.revokeAppPassword",
        &auth,
        json!({"name": "revocable"}),
    )
    .await;
    assert_eq!(revoked.status(), Status::Ok);
    let rejected = post(
        &client,
        "com.atproto.server.createSession",
        "",
        json!({
            "identifier": "foo@example.com", "password": password["password"]
        }),
    )
    .await;
    assert_eq!(rejected.status(), Status::Unauthorized);

    let now = common::oauth::now_secs();
    for (expiry, expected) in [(now - 60, false), (now + 30, true), (now + 7200, false)] {
        let response = client.get(format!("/xrpc/com.atproto.server.getServiceAuth?aud=did:web:video.example&lxm=app.bsky.video.getUploadLimits&exp={expiry}"))
            .header(Header::new("Authorization", auth.clone())).dispatch().await;
        assert_eq!(response.status().code == 200, expected);
        let body: Value = response.into_json().await.unwrap();
        assert_eq!(body["token"].is_string(), expected);
    }
    let request = client
        .get("/xrpc/com.atproto.server.getSession")
        .header(Header::new("Authorization", auth.clone()));
    let validated = validate_bearer_access_token(request.inner(), vec![AuthScope::Access])
        .await
        .unwrap();
    assert_eq!(validated.credentials.unwrap().did.as_deref(), Some(DID));
    assert!(
        validate_bearer_access_token(request.inner(), vec![AuthScope::AppPass])
            .await
            .is_err()
    );
    let proxy = rsky_pds::auth_verifier::scope::Scoped::<rsky_pds::auth_verifier::scope::RpcProxy>::from_request(request.inner()).await;
    let rocket::request::Outcome::Success(proxy) = proxy else {
        panic!("valid session rejected")
    };
    assert_eq!(proxy.requester_did().as_deref(), Some(DID));
    let missing = client.get("/xrpc/com.atproto.server.getSession");
    assert!(matches!(
        UserDidAuth::from_request(missing.inner()).await,
        rocket::request::Outcome::Error(_)
    ));

    let requested = post(
        &client,
        "com.atproto.identity.requestPlcOperationSignature",
        &auth,
        json!({}),
    )
    .await;
    assert_eq!(requested.status(), Status::Ok);
    let email_token: String = rusqlite::Connection::open(dir.path().join("account.sqlite"))
        .unwrap()
        .query_row(
            "SELECT token FROM email_token WHERE did = ?1 AND purpose = 'plc_operation'",
            [DID],
            |row| row.get(0),
        )
        .unwrap();
    let actor_key = client
        .rocket()
        .state::<rsky_pds::actor_store::ActorStore>()
        .unwrap()
        .keypair(DID)
        .await
        .unwrap();
    let published_key = rsky_crypto::utils::encode_did_key(&actor_key.public_key());
    let signed = post(
        &client,
        "com.atproto.identity.signPlcOperation",
        &auth,
        json!({"token": email_token, "verificationMethods": {"atproto": published_key}}),
    )
    .await;
    assert_eq!(signed.status(), Status::Ok);
    let signed: Value = signed.into_json().await.unwrap();
    let submitted = post(
        &client,
        "com.atproto.identity.submitPlcOperation",
        &auth,
        signed.clone(),
    )
    .await;
    assert_eq!(submitted.status(), Status::Ok);
    assert_eq!(signed["operation"]["type"], "plc_operation");
    assert!(signed["operation"]["sig"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn login_reports_database_failures_as_server_errors() {
    let (dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    common::oauth::drop_table(dir.path(), "account");
    let failed = post(
        &client,
        "com.atproto.server.createSession",
        "",
        json!({
            "identifier": "foo@example.com", "password": "password"
        }),
    )
    .await;
    assert_eq!(failed.status(), Status::InternalServerError);
}

#[tokio::test]
async fn centralized_invites_are_absent_from_the_account_view() {
    let (_dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    let account = client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .get_account(DID, None)
        .await
        .unwrap()
        .unwrap();
    let view = rsky_pds::apis::com::atproto::admin::get_account_info::format_account_view(
        account,
        vec![],
        &std::collections::BTreeMap::new(),
        false,
    );
    assert_eq!(view.did, DID);
    assert!(view.invites.is_none());
    assert!(view.invited_by.is_none());
    assert!(view.invites_disabled.is_none());
}

#[tokio::test]
async fn service_guards_accept_only_configured_issuers_and_the_recipient_audience() {
    use rsky_pds::account_manager::helpers::auth::{create_service_jwt, ServiceJwtParams};
    use rsky_pds::actor_store::ActorStore;
    use rsky_pds::auth_verifier::{verify_service_jwt, ModService, ServiceJwtOpts};
    use rsky_pds::SharedIdResolver;
    let (_dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    let key = client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .keypair(&DID.to_owned())
        .await
        .unwrap();
    common::set_published_signing_key(Some(rsky_crypto::utils::encode_did_key(&key.public_key())));
    let resolver = client.rocket().state::<SharedIdResolver>().unwrap();
    resolver
        .id_resolver
        .read()
        .await
        .did
        .ensure_resolve(&DID.to_owned(), Some(true))
        .await
        .unwrap();
    let audience = std::env::var("PDS_SERVICE_DID").unwrap();
    for (issuer, recipient, trusted) in [
        (DID, audience.as_str(), true),
        (DID, "did:web:other.example", true),
        ("did:plc:aaaaaaaaaaaaaaaaaaaaaaaa", audience.as_str(), false),
    ] {
        let token = create_service_jwt(
            ServiceJwtParams {
                iss: issuer.into(),
                aud: recipient.into(),
                exp: None,
                lxm: Some("com.atproto.admin.getAccountInfo".into()),
                jti: None,
            },
            &key,
        )
        .await
        .unwrap();
        let request = client
            .get("/xrpc/com.atproto.admin.getAccountInfo")
            .header(Header::new("Authorization", format!("Bearer {token}")));
        let result = verify_service_jwt(
            request.inner(),
            rocket::State::from(resolver),
            ServiceJwtOpts {
                aud: Some(audience.clone()),
                iss: Some(vec![DID.into()]),
            },
        )
        .await;
        if trusted && recipient == audience {
            assert_eq!(result.unwrap().iss, DID);
        } else {
            let error = result.err().unwrap().to_string();
            assert!(error.contains(if trusted {
                "BadJwtAudience"
            } else {
                "UntrustedIss"
            }));
        }
    }
    // A user-signed service token and moderator token share signature
    // verification, while the moderator also checks its configured issuer.
    std::env::set_var("PDS_MOD_SERVICE_DID", DID);
    for (recipient, expected) in [(audience.as_str(), true), ("did:web:other.example", false)] {
        let token = create_service_jwt(
            ServiceJwtParams {
                iss: DID.into(),
                aud: recipient.into(),
                exp: None,
                lxm: Some("com.atproto.admin.getAccountInfo".into()),
                jti: None,
            },
            &key,
        )
        .await
        .unwrap();
        let request = client
            .get("/xrpc/com.atproto.admin.getAccountInfo")
            .header(Header::new("Authorization", format!("Bearer {token}")));
        assert_eq!(
            matches!(
                ModService::from_request(request.inner()).await,
                rocket::request::Outcome::Success(_)
            ),
            expected
        );
        if expected {
            assert!(matches!(
                UserDidAuth::from_request(request.inner()).await,
                rocket::request::Outcome::Success(_)
            ));
        }
    }
    common::set_published_signing_key(None);
}

#[tokio::test]
async fn narrow_oauth_grants_cannot_proxy_ungranted_actor_or_feed_methods() {
    use common::oauth::*;
    let (_dir, client) = test_client().await;
    create_active_account(&client).await;
    let key = dpop_key();
    let scope = "atproto repo:app.bsky.feed.post";
    let client_id = loopback_client_id(scope);
    let (request_uri, nonce) = run_par_scoped(&client, &key, &client_id, scope).await;
    let mut session = open_authorize_page_scoped(&client, &client_id, &request_uri).await;
    let code = sign_in_and_accept_scoped(&client, &client_id, &request_uri, &mut session).await;
    let tokens = exchange_code_scoped(&client, &client_id, &key, &code, &nonce).await;
    let token = tokens["access_token"].as_str().unwrap();
    for path in [
        format!("/xrpc/app.bsky.actor.getProfile?actor={DID}"),
        format!("/xrpc/app.bsky.actor.getProfiles?actors={DID}"),
        format!("/xrpc/app.bsky.feed.getAuthorFeed?actor={DID}"),
        "/xrpc/app.bsky.feed.getTimeline".into(),
        format!("/xrpc/app.bsky.feed.getFeed?feed=at://{DID}/app.bsky.feed.generator/test"),
    ] {
        let (status, body) = dpop_get(&client, &key, token, &path).await;
        assert_eq!(status, Status::Forbidden, "{path}: {body}");
        assert_eq!(body["error"], "InsufficientScope");
    }
}

#[tokio::test]
async fn space_service_tokens_reject_implausible_issue_times_before_resolution() {
    use base64::Engine;
    use rsky_pds::actor_store::ActorStore;
    use rsky_pds::SharedIdResolver;
    let (_dir, client) = test_client().await;
    let now = common::oauth::now_secs();
    for (iat, expected) in [
        (now + 3600, "jwt iat in the future"),
        (now - 7200, "jwt iat too old"),
    ] {
        let payload = json!({"iss": DID, "aud": "did:web:recipient.example", "iat": iat,
            "exp": now + 60, "lxm": "app.example.notify", "jti": "synthetic-time-fixture"});
        let token = format!(
            "e30.{}.AA",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
        );
        let error = rsky_pds::space_auth::verify_space_service_token(
            client.rocket().state::<ActorStore>().unwrap(),
            client.rocket().state::<SharedIdResolver>().unwrap(),
            &token,
            "app.example.notify",
            "did:web:recipient.example",
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), expected);
    }
}

#[tokio::test]
async fn access_status_checks_reject_removed_and_taken_down_accounts() {
    use rsky_pds::auth_verifier::{validate_access_token, AuthError, ValidateAccessTokenOpts};
    let (dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    let token = login(&client).await;
    let db = rusqlite::Connection::open(dir.path().join("account.sqlite")).unwrap();
    for (sql, expected) in [
        (
            "UPDATE actor SET \"takedownRef\" = 'synthetic-review' WHERE did = ?1",
            "takedown",
        ),
        ("DELETE FROM actor WHERE did = ?1", "missing"),
    ] {
        db.execute(sql, [DID]).unwrap();
        let request = client
            .get("/xrpc/com.atproto.repo.createRecord")
            .header(Header::new("Authorization", format!("Bearer {token}")));
        let error = validate_access_token(
            request.inner(),
            vec![AuthScope::Access],
            Some(ValidateAccessTokenOpts {
                check_takedown: Some(true),
                check_deactivated: Some(false),
            }),
        )
        .await
        .err()
        .unwrap();
        match (expected, error.downcast_ref::<AuthError>()) {
            ("takedown", Some(AuthError::AccountTakedown(_)))
            | ("missing", Some(AuthError::AccountNotFound(_))) => (),
            _ => panic!("wrong authentication failure: {error}"),
        }
    }
}

#[tokio::test]
async fn space_proofs_cannot_be_reused_with_another_key_binding() {
    use common::oauth::*;
    let (_dir, client) = test_client().await;
    let key = dpop_key();
    let path = "/xrpc/com.atproto.space.getRecord";
    let credential = "synthetic-bound-credential";
    let proof = dpop_proof(
        &key,
        "GET",
        &format!("{}{path}", public_url(&client)),
        None,
        Some(credential),
    );
    let request = client.get(path).header(Header::new("DPoP", proof));
    let error = rsky_pds::space_auth::verify_bound_proof(
        request.inner(),
        credential,
        "another-key-thumbprint",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("thumbprint does not match"));
}

/// A loopback directory containing explicit documents, with no internet fallback.
async fn directory(body: Value) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let worker = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let request = read_fixture_request(&mut stream).await;
            let path = request.split_whitespace().nth(1).unwrap();
            let payload = if path == "/.well-known/atproto-did" {
                DID.to_owned()
            } else {
                body.to_string()
            };
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    (format!("http://localhost:{port}"), worker)
}

fn local_resolver(url: String) -> rsky_pds::SharedIdResolver {
    rsky_pds::SharedIdResolver {
        id_resolver: tokio::sync::RwLock::new(
            rsky_identity::IdResolver::new(rsky_identity::types::IdentityResolverOpts {
                timeout: Some(std::time::Duration::from_secs(1)),
                plc_url: Some(url),
                did_cache: None,
                backup_nameservers: None,
            })
            .with_network(rsky_identity::safe_fetch::NetworkPolicy::PERMISSIVE),
        ),
    }
}

#[tokio::test]
async fn identity_resolution_preserves_dids_without_claiming_an_unverified_handle() {
    use rsky_pds::apis::com::atproto::identity::resolve_identity::{
        inner_resolve_identity, resolve_handle_to_did,
    };
    let (_dir, client) = test_client().await;
    let account_manager = client.rocket().state::<AccountManager>().unwrap();
    let (url, worker) =
        directory(json!({"id": DID, "alsoKnownAs": [], "verificationMethod": [], "service": []}))
            .await;
    let resolver = local_resolver(url.clone());
    let resolved = inner_resolve_identity(
        DID.into(),
        false,
        rocket::State::from(&resolver),
        account_manager,
    )
    .await
    .unwrap();
    assert_eq!(resolved.did, DID);
    assert_eq!(resolved.handle, "handle.invalid");
    assert_eq!(resolved.did_doc["id"], DID);
    let handle = url.strip_prefix("http://").unwrap().to_owned();
    let did = resolve_handle_to_did(&handle, rocket::State::from(&resolver), account_manager)
        .await
        .unwrap();
    assert_eq!(did, DID);
    worker.abort();
    let missing = resolve_handle_to_did(
        &"localhost:1".into(),
        rocket::State::from(&resolver),
        account_manager,
    )
    .await;
    assert!(
        matches!(missing, Err(rsky_pds::apis::ApiError::BadRequest(code, _)) if code == "HandleNotFound")
    );
}

#[tokio::test]
async fn a_remote_space_issuer_uses_the_published_signing_key() {
    use rsky_pds::actor_store::ActorStore;
    let (_dir, client) = test_client().await;
    let key = secp256k1::Keypair::from_secret_key(
        &secp256k1::Secp256k1::new(),
        &secp256k1::SecretKey::from_slice(&[0x37; 32]).unwrap(),
    );
    let did_key = rsky_crypto::utils::encode_did_key(&key.public_key());
    let (url, worker) = directory(json!({"id": DID, "verificationMethod": [{
        "id": format!("{DID}#atproto"), "type": "Multikey", "controller": DID,
        "publicKeyMultibase": did_key.strip_prefix("did:key:").unwrap()
    }], "service": []}))
    .await;
    let resolver = local_resolver(url);
    let token = rsky_pds::space_auth::mint_space_service_token(
        &key,
        DID,
        "did:web:recipient.example",
        "com.atproto.space.notifyWrite",
    );
    let claims = rsky_pds::space_auth::verify_space_service_token(
        client.rocket().state::<ActorStore>().unwrap(),
        &resolver,
        &token,
        "com.atproto.space.notifyWrite",
        "did:web:recipient.example",
    )
    .await
    .unwrap();
    assert_eq!(claims.iss, DID);
    assert_eq!(claims.lxm, "com.atproto.space.notifyWrite");
    worker.abort();
}

#[tokio::test]
async fn failed_account_persistence_removes_the_new_actor_store() {
    use rsky_pds::actor_store::ActorStore;
    let (dir, client) = test_client().await;
    rusqlite::Connection::open(dir.path().join("account.sqlite")).unwrap().execute_batch(
        "CREATE TRIGGER refuse_account BEFORE INSERT ON account BEGIN SELECT RAISE(ABORT, 'synthetic storage failure'); END;"
    ).unwrap();
    let domain = client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0]
        .clone();
    let response = post(&client, "com.atproto.server.createAccount", &common::get_admin_token(), json!({
        "did": DID, "handle": format!("rollback{domain}"), "email": "rollback@example.com", "password": "password"
    })).await;
    assert_eq!(response.status(), Status::InternalServerError);
    assert!(!client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .exists(DID)
        .await
        .unwrap());
    assert!(client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .get_account(
            DID,
            Some(
                rsky_pds::account_manager::helpers::account::AvailabilityFlags {
                    include_deactivated: Some(true),
                    include_taken_down: Some(true)
                }
            )
        )
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn account_creation_does_not_report_success_when_initial_commit_publication_fails() {
    let (dir, client) = test_client().await;
    let admin = common::get_admin_token();
    let invite = post(
        &client,
        "com.atproto.server.createInviteCode",
        &admin,
        json!({"useCount": 1}),
    )
    .await;
    assert_eq!(invite.status(), Status::Ok);
    let invite: Value = invite.into_json().await.unwrap();
    rusqlite::Connection::open(dir.path().join("sequencer.sqlite")).unwrap().execute_batch(
        "CREATE TRIGGER refuse_commit BEFORE INSERT ON repo_seq WHEN NEW.\"eventType\" = 'append' BEGIN SELECT RAISE(ABORT, 'synthetic publication failure'); END;"
    ).unwrap();
    let domain = client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0]
        .clone();
    let response = post(&client, "com.atproto.server.createAccount", &admin, json!({
        "handle": format!("publication{domain}"), "email": "publication@example.com", "password": "password", "inviteCode": invite["code"]
    })).await;
    assert_eq!(response.status(), Status::InternalServerError);
    let events: Vec<String> = rusqlite::Connection::open(dir.path().join("sequencer.sqlite"))
        .unwrap()
        .prepare("SELECT \"eventType\" FROM repo_seq ORDER BY seq")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(events.contains(&"identity".to_owned()));
    assert!(events.contains(&"account".to_owned()));
    assert!(!events.contains(&"append".to_owned()));
}

#[tokio::test]
async fn admin_handle_update_persists_the_published_handle() {
    let (_dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    let domain = client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0]
        .clone();
    let handle = format!("renamed{domain}");
    let response = post(
        &client,
        "com.atproto.admin.updateAccountHandle",
        &common::get_admin_token(),
        json!({"did": DID, "handle": handle}),
    )
    .await;
    assert_eq!(response.status(), Status::Ok);
    let account = client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .get_account(DID, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.handle.as_deref(), Some(handle.as_str()));
}

#[tokio::test]
async fn oauth_validation_keeps_grants_without_permission_sets_and_rejects_wrong_tiers() {
    use common::oauth::*;
    use rsky_pds::auth_verifier::validate_access_token;
    use rsky_pds::oauth::SharedOAuthProvider;
    let (_dir, full) = test_client().await;
    create_active_account(&full).await;
    let key = dpop_key();
    let token = generic_oauth_access_token(&full, &key).await;
    let shared = full.rocket().state::<SharedOAuthProvider>().unwrap();
    let cfg = full
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .clone();
    let without_config = Client::untracked(rocket::build().manage(SharedOAuthProvider {
        provider: shared.provider.clone(),
        secure_cookies: shared.secure_cookies,
    }))
    .await
    .unwrap();
    let request = without_config
        .get("/xrpc/com.atproto.server.getSession")
        .header(Header::new("Authorization", format!("DPoP {token}")));
    let error = validate_access_token(request.inner(), vec![], None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "Server config is not available");
    let minimal = Client::untracked(rocket::build().manage(cfg).manage(SharedOAuthProvider {
        provider: shared.provider.clone(),
        secure_cookies: shared.secure_cookies,
    }))
    .await
    .unwrap();
    let path = "/xrpc/com.atproto.server.getSession";
    for scope in [AuthScope::AppPass, AuthScope::Access] {
        let nonce = shared.provider.next_dpop_nonce(now_secs()).unwrap();
        let request = minimal
            .get(path)
            .header(Header::new("Authorization", format!("DPoP {token}")))
            .header(Header::new(
                "DPoP",
                dpop_proof(
                    &key,
                    "GET",
                    &format!("{}{path}", public_url(&full)),
                    Some(&nonce),
                    Some(&token),
                ),
            ));
        let result = validate_access_token(request.inner(), vec![scope.clone()], None).await;
        if scope == AuthScope::AppPass {
            let credentials = result.unwrap().credentials.unwrap();
            assert_eq!(credentials.did.as_deref(), Some(DID));
            assert_eq!(
                credentials.granted_scopes.unwrap(),
                ["atproto", "transition:generic"]
            );
        } else {
            assert_eq!(result.err().unwrap().to_string(), "Bad token scope");
        }
    }
}

#[tokio::test]
async fn service_auth_uses_labeler_keys_and_rejects_unresolvable_issuer_keys() {
    use rsky_pds::account_manager::helpers::auth::{create_service_jwt, ServiceJwtParams};
    use rsky_pds::auth_verifier::{verify_service_jwt, ServiceJwtOpts};
    let (_dir, client) = test_client().await;
    let key = secp256k1::Keypair::from_secret_key(
        &secp256k1::Secp256k1::new(),
        &secp256k1::SecretKey::from_slice(&[0x42; 32]).unwrap(),
    );
    let did_key = rsky_crypto::utils::encode_did_key(&key.public_key());
    for (issuer, key_type, expected) in [
        (format!("{DID}#atproto_labeler"), "Multikey", None),
        (
            DID.to_owned(),
            "UnrecognizedVerificationMethod",
            Some("missing or bad key"),
        ),
        (
            "did:unsupported:example".into(),
            "Multikey",
            Some("could not resolve iss did"),
        ),
        (String::new(), "Multikey", Some("could not resolve iss did")),
    ] {
        let fragment = if issuer.contains('#') {
            "atproto_label"
        } else {
            "atproto"
        };
        let (url, worker) = directory(json!({"id": DID, "verificationMethod": [{
            "id": format!("{DID}#{fragment}"), "type": key_type, "controller": DID,
            "publicKeyMultibase": did_key.strip_prefix("did:key:").unwrap()
        }], "service": []}))
        .await;
        let resolver = local_resolver(url);
        let token = create_service_jwt(
            ServiceJwtParams {
                iss: issuer.clone(),
                aud: "did:web:recipient.example".into(),
                exp: None,
                lxm: Some("com.atproto.admin.getAccountInfo".into()),
                jti: None,
            },
            &key,
        )
        .await
        .unwrap();
        let request = client
            .get("/xrpc/com.atproto.admin.getAccountInfo")
            .header(Header::new("Authorization", format!("Bearer {token}")));
        let result = verify_service_jwt(
            request.inner(),
            rocket::State::from(&resolver),
            ServiceJwtOpts {
                aud: Some("did:web:recipient.example".into()),
                iss: None,
            },
        )
        .await;
        if let Some(message) = expected {
            assert!(result.err().unwrap().to_string().contains(message));
        } else {
            assert_eq!(result.unwrap().iss, issuer);
        }
        worker.abort();
    }
}

#[tokio::test]
async fn deactivation_detects_an_account_removed_during_the_status_change() {
    let (dir, client) = test_client().await;
    common::oauth::create_active_account(&client).await;
    rusqlite::Connection::open(dir.path().join("account.sqlite")).unwrap().execute_batch(
        "CREATE TRIGGER remove_on_deactivation AFTER UPDATE OF \"deactivatedAt\" ON actor BEGIN DELETE FROM actor WHERE did = NEW.did; END;"
    ).unwrap();
    let result = rsky_pds::apis::com::atproto::server::deactivate_account::deactivate_account_for(
        DID,
        None,
        false,
        client
            .rocket()
            .state::<rsky_pds::SharedSequencer>()
            .unwrap(),
        client.rocket().state::<AccountManager>().unwrap(),
    )
    .await;
    assert!(
        matches!(result, Err(rsky_pds::apis::ApiError::InvalidRequest(message)) if message == "Account not found")
    );
}

#[tokio::test]
async fn account_creation_cleans_up_when_the_actor_lock_cannot_be_opened() {
    use rsky_pds::actor_store::ActorStore;
    const LOCKED_DID: &str = "did:plc:dddddddddddddddddddddddd";
    let (dir, client) = test_client().await;
    let locks = rsky_pds::locks::LockDir::new(dir.path().join("rsky/locks")).unwrap();
    // A directory where the lock file belongs reproduces a real filesystem
    // failure after the actor database has been created, before its transaction.
    std::fs::create_dir(locks.path_for(LOCKED_DID).unwrap()).unwrap();
    let domain = &client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0];
    let response = post(&client, "com.atproto.server.createAccount", &common::get_admin_token(),
        json!({"did": LOCKED_DID, "handle": format!("locked{domain}"), "email": "locked@example.com", "password": "password"})).await;
    assert_eq!(response.status(), Status::InternalServerError);
    assert!(!client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .exists(LOCKED_DID)
        .await
        .unwrap());
    assert!(client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .get_account(
            LOCKED_DID,
            Some(
                rsky_pds::account_manager::helpers::account::AvailabilityFlags {
                    include_deactivated: Some(true),
                    include_taken_down: Some(true)
                }
            )
        )
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn account_creation_cleans_up_when_the_initial_repository_commit_cannot_persist() {
    use rsky_pds::actor_store::ActorStore;
    use std::time::Duration;
    const BROKEN_DID: &str = "did:plc:eeeeeeeeeeeeeeeeeeeeeeee";
    let (dir, client) = test_client().await;
    let actors = client.rocket().state::<ActorStore>().unwrap();
    let locks = rsky_pds::locks::LockDir::new(dir.path().join("rsky/locks")).unwrap();
    let exclusive = locks.try_exclusive(BROKEN_DID).unwrap().unwrap();
    let domain = &client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0];
    let admin = common::get_admin_token();
    let request = post(
        &client,
        "com.atproto.server.createAccount",
        &admin,
        json!({"did": BROKEN_DID, "handle": format!("broken{domain}"), "email": "broken@example.com", "password": "password"}),
    );
    let break_storage = async {
        // Creation has finished before transact registers itself as in flight.
        // The exclusive maintenance lock prevents it reading or committing the
        // newly migrated repository until the deliberate corruption is complete.
        tokio::time::timeout(Duration::from_secs(5), async {
            while actors.inflight_mutations(BROKEN_DID) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let db = rusqlite::Connection::open(actors.get_location(BROKEN_DID).unwrap().db_location)
            .unwrap();
        db.execute_batch("CREATE TRIGGER refuse_initial_root BEFORE INSERT ON repo_root BEGIN SELECT RAISE(ABORT, 'synthetic repository failure'); END;").unwrap();
        drop(db);
        drop(exclusive);
    };
    let (response, ()) = tokio::join!(request, break_storage);
    assert_eq!(response.status(), Status::InternalServerError);
    assert!(!actors.exists(BROKEN_DID).await.unwrap());
    assert!(client
        .rocket()
        .state::<AccountManager>()
        .unwrap()
        .get_account(
            BROKEN_DID,
            Some(
                rsky_pds::account_manager::helpers::account::AvailabilityFlags {
                    include_deactivated: Some(true),
                    include_taken_down: Some(true)
                }
            )
        )
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn failed_account_root_persistence_rolls_back_credentials_and_allows_retry() {
    const RETRY_DID: &str = "did:plc:ffffffffffffffffffffffff";
    let (dir, client) = test_client().await;
    let admin = common::get_admin_token();
    let invite = post(
        &client,
        "com.atproto.server.createInviteCode",
        &admin,
        json!({"useCount": 1}),
    )
    .await;
    assert_eq!(invite.status(), Status::Ok);
    let invite: Value = invite.into_json().await.unwrap();
    let domain = &client
        .rocket()
        .state::<rsky_pds::config::ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains[0];
    let input = json!({"did": RETRY_DID, "handle": format!("retry{domain}"), "email": "retry@example.com", "password": "password", "inviteCode": invite["code"]});
    let db = rusqlite::Connection::open(dir.path().join("account.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_root BEFORE INSERT ON repo_root BEGIN SELECT RAISE(ABORT, 'synthetic account root failure'); END;").unwrap();
    let failed = post(
        &client,
        "com.atproto.server.createAccount",
        &admin,
        input.clone(),
    )
    .await;
    assert_eq!(failed.status(), Status::InternalServerError);
    assert!(!client
        .rocket()
        .state::<rsky_pds::actor_store::ActorStore>()
        .unwrap()
        .exists(RETRY_DID)
        .await
        .unwrap());
    for (table, column) in [
        ("actor", "did"),
        ("account", "did"),
        ("refresh_token", "did"),
        ("repo_root", "did"),
        ("invite_code_use", "usedBy"),
    ] {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE \"{column}\" = ?1"),
                [RETRY_DID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "partial account data in {table}");
    }
    db.execute_batch("DROP TRIGGER refuse_root").unwrap();
    let retried = post(&client, "com.atproto.server.createAccount", &admin, input).await;
    assert_eq!(retried.status(), Status::Ok);
    let retried: Value = retried.into_json().await.unwrap();
    assert_eq!(retried["did"], RETRY_DID);
    assert!(retried["accessJwt"].is_string());
    let uses: i64 = db
        .query_row(
            "SELECT count(*) FROM invite_code_use WHERE code = ?1",
            [invite["code"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(uses, 1);
}

#[tokio::test]
async fn activation_rejects_unsupported_did_methods_forms_and_unavailable_web_documents() {
    let (_dir, client) = test_client().await;
    let actors = client
        .rocket()
        .state::<rsky_pds::actor_store::ActorStore>()
        .unwrap();
    let key = secp256k1::Keypair::from_secret_key(
        &secp256k1::Secp256k1::new(),
        &secp256k1::SecretKey::from_slice(&[43; 32]).unwrap(),
    );
    for (did, expected) in [
        (
            "did:example:aaaaaaaaaaaaaaaaaaaaaaaa",
            Some("Unsupported did method"),
        ),
        (
            "did:web:example.invalid%3A8443",
            Some("Unsupported did:web form"),
        ),
        (
            "did:web:example.invalid%3a8443",
            Some("Unsupported did:web form"),
        ),
        ("did:web:127.0.0.1", None),
        ("did:web:activation-document.invalid", None),
    ] {
        actors.create(did, &key).await.unwrap();
        let error = rsky_pds::apis::com::atproto::server::assert_valid_did_documents_for_service(
            actors,
            did.into(),
        )
        .await
        .unwrap_err();
        if let Some(expected) = expected {
            assert!(error.to_string().contains(expected), "{did}: {error}");
        }
    }
}

#[tokio::test]
async fn space_service_resolution_uses_named_services_and_rejects_missing_endpoints() {
    use rsky_pds::apis::com::atproto::space::{
        register_notify::resolve_subscriber, resolve_space_host_endpoint,
    };
    let (url, worker) = directory(json!({"id":DID,"service":[
        {"id":format!("{DID}#atproto_pds"),"type":"AtprotoPersonalDataServer","serviceEndpoint":"https://pds.example.invalid"},
        {"id":"atproto_space_host","type":"AtprotoSpaceHost","serviceEndpoint":"https://host.example.invalid"},
        {"id":format!("{DID}#atproto_space_syncer"),"type":"AtprotoSpaceSyncer","serviceEndpoint":"https://syncer.example.invalid"},
        {"id":"custom","type":"AtprotoSpaceSyncer","serviceEndpoint":"https://custom.example.invalid"}
    ]})).await;
    assert_eq!(
        resolve_space_host_endpoint(&url, DID).await.unwrap(),
        "https://host.example.invalid"
    );
    assert_eq!(
        resolve_subscriber(&url, DID).await.unwrap(),
        "https://syncer.example.invalid"
    );
    assert_eq!(
        resolve_subscriber(&url, &format!("{DID}#custom"))
            .await
            .unwrap(),
        "https://custom.example.invalid"
    );
    assert!(resolve_subscriber(&url, &format!("{DID}#absent"))
        .await
        .is_err());
    assert!(
        resolve_subscriber(&url, "did:unsupported:aaaaaaaaaaaaaaaaaaaaaaaa")
            .await
            .is_err()
    );
    worker.abort();

    let (url, worker) = directory(json!({"id":DID,"service":[]})).await;
    assert!(resolve_space_host_endpoint(&url, DID).await.is_err());
    assert!(resolve_subscriber(&url, DID).await.is_err());
    worker.abort();
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
