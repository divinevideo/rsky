//! One-shot importer: copy a pre-SQLite rsky-pds PostgreSQL dataset into the
//! SQLite layout this release uses.
//!
//! The schema is written by the server's own migrators, not by hand, so the
//! target files and their migration ledgers are exactly what the server will
//! expect when it opens them. This binary only moves rows.
//!
//! Sources (PostgreSQL schema `pds`) and destinations:
//!
//! | PostgreSQL table                     | SQLite destination        |
//! |--------------------------------------|---------------------------|
//! | `app_password`, `invite_code`,       | `<data>/account.sqlite`   |
//! | `invite_code_use`, `refresh_token`,  | (global account database) |
//! | `repo_root`, `actor`, `account`,     |                           |
//! | `email_token`                        |                           |
//! | `did_doc`                            | `<data>/did_cache.sqlite` |
//! | `repo_seq`                           | `<data>/sequencer.sqlite` |
//! | `repo_root`, `repo_block`, `record`, | `<data>/actors/<sha256(did)[0..2]>/<did>/store.sqlite` |
//! | `blob`, `record_blob`, `backlink`,   |                           |
//! | `account_pref`                       |                           |
//!
//! Blob bytes live in object storage, not the database, and are not touched.
//!
//! Run against a quiesced source and an empty data directory. Re-running into
//! the same directory is not idempotent for tables without a primary-key
//! conflict clause; the caller should clear the destination first.
//!
//! Usage:
//!
//! ```text
//! DATABASE_URL=postgres://user:pass@host/db DATA_DIR=/var/lib/rsky \
//!   migrate-from-postgres [--dry-run]
//! ```

use anyhow::{bail, Context, Result};
use postgres::Client;
use rusqlite::types::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use rsky_pds::db::sqlite::Db;

fn text(value: impl Into<String>) -> Value {
    Value::Text(value.into())
}

fn int(value: i64) -> Value {
    Value::Integer(value)
}

fn optional_text(value: Option<String>) -> Value {
    value.map(Value::Text).unwrap_or(Value::Null)
}

fn optional_int(value: Option<i32>) -> Value {
    value
        .map(|v| Value::Integer(v as i64))
        .unwrap_or(Value::Null)
}

/// Where the actor store places a DID's SQLite file:
/// `<actors>/<first two hex of sha256(did)>/<did>/store.sqlite`.
fn actor_store_path(actors_dir: &Path, did: &str) -> PathBuf {
    let digest = hex::encode(Sha256::digest(did.as_bytes()));
    actors_dir
        .join(&digest[0..2])
        .join(did)
        .join("store.sqlite")
}

/// The DID an `at://` URI belongs to, used to route `backlink` rows that carry
/// no `did` column of their own.
fn did_from_at_uri(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("at://")?;
    let did = rest.split('/').next()?;
    (!did.is_empty()).then_some(did)
}

async fn insert_all(db: &Db, statement: &'static str, rows: Vec<Vec<Value>>) -> Result<()> {
    db.run(move |conn| {
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(statement)?;
            for row in &rows {
                stmt.execute(rusqlite::params_from_iter(row.iter()))?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

struct Source {
    pg: Client,
    dry_run: bool,
}

impl Source {
    async fn count(&mut self, table: &str) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM pds.{table}");
        let row = self
            .pg
            .query_one(&sql, &[])
            .await
            .with_context(|| format!("counting pds.{table}"))?;
        Ok(row.get(0))
    }

    async fn rows(&mut self, sql: &str, table: &str) -> Result<Vec<postgres::Row>> {
        self.pg
            .query(sql, &[])
            .await
            .with_context(|| format!("reading pds.{table}"))
    }

    async fn rows_with(
        &mut self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
        table: &str,
    ) -> Result<Vec<postgres::Row>> {
        self.pg
            .query(sql, params)
            .await
            .with_context(|| format!("reading pds.{table}"))
    }
}

async fn account_rows(src: &mut Source) -> Result<Vec<Vec<Value>>> {
    let rows = src
        .rows(
            "SELECT did, name, password, \"createdAt\" FROM pds.app_password",
            "app_password",
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("name")),
                text(r.get::<_, String>("password")),
                text(r.get::<_, String>("createdAt")),
                int(0), // privileged: absent in the PostgreSQL schema
            ]
        })
        .collect())
}

async fn import_account_database(src: &mut Source, path: &Path) -> Result<()> {
    let app_password: Vec<Vec<Value>> = account_rows(src).await?;

    let invite_code: Vec<Vec<Value>> = src
        .rows(
            "SELECT code, \"availableUses\", disabled, \"forAccount\", \"createdBy\", \"createdAt\" FROM pds.invite_code",
            "invite_code",
        ).await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("code")),
                int(r.get::<_, i32>("availableUses") as i64),
                int(r.get::<_, i16>("disabled") as i64),
                text(r.get::<_, String>("forAccount")),
                text(r.get::<_, String>("createdBy")),
                text(r.get::<_, String>("createdAt")),
            ]
        })
        .collect();

    let invite_code_use: Vec<Vec<Value>> = src
        .rows(
            "SELECT code, \"usedBy\", \"usedAt\" FROM pds.invite_code_use",
            "invite_code_use",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("code")),
                text(r.get::<_, String>("usedBy")),
                text(r.get::<_, String>("usedAt")),
            ]
        })
        .collect();

    let refresh_token: Vec<Vec<Value>> = src
        .rows(
            "SELECT id, did, \"expiresAt\", \"nextId\", \"appPasswordName\" FROM pds.refresh_token",
            "refresh_token",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("id")),
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("expiresAt")),
                optional_text(r.get::<_, Option<String>>("nextId")),
                optional_text(r.get::<_, Option<String>>("appPasswordName")),
            ]
        })
        .collect();

    let repo_root: Vec<Vec<Value>> = src
        .rows(
            "SELECT did, cid, rev, \"indexedAt\" FROM pds.repo_root",
            "repo_root",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("cid")),
                text(r.get::<_, String>("rev")),
                text(r.get::<_, String>("indexedAt")),
            ]
        })
        .collect();

    let actor: Vec<Vec<Value>> = src
        .rows(
            "SELECT did, handle, \"createdAt\", \"takedownRef\" FROM pds.actor",
            "actor",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("did")),
                optional_text(r.get::<_, Option<String>>("handle")),
                text(r.get::<_, String>("createdAt")),
                optional_text(r.get::<_, Option<String>>("takedownRef")),
            ]
        })
        .collect();

    let account: Vec<Vec<Value>> = src
        .rows(
            "SELECT did, email, password, \"emailConfirmedAt\", \"invitesDisabled\" FROM pds.account",
            "account",
        ).await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("email")),
                // `recoveryKey` and `createdAt` have no column in the target
                // schema and are dropped.
                text(r.get::<_, String>("password")),
                optional_text(r.get::<_, Option<String>>("emailConfirmedAt")),
                int(r.get::<_, i16>("invitesDisabled") as i64),
            ]
        })
        .collect();

    let email_token: Vec<Vec<Value>> = src
        .rows(
            "SELECT purpose, did, token, \"requestedAt\" FROM pds.email_token",
            "email_token",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("purpose")),
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("token")),
                text(r.get::<_, String>("requestedAt")),
            ]
        })
        .collect();

    if src.dry_run {
        eprintln!(
            "account.sqlite: app_password={} invite_code={} invite_code_use={} refresh_token={} repo_root={} actor={} account={} email_token={}",
            app_password.len(),
            invite_code.len(),
            invite_code_use.len(),
            refresh_token.len(),
            repo_root.len(),
            actor.len(),
            account.len(),
            email_token.len(),
        );
        return Ok(());
    }

    let db = rsky_pds::account_manager::db::get_migrated_db(path)
        .await
        .context("opening account.sqlite")?;
    insert_all(
        &db,
        "INSERT INTO app_password (did, name, \"passwordScrypt\", \"createdAt\", privileged) VALUES (?, ?, ?, ?, ?)",
        app_password,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO invite_code (code, \"availableUses\", disabled, \"forAccount\", \"createdBy\", \"createdAt\") VALUES (?, ?, ?, ?, ?, ?)",
        invite_code,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO invite_code_use (code, \"usedBy\", \"usedAt\") VALUES (?, ?, ?)",
        invite_code_use,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO refresh_token (id, did, \"expiresAt\", \"nextId\", \"appPasswordName\") VALUES (?, ?, ?, ?, ?)",
        refresh_token,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO repo_root (did, cid, rev, \"indexedAt\") VALUES (?, ?, ?, ?)",
        repo_root,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO actor (did, handle, \"createdAt\", \"takedownRef\") VALUES (?, ?, ?, ?)",
        actor,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO account (did, email, \"passwordScrypt\", \"emailConfirmedAt\", \"invitesDisabled\") VALUES (?, ?, ?, ?, ?)",
        account,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO email_token (purpose, did, token, \"requestedAt\") VALUES (?, ?, ?, ?)",
        email_token,
    )
    .await?;
    Ok(())
}

async fn import_did_cache(src: &mut Source, path: &Path) -> Result<()> {
    let rows = src
        .rows("SELECT did, doc, \"updatedAt\" FROM pds.did_doc", "did_doc")
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("doc")),
                int(r.get::<_, i64>("updatedAt")),
            ]
        })
        .collect::<Vec<_>>();
    if src.dry_run {
        eprintln!("did_cache.sqlite: did_doc={}", rows.len());
        return Ok(());
    }
    let db = rsky_pds::did_cache::get_migrated_db(path)
        .await
        .context("opening did_cache.sqlite")?;
    insert_all(
        &db,
        "INSERT INTO did_doc (did, doc, \"updatedAt\") VALUES (?, ?, ?)",
        rows,
    )
    .await
}

async fn import_sequencer(src: &mut Source, path: &Path) -> Result<()> {
    let rows = src
        .rows(
            "SELECT seq, did, \"eventType\", event, invalidated, \"sequencedAt\" FROM pds.repo_seq",
            "repo_seq",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                int(r.get::<_, i64>("seq")),
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("eventType")),
                Value::Blob(r.get::<_, Vec<u8>>("event")),
                int(r.get::<_, i16>("invalidated") as i64),
                text(r.get::<_, String>("sequencedAt")),
            ]
        })
        .collect::<Vec<_>>();
    if src.dry_run {
        eprintln!("sequencer.sqlite: repo_seq={}", rows.len());
        return Ok(());
    }
    let db = rsky_pds::sequencer::db::get_migrated_db(path)
        .await
        .context("opening sequencer.sqlite")?;
    insert_all(
        &db,
        "INSERT INTO repo_seq (seq, did, \"eventType\", event, invalidated, \"sequencedAt\") VALUES (?, ?, ?, ?, ?, ?)",
        rows,
    )
    .await
}

async fn actor_dids(src: &mut Source) -> Result<Vec<String>> {
    let rows = src
        .rows(
            "SELECT did FROM pds.actor UNION SELECT did FROM pds.repo_root UNION SELECT did FROM pds.record",
            "actor dids",
        )
        .await?;
    let mut dids: Vec<String> = rows.iter().map(|r| r.get::<_, String>("did")).collect();
    dids.sort();
    dids.dedup();
    Ok(dids)
}

async fn import_actor_store(src: &mut Source, actors_dir: &Path, did: &str) -> Result<()> {
    let path = actor_store_path(actors_dir, did);

    let repo_root: Vec<Vec<Value>> = src
        .rows_with(
            "SELECT did, cid, rev, \"indexedAt\" FROM pds.repo_root WHERE did = $1",
            &[&did],
            "repo_root",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("did")),
                text(r.get::<_, String>("cid")),
                text(r.get::<_, String>("rev")),
                text(r.get::<_, String>("indexedAt")),
            ]
        })
        .collect::<Vec<_>>();

    let repo_block: Vec<Vec<Value>> = src
        .rows_with(
            "SELECT cid, \"repoRev\", size, content FROM pds.repo_block WHERE did = $1",
            &[&did],
            "repo_block",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("cid")),
                text(r.get::<_, String>("repoRev")),
                int(r.get::<_, i32>("size") as i64),
                Value::Blob(r.get::<_, Vec<u8>>("content")),
            ]
        })
        .collect::<Vec<_>>();

    let record: Vec<Vec<Value>> = src
        .rows_with(
            "SELECT uri, cid, collection, rkey, \"repoRev\", \"indexedAt\", \"takedownRef\" FROM pds.record WHERE did = $1",
            &[&did],
            "record",
        ).await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("uri")),
                text(r.get::<_, String>("cid")),
                text(r.get::<_, String>("collection")),
                text(r.get::<_, String>("rkey")),
                // Target column is NOT NULL; a null source revision becomes "".
                text(r.get::<_, Option<String>>("repoRev").unwrap_or_default()),
                text(r.get::<_, String>("indexedAt")),
                optional_text(r.get::<_, Option<String>>("takedownRef")),
            ]
        })
        .collect::<Vec<_>>();

    let blob: Vec<Vec<Value>> = src
        .rows_with(
            "SELECT cid, \"mimeType\", size, \"tempKey\", width, height, \"createdAt\", \"takedownRef\" FROM pds.blob WHERE did = $1",
            &[&did],
            "blob",
        ).await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("cid")),
                text(r.get::<_, String>("mimeType")),
                int(r.get::<_, i32>("size") as i64),
                optional_text(r.get::<_, Option<String>>("tempKey")),
                optional_int(r.get::<_, Option<i32>>("width")),
                optional_int(r.get::<_, Option<i32>>("height")),
                text(r.get::<_, String>("createdAt")),
                optional_text(r.get::<_, Option<String>>("takedownRef")),
            ]
        })
        .collect::<Vec<_>>();

    let record_blob: Vec<Vec<Value>> = src
        .rows_with(
            "SELECT \"blobCid\", \"recordUri\" FROM pds.record_blob WHERE did = $1",
            &[&did],
            "record_blob",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("blobCid")),
                text(r.get::<_, String>("recordUri")),
            ]
        })
        .collect::<Vec<_>>();

    // `backlink` carries no `did`; select by the `at://<did>/` URI prefix.
    let backlink_prefix = format!("at://{did}/%");
    let backlink: Vec<Vec<Value>> = src
        .pg
        .query(
            "SELECT uri, path, \"linkTo\" FROM pds.backlink WHERE uri LIKE $1",
            &[&backlink_prefix],
        )
        .await
        .context("reading pds.backlink")?
        .iter()
        .filter(|r| {
            did_from_at_uri(&r.get::<_, String>("uri"))
                .map(|d| d == did)
                .unwrap_or(false)
        })
        .map(|r| {
            vec![
                text(r.get::<_, String>("uri")),
                text(r.get::<_, String>("path")),
                text(r.get::<_, String>("linkTo")),
            ]
        })
        .collect::<Vec<_>>();

    let account_pref: Vec<Vec<Value>> = src
        .rows_with(
            "SELECT name, \"valueJson\" FROM pds.account_pref WHERE did = $1",
            &[&did],
            "account_pref",
        )
        .await?
        .iter()
        .map(|r| {
            vec![
                text(r.get::<_, String>("name")),
                text(r.get::<_, Option<String>>("valueJson").unwrap_or_default()),
            ]
        })
        .collect::<Vec<_>>();

    if src.dry_run {
        eprintln!(
            "{did}: repo_root={} repo_block={} record={} blob={} record_blob={} backlink={} account_pref={}",
            repo_root.len(),
            repo_block.len(),
            record.len(),
            blob.len(),
            record_blob.len(),
            backlink.len(),
            account_pref.len(),
        );
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let db = rsky_pds::actor_store::db::get_migrated_db(&path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    insert_all(
        &db,
        "INSERT INTO repo_root (did, cid, rev, \"indexedAt\") VALUES (?, ?, ?, ?)",
        repo_root,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO repo_block (cid, \"repoRev\", size, content) VALUES (?, ?, ?, ?)",
        repo_block,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO record (uri, cid, collection, rkey, \"repoRev\", \"indexedAt\", \"takedownRef\") VALUES (?, ?, ?, ?, ?, ?, ?)",
        record,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO blob (cid, \"mimeType\", size, \"tempKey\", width, height, \"createdAt\", \"takedownRef\") VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        blob,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO record_blob (\"blobCid\", \"recordUri\") VALUES (?, ?)",
        record_blob,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO backlink (uri, path, \"linkTo\") VALUES (?, ?, ?)",
        backlink,
    )
    .await?;
    insert_all(
        &db,
        "INSERT INTO account_pref (name, \"valueJson\") VALUES (?, ?)",
        account_pref,
    )
    .await?;
    Ok(())
}

fn data_directory() -> PathBuf {
    std::env::var("DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must point at the source PostgreSQL database")?;
    let dry_run = std::env::args().any(|a| a == "--dry-run");
    let data_dir = data_directory();
    if !dry_run && !data_dir.is_dir() {
        bail!("DATA_DIR {} is not a directory", data_dir.display());
    }

    let (pg, connection) = postgres::connect(&database_url, postgres::NoTls)
        .await
        .context("connecting to the source PostgreSQL database")?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("source PostgreSQL connection error: {error}");
        }
    });
    let mut src = Source { pg, dry_run };

    // Report the source size before touching anything.
    for table in [
        "app_password",
        "invite_code",
        "invite_code_use",
        "refresh_token",
        "actor",
        "account",
        "email_token",
        "repo_root",
        "repo_block",
        "record",
        "blob",
        "record_blob",
        "backlink",
        "account_pref",
        "did_doc",
        "repo_seq",
    ] {
        let count = src.count(table).await?;
        eprintln!("source pds.{table}: {count} rows");
    }

    import_account_database(&mut src, &data_dir.join("account.sqlite")).await?;
    import_did_cache(&mut src, &data_dir.join("did_cache.sqlite")).await?;
    import_sequencer(&mut src, &data_dir.join("sequencer.sqlite")).await?;

    let dids = actor_dids(&mut src).await?;
    eprintln!("actor stores: {} DIDs", dids.len());
    for did in dids {
        import_actor_store(&mut src, &data_dir.join("actors"), &did).await?;
    }

    if dry_run {
        eprintln!("dry run complete; no files written");
    } else {
        eprintln!("import complete");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_store_path_shards_by_did_hash() {
        // sha256("did:plc:test")[0..2] is deterministic; assert the shape.
        let path = actor_store_path(Path::new("/data/actors"), "did:plc:test");
        let expected_prefix = hex::encode(Sha256::digest(b"did:plc:test"));
        assert_eq!(
            path,
            Path::new("/data/actors")
                .join(&expected_prefix[0..2])
                .join("did:plc:test")
                .join("store.sqlite")
        );
    }

    #[test]
    fn did_from_at_uri_extracts_authority() {
        assert_eq!(
            did_from_at_uri("at://did:plc:abc/app.bsky.feed.post/3k"),
            Some("did:plc:abc")
        );
        assert_eq!(did_from_at_uri("https://example.com"), None);
        assert_eq!(did_from_at_uri("at://"), None);
    }
}
