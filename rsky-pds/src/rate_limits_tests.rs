use super::*;
use std::io::Write;
use std::sync::Arc;
use tracing::instrument::WithSubscriber;

/// Tests that change limit settings in the environment take this lock.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

fn memory_windows(limits: &RateLimits) -> &MemoryWindows {
    match &limits.store {
        Store::Memory(windows) => windows,
        Store::Redis(_) => panic!("test requires in-memory rate limits"),
    }
}

#[test]
fn the_reference_redis_address_becomes_a_url() {
    assert_eq!(
        scratch_redis_url("127.0.0.1:6379", None),
        "redis://127.0.0.1:6379"
    );
    assert_eq!(
        scratch_redis_url("127.0.0.1:6379", Some("")),
        "redis://127.0.0.1:6379"
    );
    assert_eq!(
        scratch_redis_url("redis-host:6379", Some("s3cret")),
        "redis://:s3cret@redis-host:6379"
    );
    assert_eq!(
        scratch_redis_url("rediss://cache:6380/1", Some("ignored")),
        "rediss://cache:6380/1"
    );
}

#[tokio::test]
async fn windows_fill_reset_and_report_like_the_reference() {
    let limits = RateLimits::new(
        true,
        Some("secret".into()),
        vec!["10.0.0.9".parse().unwrap()],
    );
    assert!(!limits.shared());
    let limit = limit("test", "test-0", 60, 2);
    let t0 = Instant::now();
    let wall = UNIX_EPOCH + Duration::from_secs(1_000);
    let windows = memory_windows(&limits);
    let first = RateLimits::consume_at(windows, &limit, "k", 1, t0, wall).unwrap();
    assert_eq!(
        first,
        LimitStatus {
            limit: 2,
            duration_secs: 60,
            remaining: 1,
            reset_at_secs: 1_060,
            retry_after_secs: 60,
        }
    );
    let second =
        RateLimits::consume_at(windows, &limit, "k", 1, t0 + Duration::from_secs(10), wall)
            .unwrap();
    assert_eq!(second.remaining, 0);
    let refused =
        RateLimits::consume_at(windows, &limit, "k", 1, t0 + Duration::from_secs(20), wall)
            .unwrap_err();
    assert_eq!(refused.remaining, 0);
    assert_eq!(refused.retry_after_secs, 40);
    // another key has its own window; the window resets after its duration
    assert!(RateLimits::consume_at(windows, &limit, "other", 1, t0, wall).is_ok());
    assert!(
        RateLimits::consume_at(windows, &limit, "k", 1, t0 + Duration::from_secs(60), wall).is_ok()
    );
    let headers = refused.headers();
    let names: Vec<&str> = headers.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "RateLimit-Limit",
            "RateLimit-Remaining",
            "RateLimit-Reset",
            "RateLimit-Policy",
            "Retry-After",
            "Access-Control-Expose-Headers"
        ]
    );
    assert_eq!(headers[3].value, "2;w=60");
    assert!(limits.bypasses(Some("secret"), None));
    assert!(!limits.bypasses(Some("wrong"), None));
    assert!(limits.bypasses(None, Some("10.0.0.9".parse().unwrap())));
    assert!(!limits.bypasses(None, Some("10.0.0.8".parse().unwrap())));
    assert!(limits.consume_all(&[limit], "k", 5, true).await.is_ok());
    assert!(matches!(
        limits.consume_all(&[limit], "k", 5, false).await,
        Err(ApiError::RateLimitExceeded(_))
    ));
    assert!(limits.consume(&limit, "fresh", 1).await.is_ok());
    let disabled = RateLimits::new(false, None, vec![]);
    assert!(!disabled.enabled());
    assert!(disabled
        .consume_all(&[limit], "k", 500, false)
        .await
        .is_ok());
    assert!(global_limit_applies("/xrpc/app.bsky.feed.getTimeline"));
    assert!(!global_limit_applies("/xrpc/com.atproto.sync.getRepo"));
    assert!(!global_limit_applies("/tls-check"));
    assert_eq!(limits_by_name("global-ip"), Some(300));
    assert_eq!(limits_by_name("nope"), None);
}

#[test]
fn the_table_carries_the_reference_key_prefixes() {
    assert_eq!(
        RateLimits::redis_key(&GLOBAL_IP, "1.2.3.4"),
        "rl-global-ip:1.2.3.4"
    );
    assert_eq!(
        RateLimits::redis_key(&CREATE_SESSION[1], "alice.test-1.2.3.4"),
        "com.atproto.server.createSession-1:alice.test-1.2.3.4"
    );
    assert_eq!(
        RateLimits::redis_key(&REPO_WRITE_DAY, "did:plc:a"),
        "rl-repo-write-day:did:plc:a"
    );
    let mut prefixes: Vec<&str> = ALL_LIMITS.iter().map(|limit| limit.prefix).collect();
    prefixes.sort_unstable();
    prefixes.dedup();
    assert_eq!(
        prefixes.len(),
        ALL_LIMITS.len(),
        "every limit has its own prefix"
    );
    assert!(ALL_LIMITS
        .iter()
        .all(|limit| !limit.name.is_empty() && limit.points > 0));
}

#[test]
fn stale_windows_are_pruned_once_the_table_is_large() {
    let limits = RateLimits::new(true, None, vec![]);
    let limit = limit("global-ip", "rl-global-ip", 300, 10);
    let t0 = Instant::now();
    let wall = SystemTime::now();
    let windows = memory_windows(&limits);
    for i in 0..10_001 {
        RateLimits::consume_at(windows, &limit, &format!("key-{i}"), 1, t0, wall).unwrap();
    }
    assert_eq!(windows.lock().unwrap().len(), 10_001);
    RateLimits::consume_at(
        windows,
        &limit,
        "late",
        1,
        t0 + Duration::from_secs(300),
        wall,
    )
    .unwrap();
    assert_eq!(windows.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn settings_come_from_the_environment() {
    let _env = ENV_LOCK.lock().await;
    std::env::set_var("PDS_RATE_LIMITS_ENABLED", "true");
    std::env::set_var("PDS_RATE_LIMIT_BYPASS_KEY", "k");
    std::env::set_var("PDS_RATE_LIMIT_BYPASS_IPS", "10.0.0.1, junk");
    std::env::remove_var("PDS_REDIS_SCRATCH_ADDRESS");
    let limits = RateLimits::from_env();
    assert!(limits.enabled());
    assert!(limits.bypasses(Some("k"), None));
    assert!(limits.bypasses(None, Some("10.0.0.1".parse().unwrap())));
    let connected = RateLimits::connect_from_env().await;
    assert!(connected.enabled() && !connected.shared());
    std::env::remove_var("PDS_RATE_LIMITS_ENABLED");
    std::env::remove_var("PDS_RATE_LIMIT_BYPASS_KEY");
    std::env::remove_var("PDS_RATE_LIMIT_BYPASS_IPS");
    assert!(!RateLimits::from_env().enabled());
    // a redis address is only consulted while limits are enabled
    std::env::set_var("PDS_REDIS_SCRATCH_ADDRESS", "redis://127.0.0.1:1");
    assert!(!RateLimits::connect_from_env().await.shared());
    if let Ok(url) = std::env::var("TEST_REDIS_URL") {
        std::env::set_var("PDS_RATE_LIMITS_ENABLED", "true");
        std::env::set_var("PDS_REDIS_SCRATCH_ADDRESS", url);
        assert!(RateLimits::connect_from_env().await.shared());
        std::env::remove_var("PDS_RATE_LIMITS_ENABLED");
    }
    std::env::remove_var("PDS_REDIS_SCRATCH_ADDRESS");
    assert!(RateLimits::with_redis(true, None, vec![], "not a url")
        .await
        .is_err());
}

#[tokio::test]
#[should_panic(expected = "redis is unreachable")]
async fn an_unreachable_redis_stops_the_boot() {
    let _env = ENV_LOCK.lock().await;
    std::env::set_var("PDS_RATE_LIMITS_ENABLED", "true");
    std::env::set_var("PDS_REDIS_SCRATCH_ADDRESS", "redis://127.0.0.1:1");
    let limits = RateLimits::connect_from_env().await;
    std::env::remove_var("PDS_RATE_LIMITS_ENABLED");
    std::env::remove_var("PDS_REDIS_SCRATCH_ADDRESS");
    drop(limits);
}

/// Needs `TEST_REDIS_URL`; the windows and keys must match what the
/// reference PDS writes so the two implementations share budgets.
#[tokio::test]
async fn redis_windows_are_shared_under_the_reference_keys() {
    let Ok(url) = std::env::var("TEST_REDIS_URL") else {
        eprintln!("TEST_REDIS_URL is not set; skipping the redis rate limit test");
        return;
    };
    let limits = RateLimits::with_redis(true, None, vec![], &url)
        .await
        .unwrap();
    assert!(limits.shared());
    let key = format!("shared-{}", std::process::id());
    let limit = limit("test", "rl-test", 60, 2);
    let redis_key = RateLimits::redis_key(&limit, &key);
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut raw = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::cmd("DEL")
        .arg(&redis_key)
        .query_async(&mut raw)
        .await
        .unwrap();

    let first = limits.consume(&limit, &key, 1).await.unwrap();
    assert_eq!(
        (first.limit, first.remaining, first.duration_secs),
        (2, 1, 60)
    );
    assert!((59..=60).contains(&first.retry_after_secs));
    let stored: i64 = redis::cmd("GET")
        .arg(&redis_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(stored, 1);
    let ttl: i64 = redis::cmd("TTL")
        .arg(&redis_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(ttl > 0 && ttl <= 60);
    // another process (here: a raw client) consuming the same key counts
    let _: () = redis::cmd("INCRBY")
        .arg(&redis_key)
        .arg(1)
        .query_async(&mut raw)
        .await
        .unwrap();
    let refused = limits.consume(&limit, &key, 1).await.unwrap_err();
    assert_eq!(refused.remaining, 0);
    assert!(matches!(
        limits.consume_all(&[limit], &key, 1, false).await,
        Err(ApiError::RateLimitExceeded(_))
    ));
    // a counter that lost its expiry gets one back
    let _: () = redis::cmd("PERSIST")
        .arg(&redis_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    let _ = limits.consume(&limit, &key, 1).await;
    let ttl: i64 = redis::cmd("TTL")
        .arg(&redis_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(ttl > 0);
    let _: () = redis::cmd("DEL")
        .arg(&redis_key)
        .query_async(&mut raw)
        .await
        .unwrap();

    // A malformed counter is a Redis-side failure, not a request limit.
    // The service must allow this request while operators repair Redis.
    let malformed_key = format!("malformed-{}", std::process::id());
    let malformed_redis_key = RateLimits::redis_key(&limit, &malformed_key);
    let _: () = redis::cmd("SET")
        .arg(&malformed_redis_key)
        .arg("not-an-integer")
        .query_async(&mut raw)
        .await
        .unwrap();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || CapturedLogs(sink.clone()))
        .finish();
    let fail_open = limits
        .consume(&limit, &malformed_key, 1)
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert_eq!(fail_open.remaining, limit.points);
    assert!(String::from_utf8(logs.lock().unwrap().clone())
        .unwrap()
        .contains("rate limit store unavailable; request allowed"));
    let _: () = redis::cmd("DEL")
        .arg(&malformed_redis_key)
        .query_async(&mut raw)
        .await
        .unwrap();
}
