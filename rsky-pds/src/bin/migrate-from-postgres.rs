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
//! | `app_password`, `invite_code`,       | account database          |
//! | `invite_code_use`, `refresh_token`,  |                           |
//! | `repo_root`, `actor`, `account`,     |                           |
//! | `email_token`                        |                           |
//! | `did_doc`                            | DID cache database        |
//! | `repo_seq`                           | sequencer database        |
//! | `repo_root`, `repo_block`, `record`, | `<actors>/<sha256(did)[0..2]>/<did>/store.sqlite` |
//! | `blob`, `record_blob`, `backlink`,   |                           |
//! | `account_pref`                       |                           |
//!
//! Destination paths resolve exactly as the server resolves them: from
//! `PDS_DATA_DIRECTORY` and the `PDS_*_DB_LOCATION` /
//! `PDS_ACTOR_STORE_DIRECTORY` overrides.
//!
//! Every PostgreSQL-era account signed with the shared repo signing key, so
//! each imported actor store gets that key in its `key` file. `rotate-keys`
//! later moves accounts onto their own keys.
//!
//! Blob bytes live in object storage, not the database, and are not touched.
//!
//! Run against a quiesced source and an empty destination. An actor store that
//! already exists is refused; the other databases are not idempotent for
//! tables without a primary-key conflict clause, so clear them first.
//!
//! Usage:
//!
//! ```text
//! DATABASE_URL=postgres://user:pass@host/db PDS_DATA_DIRECTORY=/var/lib/rsky \
//!   PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX=... \
//!   migrate-from-postgres [--dry-run]
//! ```
//!
//! `--dry-run` reads the source and reports per-table counts without the
//! signing key and without creating any file.

use anyhow::{bail, Context, Result};
use postgres::Client;
use rusqlite::types::Value;
use secp256k1::{Keypair, Secp256k1, SecretKey};
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

/// A destination table and the columns the importer fills, in row order.
struct Spec {
    name: &'static str,
    columns: &'static [&'static str],
}

impl Spec {
    fn insert_sql(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders = vec!["?"; self.columns.len()].join(", ");
        format!(
            "INSERT INTO {} ({columns}) VALUES ({placeholders})",
            self.name
        )
    }
}

const APP_PASSWORD: Spec = Spec {
    name: "app_password",
    columns: &["did", "name", "passwordScrypt", "createdAt", "privileged"],
};
const INVITE_CODE: Spec = Spec {
    name: "invite_code",
    columns: &[
        "code",
        "availableUses",
        "disabled",
        "forAccount",
        "createdBy",
        "createdAt",
    ],
};
const INVITE_CODE_USE: Spec = Spec {
    name: "invite_code_use",
    columns: &["code", "usedBy", "usedAt"],
};
const REFRESH_TOKEN: Spec = Spec {
    name: "refresh_token",
    columns: &["id", "did", "expiresAt", "nextId", "appPasswordName"],
};
const REPO_ROOT: Spec = Spec {
    name: "repo_root",
    columns: &["did", "cid", "rev", "indexedAt"],
};
const ACTOR: Spec = Spec {
    name: "actor",
    columns: &[
        "did",
        "handle",
        "createdAt",
        "takedownRef",
        "deactivatedAt",
        "deleteAfter",
    ],
};
const ACCOUNT: Spec = Spec {
    name: "account",
    columns: &[
        "did",
        "email",
        "passwordScrypt",
        "emailConfirmedAt",
        "invitesDisabled",
    ],
};
const EMAIL_TOKEN: Spec = Spec {
    name: "email_token",
    columns: &["purpose", "did", "token", "requestedAt"],
};
const DID_DOC: Spec = Spec {
    name: "did_doc",
    columns: &["did", "doc", "updatedAt"],
};
const REPO_SEQ: Spec = Spec {
    name: "repo_seq",
    columns: &[
        "seq",
        "did",
        "eventType",
        "event",
        "invalidated",
        "sequencedAt",
    ],
};
const REPO_BLOCK: Spec = Spec {
    name: "repo_block",
    columns: &["cid", "repoRev", "size", "content"],
};
const RECORD: Spec = Spec {
    name: "record",
    columns: &[
        "uri",
        "cid",
        "collection",
        "rkey",
        "repoRev",
        "indexedAt",
        "takedownRef",
    ],
};
const BLOB: Spec = Spec {
    name: "blob",
    columns: &[
        "cid",
        "mimeType",
        "size",
        "tempKey",
        "width",
        "height",
        "createdAt",
        "takedownRef",
    ],
};
const RECORD_BLOB: Spec = Spec {
    name: "record_blob",
    columns: &["blobCid", "recordUri"],
};
const BACKLINK: Spec = Spec {
    name: "backlink",
    columns: &["uri", "path", "linkTo"],
};
const ACCOUNT_PREF: Spec = Spec {
    name: "account_pref",
    columns: &["name", "valueJson"],
};

/// Rows read from the source for one destination table, held in memory so a
/// dry run can report them without opening any destination.
struct Table {
    spec: &'static Spec,
    rows: Vec<Vec<Value>>,
}

fn summary(tables: &[Table]) -> String {
    tables
        .iter()
        .map(|t| format!("{}={}", t.spec.name, t.rows.len()))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn write_tables(db: &Db, tables: Vec<Table>) -> Result<()> {
    db.run(move |conn| {
        let tx = conn.transaction()?;
        for table in &tables {
            let mut stmt = tx.prepare(&table.spec.insert_sql())?;
            for row in &table.rows {
                stmt.execute(rusqlite::params_from_iter(row.iter()))?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

/// The actor store directory for a DID, as `ActorStore::get_location`
/// computes it: `<actors>/<first two hex of sha256(did)>/<did>`.
fn actor_directory(actors_dir: &Path, did: &str) -> Result<PathBuf> {
    if did.is_empty()
        || did.starts_with('.')
        || did.contains('/')
        || did.contains('\\')
        || did.contains("..")
    {
        bail!("refusing unsafe DID as a path part: {did}");
    }
    let digest = hex::encode(Sha256::digest(did.as_bytes()));
    Ok(actors_dir.join(&digest[0..2]).join(did))
}

/// The DID an `at://` URI belongs to, used to route `backlink` rows that carry
/// no `did` column of their own.
fn did_from_at_uri(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("at://")?;
    let did = rest.split('/').next()?;
    (!did.is_empty()).then_some(did)
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}

/// Where the server will open each database, resolved from its own storage
/// configuration.
struct Layout {
    actors_dir: PathBuf,
    account_db: PathBuf,
    did_cache_db: PathBuf,
    sequencer_db: PathBuf,
}

impl Layout {
    fn from_env() -> Self {
        let (actor_store, service_db) = rsky_pds::config::storage_cfg_from_env();
        Layout {
            actors_dir: actor_store.directory.into(),
            account_db: service_db.account_db_location.into(),
            did_cache_db: service_db.did_cache_db_location.into(),
            sequencer_db: service_db.sequencer_db_location.into(),
        }
    }
}

fn signing_key_from_env() -> Result<Keypair> {
    const VAR: &str = "PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX";
    let encoded = std::env::var(VAR).with_context(|| {
        format!("{VAR} must hold the repo signing key the PostgreSQL deployment used")
    })?;
    let bytes = hex::decode(encoded.trim()).with_context(|| format!("{VAR} is not hex"))?;
    let secret =
        SecretKey::from_slice(&bytes).with_context(|| format!("{VAR} is not a k256 key"))?;
    Ok(Keypair::from_secret_key(&Secp256k1::new(), &secret))
}

/// Writes imported tables into the server's layout.
struct Writer {
    layout: Layout,
    signing_key: Keypair,
}

impl Writer {
    async fn account_database(&self, tables: Vec<Table>) -> Result<()> {
        let path = &self.layout.account_db;
        create_parent(path)?;
        let db = rsky_pds::account_manager::db::get_migrated_db(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        write_tables(&db, tables).await
    }

    async fn did_cache(&self, tables: Vec<Table>) -> Result<()> {
        let path = &self.layout.did_cache_db;
        create_parent(path)?;
        let db = rsky_pds::did_cache::get_migrated_db(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        write_tables(&db, tables).await
    }

    async fn sequencer(&self, tables: Vec<Table>) -> Result<()> {
        let path = &self.layout.sequencer_db;
        create_parent(path)?;
        let db = rsky_pds::sequencer::db::get_migrated_db(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        write_tables(&db, tables).await
    }

    /// Creates the actor store the way `ActorStore::create` does: the key
    /// file, then the migrated database.
    async fn actor_store(&self, did: &str, tables: Vec<Table>) -> Result<()> {
        let directory = actor_directory(&self.layout.actors_dir, did)?;
        let db_path = directory.join("store.sqlite");
        if db_path.exists() {
            bail!("actor store already exists: {}", db_path.display());
        }
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        let key_path = directory.join("key");
        std::fs::write(&key_path, self.signing_key.secret_bytes())
            .with_context(|| format!("writing {}", key_path.display()))?;
        let db = rsky_pds::actor_store::db::get_migrated_db(&db_path)
            .await
            .with_context(|| format!("opening {}", db_path.display()))?;
        write_tables(&db, tables).await
    }
}

struct Source {
    pg: Client,
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

async fn read_account_database(src: &mut Source) -> Result<Vec<Table>> {
    let app_password = src
        .rows(
            "SELECT did, name, password, \"createdAt\" FROM pds.app_password",
            "app_password",
        )
        .await?
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
        .collect();

    let invite_code = src
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

    let invite_code_use = src
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

    let refresh_token = src
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

    let repo_root = src
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

    let actor = src
        .rows(
            "SELECT did, handle, \"createdAt\", \"takedownRef\", \"deactivatedAt\", \"deleteAfter\" FROM pds.actor",
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
                optional_text(r.get::<_, Option<String>>("deactivatedAt")),
                optional_text(r.get::<_, Option<String>>("deleteAfter")),
            ]
        })
        .collect();

    let account = src
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

    let email_token = src
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

    Ok(vec![
        Table {
            spec: &APP_PASSWORD,
            rows: app_password,
        },
        Table {
            spec: &INVITE_CODE,
            rows: invite_code,
        },
        Table {
            spec: &INVITE_CODE_USE,
            rows: invite_code_use,
        },
        Table {
            spec: &REFRESH_TOKEN,
            rows: refresh_token,
        },
        Table {
            spec: &REPO_ROOT,
            rows: repo_root,
        },
        Table {
            spec: &ACTOR,
            rows: actor,
        },
        Table {
            spec: &ACCOUNT,
            rows: account,
        },
        Table {
            spec: &EMAIL_TOKEN,
            rows: email_token,
        },
    ])
}

async fn read_did_cache(src: &mut Source) -> Result<Vec<Table>> {
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
        .collect();
    Ok(vec![Table {
        spec: &DID_DOC,
        rows,
    }])
}

async fn read_sequencer(src: &mut Source) -> Result<Vec<Table>> {
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
        .collect();
    Ok(vec![Table {
        spec: &REPO_SEQ,
        rows,
    }])
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

async fn read_actor_store(src: &mut Source, did: &str) -> Result<Vec<Table>> {
    let repo_root = src
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
        .collect();

    let repo_block = src
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
        .collect();

    let record = src
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
        .collect();

    let blob = src
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
        .collect();

    let record_blob = src
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
        .collect();

    // `backlink` carries no `did`; select by the `at://<did>/` URI prefix.
    let backlink_prefix = format!("at://{did}/%");
    let backlink = src
        .rows_with(
            "SELECT uri, path, \"linkTo\" FROM pds.backlink WHERE uri LIKE $1",
            &[&backlink_prefix],
            "backlink",
        )
        .await?
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
        .collect();

    // Ordered by the source id so the preferences keep their order.
    let account_pref = src
        .rows_with(
            "SELECT name, \"valueJson\" FROM pds.account_pref WHERE did = $1 ORDER BY id",
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
        .collect();

    Ok(vec![
        Table {
            spec: &REPO_ROOT,
            rows: repo_root,
        },
        Table {
            spec: &REPO_BLOCK,
            rows: repo_block,
        },
        Table {
            spec: &RECORD,
            rows: record,
        },
        Table {
            spec: &BLOB,
            rows: blob,
        },
        Table {
            spec: &RECORD_BLOB,
            rows: record_blob,
        },
        Table {
            spec: &BACKLINK,
            rows: backlink,
        },
        Table {
            spec: &ACCOUNT_PREF,
            rows: account_pref,
        },
    ])
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must point at the source PostgreSQL database")?;
    let dry_run = std::env::args().any(|a| a == "--dry-run");
    let layout = Layout::from_env();
    eprintln!(
        "destination: account={} did_cache={} sequencer={} actors={}",
        layout.account_db.display(),
        layout.did_cache_db.display(),
        layout.sequencer_db.display(),
        layout.actors_dir.display(),
    );
    let writer = if dry_run {
        None
    } else {
        Some(Writer {
            layout,
            signing_key: signing_key_from_env()?,
        })
    };

    let (pg, connection) = postgres::connect(&database_url, postgres::NoTls)
        .await
        .context("connecting to the source PostgreSQL database")?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("source PostgreSQL connection error: {error}");
        }
    });
    let mut src = Source { pg };

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

    let tables = read_account_database(&mut src).await?;
    match &writer {
        Some(writer) => writer.account_database(tables).await?,
        None => eprintln!("account database: {}", summary(&tables)),
    }
    let tables = read_did_cache(&mut src).await?;
    match &writer {
        Some(writer) => writer.did_cache(tables).await?,
        None => eprintln!("did cache: {}", summary(&tables)),
    }
    let tables = read_sequencer(&mut src).await?;
    match &writer {
        Some(writer) => writer.sequencer(tables).await?,
        None => eprintln!("sequencer: {}", summary(&tables)),
    }

    let dids = actor_dids(&mut src).await?;
    eprintln!("actor stores: {} DIDs", dids.len());
    for did in dids {
        let tables = read_actor_store(&mut src, &did).await?;
        match &writer {
            Some(writer) => writer.actor_store(&did, tables).await?,
            None => eprintln!("{did}: {}", summary(&tables)),
        }
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
    use rsky_pds::actor_store::blobstore::MemoryBlobStore;
    use rsky_pds::actor_store::ActorStore;
    use rsky_pds::background::BackgroundQueue;
    use rsky_pds::config::ActorStoreConfig;
    use rsky_pds::lifecycle::LifecycleStore;
    use rsky_repo::types::{PreparedCreateOrUpdate, PreparedWrite, WriteOpAction};
    use std::sync::Arc;

    const SHARED_KEY_HEX: &str = "1d2f8064213bd212453fa93943c084dbbf42104d02f1f02b23a638f9a48f925a";
    const DID: &str = "did:plc:importeraaaaaaaaaaaaaaaaa";

    fn shared_key() -> Keypair {
        let secret = SecretKey::from_slice(&hex::decode(SHARED_KEY_HEX).unwrap()).unwrap();
        Keypair::from_secret_key(&Secp256k1::new(), &secret)
    }

    async fn actor_store(root: &Path, actors_dir: &Path) -> ActorStore {
        let cfg = ActorStoreConfig {
            directory: actors_dir.to_string_lossy().to_string(),
            cache_size: 10,
        };
        let lifecycle = LifecycleStore::open(root.join("rsky/lifecycle.sqlite"))
            .await
            .unwrap();
        ActorStore::new(&cfg, BackgroundQueue::default(), lifecycle)
    }

    fn post(rkey: &str, text: &str) -> PreparedCreateOrUpdate {
        let record: rsky_repo::types::RepoRecord = serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": text,
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap();
        let cid = rsky_common::ipld::cid_for_cbor(&record).unwrap();
        PreparedCreateOrUpdate {
            action: WriteOpAction::Create,
            uri: format!("at://{DID}/app.bsky.feed.post/{rkey}"),
            cid,
            swap_cid: None,
            record,
            blobs: vec![],
        }
    }

    /// Stands in for the PostgreSQL source: reads the importer's columns of
    /// each table back out of an existing SQLite file.
    async fn source_tables(path: &Path, specs: &[&'static Spec]) -> Vec<Table> {
        let db = Db::open(path).unwrap();
        let mut tables = Vec::new();
        for spec in specs {
            let sql = format!(
                "SELECT {} FROM {}",
                spec.columns
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                spec.name
            );
            let width = spec.columns.len();
            let rows = db
                .run(move |conn| {
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt
                        .query_map([], |row| {
                            (0..width)
                                .map(|i| row.get::<_, Value>(i))
                                .collect::<rusqlite::Result<Vec<_>>>()
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .await
                .unwrap();
            tables.push(Table { spec, rows });
        }
        tables
    }

    fn writer(root: &Path) -> Writer {
        Writer {
            layout: Layout {
                actors_dir: root.join("actors"),
                account_db: root.join("nested/account.sqlite"),
                did_cache_db: root.join("nested/did_cache.sqlite"),
                sequencer_db: root.join("nested/sequencer.sqlite"),
            },
            signing_key: shared_key(),
        }
    }

    /// An imported actor store opens through `ActorStore` with the shared
    /// signing key, keeps its records, and accepts a new write.
    #[tokio::test]
    async fn imported_actor_store_opens_and_accepts_a_write() {
        // A real repository, built by the server, is the source data.
        let source_dir = tempfile::tempdir().unwrap();
        let source_actors = source_dir.path().join("actors");
        let source = actor_store(source_dir.path(), &source_actors).await;
        let blobs = Arc::new(MemoryBlobStore::default());
        source.create(DID, &shared_key()).await.unwrap();
        let first = post("3jt5vlkoraa2a", "before the import");
        {
            let mut txn = source
                .transact(DID.to_owned(), blobs.clone())
                .await
                .unwrap();
            txn.create_repo(vec![], true).await.unwrap();
            txn.process_writes(vec![PreparedWrite::Create(first.clone())], None)
                .await
                .unwrap();
        }
        let source_db = actor_directory(&source_actors, DID)
            .unwrap()
            .join("store.sqlite");
        let tables = source_tables(
            &source_db,
            &[
                &REPO_ROOT,
                &REPO_BLOCK,
                &RECORD,
                &BLOB,
                &RECORD_BLOB,
                &BACKLINK,
                &ACCOUNT_PREF,
            ],
        )
        .await;
        drop(source);

        let dest_dir = tempfile::tempdir().unwrap();
        let writer = writer(dest_dir.path());
        writer.actor_store(DID, tables).await.unwrap();

        let dest = actor_store(dest_dir.path(), &writer.layout.actors_dir).await;
        assert!(dest.exists(DID).await.unwrap());
        assert_eq!(
            dest.keypair(DID).await.unwrap().secret_bytes(),
            shared_key().secret_bytes()
        );
        let mut txn = dest.transact(DID.to_owned(), blobs.clone()).await.unwrap();
        let kept = txn
            .record
            .get_record(&first.uri.clone().try_into().unwrap(), None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.cid, first.cid.to_string());
        let second = post("3jt5vlkorbb2b", "after the import");
        let commit = txn
            .process_writes(vec![PreparedWrite::Create(second.clone())], None)
            .await
            .unwrap();
        assert_eq!(commit.ops.len(), 1);
        assert!(commit.prev_data.is_some());

        // A second import into the same store is refused rather than merged.
        let again = writer.actor_store(DID, Vec::new()).await.unwrap_err();
        assert!(again.to_string().contains("already exists"), "{again}");
    }

    /// The service databases open under a missing parent directory, and the
    /// actor's deactivation state and PostgreSQL-era password survive.
    #[tokio::test]
    async fn service_databases_import_into_the_server_layout() {
        // An Argon2id hash of "secret", as the PostgreSQL-era server wrote.
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$\
14ukWqiThj4Xz77NYv01V28GbBZHY9AaZwsFswQFO0U";

        let dest_dir = tempfile::tempdir().unwrap();
        let writer = writer(dest_dir.path());
        writer
            .account_database(vec![
                Table {
                    spec: &ACTOR,
                    rows: vec![vec![
                        text(DID),
                        text("alice.test"),
                        text("2024-01-01T00:00:00.000Z"),
                        Value::Null,
                        text("2024-06-01T00:00:00.000Z"),
                        text("2024-09-01T00:00:00.000Z"),
                    ]],
                },
                Table {
                    spec: &ACCOUNT,
                    rows: vec![vec![
                        text(DID),
                        text("alice@example.com"),
                        text(hash),
                        Value::Null,
                        int(0),
                    ]],
                },
            ])
            .await
            .unwrap();
        writer
            .did_cache(vec![Table {
                spec: &DID_DOC,
                rows: vec![vec![text(DID), text("{}"), int(1)]],
            }])
            .await
            .unwrap();
        writer
            .sequencer(vec![Table {
                spec: &REPO_SEQ,
                rows: vec![vec![
                    int(7),
                    text(DID),
                    text("append"),
                    Value::Blob(vec![1, 2, 3]),
                    int(0),
                    text("2024-01-01T00:00:00.000Z"),
                ]],
            }])
            .await
            .unwrap();

        let account_db = rsky_pds::account_manager::db::get_migrated_db(&writer.layout.account_db)
            .await
            .unwrap();
        assert!(
            rsky_pds::account_manager::helpers::password::verify_account_password(
                DID,
                &"secret".to_string(),
                &account_db
            )
            .await
            .unwrap()
        );
        let deactivation = source_tables(&writer.layout.account_db, &[&ACTOR]).await;
        assert_eq!(deactivation[0].rows[0][4], text("2024-06-01T00:00:00.000Z"));
        assert_eq!(deactivation[0].rows[0][5], text("2024-09-01T00:00:00.000Z"));
        let did_docs = source_tables(&writer.layout.did_cache_db, &[&DID_DOC]).await;
        assert_eq!(did_docs[0].rows.len(), 1);
        let events = source_tables(&writer.layout.sequencer_db, &[&REPO_SEQ]).await;
        assert_eq!(events[0].rows[0][0], int(7));
    }

    #[test]
    fn insert_sql_quotes_every_column() {
        assert_eq!(
            INVITE_CODE_USE.insert_sql(),
            "INSERT INTO invite_code_use (\"code\", \"usedBy\", \"usedAt\") VALUES (?, ?, ?)"
        );
    }

    #[test]
    fn actor_directory_shards_by_did_hash() {
        let dir = actor_directory(Path::new("/data/actors"), "did:plc:test").unwrap();
        let expected_prefix = hex::encode(Sha256::digest(b"did:plc:test"));
        assert_eq!(
            dir,
            Path::new("/data/actors")
                .join(&expected_prefix[0..2])
                .join("did:plc:test")
        );
        assert!(actor_directory(Path::new("/data/actors"), "../escape").is_err());
        assert!(actor_directory(Path::new("/data/actors"), "").is_err());
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
