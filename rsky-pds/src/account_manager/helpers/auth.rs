use crate::auth_verifier::AuthScope;
use crate::db::DbConn;
use crate::models;
use anyhow::Result;
use diesel::*;
use jwt_simple::prelude::*;
use rsky_common::time::{from_micros_to_utc, MINUTE, SECOND};
use rsky_common::{get_random_str, json_to_b64url, RFC3339_VARIANT};
use secp256k1::{Keypair, Message, SecretKey};
use sha2::{Digest, Sha256};
use std::time::SystemTime;
use thiserror::Error;

pub struct CreateTokensOpts {
    pub did: String,
    pub jwt_key: Keypair,
    pub service_did: String,
    pub scope: Option<AuthScope>,
    pub jti: Option<String>,
    pub expires_in: Option<Duration>,
}

pub struct RefreshGracePeriodOpts {
    pub id: String,
    pub expires_at: String,
    pub next_id: String,
}

pub struct AuthToken {
    pub scope: AuthScope,
    pub sub: String,
    pub exp: Duration,
}

pub struct RefreshToken {
    pub scope: AuthScope, // AuthScope::Refresh
    pub sub: String,
    pub exp: Duration,
    pub jti: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServiceJwtPayload {
    pub iss: String,
    pub aud: String,
    pub exp: Option<u64>,
    pub lxm: Option<String>,
    pub jti: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServiceJwtHeader {
    pub typ: String,
    pub alg: String,
}

pub struct ServiceJwtParams {
    pub iss: String,
    pub aud: String,
    pub exp: Option<u64>,
    pub lxm: Option<String>,
    pub jti: Option<String>,
    pub keypair: SecretKey,
}

#[derive(Serialize, Deserialize)]
pub struct CustomClaimObj {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub lxm: Option<String>,
}

#[derive(Error, Debug)]
pub enum AuthHelperError {
    #[error("ConcurrentRefreshError")]
    ConcurrentRefresh,
}

pub fn create_tokens(opts: CreateTokensOpts) -> Result<(String, String)> {
    let CreateTokensOpts {
        did,
        jwt_key,
        service_did,
        scope,
        jti,
        expires_in,
    } = opts;
    let access_jwt = create_access_token(CreateTokensOpts {
        did: did.clone(),
        jwt_key,
        service_did: service_did.clone(),
        scope,
        expires_in,
        jti: None,
    })?;
    let refresh_jwt = create_refresh_token(CreateTokensOpts {
        did,
        jwt_key,
        service_did,
        jti,
        expires_in,
        scope: None,
    })?;
    Ok((access_jwt, refresh_jwt))
}

pub fn create_access_token(opts: CreateTokensOpts) -> Result<String> {
    let CreateTokensOpts {
        did,
        jwt_key,
        service_did,
        scope,
        expires_in,
        ..
    } = opts;
    let scope = scope.unwrap_or(AuthScope::Access);
    let expires_in = expires_in.unwrap_or_else(|| Duration::from_hours(2));
    let claims = Claims::with_custom_claims(
        CustomClaimObj {
            scope: scope.as_str().to_owned(),
            lxm: None,
        },
        expires_in,
    )
    .with_audience(service_did)
    .with_subject(did);
    // alg ES256K
    let key = ES256kKeyPair::from_bytes(jwt_key.secret_bytes().as_slice())?;
    let token = key.sign(claims)?;
    Ok(token)
}

pub fn create_refresh_token(opts: CreateTokensOpts) -> Result<String> {
    let CreateTokensOpts {
        did,
        jwt_key,
        service_did,
        jti,
        expires_in,
        ..
    } = opts;
    let jti = jti.unwrap_or_else(get_random_str);
    let expires_in = expires_in.unwrap_or_else(|| Duration::from_days(90));
    let claims = Claims::with_custom_claims(
        CustomClaimObj {
            scope: AuthScope::Refresh.as_str().to_owned(),
            lxm: None,
        },
        expires_in,
    )
    .with_audience(service_did)
    .with_subject(did)
    .with_jwt_id(jti);
    // alg ES256K
    let key = ES256kKeyPair::from_bytes(jwt_key.secret_bytes().as_slice())?;
    let token = key.sign(claims)?;
    Ok(token)
}

pub async fn create_service_jwt(params: ServiceJwtParams) -> Result<String> {
    let ServiceJwtParams {
        iss, aud, keypair, ..
    } = params;
    // JWT exp is NumericDate: SECONDS since the epoch (RFC 7519). MINUTE is in
    // milliseconds. The previous math ((now_micros + 60_000)/1000) produced a
    // milliseconds value that no spec-compliant verifier interprets correctly.
    let now_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("timestamp since UNIX epoch")
        .as_secs();
    let exp = params
        .exp
        .unwrap_or(now_secs + (MINUTE / SECOND) as u64);
    let lxm = params.lxm;
    let jti = get_random_str();
    let header = ServiceJwtHeader {
        typ: "JWT".to_string(),
        alg: "ES256K".to_string(),
    };
    let payload = ServiceJwtPayload {
        iss,
        aud,
        exp: Some(exp),
        lxm,
        jti: Some(jti),
    };
    let to_sign_str = format!(
        "{0}.{1}",
        json_to_b64url(&header)?,
        json_to_b64url(&payload)?
    );
    let hash = Sha256::digest(to_sign_str.clone());
    let message = Message::from_digest_slice(hash.as_ref())?;
    let mut sig = keypair.sign_ecdsa(message);
    // Convert to low-s
    sig.normalize_s();
    // ASN.1 encoded per decode_dss_signature
    let compact_sig = sig.serialize_compact();
    Ok(format!(
        "{0}.{1}",
        to_sign_str,
        base64_url::encode(&compact_sig).replace("=", "") // Base 64 encode signature bytes
    ))
}

// @NOTE unsafe for verification, should only be used w/ direct output from createRefreshToken() or createTokens()
pub fn decode_refresh_token(jwt: String, jwt_key: Keypair) -> Result<RefreshToken> {
    let key = ES256kKeyPair::from_bytes(jwt_key.secret_bytes().as_slice())?;
    let public_key = key.public_key();
    let claims = public_key.verify_token::<CustomClaimObj>(&jwt, None)?;
    assert_eq!(
        claims.custom.scope,
        AuthScope::Refresh.as_str().to_owned(),
        "not a refresh token"
    );
    Ok(RefreshToken {
        scope: AuthScope::from_str(&claims.custom.scope)?,
        sub: claims.subject.unwrap(),
        exp: claims.expires_at.unwrap(),
        jti: claims.jwt_id.unwrap(),
    })
}

pub async fn store_refresh_token(
    payload: RefreshToken,
    app_password_name: Option<String>,
    db: &DbConn,
) -> Result<()> {
    use crate::schema::pds::refresh_token::dsl as RefreshTokenSchema;

    // payload.exp is a coarsetime Duration since the epoch; from_micros_to_utc
    // needs microseconds (the previous seconds value only produced correct
    // dates because from_micros_to_utc itself misread its input as seconds).
    let exp = from_micros_to_utc((payload.exp.as_millis() as i64) * 1000);

    db.run(move |conn| {
        insert_into(RefreshTokenSchema::refresh_token)
            .values((
                RefreshTokenSchema::id.eq(payload.jti),
                RefreshTokenSchema::did.eq(payload.sub),
                RefreshTokenSchema::appPasswordName.eq(app_password_name),
                RefreshTokenSchema::expiresAt.eq(format!("{}", exp.format(RFC3339_VARIANT))),
            ))
            .on_conflict_do_nothing() // E.g. when re-granting during a refresh grace period
            .execute(conn)
    })
    .await?;

    Ok(())
}

pub async fn revoke_refresh_token(id: String, db: &DbConn) -> Result<bool> {
    use crate::schema::pds::refresh_token::dsl as RefreshTokenSchema;
    db.run(move |conn| {
        let deleted_rows = delete(RefreshTokenSchema::refresh_token)
            .filter(RefreshTokenSchema::id.eq(id))
            .get_results::<models::RefreshToken>(conn)?;

        Ok(!deleted_rows.is_empty())
    })
    .await
}

pub async fn revoke_refresh_tokens_by_did(did: &str, db: &DbConn) -> Result<bool> {
    use crate::schema::pds::refresh_token::dsl as RefreshTokenSchema;
    let did = did.to_owned();
    db.run(move |conn| {
        let deleted_rows = delete(RefreshTokenSchema::refresh_token)
            .filter(RefreshTokenSchema::did.eq(did))
            .get_results::<models::RefreshToken>(conn)?;

        Ok(!deleted_rows.is_empty())
    })
    .await
}

pub async fn revoke_app_password_refresh_token(
    did: &str,
    app_pass_name: &str,
    db: &DbConn,
) -> Result<bool> {
    use crate::schema::pds::refresh_token::dsl as RefreshTokenSchema;

    let did = did.to_owned();
    let app_pass_name = app_pass_name.to_owned();
    db.run(move |conn| {
        let deleted_rows = delete(RefreshTokenSchema::refresh_token)
            .filter(RefreshTokenSchema::did.eq(did))
            .filter(RefreshTokenSchema::appPasswordName.eq(app_pass_name))
            .get_results::<models::RefreshToken>(conn)?;

        Ok(!deleted_rows.is_empty())
    })
    .await
}

pub async fn get_refresh_token(id: &str, db: &DbConn) -> Result<Option<models::RefreshToken>> {
    use crate::schema::pds::refresh_token::dsl as RefreshTokenSchema;
    let id = id.to_owned();
    db.run(move |conn| {
        Ok(RefreshTokenSchema::refresh_token
            .find(id)
            .first(conn)
            .optional()?)
    })
    .await
}

pub async fn delete_expired_refresh_tokens(did: &str, now: String, db: &DbConn) -> Result<()> {
    use crate::schema::pds::refresh_token::dsl as RefreshTokenSchema;
    let did = did.to_owned();

    db.run(move |conn| {
        delete(RefreshTokenSchema::refresh_token)
            .filter(RefreshTokenSchema::did.eq(did))
            .filter(RefreshTokenSchema::expiresAt.le(now))
            .execute(conn)?;
        Ok(())
    })
    .await
}

pub async fn add_refresh_grace_period(opts: RefreshGracePeriodOpts, db: &DbConn) -> Result<()> {
    db.run(move |conn| {
        let RefreshGracePeriodOpts {
            id,
            expires_at,
            next_id,
        } = opts;
        use crate::schema::pds::refresh_token::dsl as RefreshTokenSchema;

        update(RefreshTokenSchema::refresh_token)
            .filter(RefreshTokenSchema::id.eq(id))
            .filter(
                RefreshTokenSchema::nextId
                    .is_null()
                    .or(RefreshTokenSchema::nextId.eq(&next_id)),
            )
            .set((
                RefreshTokenSchema::expiresAt.eq(expires_at),
                RefreshTokenSchema::nextId.eq(&next_id),
            ))
            .returning(models::RefreshToken::as_select())
            .get_results(conn)
            .map_err(|error| {
                anyhow::Error::new(AuthHelperError::ConcurrentRefresh).context(error)
            })?;
        Ok(())
    })
    .await
}

pub fn get_refresh_token_id() -> String {
    get_random_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
    use std::time::UNIX_EPOCH;

    /// Same constant the integration harness feeds to
    /// `PDS_JWT_KEY_K256_PRIVATE_KEY_HEX`.
    const TEST_JWT_KEY_HEX: &str =
        "8f2a55949068468ad5d670dfd0c0a33d5b9e7e1a2c0d2059f0f8f8779d4d078d";

    /// Any `exp` above this is not a seconds-based NumericDate. Seconds values are
    /// 10 digits until the year 5138; milliseconds values are already 13 digits.
    const MAX_PLAUSIBLE_SECONDS_EXP: u64 = 100_000_000_000;

    fn secret_key() -> SecretKey {
        SecretKey::from_slice(&hex::decode(TEST_JWT_KEY_HEX).unwrap()).unwrap()
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    async fn mint(exp: Option<u64>) -> String {
        create_service_jwt(ServiceJwtParams {
            iss: "did:web:pds.example".to_string(),
            aud: "did:web:video.bsky.app".to_string(),
            exp,
            lxm: Some("app.bsky.video.getUploadLimits".to_string()),
            jti: None,
            keypair: secret_key(),
        })
        .await
        .unwrap()
    }

    /// Decode the claims a receiving service would read off the wire.
    ///
    /// `json_to_b64url` encodes with the STANDARD base64 alphabet and then strips the
    /// padding, so `STANDARD_NO_PAD` is the decoder that actually round-trips it. (A
    /// url-safe decoder would blow up whenever the payload happens to contain `+` or
    /// `/`, which is exactly the kind of intermittent failure this test must not have.)
    fn claims_of(jwt: &str) -> ServiceJwtPayload {
        let parts = jwt.split('.').collect::<Vec<&str>>();
        assert_eq!(parts.len(), 3, "expected a three-part JWS");
        serde_json::from_slice(&STANDARD_NO_PAD.decode(parts[1]).unwrap()).unwrap()
    }

    /// Regression: `create_service_jwt` computed `(now_micros + MINUTE) / 1000`, which
    /// yields MILLIseconds. RFC 7519 NumericDate is SECONDS, so every token this PDS
    /// minted carried an `exp` ~1000x in the future to a spec-compliant reader -- and
    /// our own verifier, which compared it against microseconds, treated it as already
    /// expired.
    #[tokio::test]
    async fn create_service_jwt_mints_a_seconds_numericdate_exp() {
        let before = now_secs();

        let exp = claims_of(&mint(None).await).exp.expect("exp claim");

        assert!(
            exp < MAX_PLAUSIBLE_SECONDS_EXP,
            "exp {exp} is not a seconds NumericDate -- looks like milliseconds"
        );
        // Default service-token lifetime is one minute.
        assert!(
            exp >= before + 55 && exp <= now_secs() + 65,
            "default exp should be ~60s out; got {exp} against now={before}"
        );
    }

    /// A caller-supplied `exp` (e.g. video.bsky.app needs the token to outlive a
    /// transcode) must be passed through untouched, in seconds.
    #[tokio::test]
    async fn create_service_jwt_honors_an_explicit_exp() {
        let requested = now_secs() + 1800;

        let exp = claims_of(&mint(Some(requested)).await)
            .exp
            .expect("exp claim");

        assert_eq!(exp, requested);
    }

    /// The rest of the claim set the appview/video service relies on.
    #[tokio::test]
    async fn create_service_jwt_carries_iss_aud_and_lxm() {
        let claims = claims_of(&mint(None).await);

        assert_eq!(claims.iss, "did:web:pds.example");
        assert_eq!(claims.aud, "did:web:video.bsky.app");
        assert_eq!(
            claims.lxm.as_deref(),
            Some("app.bsky.video.getUploadLimits")
        );
        assert!(claims.jti.is_some(), "service tokens must carry a jti");
    }
}
