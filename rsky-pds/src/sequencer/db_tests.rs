use super::*;

#[tokio::test]
async fn migrates_sequencer_db_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = get_migrated_db(dir.path().join("sequencer.sqlite"))
        .await
        .unwrap();
    // migrating again is a no-op
    migrate_to_latest(&db, SEQUENCER_DB_MIGRATIONS_SET)
        .await
        .unwrap();
    let tables: Vec<String> = db
        .run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<String>, rusqlite::Error>>()?;
            Ok(names)
        })
        .await
        .unwrap();
    assert_eq!(
        tables,
        ["kysely_migration", "kysely_migration_lock", "repo_seq"]
    );
}
