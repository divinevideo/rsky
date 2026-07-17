use crate::account_manager::helpers::auth::{create_service_jwt, ServiceJwtParams};
use crate::apis::ApiError;
use crate::auth_verifier::AccessFull;
use crate::pipethrough::{PRIVILEGED_METHODS, PROTECTED_METHODS};
use anyhow::{bail, Result};
use rocket::serde::json::Json;
use rsky_common::time::{HOUR, MINUTE};
use rsky_lexicon::com::atproto::server::GetServiceAuthOutput;
use secp256k1::SecretKey;
use std::env;
use std::time::SystemTime;

/// Validate a client-supplied service-auth `exp` (Unix epoch SECONDS per the
/// lexicon) against `now_secs`, entirely in `u64` seconds.
///
/// The previous implementation converted `exp` to microseconds with
/// `(exp as i64) * 1_000_000` before comparing. That multiplication overflows
/// `i64` for large `exp`: in debug builds it panics, and in release builds it
/// wraps silently, so a crafted `exp` (e.g. `2^62 + target`) could wrap back to
/// a small in-range value, clear the one-hour bound, and then be signed
/// *unchanged* into a token valid for millions of years. Comparing in seconds
/// with no multiplication removes any overflow, so the value that is checked is
/// exactly the value that is signed.
fn check_service_auth_exp(exp: u64, now_secs: u64, has_lxm: bool) -> Result<()> {
    if exp <= now_secs {
        bail!("BadExpiration: expiration is in past");
    }
    // HOUR and MINUTE from rsky_common::time are expressed in MILLIseconds.
    let hour_secs = (HOUR / 1000) as u64;
    let minute_secs = (MINUTE / 1000) as u64;
    let diff_secs = exp - now_secs;
    if diff_secs > hour_secs {
        bail!("BadExpiration: cannot request a token with an expiration more than an hour in the future");
    } else if !has_lxm && diff_secs > minute_secs {
        bail!("BadExpiration: cannot request a method-less token with an expiration more than a minute in the future");
    }
    Ok(())
}

pub async fn inner_get_service_auth(
    aud: String,
    exp: Option<u64>,
    lxm: Option<String>,
    auth: AccessFull,
) -> Result<String> {
    let credentials = auth.access.credentials.unwrap();
    let did = credentials.clone().did.unwrap();
    // We just use the repo signing key
    let private_key = env::var("PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX").unwrap();
    let keypair = SecretKey::from_slice(&hex::decode(private_key.as_bytes()).unwrap()).unwrap();
    if let Some(exp) = exp {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system time is before the Unix epoch")
            .as_secs();
        check_service_auth_exp(exp, now_secs, lxm.is_some())?;
    }
    if let Some(ref lxm) = lxm {
        if PROTECTED_METHODS.contains(lxm.as_str()) {
            bail!("cannot request a service auth token for the following protected method: {lxm}");
        }
        if credentials.is_privileged.unwrap_or(false) && PRIVILEGED_METHODS.contains(lxm.as_str()) {
            bail!("insufficient access to request a service auth token for the following method: {lxm}");
        }
    }
    // Honor the validated client-requested expiration (previously dropped,
    // forcing every token to the 60s default — too short for callers like
    // video.bsky.app that hold the token across a transcode).
    create_service_jwt(ServiceJwtParams {
        iss: did,
        aud,
        exp,
        lxm,
        jti: None,
        keypair,
    })
    .await
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
    auth: AccessFull,
) -> Result<Json<GetServiceAuthOutput>, ApiError> {
    match inner_get_service_auth(aud, exp, lxm, auth).await {
        Ok(token) => Ok(Json(GetServiceAuthOutput { token })),
        Err(error) => {
            tracing::error!("Internal Error: {error}");
            Err(ApiError::RuntimeError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::check_service_auth_exp;

    // A crafted exp that the pre-fix code mishandled: `2^62 + target` seconds.
    // Because `2^62 * 1_000_000 ≡ 0 (mod 2^64)`, the old `(exp as i64) * 1_000_000`
    // collapsed this to `target` micros — a small, in-range timestamp — while the
    // real value is ~1.5e11 years in the future.
    const NOW: u64 = 1_700_000_000;
    fn crafted_exp() -> u64 {
        (1u64 << 62) + NOW + 1800
    }

    #[test]
    fn old_multiplication_overflows_for_the_crafted_exp() {
        // This is the vulnerability: the pre-fix conversion cannot even be
        // computed without overflow. `checked_mul` returning None proves the
        // `(exp as i64) * 1_000_000` step wrapped (release) or panicked (debug).
        assert!((crafted_exp() as i64).checked_mul(1_000_000).is_none());
    }

    #[test]
    fn crafted_huge_exp_is_rejected_not_accepted() {
        // The seconds-only check has no multiplication, so it sees the true
        // (astronomically large) value and rejects it as out of bounds.
        let err = check_service_auth_exp(crafted_exp(), NOW, true)
            .expect_err("a millions-of-years exp must be rejected")
            .to_string();
        assert!(err.contains("BadExpiration"), "{err}");
    }

    #[test]
    fn valid_near_future_exp_is_accepted() {
        assert!(check_service_auth_exp(NOW + 300, NOW, true).is_ok());
    }

    #[test]
    fn past_exp_is_rejected() {
        let err = check_service_auth_exp(NOW - 1, NOW, true)
            .expect_err("a past exp must be rejected")
            .to_string();
        assert!(
            err.contains("BadExpiration: expiration is in past"),
            "{err}"
        );
    }

    #[test]
    fn methodless_token_bounded_to_a_minute() {
        // Without an lxm the bound is one minute, not one hour.
        assert!(check_service_auth_exp(NOW + 30, NOW, false).is_ok());
        let err = check_service_auth_exp(NOW + 120, NOW, false)
            .expect_err("method-less token beyond a minute must be rejected")
            .to_string();
        assert!(err.contains("more than a minute"), "{err}");
        // The same 2-minute exp is fine when a method is bound (one-hour bound).
        assert!(check_service_auth_exp(NOW + 120, NOW, true).is_ok());
    }
}
