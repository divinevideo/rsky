use super::*;
use crate::actor_store::blobstore::{BlobStore, MemoryBlobStore};
use crate::actor_store::ActorStore;
use crate::background::BackgroundQueue;
use crate::config::ActorStoreConfig;
use crate::crawlers::Crawlers;
use crate::sequencer::{RequestSeqRangeOpts, Sequencer};
use rsky_repo::types::{PreparedCreateOrUpdate, PreparedWrite, WriteOpAction};
use secp256k1::{Keypair, Secp256k1, SecretKey};
use std::sync::Arc;

const DID: &str = "did:plc:publisher";

struct World {
    _dir: tempfile::TempDir,
    actor_store: ActorStore,
    sequencer: SharedSequencer,
    account_manager: AccountManager,
    blobstore: Arc<MemoryBlobStore>,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::open(dir.path().join("rsky/lifecycle.sqlite"))
        .await
        .unwrap();
    let sequencer_db = crate::sequencer::db::get_migrated_db(dir.path().join("sequencer.sqlite"))
        .await
        .unwrap();
    let sequencer = SharedSequencer {
        sequencer: tokio::sync::RwLock::new(Sequencer::new(
            sequencer_db,
            Crawlers::new("pds.test".to_owned(), vec![]),
            None,
        )),
    };
    let actor_store = ActorStore::new(
        &ActorStoreConfig {
            directory: dir.path().join("actors").to_str().unwrap().to_owned(),
            cache_size: 10,
        },
        BackgroundQueue::default(),
        lifecycle,
    );
    let keypair = Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&[9u8; 32]).unwrap(),
    );
    actor_store.create(DID, &keypair).await.unwrap();
    let account_manager = AccountManager::new(
        crate::account_manager::db::get_migrated_db(dir.path().join("account.sqlite"))
            .await
            .unwrap(),
    );
    World {
        _dir: dir,
        actor_store,
        sequencer,
        account_manager,
        blobstore: Arc::new(MemoryBlobStore::default()),
    }
}

fn post(rkey: &str, text: &str) -> PreparedWrite {
    let record: rsky_repo::types::RepoRecord = serde_json::from_value(serde_json::json!({
        "$type": "app.bsky.feed.post",
        "text": text,
        "createdAt": "2023-01-01T00:00:00.000Z",
    }))
    .unwrap();
    let cid = rsky_common::ipld::cid_for_cbor(&record).unwrap();
    PreparedWrite::Create(PreparedCreateOrUpdate {
        action: WriteOpAction::Create,
        uri: format!("at://{DID}/app.bsky.feed.post/{rkey}"),
        cid,
        swap_cid: None,
        record,
        blobs: vec![],
    })
}

async fn init_repo(world: &World) {
    let txn = world
        .actor_store
        .transact(DID.to_owned(), world.blobstore.clone())
        .await
        .unwrap();
    txn.create_repo(vec![], true).await.unwrap();
}

async fn write_post(world: &World, rkey: &str) {
    let mut txn = world
        .actor_store
        .transact(DID.to_owned(), world.blobstore.clone())
        .await
        .unwrap();
    txn.process_writes(vec![post(rkey, "hello")], None)
        .await
        .unwrap();
}

async fn event_types(world: &World) -> Vec<String> {
    let lock = world.sequencer.sequencer.read().await;
    lock.rows_for_did_after(DID, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.event_type)
        .collect()
}

async fn intents(world: &World) -> Vec<StoredIntent> {
    world
        .actor_store
        .read(DID.to_owned(), world.blobstore.clone())
        .await
        .unwrap()
        .all_intents()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_write_commits_its_intents_and_publishes_them_once() {
    let world = world().await;
    init_repo(&world).await;
    let pending = intents(&world).await;
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].event_type, "append");
    assert_eq!(pending[1].event_type, "sync");
    assert!(pending.iter().all(|intent| intent.state == "pending"));
    assert_eq!(
        world.actor_store.lifecycle.pending_work().await.unwrap(),
        [DID]
    );
    assert!(event_types(&world).await.is_empty());

    let seqs = publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        None,
    )
    .await
    .unwrap();
    assert_eq!(seqs, [1, 2]);
    assert_eq!(event_types(&world).await, ["append", "sync"]);
    let delivered = intents(&world).await;
    assert!(delivered.iter().all(|intent| intent.state == "delivered"));
    assert_eq!(delivered[0].seq, Some(1));
    assert_eq!(delivered[0].seq_floor, Some(0));
    assert!(world
        .actor_store
        .lifecycle
        .pending_work()
        .await
        .unwrap()
        .is_empty());

    let mark = world
        .actor_store
        .lifecycle
        .frontier_watermark(DID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mark.creation_seq, Some(1));
    assert_eq!(mark.max_rev.as_deref(), Some(delivered[0].rev.as_str()));

    // nothing left to publish
    let again = publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        None,
    )
    .await
    .unwrap();
    assert!(again.is_empty());
    assert_eq!(event_types(&world).await.len(), 2);

    // the stream sees the rows the publisher inserted
    let lock = world.sequencer.sequencer.read().await;
    let evts = lock
        .request_seq_range(RequestSeqRangeOpts {
            earliest_seq: None,
            latest_seq: None,
            earliest_time: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(evts.len(), 2);
}

#[tokio::test]
async fn a_crash_after_the_floor_delivers_exactly_once() {
    let world = world().await;
    init_repo(&world).await;
    publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        None,
    )
    .await
    .unwrap();
    write_post(&world, "3lfixtureaa2a").await;

    let stopped = publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        Some(PublishStep::Floored),
    )
    .await
    .unwrap();
    assert!(stopped.is_empty());
    let pending = intents(&world).await;
    assert_eq!(pending[2].state, "pending");
    assert_eq!(pending[2].seq_floor, Some(2));
    assert_eq!(event_types(&world).await.len(), 2);

    let seqs = publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        None,
    )
    .await
    .unwrap();
    assert_eq!(seqs, [3]);
    assert_eq!(event_types(&world).await, ["append", "sync", "append"]);
}

#[tokio::test]
async fn a_crash_after_sequencing_recognises_the_row() {
    let world = world().await;
    init_repo(&world).await;
    publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        None,
    )
    .await
    .unwrap();
    write_post(&world, "3lfixtureaa2b").await;

    publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        Some(PublishStep::Sequenced),
    )
    .await
    .unwrap();
    assert_eq!(event_types(&world).await.len(), 3);
    assert_eq!(intents(&world).await[2].state, "pending");

    let seqs = publish_pending(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        DID,
        None,
    )
    .await
    .unwrap();
    assert_eq!(seqs, [3]);
    assert_eq!(event_types(&world).await.len(), 3);
    let delivered = intents(&world).await;
    assert_eq!(delivered[2].state, "delivered");
    assert_eq!(delivered[2].seq, Some(3));
    // the same for the two-intent creation batch
    assert_eq!(delivered[0].seq, Some(1));
}

#[tokio::test]
async fn resume_finishes_marked_actors_and_forgets_deleted_ones() {
    let world = world().await;
    init_repo(&world).await;
    world
        .actor_store
        .lifecycle
        .mark_pending_work("did:plc:gone")
        .await
        .unwrap();
    let blobstore = world.blobstore.clone();
    let mut resumed = resume_pending_work(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        |_| blobstore.clone(),
    )
    .await
    .unwrap();
    resumed.sort();
    assert_eq!(resumed, ["did:plc:gone", DID]);
    world.actor_store.background_queue.process_all().await;
    assert_eq!(event_types(&world).await, ["append", "sync"]);
    assert!(world
        .actor_store
        .lifecycle
        .pending_work()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_absent_actor_is_left_to_its_writer() {
    let world = world().await;
    init_repo(&world).await;
    let allowlist = world._dir.path().join("write-allowlist.toml");
    std::fs::write(&allowlist, "version = 1\ndefault = \"absent\"\n").unwrap();
    let admission = Arc::new(crate::admission::Admission::from_file(&allowlist).unwrap());
    let restricted = ActorStore::new(
        &ActorStoreConfig {
            directory: world
                ._dir
                .path()
                .join("actors")
                .to_str()
                .unwrap()
                .to_owned(),
            cache_size: 10,
        },
        BackgroundQueue::default(),
        world.actor_store.lifecycle.clone(),
    )
    .with_admission(admission);
    let refused = publish_pending(
        &restricted,
        &world.sequencer,
        &world.account_manager,
        DID,
        None,
    )
    .await
    .unwrap_err();
    assert!(refused
        .downcast_ref::<crate::admission::NotAdmitted>()
        .is_some());
    let blobstore = world.blobstore.clone();
    let resumed = resume_pending_work(
        &restricted,
        &world.sequencer,
        &world.account_manager,
        |_| blobstore.clone(),
    )
    .await
    .unwrap();
    assert!(resumed.is_empty());
    assert_eq!(restricted.lifecycle.pending_work().await.unwrap(), [DID]);
    assert!(event_types(&world).await.is_empty());
}

#[test]
fn row_matching_by_event_identity() {
    let intent = StoredIntent {
        id: 1,
        rev: "3lfixtureaa2a".to_owned(),
        cid: "bafkreibjfgx2gprinfvicegelk5kosd6y2frmqpqzwqkg7usac74l3t2v4".to_owned(),
        event_type: "sync".to_owned(),
        event: vec![1, 2, 3],
        state: "pending".to_owned(),
        seq_floor: None,
        seq: None,
    };
    let sync = SyncEvt {
        did: DID.to_owned(),
        blocks: vec![],
        rev: intent.rev.clone(),
    };
    let row = RepoSeq::new(
        DID.to_owned(),
        "sync".to_owned(),
        rsky_common::struct_to_cbor(&sync).unwrap(),
        rsky_common::now(),
    );
    assert!(row_matches(&row, &intent));
    let other_rev = RepoSeq::new(
        DID.to_owned(),
        "sync".to_owned(),
        rsky_common::struct_to_cbor(&SyncEvt {
            rev: "3lfixtureaa2b".to_owned(),
            ..sync
        })
        .unwrap(),
        rsky_common::now(),
    );
    assert!(!row_matches(&other_rev, &intent));
    let garbage = RepoSeq::new(
        DID.to_owned(),
        "sync".to_owned(),
        vec![0xff],
        rsky_common::now(),
    );
    assert!(!row_matches(&garbage, &intent));
    let wrong_type = RepoSeq::new(
        DID.to_owned(),
        "append".to_owned(),
        vec![0xff],
        rsky_common::now(),
    );
    assert!(!row_matches(&wrong_type, &intent));
    let opaque = StoredIntent {
        event_type: "identity".to_owned(),
        ..intent
    };
    let same_bytes = RepoSeq::new(
        DID.to_owned(),
        "identity".to_owned(),
        vec![1, 2, 3],
        rsky_common::now(),
    );
    assert!(row_matches(&same_bytes, &opaque));
    let garbage_append = RepoSeq::new(
        DID.to_owned(),
        "append".to_owned(),
        vec![0xff],
        rsky_common::now(),
    );
    let append_intent = StoredIntent {
        event_type: "append".to_owned(),
        ..opaque
    };
    assert!(!row_matches(&garbage_append, &append_intent));
}

/// A crash between the actor commit and the account root update leaves
/// the root behind; startup finishes it before the pending mark clears.
#[tokio::test]
async fn resume_advances_the_account_root_left_behind_by_a_crash() {
    let world = world().await;
    init_repo(&world).await;
    write_post(&world, "3lfixtureaa2c").await;
    assert!(world
        .account_manager
        .get_repo_root(DID)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        world.actor_store.lifecycle.pending_work().await.unwrap(),
        vec![DID.to_owned()]
    );
    let blobstore: Arc<dyn BlobStore> = world.blobstore.clone();
    let resumed = resume_pending_work(
        &world.actor_store,
        &world.sequencer,
        &world.account_manager,
        |_| blobstore.clone(),
    )
    .await
    .unwrap();
    assert_eq!(resumed, vec![DID.to_owned()]);
    let store_root = world
        .actor_store
        .read(DID.to_owned(), world.blobstore.clone())
        .await
        .unwrap()
        .storage
        .read()
        .await
        .get_root_detailed()
        .await
        .unwrap();
    assert_eq!(
        world.account_manager.get_repo_root(DID).await.unwrap(),
        Some((store_root.cid.to_string(), store_root.rev.clone()))
    );
    assert!(world
        .actor_store
        .lifecycle
        .pending_work()
        .await
        .unwrap()
        .is_empty());
    // a second run finds the root current and touches nothing
    let db = world.actor_store.write_db(DID).await.unwrap();
    assert!(sync_account_root(&db, &world.account_manager, DID)
        .await
        .unwrap());
}
