use super::*;

fn issuer_keypair() -> Keypair {
    // fixed secret so the test carries no unseeded randomness
    let secret = secp256k1::SecretKey::from_slice(&[0x17u8; 32]).unwrap();
    Keypair::from_secret_key(&secp256k1::Secp256k1::new(), &secret)
}

/// A token minted by `create_service_jwt` must verify against the mint
/// keypair's did:key — the full round trip other PDSes exercise. This is
/// the test that catches an encode/verify mismatch a claims-only test
/// cannot.
#[tokio::test]
async fn a_malformed_signature_is_rejected_before_issuer_resolution() {
    let mut jwt = token(None);
    jwt.truncate(jwt.rfind('.').unwrap() + 1);
    jwt.push_str("%%%invalid");
    let error = verify_jwt(jwt, Some("did:web:service".into()), None, no_key)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "BadJwt: invalid signature encoding");
}

#[tokio::test]
async fn a_minted_token_verifies_end_to_end() {
    use rsky_crypto::utils::encode_did_key;
    let keypair = issuer_keypair();
    let jwt = create_service_jwt(
        ServiceJwtParams {
            iss: "did:plc:issuer".to_string(),
            aud: "did:web:service".to_string(),
            exp: None,
            lxm: Some("com.atproto.server.createAccount".to_string()),
            jti: None,
        },
        &keypair,
    )
    .await
    .unwrap();
    let did_key = encode_did_key(&keypair.public_key());
    let payload = verify_jwt(
        jwt,
        Some("did:web:service".to_string()),
        Some("com.atproto.server.createAccount"),
        move |_iss, _refresh| {
            let did_key = did_key.clone();
            async move { Ok(did_key) }
        },
    )
    .await
    .unwrap();
    assert_eq!(payload.iss, "did:plc:issuer");
}

/// `iat`/`exp` are microseconds here, matching `verify_jwt`.
fn token(lxm: Option<&str>) -> String {
    let payload = serde_json::json!({
        "iss": "did:plc:issuer",
        "aud": "did:web:service",
        "exp": u64::MAX,
        "lxm": lxm,
    });
    let header = Base64UrlUnpadded::encode_string(br#"{"typ":"JWT","alg":"ES256K"}"#);
    let payload =
        Base64UrlUnpadded::encode_string(serde_json::to_vec(&payload).unwrap().as_slice());
    // A decodable-but-wrong signature, so claim checks decide first and
    // the key fetch is the furthest a passing claim set can reach.
    let sig = Base64UrlUnpadded::encode_string(&[0u8; 64]);
    format!("{header}.{payload}.{sig}")
}

async fn no_key(_iss: String, _refresh: bool) -> Result<String> {
    bail!("the lexicon-method check must decide before a key is fetched")
}

#[tokio::test]
async fn a_bound_token_is_refused_at_another_method() {
    let error = verify_jwt(
        token(Some("com.atproto.repo.createRecord")),
        Some("did:web:service".to_string()),
        Some("com.atproto.repo.deleteRecord"),
        no_key,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("BadJwtLexiconMethod"), "{error}");
}

#[tokio::test]
async fn a_bound_token_is_refused_where_no_method_is_named() {
    let error = verify_jwt(
        token(Some("com.atproto.repo.createRecord")),
        Some("did:web:service".to_string()),
        None,
        no_key,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("BadJwtLexiconMethod"), "{error}");
}

#[tokio::test]
async fn a_matching_binding_passes_the_check() {
    // Reaching the signature is the assertion: the method gate let it by.
    let error = verify_jwt(
        token(Some("com.atproto.repo.createRecord")),
        Some("did:web:service".to_string()),
        Some("com.atproto.repo.createRecord"),
        no_key,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("key is fetched"), "{error}");
}

#[tokio::test]
async fn an_unbound_token_stays_as_broad_as_it_was_issued() {
    let error = verify_jwt(
        token(None),
        Some("did:web:service".to_string()),
        Some("com.atproto.repo.createRecord"),
        no_key,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("key is fetched"), "{error}");
}
