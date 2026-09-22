use super::*;

#[tokio::test]
async fn older_actor_store_without_repair_table_has_no_local_repair_steps() {
    let dir = tempfile::tempdir().unwrap();
    let db = get_migrated_db(dir.path().join("actor.sqlite"))
        .await
        .unwrap();
    db.run(|conn| {
        conn.execute("DROP TABLE repair_step", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let reader = ActorStoreReader::new(
        "did:example:older-store".to_string(),
        db,
        blobstore::unavailable(),
        BackgroundQueue::default(),
        dir.path().join("keys"),
        false,
    );

    assert!(reader.repair_steps("repair-1").await.unwrap().is_empty());
}
