mod common;

use rocket::http::{ContentType, Header, Status};
use rsky_pds::actor_store::ActorStore;
use rsky_pds::crawlers::Crawlers;
use rsky_pds::lifecycle::LifecycleStore;
use rsky_pds::rate_limits::{Limit, RateLimits};
use rsky_pds::sequencer::Sequencer;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

struct CapturedLog(Mutex<Vec<String>>);

static LOG: CapturedLog = CapturedLog(Mutex::new(Vec::new()));

impl tracing::log::Log for CapturedLog {
    fn enabled(&self, metadata: &tracing::log::Metadata<'_>) -> bool {
        metadata.target().starts_with("rsky_pds")
    }

    fn log(&self, record: &tracing::log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

async fn wait_for_log(message: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(line) = LOG
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|line| line.contains(message))
                .cloned()
            {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing log message {message:?}"))
}

#[tokio::test]
async fn operational_failures_reach_the_log_facade_without_a_tracing_subscriber() {
    // Libraries can be embedded in a process using the log facade alone.
    // Keep this binary isolated: setting a tracing dispatcher permanently
    // disables tracing's compatibility fallback, even after its guard drops.
    assert!(!tracing::dispatcher::has_been_set());
    tracing::log::set_logger(&LOG).unwrap();
    tracing::log::set_max_level(tracing::log::LevelFilter::Debug);
    std::env::set_var("PDS_READ_ONLY", "false");
    std::env::set_var("PDS_COEXISTENCE", "false");
    std::env::set_var("PDS_BLOB_GC_ENABLED", "false");

    let dir = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::open(dir.path().join("rsky/lifecycle.sqlite"))
        .await
        .unwrap();
    lifecycle
        .tombstone("did:example:interrupted")
        .await
        .unwrap();
    let client = common::get_client_in(dir.path()).await;
    assert!(lifecycle.open_tombstones().await.unwrap().is_empty());
    let line = wait_for_log("resumed incomplete account deletions").await;
    assert!(line.contains("count=1"), "{line}");

    // A malformed Redis counter must fail open and report the store error.
    if let Ok(url) = std::env::var("TEST_REDIS_URL") {
        let limits = RateLimits::with_redis(true, None, vec![], &url)
            .await
            .unwrap();
        let limit = Limit {
            name: "log-fallback",
            prefix: "test-log-fallback",
            duration_secs: 60,
            points: 2,
        };
        let key = std::process::id().to_string();
        let redis_key = RateLimits::redis_key(&limit, &key);
        let mut redis = redis::Client::open(url)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        redis::cmd("SET")
            .arg(&redis_key)
            .arg("not-an-integer")
            .arg("EX")
            .arg(60)
            .query_async::<()>(&mut redis)
            .await
            .unwrap();
        let result = limits.consume(&limit, &key, 1).await.unwrap();
        assert_eq!(result.remaining, limit.points);
        wait_for_log("rate limit store unavailable; request allowed").await;
        redis::cmd("DEL")
            .arg(&redis_key)
            .query_async::<usize>(&mut redis)
            .await
            .unwrap();
    }

    common::oauth::create_active_account(&client).await;
    let response = client
        .post("/xrpc/com.atproto.server.createSession")
        .header(ContentType::JSON)
        .body(json!({"identifier":"foo@example.com","password":"password"}).to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let session: Value = response.into_json().await.unwrap();
    tracing::log::set_max_level(tracing::log::LevelFilter::Debug);
    let response = client
        .post("/xrpc/com.atproto.space.createRecord")
        .header(ContentType::JSON)
        .header(Header::new(
            "Authorization",
            format!("Bearer {}", session["accessJwt"].as_str().unwrap()),
        ))
        .body(
            json!({
                "space":"at://did:web:127.0.0.1/space/com.example.room/remote",
                "repo":"did:plc:khvyd3oiw46vif5gm7hijslk",
                "collection":"com.example.record",
                "rkey":"fallback",
                "record":{"text":"persist despite unavailable remote notification host"}
            })
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    assert!(response.into_json::<Value>().await.unwrap()["cid"].is_string());
    client
        .rocket()
        .state::<ActorStore>()
        .unwrap()
        .background_queue
        .process_all()
        .await;
    let line = wait_for_log("could not resolve space host for auto-registration").await;
    assert!(line.contains("did:web:127.0.0.1"), "{line}");

    let db = rsky_pds::sequencer::db::get_migrated_db(dir.path().join("fault.sqlite"))
        .await
        .unwrap();
    let mut sequencer = Sequencer::new(
        db.clone(),
        Crawlers::new("pds.example.test".into(), vec![]),
        None,
    );
    let first = sequencer
        .sequence_identity_evt("did:example:sequenced".into(), None)
        .await
        .unwrap();
    let task = sequencer.clone().spawn();
    tokio::time::timeout(Duration::from_secs(5), async {
        while sequencer.last_seen() != first {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    db.run(|conn| {
        conn.execute_batch("DROP TABLE repo_seq")?;
        Ok(())
    })
    .await
    .unwrap();
    let line = wait_for_log("sequencer failed to poll db").await;
    assert!(line.contains("no such table: repo_seq"), "{line}");
    assert!(line.contains(&format!("last_seen: {first}")), "{line}");
    assert!(!task.is_finished());
    sequencer.destroy().await;
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(!tracing::dispatcher::has_been_set());
}
