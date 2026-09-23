use crate::db::sqlite::Db;
use anyhow::{anyhow, bail, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::rngs::OsRng;
use rand::RngCore;
use rsky_common::{get_random_str, now};
use rsky_lexicon::com::atproto::server::CreateAppPasswordOutput;
use rusqlite::{params, OptionalExtension};
use scrypt::{scrypt, Params as ScryptParams};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// An app password a session was created with, and whether it may reach
/// privileged methods.
#[derive(Debug, Clone, PartialEq)]
pub struct AppPassDescript {
    pub name: String,
    pub privileged: bool,
}

pub struct UpdateUserPasswordOpts {
    pub did: String,
    pub password_encrypted: String,
}

/// Scrypt cost parameters, chosen to match the reference TypeScript PDS
/// (`packages/pds/src/account-manager/helpers/scrypt.ts`), which calls
/// Node's `crypto.scrypt(password, salt, 64, cb)` with no options object.
/// Node's documented defaults for the omitted options are `N = 16384`
/// (`log2(N) = 14`), `r = 8`, `p = 1`.
const SCRYPT_LOG_N: u8 = 14;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
/// Derived key length in bytes: pds's `scrypt.ts` always requests 64 bytes.
const SCRYPT_KEY_LEN: usize = 64;
/// Random salt length in bytes (before hex-encoding), matching pds's
/// `crypto.randomBytes(16).toString('hex')` in `genSaltAndHash`.
const SCRYPT_SALT_LEN: usize = 16;

fn scrypt_params() -> ScryptParams {
    ScryptParams::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P)
        .expect("static scrypt parameters are always valid")
}

pub async fn verify_account_password(did: &str, password: &String, db: &Db) -> Result<bool> {
    let did = did.to_owned();
    let found: Option<String> = db
        .run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT \"passwordScrypt\" FROM account WHERE did = ?1",
                    params![did],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await?;
    if let Some(stored_hash) = found {
        let password = password.clone();
        Ok(tokio::task::spawn_blocking(move || verify(&password, &stored_hash)).await??)
    } else {
        Ok(false)
    }
}

pub async fn verify_app_password(
    did: &str,
    password: &str,
    db: &Db,
) -> Result<Option<AppPassDescript>> {
    let did = did.to_owned();
    let password = password.to_owned();
    let password_encrypted = hash_app_password(&did, &password).await?;
    let lookup_did = did.clone();
    let found = db
        .run(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT name, privileged FROM app_password \
                 WHERE did = ?1 AND \"passwordScrypt\" = ?2",
                    params![lookup_did, password_encrypted],
                    |row| {
                        Ok(AppPassDescript {
                            name: row.get(0)?,
                            privileged: row.get::<_, i64>(1)? == 1,
                        })
                    },
                )
                .optional()?)
        })
        .await?;
    if found.is_some() {
        return Ok(found);
    }

    // Migrated app passwords retain the deterministic Argon2 hashes used by
    // the old equality lookup. Compute that legacy form off the async worker.
    let legacy_did = did.clone();
    let legacy_password = password.clone();
    let legacy_password_encrypted = tokio::task::spawn_blocking(move || {
        hash_legacy_app_password(&legacy_did, &legacy_password)
    })
    .await??;
    db.run(move |conn| {
        Ok(conn
            .query_row(
                "SELECT name, privileged FROM app_password \
                     WHERE did = ?1 AND \"passwordScrypt\" = ?2",
                params![did, legacy_password_encrypted],
                |row| {
                    Ok(AppPassDescript {
                        name: row.get(0)?,
                        privileged: row.get::<_, i64>(1)? == 1,
                    })
                },
            )
            .optional()?)
    })
    .await
}

/// Hash a brand-new password with a freshly generated random salt.
///
/// This always produces a **scrypt** hash, in the exact `<hex salt>:<hex
/// derived key>` shape written by pds's `scrypt.ts genSaltAndHash`, so rows
/// created going forward (new accounts, password resets, `updateAccountPassword`)
/// are readable by either rsky-pds or the reference TS pds against the same
/// database.
pub fn gen_salt_and_hash(password: String) -> Result<String> {
    let mut salt_bytes = [0u8; SCRYPT_SALT_LEN];
    OsRng.fill_bytes(&mut salt_bytes);
    let salt = hex::encode(salt_bytes);
    hash_with_salt(&password, &salt)
}

/// Hash `password` with the scrypt KDF using `salt` verbatim as the salt
/// input bytes.
///
/// Note: this intentionally mirrors Node's `crypto.scrypt(password, salt, 64,
/// cb)`, which -- when `salt` is a JS string -- treats it as its raw
/// UTF-8/ASCII byte representation, **not** as hex-decoded bytes. So when
/// `salt` is itself a hex-encoded string (as produced by `gen_salt_and_hash`
/// and `hash_app_password`), the bytes actually fed into scrypt are the ASCII
/// bytes of that hex string, not the 16 raw bytes it represents. Matching
/// this exactly is required for interop with the TS pds's stored hashes.
pub fn hash_with_salt(password: &String, salt: &str) -> Result<String> {
    let params = scrypt_params();
    let mut derived_key = [0u8; SCRYPT_KEY_LEN];
    scrypt(
        password.as_bytes(),
        salt.as_bytes(),
        &params,
        &mut derived_key,
    )
    .map_err(|error| anyhow!(error.to_string()))?;
    Ok(format!("{}:{}", salt, hex::encode(derived_key)))
}

/// Verify `password` against `stored_hash`.
///
/// Dispatches on the stored hash's own encoding so that both algorithms can
/// coexist in the `account.password` column during (and after) the Argon2 ->
/// scrypt migration:
///   - Argon2 PHC strings always start with `$` (e.g. `$argon2id$v=19$...`).
///   - scrypt hashes are `<hex salt>:<hex derived key>` and never start with
///     `$`.
///
/// `gen_salt_and_hash`/`hash_with_salt` only ever produce scrypt hashes going
/// forward, but real rsky-pds deployments already have accounts hashed with
/// Argon2. Rather than force a mass password reset, old rows keep verifying
/// against Argon2 forever; only newly hashed/reset passwords move to scrypt.
pub fn verify(password: &String, stored_hash: &str) -> Result<bool> {
    if stored_hash.starts_with('$') {
        verify_argon2(password, stored_hash)
    } else {
        verify_scrypt(password, stored_hash)
    }
}

fn verify_argon2(password: &String, stored_hash: &str) -> Result<bool> {
    let parsed_hash = PasswordHash::new(stored_hash).map_err(|error| anyhow!(error.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_ref(), &parsed_hash)
        .is_ok())
}

fn verify_scrypt(password: &String, stored_hash: &str) -> Result<bool> {
    let (salt, expected_hex) = stored_hash
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid scrypt hash format: missing ':' separator"))?;
    let derived = hash_with_salt(password, salt)?;
    let (_, derived_hex) = derived
        .split_once(':')
        .expect("hash_with_salt always returns a '<salt>:<hash>' string");
    // Constant-time comparison of the hex-encoded derived keys, as called
    // out by the scrypt crate's own doc comment on `scrypt::scrypt`.
    Ok(derived_hex.as_bytes().ct_eq(expected_hex.as_bytes()).into())
}

pub async fn hash_app_password(did: &String, password: &String) -> Result<String> {
    let did = did.clone();
    let password = password.clone();
    Ok(tokio::task::spawn_blocking(move || {
        let digest = Sha256::digest(did);
        let salt = hex::encode(&digest[0..16]);
        hash_with_salt(&password, &salt)
    })
    .await??)
}

fn hash_legacy_app_password(did: &str, password: &str) -> Result<String> {
    let salt =
        SaltString::encode_b64(&Sha256::digest(did)).map_err(|error| anyhow!(error.to_string()))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow!(error.to_string()))
}

/// create an app password with format:
/// 1234-abcd-5678-efgh
pub async fn create_app_password(
    did: String,
    name: String,
    db: &Db,
) -> Result<CreateAppPasswordOutput> {
    let str = &get_random_str()[0..16].to_lowercase();
    let chunks = [&str[0..4], &str[4..8], &str[8..12], &str[12..16]];
    let password = chunks.join("-");
    let password_encrypted = hash_app_password(&did, &password).await?;

    let created_at = now();

    db.run(move |conn| {
        let got: Option<String> = conn
            .query_row(
                "INSERT INTO app_password (did, name, \"passwordScrypt\", \"createdAt\") \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (did, name) DO NOTHING \
                 RETURNING name",
                params![did, name, password_encrypted, created_at],
                |row| row.get(0),
            )
            .optional()?;
        if got.is_some() {
            Ok(CreateAppPasswordOutput {
                name: name.clone(),
                password: password.clone(),
                created_at: created_at.clone(),
            })
        } else {
            bail!("could not create app-specific password")
        }
    })
    .await
}

/// Every app password of `did` as `(name, created_at, privileged)`.
pub async fn list_app_passwords(did: &str, db: &Db) -> Result<Vec<(String, String, bool)>> {
    let did = did.to_owned();
    db.run(move |conn| {
        let mut stmt = conn
            .prepare("SELECT name, \"createdAt\", privileged FROM app_password WHERE did = ?1")?;
        let rows = stmt
            .query_map(params![did], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? == 1))
            })?
            .collect::<Result<Vec<(String, String, bool)>, rusqlite::Error>>()?;
        Ok(rows)
    })
    .await
}

pub async fn update_user_password(opts: UpdateUserPasswordOpts, db: &Db) -> Result<()> {
    db.run(move |conn| {
        conn.execute(
            "UPDATE account SET \"passwordScrypt\" = ?1 WHERE did = ?2",
            params![opts.password_encrypted, opts.did],
        )?;
        Ok(())
    })
    .await
}

pub async fn delete_app_password(did: &str, name: &str, db: &Db) -> Result<()> {
    let did = did.to_owned();
    let name = name.to_owned();
    db.run(move |conn| {
        conn.execute(
            "DELETE FROM app_password WHERE did = ?1 AND name = ?2",
            params![did, name],
        )?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::password_hash::{PasswordHasher, SaltString};

    const APP_DID: &str = "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_APP_DID: &str = "did:plc:bbbbbbbbbbbbbbbbbbbbbbbb";

    // Reproduce the pre-SQLite implementation: the full SHA256(DID) digest
    // is the Argon2 salt, encoded as unpadded base64 in the PHC string.
    fn legacy_app_hash(did: &str, password: &str) -> String {
        let salt = SaltString::encode_b64(&Sha256::digest(did)).unwrap();
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    async fn app_password_db(rows: Vec<(&str, &str, String, bool)>) -> Db {
        let db = crate::account_manager::db::get_migrated_db(":memory:")
            .await
            .unwrap();
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(did, name, hash, privileged)| {
                (did.to_owned(), name.to_owned(), hash, privileged)
            })
            .collect();
        db.run(move |conn| {
            for (did, name, hash, privileged) in &rows {
                conn.execute(
                    "INSERT INTO app_password (did, name, \"passwordScrypt\", \"createdAt\", privileged) \
                     VALUES (?1, ?2, ?3, '2026-01-01T00:00:00.000Z', ?4)",
                    params![did, name, hash, privileged],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn migrated_argon2_app_password_preserves_name_and_privileges() {
        let db = app_password_db(vec![
            (
                APP_DID,
                "limited",
                legacy_app_hash(APP_DID, "abcd-efgh-ijkl-mnop"),
                false,
            ),
            (
                APP_DID,
                "privileged",
                legacy_app_hash(APP_DID, "qrst-uvwx-yzab-cdef"),
                true,
            ),
        ])
        .await;

        for (password, name, privileged) in [
            ("abcd-efgh-ijkl-mnop", "limited", false),
            ("qrst-uvwx-yzab-cdef", "privileged", true),
        ] {
            assert_eq!(
                verify_app_password(APP_DID, password, &db).await.unwrap(),
                Some(AppPassDescript {
                    name: name.to_owned(),
                    privileged
                }),
            );
        }
    }

    #[tokio::test]
    async fn app_passwords_reject_wrong_passwords_and_other_accounts() {
        let scrypt_hash = hash_app_password(&APP_DID.to_owned(), &"new-password".to_owned())
            .await
            .unwrap();
        let db = app_password_db(vec![
            (
                APP_DID,
                "legacy",
                legacy_app_hash(APP_DID, "old-password"),
                false,
            ),
            (APP_DID, "current", scrypt_hash, true),
            (
                OTHER_APP_DID,
                "unrelated",
                legacy_app_hash(OTHER_APP_DID, "unrelated-password"),
                false,
            ),
        ])
        .await;
        for (did, password) in [
            (APP_DID, "wrong-password"),
            (OTHER_APP_DID, "old-password"),
            (OTHER_APP_DID, "new-password"),
            ("did:plc:cccccccccccccccccccccccc", "old-password"),
        ] {
            assert_eq!(verify_app_password(did, password, &db).await.unwrap(), None);
        }
        assert_eq!(
            verify_app_password(APP_DID, "new-password", &db)
                .await
                .unwrap(),
            Some(AppPassDescript {
                name: "current".to_owned(),
                privileged: true
            }),
        );
    }

    #[tokio::test]
    async fn malformed_app_hash_does_not_block_valid_legacy_password() {
        let db = app_password_db(vec![
            (APP_DID, "a-malformed", "$argon2id$invalid".to_owned(), true),
            (APP_DID, "b-unknown", "not-a-hash".to_owned(), true),
            (
                APP_DID,
                "c-valid",
                legacy_app_hash(APP_DID, "old-password"),
                false,
            ),
        ])
        .await;
        assert_eq!(
            verify_app_password(APP_DID, "old-password", &db)
                .await
                .unwrap(),
            Some(AppPassDescript {
                name: "c-valid".to_owned(),
                privileged: false
            }),
        );
        assert_eq!(
            verify_app_password(APP_DID, "wrong", &db).await.unwrap(),
            None
        );
    }

    /// A scrypt hash produced by our own `gen_salt_and_hash`/`hash_with_salt`
    /// round-trips through `verify`.
    #[test]
    fn scrypt_hash_round_trips() {
        let hash = gen_salt_and_hash("secret".to_owned()).unwrap();
        // Sanity-check the shape: no PHC `$` prefix, exactly one `:` field
        // separator, 32 hex chars of salt, 128 hex chars (64 bytes) of key.
        assert!(!hash.starts_with('$'));
        let (salt, key) = hash.split_once(':').unwrap();
        assert_eq!(salt.len(), SCRYPT_SALT_LEN * 2);
        assert_eq!(key.len(), SCRYPT_KEY_LEN * 2);

        assert!(verify(&"secret".to_owned(), &hash).unwrap());
        assert!(!verify(&"other".to_owned(), &hash).unwrap());
    }

    /// A hand-constructed scrypt hash in pds's exact stored format (`<hex
    /// salt>:<hex derived key>`, produced independently via Python's
    /// `hashlib.scrypt` with N=16384, r=8, p=1, dklen=64, and the salt bytes
    /// being the ASCII bytes of the hex salt string itself -- exactly what
    /// Node's `crypto.scrypt` does when given a string salt) verifies
    /// correctly, proving interop with the reference TS pds.
    #[test]
    fn interops_with_reference_pds_scrypt_format() {
        let stored_hash = "aabbccddeeff00112233445566778899:\
3d2a91e248809343123c0186c87868141ad7be0efcdd1939b28a85c3a9a6f84\
501a21efec04cedc29e2d7dad96021bf0109d0ffb2c7be13faf3f9eac5adfed25";
        assert!(verify(&"correct horse battery staple".to_owned(), stored_hash).unwrap());
        assert!(!verify(&"wrong password".to_owned(), stored_hash).unwrap());
    }

    /// A pre-existing Argon2-format hash (as produced by the old
    /// `gen_salt_and_hash`, before the scrypt migration) still verifies, so
    /// accounts created before this change are never locked out.
    #[test]
    fn legacy_argon2_hash_still_verifies() {
        // A real Argon2id PHC string for the password "secret", generated
        // with the old argon2-based `gen_salt_and_hash`.
        let stored_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$\
14ukWqiThj4Xz77NYv01V28GbBZHY9AaZwsFswQFO0U";
        assert!(verify(&"secret".to_owned(), stored_hash).unwrap());
        assert!(!verify(&"other".to_owned(), stored_hash).unwrap());
    }

    #[test]
    fn rejects_malformed_hash() {
        assert!(verify(&"secret".to_owned(), "not-a-recognized-hash-format").is_err());
    }

    #[test]
    fn hash_with_salt_accepts_arbitrary_salt_strings() {
        // pds's `hashWithSalt` performs no validation on the salt string --
        // any bytes are valid scrypt salt input -- so we match that
        // permissiveness rather than rejecting "unusual" salts.
        let hash = hash_with_salt(&"secret".to_owned(), "not/a valid+hex!salt").unwrap();
        assert!(verify(&"secret".to_owned(), &hash).unwrap());
    }
}
