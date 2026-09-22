use std::process::Command;

#[tokio::test]
async fn record_restore_records_the_current_repo_revision() {
    use rsky_pds::actor_store::db::get_migrated_db;
    use rsky_pds::lifecycle::LifecycleStore;
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let did = "did:example:restored";
    let shard = hex::encode(Sha256::digest(did.as_bytes()));
    let actor_path = dir.path().join("actors").join(&shard[..2]).join(did);
    std::fs::create_dir_all(&actor_path).unwrap();
    std::fs::create_dir_all(dir.path().join("rsky")).unwrap();
    let db = get_migrated_db(actor_path.join("store.sqlite"))
        .await
        .unwrap();
    let cid = rsky_common::ipld::sha256_to_cid(Sha256::digest(b"root").to_vec());
    db.run(move |conn| {
        conn.execute(
            "INSERT INTO repo_root (did, cid, rev, \"indexedAt\") VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                did,
                cid.to_string(),
                "3lrestored",
                "2026-01-01T00:00:00.000Z"
            ],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    drop(db);

    let output = Command::new(env!("CARGO_BIN_EXE_rsky-pds"))
        .args(["--record-restore", did])
        .env("PDS_DATA_DIRECTORY", dir.path())
        .env("PDS_BLOBSTORE_DISK_LOCATION", dir.path().join("blobs"))
        .env("PDS_JWT_SECRET", "local-maintenance-test-secret")
        .env(
            "PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX",
            "0000000000000000000000000000000000000000000000000000000000000002",
        )
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ENDPOINT", "http://127.0.0.1:9")
        .env_remove("PDS_BLOBSTORE_S3_BUCKET")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "restore command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["did"], did);
    assert_eq!(result["resultingRev"], "3lrestored");
    assert_eq!(result["restoreEvents"], 1);
    let lifecycle = LifecycleStore::open(dir.path().join("rsky/lifecycle.sqlite"))
        .await
        .unwrap();
    assert_eq!(lifecycle.restore_event_count(did).await.unwrap(), 1);
}

#[tokio::test]
async fn misconfigured_global_mailer_falls_back_to_logging() {
    let old_smtp = std::env::var_os("PDS_EMAIL_SMTP_URL");
    let old_from = std::env::var_os("PDS_EMAIL_FROM_ADDRESS");
    std::env::set_var("PDS_EMAIL_SMTP_URL", "smtp://127.0.0.1:9");
    std::env::remove_var("PDS_EMAIL_FROM_ADDRESS");
    let mailer = rsky_pds::mailer::mailer();
    match old_smtp {
        Some(value) => std::env::set_var("PDS_EMAIL_SMTP_URL", value),
        None => std::env::remove_var("PDS_EMAIL_SMTP_URL"),
    }
    match old_from {
        Some(value) => std::env::set_var("PDS_EMAIL_FROM_ADDRESS", value),
        None => std::env::remove_var("PDS_EMAIL_FROM_ADDRESS"),
    }

    assert!(!mailer.is_smtp());
    mailer
        .send_html(
            "recipient@example.test",
            "status",
            "<p>safe fallback</p>",
            None,
        )
        .await
        .unwrap();
}

#[test]
fn maintenance_logging_accepts_text_and_repeated_traced_initialization() {
    use rsky_pds::logging::{self, LogFormat, LogTarget};

    logging::init_to(LogFormat::Text, LogTarget::Stderr);
    assert!(tracing::enabled!(tracing::Level::INFO));
    tracing::info!(operation = "rotate", "maintenance logging is active");
    // An operator may invoke the initializer from more than one bootstrap
    // path; the second registration must leave the first subscriber usable.
    logging::init_to(LogFormat::Traced, LogTarget::Stderr);
    assert!(tracing::enabled!(tracing::Level::INFO));
}

#[test]
fn rotate_keys_dry_run_reports_an_empty_account_database() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("rsky")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rotate-keys"))
        .arg("--dry-run")
        .env("PDS_DATA_DIRECTORY", dir.path())
        .env("PDS_BLOBSTORE_DISK_LOCATION", dir.path().join("blobs"))
        .env(
            "PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX",
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .env(
            "PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX",
            "0000000000000000000000000000000000000000000000000000000000000002",
        )
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ENDPOINT", "http://127.0.0.1:9")
        .env_remove("PDS_BLOBSTORE_S3_BUCKET")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "rotation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(logs.contains("signing key rotation finished"), "{logs}");
    assert!(logs.contains("scanned=0"), "{logs}");
    assert!(logs.contains("rotated=0"), "{logs}");

    // A requested PLC identity whose directory is unavailable must be
    // reported as failed and make the operator command exit nonzero.
    let failed = Command::new(env!("CARGO_BIN_EXE_rotate-keys"))
        .args(["--dry-run", "--did", "did:plc:synthetic"])
        .env("PDS_DATA_DIRECTORY", dir.path())
        .env("PDS_BLOBSTORE_DISK_LOCATION", dir.path().join("blobs"))
        .env(
            "PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX",
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .env(
            "PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX",
            "0000000000000000000000000000000000000000000000000000000000000002",
        )
        .env("PDS_DID_PLC_URL", "http://127.0.0.1:9")
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ENDPOINT", "http://127.0.0.1:9")
        .env_remove("PDS_BLOBSTORE_S3_BUCKET")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(logs.contains("failed=1"), "{logs}");
    assert!(logs.contains("1 account(s) failed to rotate"), "{logs}");
}
