use crate::account_manager::helpers::auth::{create_service_jwt, ServiceJwtParams};
use crate::actor_store::ActorStore;
use crate::apis::ApiError;
use crate::auth_verifier::scope::{NoScopeRequired, Scoped};
use crate::metrics::{record_service_token_denied, record_service_token_issued};
use crate::oauth_scope::GrantedScopes;
use crate::pipethrough::{PRIVILEGED_METHODS, PROTECTED_METHODS};
use anyhow::{bail, Result};
use rocket::serde::json::Json;
use rocket::State;
use rsky_common::time::{HOUR, MINUTE};
use rsky_lexicon::com::atproto::server::GetServiceAuthOutput;
use rsky_syntax::did::ensure_valid_did;
use std::time::SystemTime;

/// `exp` is a JWT NumericDate in seconds. Compare the value that will be
/// signed, without converting a client-supplied `u64` through microseconds.
fn check_service_auth_exp(exp: u64, now_secs: u64, has_lxm: bool) -> Result<()> {
    if exp <= now_secs {
        bail!("BadExpiration: expiration is in past");
    }
    // The shared time constants are milliseconds.
    let diff_secs = exp - now_secs;
    if diff_secs > (HOUR / 1000) as u64 {
        bail!("BadExpiration: cannot request a token with an expiration more than an hour in the future");
    }
    if !has_lxm && diff_secs > (MINUTE / 1000) as u64 {
        bail!("BadExpiration: cannot request a method-less token with an expiration more than a minute in the future");
    }
    Ok(())
}

/// Denies a request for a service-auth token when the *requested* method
/// (`lxm`) is privileged (the `chat.bsky.*` surface plus
/// `com.atproto.server.createAccount`) and the *caller's own* session is not
/// itself privileged.
///
/// Mirrors the upstream TS reference (`getServiceAuth.ts`):
/// `lxm != null && PRIVILEGED_METHODS.has(lxm) && !isAccessPrivileged(scope)`.
///
/// This is intentionally the inverse of a naive "gate privileged sessions"
/// check: a plain (non-privileged) app-password session must never be able
/// to mint a token for a privileged method such as
/// `chat.bsky.convo.getMessages`, while a fully-privileged session (full
/// `Access` or `AppPassPrivileged`) must be allowed to request a token for
/// any method, privileged or not.
fn ensure_lxm_access(lxm: &str, is_privileged: bool) -> Result<()> {
    if PRIVILEGED_METHODS.contains(lxm) && !is_privileged {
        bail!(
            "insufficient access to request a service auth token for the following method: {lxm}"
        );
    }
    Ok(())
}

/// Denies an OAuth session a token its grants do not cover. A session
/// without granted scopes (app password, legacy login) is governed by the
/// privilege check alone. `transition:generic` covers every method outside
/// `chat.bsky.*`, `transition:chat.bsky` covers that surface, and a granular
/// session needs a matching `rpc:` grant (upstream `allowsRpc`).
fn ensure_rpc_grant(granted: Option<&[String]>, aud: &str, lxm: Option<&str>) -> Result<()> {
    let Some(granted) = granted else {
        return Ok(());
    };
    let lxm = lxm.unwrap_or("*");
    let is_chat = lxm.starts_with("chat.bsky.");
    let scopes = GrantedScopes::parse(granted);
    let allowed = (scopes.has_transition("generic") && !is_chat)
        || (scopes.has_transition("chat.bsky") && is_chat)
        || scopes.allows_rpc(lxm, aud);
    if !allowed {
        bail!("insufficient access to request a service auth token for {lxm} at {aud}");
    }
    Ok(())
}

/// Validates that `aud` is a syntactically valid atproto DID, optionally
/// followed by a `#serviceId` fragment (e.g. `did:web:example.com#atproto_labeler`),
/// matching the upstream check `isAtprotoDid(aud) || isAtprotoDidRefAbsolute(aud)`.
fn ensure_valid_aud(aud: &str) -> Result<()> {
    let did_part = aud.split('#').next().unwrap_or(aud);
    ensure_valid_did(did_part)
        .map_err(|_| anyhow::anyhow!("aud must be a valid atproto DID or did#serviceId reference"))
}

pub async fn inner_get_service_auth(
    aud: String,
    exp: Option<u64>,
    lxm: Option<String>,
    auth: Scoped<NoScopeRequired>,
    actor_store: &State<ActorStore>,
) -> Result<String> {
    let credentials = auth.credentials().await?.clone().unwrap();
    let did = credentials.clone().did.unwrap();
    ensure_valid_aud(&aud)?;
    // `exp` is seconds since the epoch (RFC 7519 §4.1.4).
    if let Some(exp) = exp {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("timestamp since UNIX epoch")
            .as_secs();
        check_service_auth_exp(exp, now_secs, lxm.is_some())?;
    }
    if let Some(ref lxm) = lxm {
        if PROTECTED_METHODS.contains(lxm.as_str()) {
            bail!("cannot request a service auth token for the following protected method: {lxm}");
        }
        ensure_lxm_access(lxm.as_str(), credentials.is_privileged.unwrap_or(false))?;
    }
    ensure_rpc_grant(credentials.granted_scopes.as_deref(), &aud, lxm.as_deref())?;
    let keypair = actor_store.keypair(&did).await?;
    create_service_jwt(
        ServiceJwtParams {
            iss: did,
            aud,
            exp,
            lxm,
            jti: None,
        },
        &keypair,
    )
    .await
}

/// Classifies a denial from [`inner_get_service_auth`] into a small,
/// low-cardinality reason label for [`record_service_token_denied`].
///
/// Matches on the `bail!` message text rather than a typed error, since
/// `inner_get_service_auth` returns a plain `anyhow::Result`; the messages
/// matched here are exactly the ones raised above, so this stays in sync by
/// construction as long as both live in this file. Falls back to
/// `"internal_error"` for anything unrecognized (e.g. a `keypair`/JWT
/// signing failure) rather than growing an ever-expanding label set.
fn deny_reason(err: &anyhow::Error) -> &'static str {
    let msg = err.to_string();
    if msg.starts_with("BadExpiration") {
        "bad_expiration"
    } else if msg.contains("protected method") {
        "protected_method"
    } else if msg.contains("insufficient access") {
        "insufficient_privilege"
    } else {
        "internal_error"
    }
}

/// Get a signed token on behalf of the requesting DID for the requested service.
#[tracing::instrument(skip_all)]
#[rocket::get("/xrpc/com.atproto.server.getServiceAuth?<aud>&<exp>&<lxm>")]
pub async fn get_service_auth(
    // The DID of the service that the token will be used to authenticate with
    aud: String,
    // The time in Unix Epoch seconds that the JWT expires. Defaults to 60 seconds in the future.
    // The service may enforce certain time bounds on tokens depending on the requested scope.
    exp: Option<u64>,
    // Lexicon (XRPC) method to bind the requested token to
    lxm: Option<String>,
    auth: Scoped<NoScopeRequired>,
    actor_store: &State<ActorStore>,
) -> Result<Json<GetServiceAuthOutput>, ApiError> {
    match inner_get_service_auth(aud, exp, lxm, auth, actor_store).await {
        Ok(token) => {
            record_service_token_issued();
            Ok(Json(GetServiceAuthOutput { token }))
        }
        Err(error) => {
            record_service_token_denied(deny_reason(&error));
            tracing::error!("Internal Error: {error}");
            Err(ApiError::RuntimeError)
        }
    }
}

#[cfg(test)]
#[path = "get_service_auth_tests.rs"]
mod tests;
