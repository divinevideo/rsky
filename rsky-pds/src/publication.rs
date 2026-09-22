//! Delivery of committed publication intents to the sequencer.
//!
//! A write commits its intent with its blocks (see `ActorStoreTransactor`),
//! and the publisher turns each pending intent into exactly one sequencer
//! row: it records the sequencer's head before inserting, so after a crash
//! it can recognise a row it already inserted instead of inserting it again,
//! and it acknowledges the row in the store only after the sequencer has
//! made it durable.

use crate::account_manager::AccountManager;
use crate::actor_store::blob::BlobReader;
use crate::actor_store::db::ActorDb;
use crate::actor_store::repo::sql_repo::SqlRepoReader;
use crate::actor_store::{pending_intents_in, ActorStore, StoredIntent};
use crate::lifecycle::LifecycleStore;
use crate::models::models::RepoSeq;
use crate::sequencer::events::{CommitEvt, SyncEvt};
use crate::SharedSequencer;
use anyhow::Result;
use rsky_common::cbor_to_struct;
use rusqlite::params;

/// The moves of a delivery, in order; tests stop after one of them to stand
/// in for a crash there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStep {
    /// The sequencer head was recorded on the intent.
    Floored,
    /// The row was inserted into the sequencer.
    Sequenced,
    /// The row was acknowledged in the store.
    Acked,
}

/// Whether a sequencer row is the delivery of `intent`.
fn row_matches(row: &RepoSeq, intent: &StoredIntent) -> bool {
    if row.event_type != intent.event_type {
        return false;
    }
    match row.event_type.as_str() {
        "append" => cbor_to_struct::<CommitEvt>(row.event.clone())
            .map(|evt| evt.rev == intent.rev && evt.commit.to_string() == intent.cid)
            .unwrap_or(false),
        "sync" => cbor_to_struct::<SyncEvt>(row.event.clone())
            .map(|evt| evt.rev == intent.rev)
            .unwrap_or(false),
        _ => row.event == intent.event,
    }
}

async fn record_floor(db: &ActorDb, intent_id: i64, floor: i64) -> Result<()> {
    db.run(move |conn| {
        conn.execute(
            "UPDATE publish_intent SET \"seqFloor\" = ?1 WHERE id = ?2 AND \"seqFloor\" IS NULL",
            params![floor, intent_id],
        )?;
        Ok(())
    })
    .await
}

async fn acknowledge(db: &ActorDb, intent_id: i64, seq: i64) -> Result<()> {
    let now = rsky_common::now();
    db.tx(move |tx| {
        tx.execute(
            "UPDATE publish_intent SET state = 'delivered', seq = ?1 WHERE id = ?2",
            params![seq, intent_id],
        )?;
        tx.execute(
            "INSERT INTO publish_ack (\"intentId\", seq, \"ackedAt\") VALUES (?1, ?2, ?3) \
             ON CONFLICT (\"intentId\") DO NOTHING",
            params![intent_id, seq, now],
        )?;
        Ok(())
    })
    .await
}

/// Delivers every pending intent of `did`, oldest first, and returns the
/// sequence numbers they received. An intent whose row already exists from
/// an earlier attempt is acknowledged against that row.
pub async fn publish_pending(
    actor_store: &ActorStore,
    sequencer: &SharedSequencer,
    account_manager: &AccountManager,
    did: &str,
    stop_after: Option<PublishStep>,
) -> Result<Vec<i64>> {
    let _guard = actor_store.publish_lock(did).lock_owned().await;
    actor_store.admission.admit_worker(did)?;
    if !actor_store.exists(did).await? {
        actor_store.lifecycle.clear_pending_work(did).await?;
        return Ok(vec![]);
    }
    let db = actor_store.write_db(did).await?;
    let intents = db.run(|conn| pending_intents_in(conn)).await?;
    let mut seqs = Vec::with_capacity(intents.len());
    for intent in intents {
        let floor = match intent.seq_floor {
            Some(floor) => floor,
            None => {
                let head = sequencer.sequencer.read().await.curr().await?.unwrap_or(0);
                record_floor(&db, intent.id, head).await?;
                head
            }
        };
        if stop_after == Some(PublishStep::Floored) {
            return Ok(seqs);
        }
        let already = sequencer
            .sequencer
            .read()
            .await
            .rows_for_did_after(did, floor)
            .await?
            .into_iter()
            .find(|row| row_matches(row, &intent))
            .and_then(|row| row.seq);
        let seq = match already {
            Some(seq) => {
                tracing::warn!(did, intent = intent.id, seq, "intent was already sequenced");
                seq
            }
            None => {
                let mut lock = sequencer.sequencer.write().await;
                lock.sequence_evt(RepoSeq::new(
                    did.to_owned(),
                    intent.event_type.clone(),
                    intent.event.clone(),
                    rsky_common::now(),
                ))
                .await?
            }
        };
        if stop_after == Some(PublishStep::Sequenced) {
            return Ok(seqs);
        }
        acknowledge(&db, intent.id, seq).await?;
        if intent.event_type == "append" {
            let creation = cbor_to_struct::<CommitEvt>(intent.event.clone())
                .map(|evt| evt.since.is_none())
                .unwrap_or(false);
            actor_store
                .lifecycle
                .record_publication(did, &intent.rev, seq, creation)
                .await?;
        }
        seqs.push(seq);
    }
    let root_current = sync_account_root(&db, account_manager, did).await?;
    let blob = BlobReader::new(
        crate::actor_store::blobstore::unavailable(),
        db.clone(),
        actor_store.background_queue.clone(),
        actor_store.coexistence,
    );
    settle_pending_work(&actor_store.lifecycle, &db, &blob, did, root_current).await?;
    Ok(seqs)
}

/// Brings the account database's root up to the actor store's, which a
/// crash between the actor commit and that update leaves behind. Returns
/// whether the two agree afterwards.
pub async fn sync_account_root(
    db: &ActorDb,
    account_manager: &AccountManager,
    did: &str,
) -> Result<bool> {
    let storage = SqlRepoReader::new(did.to_owned(), None, db.clone());
    let Ok(store_root) = storage.get_root_detailed().await else {
        return Ok(false);
    };
    let account_root = account_manager.get_repo_root(did).await?;
    let current = account_root
        .as_ref()
        .is_some_and(|(cid, rev)| *cid == store_root.cid.to_string() && *rev == store_root.rev);
    if current {
        return Ok(true);
    }
    tracing::info!(%did, rev = %store_root.rev, "advancing the account root to the actor store's");
    account_manager
        .update_repo_root_as_worker(did.to_owned(), store_root.cid, store_root.rev)
        .await?;
    Ok(true)
}

/// Clears the actor's pending mark when no intent is undelivered, no blob
/// work is outstanding, and the account root is current.
pub async fn settle_pending_work(
    lifecycle: &LifecycleStore,
    db: &ActorDb,
    blob: &BlobReader,
    did: &str,
    root_current: bool,
) -> Result<()> {
    let pending = db.run(|conn| pending_intents_in(conn)).await?;
    if root_current && pending.is_empty() && blob.nonterminal_blob_work().await? == 0 {
        lifecycle.clear_pending_work(did).await?;
    }
    Ok(())
}

/// Finishes the publication and blob work every marked actor was left with.
pub async fn resume_pending_work(
    actor_store: &ActorStore,
    sequencer: &SharedSequencer,
    account_manager: &AccountManager,
    blobstore_for: impl Fn(&str) -> std::sync::Arc<dyn crate::actor_store::blobstore::BlobStore>,
) -> Result<Vec<String>> {
    let mut resumed = Vec::new();
    for did in actor_store.lifecycle.pending_work().await? {
        tracing::warn!(%did, "resuming publication left by a previous process");
        if let Err(refused) = actor_store.admission.admit_worker(&did) {
            tracing::warn!(%refused, "publication left for the actor's writer");
            continue;
        }
        publish_pending(actor_store, sequencer, account_manager, &did, None).await?;
        if actor_store.exists(&did).await? {
            actor_store.queue_blob_work(&did, blobstore_for(&did));
        }
        resumed.push(did);
    }
    Ok(resumed)
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
