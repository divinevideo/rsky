mod common;

use rsky_pds::account_manager::{db::get_migrated_db as get_account_db, AccountManager};
use rsky_pds::actor_store::blobstore::MemoryBlobStore;
use rsky_pds::actor_store::ActorStore;
use rsky_pds::background::BackgroundQueue;
use rsky_pds::config::ActorStoreConfig;
use rsky_pds::crawlers::Crawlers;
use rsky_pds::lifecycle::LifecycleStore;
use rsky_pds::sequencer::{db::get_db as get_unmigrated_sequencer_db, Sequencer};
use secp256k1::{Keypair, Secp256k1, SecretKey};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn storage_env(dir: &Path) {
    for (key, name) in [
        ("PDS_ACTOR_STORE_DIRECTORY", "actors"),
        ("PDS_ACCOUNT_DB_LOCATION", "account.sqlite"),
        ("PDS_SEQUENCER_DB_LOCATION", "sequencer.sqlite"),
        ("PDS_DID_CACHE_DB_LOCATION", "did_cache.sqlite"),
        ("PDS_LIFECYCLE_DB", "rsky/lifecycle.sqlite"),
        ("PDS_LOCK_DIR", "rsky/locks"),
        ("PDS_BLOB_ATTEMPTS_DB", "rsky/blob-attempts.sqlite"),
        ("PDS_BLOB_GENERATIONS_DB", "rsky/blob-generations.sqlite"),
        ("PDS_REPAIR_DB", "rsky/repair.sqlite"),
    ] {
        std::env::set_var(key, dir.join(name));
    }
    std::env::set_var("PDS_DATA_DIRECTORY", dir);
    std::env::set_var("PDS_BLOBSTORE_DISK_LOCATION", dir.join("blobs"));
    std::env::remove_var("PDS_BLOBSTORE_S3_BUCKET");
    std::env::set_var("PDS_COEXISTENCE", "false");
    std::env::set_var("PDS_READ_ONLY", "false");
    std::fs::create_dir_all(dir.join("rsky")).unwrap();
}

async fn wait_for_log(logs: &Arc<Mutex<Vec<u8>>>, needle: &str) {
    let started = Instant::now();
    loop {
        let text = String::from_utf8_lossy(&logs.lock().unwrap()).to_string();
        if text.contains(needle) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "collector did not log {needle:?}: {text}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn boot_resumes_deletions_and_reports_both_collector_failures() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || CapturedLogs(sink.clone()))
            .with_max_level(tracing::Level::INFO)
            .finish(),
    )
    .unwrap();

    // Startup must finish an interrupted deletion before the service is
    // available, even when the account and actor store are already absent.
    let resume_dir = tempfile::tempdir().unwrap();
    storage_env(resume_dir.path());
    std::env::set_var("PDS_BLOB_GC_ENABLED", "false");
    let lifecycle = LifecycleStore::open(resume_dir.path().join("rsky/lifecycle.sqlite"))
        .await
        .unwrap();
    let did = "did:example:interrupted-delete";
    lifecycle.tombstone(did).await.unwrap();
    let _client = common::get_client_in(resume_dir.path()).await;
    assert!(lifecycle.open_tombstones().await.unwrap().is_empty());
    assert!(lifecycle.purge_obligation_of(did).await.unwrap().is_some());
    wait_for_log(&logs, "resumed incomplete account deletions").await;

    // The booted service uses explicit fixture DB paths, while the
    // background collector opens its env-configured maintenance paths.
    // An invalid repair path makes that second open fail without taking
    // the service down.
    let open_dir = tempfile::tempdir().unwrap();
    storage_env(open_dir.path());
    std::env::set_var("PDS_BLOB_GC_ENABLED", "true");
    std::env::set_var("PDS_REPAIR_DB", "/dev/null/repair.sqlite");
    let _client = common::get_client_in(open_dir.path()).await;
    wait_for_log(&logs, "blob collector could not open the data directory").await;

    // A valid maintenance setup with a missing generation registry opens
    // successfully, then reports the collector pass failure. The service
    // remains booted and can answer health requests.
    let pass_dir = tempfile::tempdir().unwrap();
    storage_env(pass_dir.path());
    std::env::set_var(
        "PDS_BLOB_GENERATIONS_DB",
        pass_dir.path().join("rsky/missing-generations.sqlite"),
    );
    let client = common::get_client_in(pass_dir.path()).await;
    wait_for_log(&logs, "blob collector pass failed").await;
    let response = client.get("/xrpc/_health").dispatch().await;
    assert_eq!(response.status(), rocket::http::Status::Ok);

    // Commit a real repository while no service is running. Boot must
    // deliver both creation intents, advance the account root, and clear
    // the pending-work marker.
    let publication_dir = tempfile::tempdir().unwrap();
    storage_env(publication_dir.path());
    std::env::set_var("PDS_BLOB_GC_ENABLED", "false");
    let publication_lifecycle =
        LifecycleStore::open(publication_dir.path().join("rsky/lifecycle.sqlite"))
            .await
            .unwrap();
    let actor_store = ActorStore::new(
        &ActorStoreConfig {
            directory: publication_dir
                .path()
                .join("actors")
                .to_str()
                .unwrap()
                .to_owned(),
            cache_size: 10,
        },
        BackgroundQueue::default(),
        publication_lifecycle.clone(),
    );
    let publication_did = "did:plc:bootpending";
    let keypair = Keypair::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&[11u8; 32]).unwrap(),
    );
    actor_store.create(publication_did, &keypair).await.unwrap();
    let blobstore = Arc::new(MemoryBlobStore::default());
    actor_store
        .transact(publication_did.to_owned(), blobstore.clone())
        .await
        .unwrap()
        .create_repo(vec![], true)
        .await
        .unwrap();
    assert_eq!(
        publication_lifecycle.pending_work().await.unwrap(),
        [publication_did]
    );
    let _client = common::get_client_in(publication_dir.path()).await;
    wait_for_log(&logs, "resumed publication").await;
    let account_manager = AccountManager::new(
        get_account_db(publication_dir.path().join("account.sqlite"))
            .await
            .unwrap(),
    );
    assert!(account_manager
        .get_repo_root(publication_did)
        .await
        .unwrap()
        .is_some());
    let sequencer = Sequencer::new(
        get_unmigrated_sequencer_db(publication_dir.path().join("sequencer.sqlite")).unwrap(),
        Crawlers::new("pds.test".into(), vec![]),
        None,
    );
    let events = sequencer
        .rows_for_did_after(publication_did, 0)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["append", "sync"]
    );
    assert!(publication_lifecycle
        .pending_work()
        .await
        .unwrap()
        .is_empty());

    // A sequencer whose database has not been migrated fails its initial
    // query. The background task must report the failure and exit promptly.
    let seq_dir = tempfile::tempdir().unwrap();
    let seq_db = get_unmigrated_sequencer_db(seq_dir.path().join("sequencer.sqlite")).unwrap();
    let sequencer = Sequencer::new(seq_db, Crawlers::new("pds.test".into(), vec![]), None);
    tokio::time::timeout(Duration::from_secs(5), sequencer.spawn())
        .await
        .expect("failed sequencer task did not exit")
        .expect("sequencer task panicked");
    wait_for_log(&logs, "Sequencer exited").await;
}
