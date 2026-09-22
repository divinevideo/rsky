//! Auth for permissioned-data routes: space-credential verification, the
//! session-scope seam, delegation-token minting, and the actor-key service
//! tokens used for write notifications.
//!
//! ## A7 seam
//!
//! The OAuth provider track (A7) will carry `space:` scope strings on
//! sessions. Until it lands, sessions minted by the legacy token path carry no
//! space scopes, so [`session_space_scopes`] returns `None` and
//! [`session_permits`] falls back to treating a full-access session as the
//! broadest grant. Route-level ownership checks still constrain reads to the
//! caller's own repo and writes to the caller's own authorship, which matches
//! the spec's session-side surface (whole-space reads always require a space
//! credential). When A7 merges, only [`session_space_scopes`] needs to parse
//! the session's scope carrier; every route already evaluates through
//! [`session_permits`].

use crate::actor_store::ActorStore;
use crate::apis::ApiError;
use crate::auth_verifier::{
    bearer_token_from_req, dpop_token_from_req, validate_access_token, AuthScope, Credentials,
};
use crate::space_scope::{self, SpaceRequest, SpaceScope};
use crate::SharedIdResolver;
use anyhow::{bail, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome, Request};
use rocket::State;
use rsky_common::get_random_str;
use rsky_common::get_verification_material;
use rsky_crypto::utils::encode_did_key;
use rsky_identity::did::atproto_data::get_did_key_from_multibase;
use rsky_oauth::dpop::{DpopManager, InMemoryReplayStore};
use rsky_space::credential::{
    self, JwtHeader, SpaceClaims, CREDENTIAL_TYP, DELEGATION_TTL_SECS, DELEGATION_TYP,
};
use rsky_space::space_id::SpaceId;
use secp256k1::{Keypair, Message};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::SystemTime;

/// DPoP verification for the space surface.
///
/// Its own manager rather than the OAuth provider's: space credentials are not
/// OAuth tokens, and issuance does not challenge with nonces, so a proof
/// arrives without one. The replay store is what makes a proof single-use.
pub struct SharedSpaceDpop {
    pub dpop: DpopManager,
}

impl Default for SharedSpaceDpop {
    fn default() -> Self {
        Self {
            dpop: DpopManager::new(None, Box::new(InMemoryReplayStore::default())),
        }
    }
}

fn dpop_request_uri(req: &Request) -> Result<String> {
    let Some(cfg) = req.rocket().state::<crate::config::ServerConfig>() else {
        bail!("server config is not available")
    };
    Ok(format!("{}{}", cfg.service.public_url, req.uri()))
}

async fn check_space_proof(
    req: &Request<'_>,
    access_token: Option<&str>,
) -> Result<rsky_oauth::dpop::DpopProof> {
    let shared = req
        .guard::<&State<SharedSpaceDpop>>()
        .await
        .expect("SharedSpaceDpop managed");
    let uri = dpop_request_uri(req)?;
    let headers: Vec<String> = req.headers().get("dpop").map(String::from).collect();
    let refs: Vec<&str> = headers.iter().map(String::as_str).collect();
    let proof = shared
        .dpop
        .check_proof(
            &rsky_oauth::dpop::DpopRequest {
                method: req.method().as_str(),
                uri: &uri,
                dpop_headers: &refs,
                access_token,
            },
            None,
            now_secs(),
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    proof.ok_or_else(|| anyhow::anyhow!("missing DPoP proof"))
}

/// The thumbprint a credential minted for this request must be bound to.
///
/// Taken from the verified proof's own key, never from a request field: a
/// field is an assertion anyone holding a delegation token can make about a
/// key someone else controls. The proof carries no `ath`, because a delegation
/// token is a grant rather than an access token.
pub async fn verify_issuance_proof(req: &Request<'_>) -> Result<String> {
    Ok(check_space_proof(req, None).await?.jkt)
}

/// Confirm the presenter holds the key the credential is bound to.
pub async fn verify_bound_proof(
    req: &Request<'_>,
    credential: &str,
    bound_jkt: &str,
) -> Result<()> {
    let proof = check_space_proof(req, Some(credential)).await?;
    if proof.jkt != bound_jkt {
        bail!("DPoP key thumbprint does not match the credential binding");
    }
    Ok(())
}

pub const NOTIFY_WRITE_LXM: &str = "com.atproto.space.notifyWrite";
pub const NOTIFY_SPACE_DELETED_LXM: &str = "com.atproto.space.notifySpaceDeleted";
pub const SPACE_SERVICE_TOKEN_TTL_SECS: u64 = 60;
/// Allowance for clock drift between the issuer and this PDS when checking
/// `iat`/`exp` on inbound service tokens.
pub const SPACE_SERVICE_CLOCK_SKEW_SECS: u64 = 30;

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("time after unix epoch")
        .as_secs()
}

/// Sign bytes the atproto way: sha256, ECDSA, low-S, compact `r||s`.
pub fn sign_with_keypair(keypair: &Keypair, input: &[u8]) -> Vec<u8> {
    let digest = Sha256::digest(input);
    let message = Message::from_digest(digest.into());
    let mut sig = keypair.secret_key().sign_ecdsa(message);
    sig.normalize_s();
    sig.serialize_compact().to_vec()
}

/// Resolve a DID's atproto signing key as a `did:key`. Accounts hosted here
/// are answered from the local actor store; remote DIDs resolve through the
/// id resolver's DID document, preferring the fragments in `key_ids` order.
pub async fn resolve_signing_did_key(
    actor_store: &ActorStore,
    id_resolver: &SharedIdResolver,
    did: &str,
    key_ids: &[&str],
) -> Result<String> {
    if actor_store.exists(did).await.unwrap_or(false) {
        let keypair = actor_store.keypair(did).await?;
        return Ok(encode_did_key(&keypair.public_key()));
    }
    let did_doc = {
        let lock = id_resolver.id_resolver.read().await;
        lock.did.ensure_resolve(&did.to_string(), None).await?
    };
    for key_id in key_ids {
        if let Some(material) = get_verification_material(&did_doc, key_id) {
            if let Some(did_key) = get_did_key_from_multibase(material)? {
                return Ok(did_key);
            }
        }
    }
    bail!("no usable signing key in DID document for {did}")
}

/// Signing-key fragments for verifying space credentials: `#atproto_space`
/// with `#atproto` fallback (spec §Space authority).
pub const SPACE_KEY_IDS: &[&str] = &["atproto_space", "atproto"];
/// Delegation tokens are always signed by the account `#atproto` key.
pub const ATPROTO_KEY_IDS: &[&str] = &["atproto"];

fn jwt_typ(token: &str) -> Option<String> {
    let header_b64 = token.split('.').next()?;
    let header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_b64).ok()?).ok()?;
    header
        .get("typ")
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

/// A verified `atproto-space-credential+jwt` presented as a bearer token: the
/// space URI (`sub`), whose authority (`iss`) signed it.
pub struct SpaceCredentialAuth {
    pub space_uri: String,
    pub authority: String,
}

async fn verify_space_credential_token(
    req: &Request<'_>,
    token: &str,
) -> Result<SpaceCredentialAuth> {
    // Scheme discipline: a credential reads every repo in its space, so
    // presenting one as a bearer token makes it a shared secret. `Bearer` is
    // refused even with a valid proof beside it.
    if dpop_token_from_req(req).as_deref() != Some(token) {
        bail!("space credentials must be presented under the DPoP scheme");
    }
    let decoded = credential::decode(token).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if decoded.header.typ != CREDENTIAL_TYP {
        bail!("not a space credential");
    }
    let space_uri = decoded.claims.sub.clone();
    let authority = decoded.claims.iss.clone();
    let space = SpaceId::parse(&space_uri).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if space.authority != authority {
        bail!("credential issuer is not the space authority");
    }
    let actor_store = req
        .guard::<&State<ActorStore>>()
        .await
        .expect("ActorStore managed");
    let id_resolver = req
        .guard::<&State<SharedIdResolver>>()
        .await
        .expect("SharedIdResolver managed");
    let did_key =
        resolve_signing_did_key(actor_store, id_resolver, &authority, SPACE_KEY_IDS).await?;
    let bound_jkt =
        credential::verify_space_credential(token, &space_uri, &authority, &did_key, now_secs())
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    verify_bound_proof(req, token, &bound_jkt).await?;
    Ok(SpaceCredentialAuth {
        space_uri,
        authority,
    })
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SpaceCredentialAuth {
    type Error = ApiError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let token = match dpop_token_from_req(req) {
            Some(token) => token,
            None => {
                let error = ApiError::AuthRequiredError("space credential required".to_string());
                req.local_cache(|| Some(error.clone()));
                return Outcome::Error((Status::Unauthorized, error));
            }
        };
        match verify_space_credential_token(req, &token).await {
            Ok(auth) => Outcome::Success(auth),
            Err(error) => {
                tracing::debug!(%error, "space credential rejected");
                let error = ApiError::InvalidToken("Token is invalid".to_string());
                req.local_cache(|| Some(error.clone()));
                Outcome::Error((Status::Unauthorized, error))
            }
        }
    }
}

/// Read/sync methods accept either a covering OAuth session or a valid space
/// credential (spec §Read access).
pub enum SpaceReadAuth {
    Session {
        did: String,
        credentials: Credentials,
    },
    Credential(SpaceCredentialAuth),
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SpaceReadAuth {
    type Error = ApiError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let token = match dpop_token_from_req(req).or(bearer_token_from_req(req).ok().flatten()) {
            Some(token) => token,
            None => {
                let error = ApiError::AuthRequiredError("authentication required".to_string());
                req.local_cache(|| Some(error.clone()));
                return Outcome::Error((Status::Unauthorized, error));
            }
        };
        if jwt_typ(&token).as_deref() == Some(CREDENTIAL_TYP) {
            return match verify_space_credential_token(req, &token).await {
                Ok(auth) => Outcome::Success(SpaceReadAuth::Credential(auth)),
                Err(error) => {
                    tracing::debug!(%error, "space credential rejected");
                    let error = ApiError::InvalidToken("Token is invalid".to_string());
                    req.local_cache(|| Some(error.clone()));
                    Outcome::Error((Status::Unauthorized, error))
                }
            };
        }
        match validate_access_token(
            req,
            vec![
                AuthScope::Access,
                AuthScope::AppPass,
                AuthScope::AppPassPrivileged,
            ],
            None,
        )
        .await
        {
            Ok(access) => {
                let credentials = access.credentials.expect("credentials populated");
                let did = credentials.did.clone().expect("did populated");
                Outcome::Success(SpaceReadAuth::Session { did, credentials })
            }
            Err(error) => {
                let error = ApiError::InvalidRequest(error.to_string());
                req.local_cache(|| Some(error.clone()));
                Outcome::Error((Status::BadRequest, error))
            }
        }
    }
}

/// The `space:` grants a session carries, parsed.
///
/// `None` means the session has no scope grammar to evaluate at all: an app
/// password or a legacy access token, which predate the model and are governed
/// by route-level ownership checks alone. `Some(vec![])` is different -- a
/// scoped session that was granted no space access, which is a denial.
///
/// A grant that arrives inside an `include:` permission set is not seen here:
/// resolving a permission set means fetching its `com.atproto.lexicon.schema`
/// record, which this function does not do. An unresolved set therefore
/// confers nothing rather than everything, which is the safe direction to be
/// wrong in.
pub fn session_space_scopes(credentials: &Credentials) -> Option<Vec<SpaceScope>> {
    let granted = credentials.granted_scopes.as_ref()?;
    let scopes = crate::oauth_scope::GrantedScopes::parse(granted);
    // A `transition:*` session is asking for the app-password model, so it is
    // governed the way an app password is rather than by a scope grammar it
    // never spoke.
    if scopes.has_transition("generic") || scopes.has_transition("chat.bsky") {
        return None;
    }
    Some(
        scopes
            .space_grants()
            .iter()
            .filter_map(|grant| match SpaceScope::parse(grant) {
                Ok(scope) => Some(scope),
                Err(error) => {
                    tracing::warn!(%grant, %error, "ignoring unparseable space scope");
                    None
                }
            })
            .collect(),
    )
}

/// Evaluate a session against a space request. When the session speaks the
/// scope grammar its `space:` grants are authoritative -- including when it
/// has none, which denies. Sessions that predate the grammar fall back to
/// full-access semantics, where route-level ownership checks still apply.
pub fn session_permits(
    credentials: &Credentials,
    session_did: &str,
    space: &SpaceId,
    request: &SpaceRequest,
) -> bool {
    match session_space_scopes(credentials) {
        Some(scopes) => space_scope::authorize(
            &scopes,
            session_did,
            &space.authority,
            &space.space_type,
            &space.skey,
            request,
        ),
        None => true,
    }
}

/// Authorize a read/sync request against a repo in a space. A credential
/// grants whole-space access; a session reads only the holder's own repo.
pub fn authorize_space_read(
    auth: &SpaceReadAuth,
    space: &SpaceId,
    repo_did: &str,
    request: &SpaceRequest,
) -> Result<(), ApiError> {
    match auth {
        SpaceReadAuth::Credential(credential) => {
            if credential.space_uri == space.uri() {
                Ok(())
            } else {
                Err(ApiError::InvalidToken("Token is invalid".to_string()))
            }
        }
        SpaceReadAuth::Session { did, credentials } => {
            if did != repo_did {
                return Err(ApiError::AuthRequiredError(
                    "a space credential is required to read another account's repo".to_string(),
                ));
            }
            if session_permits(credentials, did, space, request) {
                Ok(())
            } else {
                Err(ApiError::AuthRequiredError(
                    "session does not cover this space".to_string(),
                ))
            }
        }
    }
}

/// Mint a delegation token: `typ atproto-space-delegation+jwt`, `kid #atproto`,
/// `iss` the user, `sub` the space, `aud` the authority's space host, 60s
/// expiry, random `jti`, signed with the account's signing key.
pub fn mint_delegation_token(keypair: &Keypair, user_did: &str, space: &SpaceId) -> Result<String> {
    let now = now_secs();
    let header = JwtHeader {
        typ: DELEGATION_TYP.to_string(),
        alg: rsky_crypto::constants::SECP256K1_JWT_ALG.to_string(),
        kid: Some("#atproto".to_string()),
    };
    let claims = SpaceClaims {
        iss: user_did.to_string(),
        sub: space.uri(),
        aud: Some(format!("{}#atproto_space_host", space.authority)),
        iat: now,
        exp: now + DELEGATION_TTL_SECS,
        jti: get_random_str(),
        cnf: None,
    };
    credential::encode(&header, &claims, |input| {
        Ok(sign_with_keypair(keypair, input))
    })
    .map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// Service-auth claims for space write notifications (spec §Write
/// notifications): `iss` the writing account, `aud` the receiving identity,
/// short-lived, method-bound. Times are unix seconds per the proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceServiceClaims {
    pub iss: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub lxm: String,
    pub jti: String,
}

/// Mint a service token signed by the actor's own signing key.
pub fn mint_space_service_token(keypair: &Keypair, iss: &str, aud: &str, lxm: &str) -> String {
    let header =
        serde_json::json!({"typ": "JWT", "alg": rsky_crypto::constants::SECP256K1_JWT_ALG});
    let now = now_secs();
    // These claims contain only strings and integers, so JSON rendering and
    // signing with the validated keypair cannot fail.
    let claims = serde_json::json!({
        "iss": iss,
        "aud": aud,
        "iat": now,
        "exp": now + SPACE_SERVICE_TOKEN_TTL_SECS,
        "lxm": lxm,
        "jti": get_random_str(),
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let sig = sign_with_keypair(keypair, signing_input.as_bytes());
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig))
}

/// Verify an inbound space service token: expiry, `lxm` binding, and the
/// issuer's signature (local accounts answered from the actor store, remote
/// issuers from their DID document `#atproto` key). Returns the claims; the
/// caller checks `iss`/`aud` against the request semantics.
pub async fn verify_space_service_token(
    actor_store: &ActorStore,
    id_resolver: &SharedIdResolver,
    token: &str,
    expected_lxm: &str,
    expected_aud: &str,
) -> Result<SpaceServiceClaims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        bail!("poorly formatted jwt");
    }
    let claims: SpaceServiceClaims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1])?)?;
    let now = now_secs();
    if claims.exp <= now {
        bail!("jwt expired");
    }
    // iat must be present and recent: not from the future (small skew) and
    // not older than the token's own lifetime (XRPC service-auth semantics).
    if claims.iat > now + SPACE_SERVICE_CLOCK_SKEW_SECS {
        bail!("jwt iat in the future");
    }
    if now.saturating_sub(claims.iat) > SPACE_SERVICE_TOKEN_TTL_SECS + SPACE_SERVICE_CLOCK_SKEW_SECS
    {
        bail!("jwt iat too old");
    }
    // aud must name the recipient (recipient audience validation): the bare
    // DID of the token audience must be the recipient this PDS acts as, or
    // any account this PDS hosts (broadcast notifications name a specific
    // local subscriber rather than the caller's expected recipient).
    let aud_did = claims.aud.split('#').next().unwrap_or(&claims.aud);
    if aud_did != expected_aud && !actor_store.exists(aud_did).await.unwrap_or(false) {
        bail!("bad jwt audience");
    }
    if claims.lxm != expected_lxm {
        bail!("bad lxm: expected {expected_lxm}");
    }
    let iss_did = claims.iss.split('#').next().unwrap_or(&claims.iss);
    let did_key =
        resolve_signing_did_key(actor_store, id_resolver, iss_did, ATPROTO_KEY_IDS).await?;
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let digest = Sha256::digest(signing_input.as_bytes());
    let sig = URL_SAFE_NO_PAD.decode(parts[2])?;
    let valid = rsky_crypto::verify::verify_signature_digest(&did_key, &digest, &sig, None)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if !valid {
        bail!("bad jwt signature");
    }
    Ok(claims)
}

#[cfg(test)]
#[path = "space_auth_tests.rs"]
mod tests;
