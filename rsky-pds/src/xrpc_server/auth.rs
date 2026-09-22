use crate::account_manager::helpers::auth::{create_service_jwt, ServiceJwtParams};
use anyhow::{anyhow, bail, Result};
use atrium_api::xrpc::http::HeaderMap;
use base64ct::{Base64, Base64UrlUnpadded, Encoding};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use rsky_crypto::types::VerifyOptions;
use rsky_crypto::verify::verify_signature_digest;
use secp256k1::Keypair;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub struct ServiceJwtPayload {
    pub iss: String,
    pub aud: String,
    pub exp: Option<Duration>,
    /// The single method this token was minted for, when it names one.
    pub lxm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JwtPayload {
    pub iss: String,
    pub aud: String,
    pub exp: u64,
    /// Method binding (atproto inter-service auth). Absent on tokens minted
    /// before the claim existed, which stay as broad as they were issued.
    #[serde(default)]
    pub lxm: Option<String>,
}

pub async fn create_service_auth_headers(
    params: ServiceJwtParams,
    keypair: &Keypair,
) -> Result<HeaderMap> {
    let jwt = create_service_jwt(params, keypair).await?;
    let jwt_str = format!("Bearer {jwt}");
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_str(&jwt_str)?);
    Ok(headers)
}

pub fn parse_b64_url_to_json(b64: &str) -> Result<JwtPayload> {
    // JWT segments are base64url unpadded; standard base64 is accepted as a
    // fallback for tokens minted by older rsky builds.
    let bytes = Base64UrlUnpadded::decode_vec(b64)
        .or_else(|_| Base64::decode_vec(b64))
        .map_err(|err| anyhow!(err.to_string()))?;
    Ok(serde_json::from_slice::<JwtPayload>(bytes.as_slice())?)
}

pub fn parse_payload(b64: &str) -> Result<JwtPayload> {
    let payload = parse_b64_url_to_json(b64)?;
    Ok(payload)
}

#[tracing::instrument(skip_all)]
/// Verify an inbound service-auth JWT.
///
/// `lxm` is the method being called. A token that names a method may only be
/// used for that method: without this check a token minted to call one
/// endpoint can be replayed against every other endpoint this service serves,
/// which is the whole point of the claim.
pub async fn verify_jwt<G, Fut>(
    jwt_str: String,
    own_did: Option<String>, // None indicates to skip the audience check
    lxm: Option<&str>,
    get_signing_key: G,
) -> Result<ServiceJwtPayload>
where
    G: Fn(String, bool) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let parts = jwt_str.split(".").collect::<Vec<&str>>();
    match (parts.first(), parts.get(1), parts.get(2)) {
        (Some(_), Some(parts_1), Some(sig)) if parts.len() == 3 => {
            let parts_1 = *parts_1;
            let sig = *sig;
            let payload = parse_payload(parts_1)?;
            // `exp` is seconds since the epoch (RFC 7519 §4.1.4), which is
            // what every other implementation mints.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("timestamp since UNIX epoch")
                .as_secs();
            if now > payload.exp {
                bail!("JwtExpired: jwt expired")
            }
            if own_did.is_some() && payload.aud != own_did.unwrap() {
                bail!("BadJwtAudience: jwt audience does not match service did")
            }
            match (&payload.lxm, lxm) {
                (Some(bound), Some(called)) if bound != called => {
                    bail!("BadJwtLexiconMethod: jwt was minted for {bound}, not {called}")
                }
                (Some(bound), None) => {
                    bail!("BadJwtLexiconMethod: jwt was minted for {bound}")
                }
                _ => {}
            }
            // The signature covers the ASCII of `header.payload`; both curves
            // sign its sha256 digest. The signature segment is base64url raw
            // bytes (compact or DER).
            let msg_digest = Sha256::digest(parts[0..2].join(".").as_bytes());
            let sig_bytes = Base64UrlUnpadded::decode_vec(sig)
                .or_else(|_| Base64::decode_vec(sig))
                .map_err(|_| anyhow!("BadJwt: invalid signature encoding"))?;
            let verify_signature_with_key = |key: String| -> Result<bool> {
                verify_signature_digest(
                    &key,
                    msg_digest.as_slice(),
                    sig_bytes.as_slice(),
                    Some(VerifyOptions {
                        allow_malleable_sig: Some(true),
                    }),
                )
            };

            let signing_key = get_signing_key(payload.iss.clone(), false).await?;

            let mut valid_sig: bool = match verify_signature_with_key(signing_key.clone()) {
                Ok(is_valid) => is_valid,
                Err(err) => {
                    tracing::error!("Error received: {}", err);
                    bail!("BadJwtSignature: could not verify jwt signature")
                }
            };

            if !valid_sig {
                // get fresh signing key in case it failed due to a recent rotation
                let fresh_signing_key = get_signing_key(payload.iss.clone(), true).await?;
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
                exp: Some(Duration::from_secs(payload.exp)),
                lxm: payload.lxm,
            })
        }
        _ => bail!("BadJwt: poorly formatted jwt"),
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
