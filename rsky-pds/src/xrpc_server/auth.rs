use crate::account_manager::helpers::auth::{create_service_jwt, ServiceJwtParams};
use anyhow::{anyhow, bail, Result};
use atrium_api::xrpc::http::HeaderMap;
use base64ct::{Base64, Encoding};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use rsky_crypto::types::VerifyOptions;
use rsky_crypto::verify::verify_signature;
use std::time::{Duration, SystemTime};

pub struct ServiceJwtPayload {
    pub iss: String,
    pub aud: String,
    pub exp: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JwtPayload {
    pub iss: String,
    pub aud: String,
    pub exp: u64,
}

pub async fn create_service_auth_headers(params: ServiceJwtParams) -> Result<HeaderMap> {
    let jwt = create_service_jwt(params).await?;
    let jwt_str = format!("Bearer {jwt}");
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_str(&jwt_str)?);
    Ok(headers)
}

pub fn parse_b64_url_to_json(b64: &str) -> Result<JwtPayload> {
    Ok(serde_json::from_slice::<JwtPayload>(
        Base64::decode_vec(b64)
            .map_err(|err| anyhow!(err.to_string()))?
            .as_slice(),
    )?)
}

pub fn parse_payload(b64: &str) -> Result<JwtPayload> {
    let payload = parse_b64_url_to_json(b64)?;
    Ok(payload)
}

#[tracing::instrument(skip_all)]
pub async fn verify_jwt<G>(
    jwt_str: String,
    own_did: Option<String>, // None indicates to skip the audience check
    get_signing_key: G,
) -> Result<ServiceJwtPayload>
where
    G: Fn(String, bool) -> Result<String>,
{
    let parts = jwt_str.split(".").collect::<Vec<&str>>();
    match (parts.first(), parts.get(1), parts.get(2)) {
        (Some(_), Some(parts_1), Some(sig)) if parts.len() == 3 => {
            let parts_1 = *parts_1;
            let sig = *sig;
            let payload = parse_payload(parts_1)?;
            // JWT exp is NumericDate: SECONDS since the epoch (RFC 7519). The
            // previous check compared against microseconds, which rejected
            // every spec-compliant token as expired. Tolerate legacy tokens
            // that were minted with a milliseconds exp (13-digit values) by
            // normalising them down to seconds before comparing.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("timestamp since UNIX epoch")
                .as_secs();
            let exp_secs = if payload.exp > 100_000_000_000 {
                payload.exp / 1000
            } else {
                payload.exp
            };
            if now > exp_secs {
                bail!("JwtExpired: jwt expired")
            }
            if own_did.is_some() && payload.aud != own_did.unwrap() {
                bail!("BadJwtAudience: jwt audience does not match service did")
            }
            let msg_bytes = parts[0..2].join(".").into_bytes();
            let sig_bytes = Base64::encode_string(sig.as_bytes())
                .replace("=", "")
                .into_bytes();
            let verify_signature_with_key = |key: String| -> Result<bool> {
                verify_signature(
                    &key,
                    msg_bytes.as_slice(),
                    sig_bytes.as_slice(),
                    Some(VerifyOptions {
                        allow_malleable_sig: Some(true),
                    }),
                )
            };

            let signing_key = get_signing_key(payload.iss.clone(), false)?;

            let mut valid_sig: bool = match verify_signature_with_key(signing_key.clone()) {
                Ok(is_valid) => is_valid,
                Err(err) => {
                    tracing::error!("Error received: {}", err);
                    bail!("BadJwtSignature: could not verify jwt signature")
                }
            };

            if !valid_sig {
                // get fresh signing key in case it failed due to a recent rotation
                let fresh_signing_key = get_signing_key(payload.iss.clone(), true)?;
                valid_sig = if fresh_signing_key != signing_key {
                    match verify_signature_with_key(fresh_signing_key) {
                        Ok(is_valid) => is_valid,
                        Err(err) => {
                            tracing::error!("Error received: {}", err);
                            bail!("BadJwtSignature: could not verify jwt signature")
                        }
                    }
                } else {
                    false
                };
            }

            if !valid_sig {
                bail!("BadJwtSignature: jwt signature does not match jwt issuer")
            }

            Ok(ServiceJwtPayload {
                iss: payload.iss,
                aud: payload.aud,
                exp: Some(Duration::from_secs(exp_secs)),
            })
        }
        _ => bail!("BadJwt: poorly formatted jwt"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_manager::helpers::auth::{create_service_jwt, ServiceJwtParams};
    use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
    use secp256k1::SecretKey;

    /// Same constant the integration harness feeds to `PDS_JWT_KEY_K256_PRIVATE_KEY_HEX`.
    const TEST_JWT_KEY_HEX: &str =
        "8f2a55949068468ad5d670dfd0c0a33d5b9e7e1a2c0d2059f0f8f8779d4d078d";
    const ISS: &str = "did:web:pds.example";
    const AUD: &str = "did:web:video.bsky.app";
    /// Not a real signature -- these tests pin the expiry gate, which runs first.
    const DUMMY_SIG: &str = "AA";

    /// Any `exp` above this is not a seconds-based NumericDate. Seconds values are
    /// 10 digits until the year 5138; millisecond values are already 13 digits. This
    /// is the same threshold `verify_jwt` uses to spot a legacy token.
    const MAX_PLAUSIBLE_SECONDS_EXP: u64 = 100_000_000_000;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Build a token carrying `exp`, encoded the way `parse_payload` decodes.
    ///
    /// Two pre-existing defects in this module make a *real* minted token unusable
    /// here, and both are orthogonal to the time-unit fix under test:
    ///
    /// 1. `json_to_b64url` (the minter) STRIPS base64 padding, but
    ///    `parse_b64_url_to_json` decodes with `base64ct::Base64::decode_vec`, which
    ///    REQUIRES it -- so a minted payload only parses when its length happens to be
    ///    a multiple of 3, and otherwise dies with "invalid Base64 encoding".
    /// 2. `verify_jwt` hands the raw, unhashed `header.payload` bytes to `verify_sig`,
    ///    which needs a 32-byte digest, and re-base64-encodes an already-encoded
    ///    signature -- so the signature check can never succeed for any token.
    ///
    /// Encoding WITH padding here routes around (1) so that the expiry gate -- the only
    /// logic this change actually touches -- can be pinned deterministically. Without
    /// this, the payload fails to parse and every assertion below passes vacuously.
    fn jwt_with_exp(exp: u64) -> String {
        let header = Base64::encode_string(br#"{"typ":"JWT","alg":"ES256K"}"#);
        let payload = Base64::encode_string(
            serde_json::json!({ "iss": ISS, "aud": AUD, "exp": exp })
                .to_string()
                .as_bytes(),
        );
        format!("{header}.{payload}.{DUMMY_SIG}")
    }

    async fn verify(jwt: String) -> Result<ServiceJwtPayload> {
        // The signing key a real caller's DID document would publish. Its value is
        // irrelevant: these tests never get a valid signature past defect (2) above.
        verify_jwt(jwt, Some(AUD.to_string()), |_iss, _force_refresh| {
            Ok("did:key:zQ3shokFTS3brHcDQrn82RUDfCZESWL1ZdCEJwekUDPQiYBme".to_string())
        })
        .await
    }

    /// The token cleared the expiry gate (and the audience gate) and got as far as the
    /// signature check.
    ///
    /// This is deliberately NOT "did not say JwtExpired": a payload that fails to parse
    /// also does not say JwtExpired, so that weaker form would pass vacuously. Requiring
    /// the verifier to reach the *signature* step proves the expiry gate accepted it.
    fn assert_reached_signature_check(result: Result<ServiceJwtPayload>) {
        match result {
            // If defect (2) is ever fixed, a well-formed token verifies outright --
            // which likewise means the expiry gate accepted it.
            Ok(_) => {}
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("BadJwtSignature"),
                    "token should have cleared the expiry gate and reached the signature \
                     check, but verification failed earlier with: {msg}"
                );
            }
        }
    }

    fn assert_rejected_as_expired(result: Result<ServiceJwtPayload>) {
        match result {
            Ok(_) => panic!("verifier accepted a token that expired an hour ago"),
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("JwtExpired"),
                    "expected JwtExpired, got: {msg}"
                );
            }
        }
    }

    /// Regression: `create_service_jwt` minted `exp` in MILLIseconds while `verify_jwt`
    /// compared it against MICROseconds, so a token this PDS had just minted was already
    /// "expired" according to this PDS's own verifier. Feed the real minter's `exp` to the
    /// real verifier and require the two to agree.
    #[tokio::test]
    async fn verifier_accepts_the_exp_our_own_minter_produces() {
        let jwt = create_service_jwt(ServiceJwtParams {
            iss: ISS.to_string(),
            aud: AUD.to_string(),
            exp: None, // default lifetime
            lxm: Some("app.bsky.video.getUploadLimits".to_string()),
            jti: None,
            keypair: SecretKey::from_slice(&hex::decode(TEST_JWT_KEY_HEX).unwrap()).unwrap(),
        })
        .await
        .unwrap();

        // Decode the way `json_to_b64url` encoded: STANDARD alphabet, padding stripped.
        // (Not `parse_payload` -- that is the padded decoder broken by defect (1).)
        let claims: serde_json::Value = serde_json::from_slice(
            &STANDARD_NO_PAD
                .decode(jwt.split('.').nth(1).unwrap())
                .expect("minted payload should decode as unpadded standard base64"),
        )
        .unwrap();
        let minted_exp = claims["exp"].as_u64().expect("exp claim");

        assert_reached_signature_check(verify(jwt_with_exp(minted_exp)).await);
    }

    /// Spec-compliant tokens from other services (video.bsky.app, mod services) carry a
    /// seconds-based NumericDate per RFC 7519. The old verifier compared them against a
    /// microsecond clock and rejected every one as expired.
    #[tokio::test]
    async fn verify_jwt_accepts_spec_compliant_seconds_exp() {
        let exp = now_secs() + 300;
        assert!(exp < MAX_PLAUSIBLE_SECONDS_EXP);

        assert_reached_signature_check(verify(jwt_with_exp(exp)).await);
    }

    /// Rollout tolerance: tokens minted by the pre-fix code carry a 13-digit millisecond
    /// `exp`. They must keep working until they age out.
    #[tokio::test]
    async fn verify_jwt_accepts_legacy_millisecond_exp() {
        let exp_ms = (now_secs() + 300) * 1000;
        assert!(
            exp_ms > MAX_PLAUSIBLE_SECONDS_EXP,
            "this test is only meaningful for a 13-digit ms exp, got {exp_ms}"
        );

        assert_reached_signature_check(verify(jwt_with_exp(exp_ms)).await);
    }

    /// Guard: tolerating legacy units must not decay into accepting everything. Backdated
    /// an hour, far beyond any clock-skew tolerance.
    #[tokio::test]
    async fn verify_jwt_rejects_a_genuinely_expired_seconds_token() {
        assert_rejected_as_expired(verify(jwt_with_exp(now_secs() - 3600)).await);
    }

    /// Guard: the legacy-millisecond branch must still expire.
    #[tokio::test]
    async fn verify_jwt_rejects_a_genuinely_expired_legacy_millisecond_token() {
        assert_rejected_as_expired(verify(jwt_with_exp((now_secs() - 3600) * 1000)).await);
    }
}
