// based on https://github.com/bluesky-social/atproto/blob/main/packages/pds/src/sequencer/db

use crate::db::migrator::{migrate_to_latest, Migration, MigrationSet};
use crate::db::sqlite::{Db, Synchronous};
use anyhow::Result;
use std::path::Path;

pub type SequencerDb = Db;

pub const SEQUENCER_DB_MIGRATIONS_SET: MigrationSet = MigrationSet {
    shared: SEQUENCER_DB_MIGRATIONS,
    local: &[],
    legacy: None,
};

pub const SEQUENCER_DB_MIGRATIONS: &[Migration] = &[Migration {
    name: "001",
    sql: "\
    CREATE TABLE repo_seq (\
        seq INTEGER PRIMARY KEY AUTOINCREMENT, \
        did TEXT NOT NULL, \
        \"eventType\" TEXT NOT NULL, \
        event BLOB NOT NULL, \
        invalidated INTEGER NOT NULL DEFAULT 0, \
        \"sequencedAt\" TEXT NOT NULL\
    );\
    CREATE INDEX repo_seq_did_idx ON repo_seq (did);\
    CREATE INDEX repo_seq_event_type_idx ON repo_seq (\"eventType\");\
    CREATE INDEX repo_seq_sequenced_at_index ON repo_seq (\"sequencedAt\");",
}];

pub fn get_db(location: impl AsRef<Path>) -> Result<SequencerDb> {
    Db::open(location)
}

/// The sequencer is the publication record; an event it has returned a
/// sequence number for must survive a power loss.
pub async fn get_migrated_db(location: impl AsRef<Path>) -> Result<SequencerDb> {
    let db = Db::open_with(location, Synchronous::Full)?;
    migrate_to_latest(&db, SEQUENCER_DB_MIGRATIONS_SET).await?;
    Ok(db)
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
