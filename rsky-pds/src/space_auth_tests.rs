use super::*;
use secp256k1::{Secp256k1, SecretKey};

fn keypair() -> Keypair {
    let secp = Secp256k1::new();
    Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[0x55u8; 32]).unwrap())
}

fn space() -> SpaceId {
    SpaceId::new("did:plc:auth", "com.example.forum", "self")
}

fn session(granted: Option<&[&str]>) -> Credentials {
    Credentials {
        r#type: "oauth".to_string(),
        granted_scopes: granted.map(|scopes| scopes.iter().map(|s| (*s).to_string()).collect()),
        did: Some("did:plc:member".to_string()),
        scope: Some(AuthScope::Access),
        audience: None,
        token_id: None,
        aud: None,
        iss: None,
        is_privileged: None,
    }
}

#[tokio::test]
async fn proof_issuance_requires_the_server_public_url() {
    let client = rocket::local::asynchronous::Client::untracked(
        rocket::build().manage(SharedSpaceDpop::default()),
    )
    .await
    .unwrap();
    let request = client.get("/xrpc/com.atproto.space.getCredentials");
    let error = verify_issuance_proof(request.inner()).await.unwrap_err();
    assert_eq!(error.to_string(), "server config is not available");
}

#[tokio::test]
async fn a_space_credential_requires_dpop_even_before_claims_are_decoded() {
    let client = rocket::local::asynchronous::Client::untracked(rocket::build())
        .await
        .unwrap();
    let request =
        client
            .get("/xrpc/com.atproto.space.getRecord")
            .header(rocket::http::Header::new(
                "Authorization",
                "Bearer synthetic-credential",
            ));
    let error = verify_space_credential_token(request.inner(), "synthetic-credential")
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("DPoP scheme"));
}

#[tokio::test]
async fn delegation_tokens_are_not_space_credentials() {
    let client = rocket::local::asynchronous::Client::untracked(rocket::build())
        .await
        .unwrap();
    let space = SpaceId::new(
        "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa",
        "com.example.forum",
        "self",
    );
    let token =
        mint_delegation_token(&keypair(), "did:plc:bbbbbbbbbbbbbbbbbbbbbbbb", &space).unwrap();
    let request =
        client
            .get("/xrpc/com.atproto.space.getRecord")
            .header(rocket::http::Header::new(
                "Authorization",
                format!("DPoP {token}"),
            ));
    let error = verify_space_credential_token(request.inner(), &token)
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "not a space credential");
}

#[test]
fn owning_a_repo_does_not_override_the_sessions_space_grant() {
    let did = "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa";
    let auth = SpaceReadAuth::Session {
        did: did.into(),
        credentials: session(Some(&[
            "atproto",
            "space:com.example.other?action=read_self",
        ])),
    };
    assert!(matches!(
        authorize_space_read(
            &auth,
            &space(),
            did,
            &SpaceRequest::ReadSelf { collection: None }
        ),
        Err(ApiError::AuthRequiredError(_))
    ));
}

#[test]
fn a_session_that_never_spoke_the_grammar_is_not_narrowed_by_it() {
    // App passwords and legacy access tokens: route-level ownership checks
    // are what constrain them, as before.
    assert!(session_space_scopes(&session(None)).is_none());
    // So is an OAuth session that asked for the app-password model.
    assert!(session_space_scopes(&session(Some(&["atproto", "transition:generic"]))).is_none());
    assert!(session_permits(
        &session(None),
        "did:plc:member",
        &space(),
        &SpaceRequest::Read
    ));
}

#[test]
fn a_scoped_session_gets_exactly_the_space_access_it_asked_for() {
    let creds = session(Some(&[
        "atproto",
        "space:com.example.forum?authority=did:plc:auth&skey=self&action=read_self",
    ]));
    assert_eq!(session_space_scopes(&creds).unwrap().len(), 1);
    assert!(session_permits(
        &creds,
        "did:plc:member",
        &space(),
        &SpaceRequest::ReadSelf { collection: None }
    ));
    // Reading the whole space is a different grant, and was not given.
    assert!(!session_permits(
        &creds,
        "did:plc:member",
        &space(),
        &SpaceRequest::Read
    ));
}

/// Bulleted's production scope string, verbatim from
/// `https://bulleted.app/oauth-client-metadata.json`.
const BULLETED_SCOPE: &str = "atproto include:app.bulleted.authFull blob:image/* \
     include:app.bulleted.spaceAccess \
     space:app.bulleted.space?manage=create&manage=update&manage=delete&action=read_self";

/// What the inline `space:` grant confers on its own, before the permission
/// set beside it is resolved.
///
/// This is the shape that shipped broken: `authority` defaults to `self`,
/// so the inline grant covers only spaces the user anchors, and every
/// operation on a shared space is denied. The fix is resolution, not a
/// looser default -- so this test pins the unresolved behaviour to prove
/// the resolution is doing the work.
#[test]
fn the_inline_grant_alone_does_not_reach_a_shared_space() {
    let granted: Vec<&str> = BULLETED_SCOPE.split_ascii_whitespace().collect();
    let creds = session(Some(&granted));
    let shared = SpaceId::new("did:plc:someoneelse", "app.bulleted.space", "main");
    for request in [
        SpaceRequest::ReadSelf { collection: None },
        SpaceRequest::Read,
        SpaceRequest::Write {
            action: space_scope::SpaceAction::Create,
            collection: "app.bulleted.node".to_string(),
        },
    ] {
        assert!(
            !session_permits(&creds, "did:plc:member", &shared, &request),
            "{request:?} should need the permission set"
        );
    }
    // Its own spaces it can manage, which is all the inline grant claims.
    let own = SpaceId::new("did:plc:member", "app.bulleted.space", "main");
    assert!(session_permits(
        &creds,
        "did:plc:member",
        &own,
        &SpaceRequest::Manage(space_scope::ManageOp::Create)
    ));
}

/// The same session once `include:app.bulleted.spaceAccess` is resolved.
///
/// The expansion is what `permission_set::expand_includes` produces from
/// the published record; this test asserts the enforcement seam accepts it,
/// so the two halves cannot drift apart.
#[test]
fn the_resolved_permission_set_reaches_a_shared_space() {
    let mut granted: Vec<String> = BULLETED_SCOPE
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    granted.push(
        "space:app.bulleted.space?authority=*\
         &collection=app.bulleted.node&collection=app.bulleted.note\
         &collection=app.bulleted.outline&collection=app.bulleted.mirror\
         &collection=app.bulleted.comment&collection=app.bulleted.commentPolicy\
         &action=read&action=create&action=update&action=delete"
            .to_string(),
    );
    let refs: Vec<&str> = granted.iter().map(String::as_str).collect();
    let creds = session(Some(&refs));
    let shared = SpaceId::new("did:plc:someoneelse", "app.bulleted.space", "main");

    assert!(session_permits(
        &creds,
        "did:plc:member",
        &shared,
        &SpaceRequest::ReadSelf { collection: None }
    ));
    assert!(session_permits(
        &creds,
        "did:plc:member",
        &shared,
        &SpaceRequest::Read
    ));
    assert!(session_permits(
        &creds,
        "did:plc:member",
        &shared,
        &SpaceRequest::Write {
            action: space_scope::SpaceAction::Create,
            collection: "app.bulleted.node".to_string(),
        }
    ));
    // The set names its collections, so it does not confer others.
    assert!(!session_permits(
        &creds,
        "did:plc:member",
        &shared,
        &SpaceRequest::Write {
            action: space_scope::SpaceAction::Create,
            collection: "com.example.something".to_string(),
        }
    ));
}

#[test]
fn a_scoped_session_with_no_space_grant_is_denied() {
    // Including one whose space access would come from an `include:` set:
    // an unresolved permission set confers nothing, not everything.
    let creds = session(Some(&["atproto", "include:app.example.spaceAccess"]));
    assert_eq!(session_space_scopes(&creds).unwrap().len(), 0);
    assert!(!session_permits(
        &creds,
        "did:plc:member",
        &space(),
        &SpaceRequest::ReadSelf { collection: None }
    ));
}

#[test]
fn an_unparseable_space_grant_confers_nothing_rather_than_failing_the_session() {
    let creds = session(Some(&["atproto", "space:not a space type?action=bogus"]));
    assert_eq!(session_space_scopes(&creds).unwrap().len(), 0);
}

#[test]
fn delegation_token_verifies_against_the_account_key() {
    let keypair = keypair();
    let did_key = encode_did_key(&keypair.public_key());
    let space = space();
    let jwt = mint_delegation_token(&keypair, "did:plc:user", &space).unwrap();
    assert_eq!(jwt_typ(&jwt).as_deref(), Some(DELEGATION_TYP));
    let iss = credential::verify_delegation_token(
        &jwt,
        &space.uri(),
        "did:plc:auth",
        &did_key,
        now_secs(),
    )
    .unwrap();
    assert_eq!(iss, "did:plc:user");
    // jtis are random per token
    let second = mint_delegation_token(&keypair, "did:plc:user", &space).unwrap();
    let a = credential::decode(&jwt).unwrap().claims.jti;
    let b = credential::decode(&second).unwrap().claims.jti;
    assert_ne!(a, b);
}

#[test]
fn jwt_typ_reads_the_header() {
    assert_eq!(jwt_typ("garbage"), None);
    assert_eq!(jwt_typ("bm90anNvbg.x.y"), None);
    let jwt = mint_space_service_token(&keypair(), "did:plc:a", "did:plc:b", NOTIFY_WRITE_LXM);
    assert_eq!(jwt_typ(&jwt).as_deref(), Some("JWT"));
}

#[tokio::test]
async fn service_token_roundtrip_and_rejections() {
    // A local-account issuer resolves through the actor store.
    let dir = tempfile::tempdir().unwrap();
    let lifecycle =
        crate::lifecycle::LifecycleStore::open(dir.path().join("rsky/lifecycle.sqlite"))
            .await
            .unwrap();
    let actor_store = ActorStore::new(
        &crate::config::ActorStoreConfig {
            directory: dir.path().join("actors").to_str().unwrap().to_string(),
            cache_size: 10,
        },
        crate::background::BackgroundQueue::default(),
        lifecycle,
    );
    let keypair = keypair();
    actor_store
        .create("did:plc:writer", &keypair)
        .await
        .unwrap();
    let id_resolver = SharedIdResolver {
        id_resolver: tokio::sync::RwLock::new(rsky_identity::IdResolver::new(
            rsky_identity::types::IdentityResolverOpts {
                timeout: None,
                plc_url: Some("http://127.0.0.1:1".to_string()),
                did_cache: None,
                backup_nameservers: None,
            },
        )),
    };

    let token = mint_space_service_token(
        &keypair,
        "did:plc:writer",
        "did:plc:auth#atproto_space_host",
        NOTIFY_WRITE_LXM,
    );
    let claims = verify_space_service_token(
        &actor_store,
        &id_resolver,
        &token,
        NOTIFY_WRITE_LXM,
        "did:plc:auth",
    )
    .await
    .unwrap();
    assert_eq!(claims.iss, "did:plc:writer");
    assert_eq!(claims.aud, "did:plc:auth#atproto_space_host");

    // wrong lxm
    let err = verify_space_service_token(
        &actor_store,
        &id_resolver,
        &token,
        NOTIFY_SPACE_DELETED_LXM,
        "did:plc:auth",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("bad lxm"));

    // malformed
    assert!(verify_space_service_token(
        &actor_store,
        &id_resolver,
        "a.b",
        NOTIFY_WRITE_LXM,
        "did:plc:auth"
    )
    .await
    .is_err());

    // expired
    let expired_claims = SpaceServiceClaims {
        iss: "did:plc:writer".to_string(),
        aud: "did:plc:auth".to_string(),
        iat: 900,
        exp: 1000,
        lxm: NOTIFY_WRITE_LXM.to_string(),
        jti: "expired".to_string(),
    };
    let header = serde_json::json!({"typ": "JWT", "alg": "ES256K"});
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&expired_claims).unwrap())
    );
    let sig = sign_with_keypair(&keypair, signing_input.as_bytes());
    let expired = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig));
    let err = verify_space_service_token(
        &actor_store,
        &id_resolver,
        &expired,
        NOTIFY_WRITE_LXM,
        "did:plc:auth",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("expired"));

    // tampered signature
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    let mut sig = URL_SAFE_NO_PAD.decode(&parts[2]).unwrap();
    sig[0] ^= 0xFF;
    parts[2] = URL_SAFE_NO_PAD.encode(sig);
    assert!(verify_space_service_token(
        &actor_store,
        &id_resolver,
        &parts.join("."),
        NOTIFY_WRITE_LXM,
        "did:plc:auth",
    )
    .await
    .is_err());

    // unknown issuer: resolution fails (unreachable plc)
    let other = mint_space_service_token(
        &keypair,
        "did:plc:unknownissuer",
        "did:plc:auth",
        NOTIFY_WRITE_LXM,
    );
    assert!(verify_space_service_token(
        &actor_store,
        &id_resolver,
        &other,
        NOTIFY_WRITE_LXM,
        "did:plc:auth"
    )
    .await
    .is_err());

    // local key resolution shortcut yields the account key
    let did_key =
        resolve_signing_did_key(&actor_store, &id_resolver, "did:plc:writer", SPACE_KEY_IDS)
            .await
            .unwrap();
    assert_eq!(did_key, encode_did_key(&keypair.public_key()));
}
