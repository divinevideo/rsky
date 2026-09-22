use super::*;
use crate::actor_store::blobstore::MemoryBlobStore;
use crate::actor_store::db::get_migrated_db;
use futures::future::BoxFuture;
use rsky_repo::types::{BlobConstraint, PreparedCreateOrUpdate, PreparedDelete, WriteOpAction};

struct TestBlobReader {
    reader: BlobReader,
    store: Arc<MemoryBlobStore>,
    _dir: tempfile::TempDir,
}

/// A backend that acknowledges a restored copy before a HEAD sees it.
/// All byte operations still use the real in-memory blob store.
struct DelayedRestoredHead(Arc<MemoryBlobStore>);

impl BlobStore for DelayedRestoredHead {
    fn put_temp(&self, bytes: Vec<u8>) -> BoxFuture<'_, Result<String>> {
        self.0.put_temp(bytes)
    }
    fn make_permanent(&self, key: String, cid: Cid) -> BoxFuture<'_, Result<()>> {
        self.0.make_permanent(key, cid)
    }
    fn put_permanent(&self, cid: Cid, bytes: Vec<u8>) -> BoxFuture<'_, Result<()>> {
        self.0.put_permanent(cid, bytes)
    }
    fn quarantine(&self, cid: Cid) -> BoxFuture<'_, Result<()>> {
        self.0.quarantine(cid)
    }
    fn unquarantine(&self, cid: Cid) -> BoxFuture<'_, Result<()>> {
        self.0.unquarantine(cid)
    }
    fn get_bytes(&self, cid: Cid) -> BoxFuture<'_, Result<Vec<u8>>> {
        self.0.get_bytes(cid)
    }
    fn get_stream(&self, cid: Cid) -> BoxFuture<'_, Result<ByteStream>> {
        self.0.get_stream(cid)
    }
    fn has_temp(&self, key: String) -> BoxFuture<'_, Result<bool>> {
        BlobStore::has_temp(&*self.0, key)
    }
    fn has_stored(&self, _cid: Cid) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn delete(&self, cid: Cid) -> BoxFuture<'_, Result<()>> {
        self.0.delete(cid)
    }
    fn delete_many(&self, cids: Vec<Cid>) -> BoxFuture<'_, Result<()>> {
        self.0.delete_many(cids)
    }
    fn make_permanent_copy_only(&self, key: String, cid: Cid) -> BoxFuture<'_, Result<()>> {
        self.0.make_permanent_copy_only(key, cid)
    }
    fn has_quarantined(&self, cid: Cid) -> BoxFuture<'_, Result<bool>> {
        BlobStore::has_quarantined(&*self.0, cid)
    }
    fn restore_copy_only(&self, cid: Cid) -> BoxFuture<'_, Result<()>> {
        self.0.restore_copy_only(cid)
    }
}

async fn test_reader() -> TestBlobReader {
    let dir = tempfile::tempdir().unwrap();
    let db = get_migrated_db(dir.path().join("store.sqlite"))
        .await
        .unwrap();
    let store = Arc::new(MemoryBlobStore::default());
    let reader = BlobReader::new(store.clone(), db, BackgroundQueue::default(), false);
    TestBlobReader {
        reader,
        store,
        _dir: dir,
    }
}

async fn upload(t: &TestBlobReader, bytes: &[u8]) -> BlobRef {
    let metadata = t
        .reader
        .upload_blob_and_get_metadata("text/plain".to_owned(), bytes.to_vec())
        .await
        .unwrap();
    t.reader.track_untethered_blob(metadata).await.unwrap()
}

fn prepared_ref(blob: &BlobRef) -> PreparedBlobRef {
    PreparedBlobRef {
        cid: blob.get_cid().unwrap(),
        mime_type: blob.get_mime_type().to_string(),
        constraints: BlobConstraint {
            max_size: None,
            accept: None,
        },
    }
}

/// An uploaded blob is not served until a record references it and it
/// has been promoted out of temporary storage, on both readers of a
/// shared data directory.
#[tokio::test]
async fn unreferenced_uploads_are_not_served() {
    let t = test_reader().await;
    let blob = upload(&t, b"pending blob bytes").await;
    let cid = blob.get_cid().unwrap();
    let err = t.reader.get_blob_metadata(cid).await.map(drop).unwrap_err();
    assert_eq!(err.to_string(), "Blob not found");
    assert!(t.reader.get_blob(cid).await.is_err());
    // uploading the same bytes again changes nothing
    let again = upload(&t, b"pending blob bytes").await;
    assert_eq!(again.get_cid().unwrap(), cid);
    assert!(t.reader.get_blob_metadata(cid).await.is_err());
    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&blob))
        .await
        .unwrap();
    assert_eq!(t.reader.get_blob_metadata(cid).await.unwrap().size, 18);
}

#[tokio::test]
async fn upload_track_and_promote_blob() {
    let t = test_reader().await;
    let blob = upload(&t, b"some blob bytes").await;
    let cid = blob.get_cid().unwrap();
    assert_eq!(blob.get_mime_type(), "text/plain");
    assert_eq!(t.reader.blob_count().await.unwrap(), 1);
    // still in temp storage
    assert!(!t.store.has_stored(cid).await.unwrap());

    // re-tracking the same blob refreshes the temp key rather than failing
    let metadata = t
        .reader
        .upload_blob_and_get_metadata("text/plain".to_owned(), b"some blob bytes".to_vec())
        .await
        .unwrap();
    t.reader.track_untethered_blob(metadata).await.unwrap();

    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&blob))
        .await
        .unwrap();
    assert!(t.store.has_stored(cid).await.unwrap());
    let metadata = t.reader.get_blob_metadata(cid).await.unwrap();
    assert_eq!(metadata.size, 15);
    assert_eq!(metadata.mime_type.as_deref(), Some("text/plain"));
    let output = t.reader.get_blob(cid).await.unwrap();
    assert_eq!(
        output.stream.collect().await.unwrap().to_vec(),
        b"some blob bytes"
    );
    // promoting again is a no-op
    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&blob))
        .await
        .unwrap();
}

#[tokio::test]
async fn verify_blob_checks_the_stored_mime_type_only() {
    let t = test_reader().await;
    let blob = upload(&t, b"constrained").await;
    let mut wrong_mime = prepared_ref(&blob);
    wrong_mime.mime_type = "image/png".to_owned();
    let err = t
        .reader
        .verify_blob_and_make_permanent(wrong_mime)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<BlobMismatch>().is_some());
    assert_eq!(
        err.to_string(),
        "Referenced Mimetype does not match stored blob. Expected: text/plain, Got: image/png"
    );

    // lexicon constraints are not enforced on writes, as in the reference
    let mut too_large = prepared_ref(&blob);
    too_large.constraints.max_size = Some(1);
    t.reader
        .verify_blob_and_make_permanent(too_large)
        .await
        .unwrap();

    let mut wrong_accept = prepared_ref(&blob);
    wrong_accept.constraints.accept = Some(vec!["image/*".to_owned()]);
    t.reader
        .verify_blob_and_make_permanent(wrong_accept)
        .await
        .unwrap();

    let mut accept_any = prepared_ref(&blob);
    accept_any.constraints.accept = Some(vec!["*/*".to_owned()]);
    t.reader
        .verify_blob_and_make_permanent(accept_any)
        .await
        .unwrap();

    let missing = PreparedBlobRef {
        cid: sha256_to_cid(Sha256::digest(b"missing").to_vec()),
        mime_type: "text/plain".to_owned(),
        constraints: BlobConstraint {
            max_size: None,
            accept: None,
        },
    };
    assert!(t
        .reader
        .verify_blob_and_make_permanent(missing)
        .await
        .is_err());
}

#[tokio::test]
async fn accepted_mime_globs() {
    assert!(accepted_mime("image/png".to_owned(), vec!["*/*".to_owned()]).await);
    assert!(accepted_mime("image/png".to_owned(), vec!["image/*".to_owned()]).await);
    assert!(!accepted_mime("text/plain".to_owned(), vec!["image/*".to_owned()]).await);
    assert!(accepted_mime("text/plain".to_owned(), vec!["text/plain".to_owned()]).await);
    assert!(!accepted_mime("text/plain".to_owned(), vec!["image/png".to_owned()]).await);
}

#[tokio::test]
async fn associates_blobs_with_records() {
    let t = test_reader().await;
    let blob = upload(&t, b"blob for record").await;
    let cid = blob.get_cid().unwrap();
    let record_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a";
    t.reader
        .associate_blob(prepared_ref(&blob), record_uri.to_owned())
        .await
        .unwrap();
    // idempotent
    t.reader
        .associate_blob(prepared_ref(&blob), record_uri.to_owned())
        .await
        .unwrap();
    assert_eq!(
        t.reader.get_records_for_blob(cid).await.unwrap(),
        [record_uri]
    );
    assert_eq!(t.reader.record_blob_count().await.unwrap(), 1);
    assert_eq!(t.reader.get_blob_cids().await.unwrap(), [cid]);
}

#[tokio::test]
async fn takedown_lifecycle() {
    let t = test_reader().await;
    let blob = upload(&t, b"takedown me").await;
    let cid = blob.get_cid().unwrap();
    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&blob))
        .await
        .unwrap();

    let status = t
        .reader
        .get_blob_takedown_status(cid)
        .await
        .unwrap()
        .unwrap();
    assert!(!status.applied && status.r#ref.is_none());
    t.reader
        .update_blob_takedown_status(
            cid,
            StatusAttr {
                applied: true,
                r#ref: Some("ref-1".to_owned()),
            },
        )
        .await
        .unwrap();
    assert!(t.store.has_quarantined(&cid));
    let status = t
        .reader
        .get_blob_takedown_status(cid)
        .await
        .unwrap()
        .unwrap();
    assert!(status.applied);
    assert_eq!(status.r#ref.as_deref(), Some("ref-1"));
    // metadata is hidden while taken down
    assert!(t.reader.get_blob_metadata(cid).await.is_err());
    assert!(t.reader.get_blob(cid).await.is_err());
    // taken-down blob cannot be re-uploaded
    let metadata = t
        .reader
        .upload_blob_and_get_metadata("text/plain".to_owned(), b"takedown me".to_vec())
        .await
        .unwrap();
    assert!(t.reader.track_untethered_blob(metadata).await.is_err());

    // takedown without a ref defaults to a timestamp
    t.reader
        .update_blob_takedown_status(
            cid,
            StatusAttr {
                applied: false,
                r#ref: None,
            },
        )
        .await
        .unwrap();
    assert!(!t.store.has_quarantined(&cid));
    t.reader
        .update_blob_takedown_status(
            cid,
            StatusAttr {
                applied: true,
                r#ref: None,
            },
        )
        .await
        .unwrap();
    let status = t
        .reader
        .get_blob_takedown_status(cid)
        .await
        .unwrap()
        .unwrap();
    assert!(status.applied && status.r#ref.is_some());
    // unknown blob has no status
    let unknown = sha256_to_cid(Sha256::digest(b"unknown").to_vec());
    assert!(t
        .reader
        .get_blob_takedown_status(unknown)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn lists_blobs_and_missing_blobs() {
    let t = test_reader().await;
    let blob = upload(&t, b"listed blob").await;
    let cid = blob.get_cid().unwrap();
    let record_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a";
    t.reader
        .associate_blob(prepared_ref(&blob), record_uri.to_owned())
        .await
        .unwrap();
    // record row for the `since` join
    t.reader
        .db
        .run(move |conn| {
            conn.execute(
                "INSERT INTO record (uri, cid, collection, rkey, \"repoRev\", \"indexedAt\") \
                 VALUES (?1, 'bafyfake', 'app.bsky.feed.post', '3jt5vlkoraa2a', 'rev-2', 'now')",
                [record_uri],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let all = t
        .reader
        .list_blobs(ListBlobsOpts {
            since: None,
            cursor: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(all, [cid.to_string()]);
    let since = t
        .reader
        .list_blobs(ListBlobsOpts {
            since: Some("rev-1".to_owned()),
            cursor: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(since, [cid.to_string()]);
    let since_after = t
        .reader
        .list_blobs(ListBlobsOpts {
            since: Some("rev-2".to_owned()),
            cursor: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert!(since_after.is_empty());
    let cursored = t
        .reader
        .list_blobs(ListBlobsOpts {
            since: None,
            cursor: Some(cid.to_string()),
            limit: 10,
        })
        .await
        .unwrap();
    assert!(cursored.is_empty());

    // a record_blob row without a blob row is a missing blob
    let missing_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkorbb2b";
    t.reader
        .db
        .run(move |conn| {
            conn.execute(
                "INSERT INTO record_blob (\"blobCid\", \"recordUri\") VALUES ('bafymissing', ?1)",
                [missing_uri],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let missing = t
        .reader
        .list_missing_blobs(ListMissingBlobsOpts {
            cursor: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].cid, "bafymissing");
    assert_eq!(missing[0].record_uri, missing_uri);
    let missing_cursored = t
        .reader
        .list_missing_blobs(ListMissingBlobsOpts {
            cursor: Some("bafymissing".to_owned()),
            limit: 10,
        })
        .await
        .unwrap();
    assert!(missing_cursored.is_empty());
    assert!(t
        .reader
        .list_missing_blobs(ListMissingBlobsOpts {
            cursor: None,
            limit: 1001,
        })
        .await
        .is_err());
}

#[test]
fn blob_work_states_are_all_classified_and_round_trip() {
    for state in BlobWorkState::ALL {
        assert_eq!(BlobWorkState::parse(state.as_str()).unwrap(), state);
        // every variant answers the terminal question without panicking
        let _ = state.is_terminal();
    }
    assert!(!BlobWorkState::DeletePending.is_terminal());
    assert!(!BlobWorkState::RestorePending.is_terminal());
    assert!(BlobWorkState::GcDeferred.is_terminal());
    assert!(BlobWorkState::Superseded.is_terminal());
    assert!(BlobWorkState::Done.is_terminal());
    assert!(BlobWorkState::parse("promote-someday").is_err());
    for kind in [
        BlobWorkKind::Permanent,
        BlobWorkKind::Temp,
        BlobWorkKind::Quarantine,
    ] {
        assert_eq!(BlobWorkKind::parse(kind.as_str()).unwrap(), kind);
    }
    assert!(BlobWorkKind::parse("generation").is_err());
}

fn post_with(uri: &str, blob: &BlobRef) -> PreparedWrite {
    PreparedWrite::Create(PreparedCreateOrUpdate {
        action: WriteOpAction::Create,
        uri: uri.to_owned(),
        cid: blob.get_cid().unwrap(),
        swap_cid: None,
        record: serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": "with blob",
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap(),
        blobs: vec![prepared_ref(blob)],
    })
}

fn takedown(applied: bool) -> StatusAttr {
    StatusAttr {
        applied,
        r#ref: Some("mod-ref".to_owned()),
    }
}

/// Under coexistence nothing is ever deleted or moved: promotion copies
/// and journals the temporary object, a takedown is a flag, and a
/// reversal with the permanent object present clears the flag alone.
#[tokio::test]
async fn coexistence_never_deletes_or_moves_an_object() {
    let mut t = test_reader().await;
    t.reader.coexistence = true;
    let blob = upload(&t, b"shared object").await;
    let cid = blob.get_cid().unwrap();
    let uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a";
    t.reader
        .process_write_blobs(vec![post_with(uri, &blob)])
        .await
        .unwrap();
    t.reader.background_queue.process_all().await;
    assert!(t.store.has_stored(cid).await.unwrap());
    let work = t.reader.blob_work().await.unwrap();
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].kind, BlobWorkKind::Temp);
    assert_eq!(work[0].state, BlobWorkState::GcDeferred);
    assert!(t.store.has_temp(&work[0].key), "the temporary object stays");
    assert_eq!(work[0].cid.as_deref(), Some(cid.to_string().as_str()));
    assert!(t.reader.get_blob_metadata(cid).await.is_ok());

    // a re-upload of the same bytes leaves a temporary object nothing
    // will reference; it is journaled for the collector
    let again = upload(&t, b"shared object").await;
    assert_eq!(again.get_cid().unwrap(), cid);
    t.reader.background_queue.process_all().await;
    assert!(t.reader.get_blob_metadata(cid).await.is_ok());
    let work = t.reader.blob_work().await.unwrap();
    assert_eq!(work.len(), 2);
    assert_eq!(work[1].kind, BlobWorkKind::Temp);
    assert!(t.store.has_temp(&work[1].key));

    // takedown: flag only, object untouched, version advanced
    t.reader
        .update_blob_takedown_status(cid, takedown(true))
        .await
        .unwrap();
    assert!(t.reader.get_blob_metadata(cid).await.is_err());
    assert!(t.store.has_stored(cid).await.unwrap());
    assert!(!t.store.has_quarantined(&cid));
    assert_eq!(t.reader.moderation_version(cid).await.unwrap(), 1);
    // reversal with the permanent object present
    t.reader
        .update_blob_takedown_status(cid, takedown(false))
        .await
        .unwrap();
    t.reader.queue_blob_work();
    t.reader.background_queue.process_all().await;
    assert!(t.reader.get_blob_metadata(cid).await.is_ok());
    assert_eq!(t.reader.moderation_version(cid).await.unwrap(), 2);

    // dereferencing journals the permanent object instead of deleting it
    t.reader
        .process_write_blobs(vec![PreparedWrite::Delete(PreparedDelete {
            action: WriteOpAction::Delete,
            uri: uri.to_owned(),
            swap_cid: None,
        })])
        .await
        .unwrap();
    t.reader.background_queue.process_all().await;
    assert!(t.store.has_stored(cid).await.unwrap());
    assert_eq!(t.reader.nonterminal_blob_work().await.unwrap(), 0);
    assert_eq!(t.store.destructive_calls(), 0);
}

/// A takedown the reference implementation applied moved the object to
/// quarantine; reversing it here restores the object by copy, confirms
/// the copy, and only then clears the flag. Each step resumes after a
/// crash, and the quarantined source is kept for the collector.
#[tokio::test]
async fn reversal_restores_a_reference_quarantine_by_copy() {
    let mut t = test_reader().await;
    t.reader.coexistence = true;
    let blob = upload(&t, b"quarantined by the reference").await;
    let cid = blob.get_cid().unwrap();
    // the reference's takedown: flag set, permanent object moved away
    t.reader
        .db
        .run(move |conn| {
            conn.execute(
                "UPDATE blob SET \"takedownRef\" = 'ts-ref', \"tempKey\" = NULL WHERE cid = ?1",
                [cid.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    t.store
        .put_quarantined(cid, b"quarantined by the reference".to_vec());
    assert!(!t.store.has_stored(cid).await.unwrap());

    t.reader
        .update_blob_takedown_status(cid, takedown(false))
        .await
        .unwrap();
    // the flag is not cleared until the object is back
    assert!(t.reader.get_blob_metadata(cid).await.is_err());
    let row = t.reader.blob_work().await.unwrap().remove(0);
    assert_eq!(row.kind, BlobWorkKind::Permanent);
    assert_eq!(row.state, BlobWorkState::RestorePending);
    assert_eq!(row.version, Some(1));
    assert_eq!(t.reader.nonterminal_blob_work().await.unwrap(), 1);
    // crash after the copy: the object is back, the flag is not
    t.reader
        .run_restore(&row, Some(RestoreStep::Copied))
        .await
        .unwrap();
    assert!(t.store.has_stored(cid).await.unwrap());
    assert!(t.reader.get_blob_metadata(cid).await.is_err());
    // crash after the confirmation: same
    t.reader
        .run_restore(&row, Some(RestoreStep::Confirmed))
        .await
        .unwrap();
    assert!(t.reader.get_blob_metadata(cid).await.is_err());
    // the worker finishes what was left
    t.reader.queue_blob_work();
    t.reader.background_queue.process_all().await;
    assert!(t.reader.get_blob_metadata(cid).await.is_ok());
    assert!(t.store.has_stored(cid).await.unwrap());
    assert!(t.store.has_quarantined(&cid), "the source is retained");
    let work = t.reader.blob_work().await.unwrap();
    assert_eq!(work[0].state, BlobWorkState::Done);
    assert_eq!(work[1].kind, BlobWorkKind::Quarantine);
    assert_eq!(work[1].state, BlobWorkState::GcDeferred);
    assert_eq!(t.reader.nonterminal_blob_work().await.unwrap(), 0);
    assert_eq!(t.store.destructive_calls(), 0);

    // a reversal with neither object present clears the flag, as the
    // reference does when there is nothing to move back
    let ghost = sha256_to_cid(Sha256::digest(b"ghost").to_vec());
    t.reader
        .db
        .run(move |conn| {
            conn.execute(
                "INSERT INTO blob (cid, \"mimeType\", size, \"createdAt\", \"takedownRef\") \
                 VALUES (?1, 'text/plain', 5, '2024-01-01T00:00:00.000Z', 'ref')",
                [ghost.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    t.reader
        .update_blob_takedown_status(ghost, takedown(false))
        .await
        .unwrap();
    assert!(t
        .reader
        .get_blob_takedown_status(ghost)
        .await
        .unwrap()
        .map(|status| !status.applied)
        .unwrap_or(false));
}

#[tokio::test]
async fn restoration_keeps_takedown_until_the_copy_is_readable() {
    let mut t = test_reader().await;
    t.reader.coexistence = true;
    let blob = upload(&t, b"eventually visible").await;
    let cid = blob.get_cid().unwrap();
    t.reader
        .db
        .run(move |conn| {
            conn.execute(
                "UPDATE blob SET \"takedownRef\" = 'reference', \"tempKey\" = NULL WHERE cid = ?1",
                [cid.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    t.store.put_quarantined(cid, b"eventually visible".to_vec());
    t.reader
        .update_blob_takedown_status(cid, takedown(false))
        .await
        .unwrap();
    let work = t.reader.blob_work().await.unwrap().remove(0);
    t.reader.blobstore = Arc::new(DelayedRestoredHead(t.store.clone()));

    let error = t.reader.run_restore(&work, None).await.unwrap_err();
    assert!(error.to_string().contains("is not readable yet"));
    assert!(
        t.store.has_stored(cid).await.unwrap(),
        "the copy reached storage"
    );
    assert!(
        t.reader.get_blob_metadata(cid).await.is_err(),
        "the takedown must remain until a readable copy is confirmed"
    );
    assert_eq!(
        t.reader.blob_work().await.unwrap()[0].state,
        BlobWorkState::RestorePending
    );
}

/// Takedown, restoration copies then crashes, restriction cleared,
/// takedown again, restoration resumes last: the resumed restoration
/// finds a newer version and ends superseded, and the newer takedown
/// stands.
#[tokio::test]
async fn a_resumed_restoration_never_clears_a_newer_takedown() {
    let mut t = test_reader().await;
    t.reader.coexistence = true;
    let blob = upload(&t, b"aba").await;
    let cid = blob.get_cid().unwrap();
    t.reader
        .db
        .run(move |conn| {
            conn.execute(
                "UPDATE blob SET \"takedownRef\" = 'ts-ref', \"tempKey\" = NULL WHERE cid = ?1",
                [cid.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    t.store.put_quarantined(cid, b"aba".to_vec());

    // request the restoration; the worker copies and then crashes
    t.reader
        .update_blob_takedown_status(cid, takedown(false))
        .await
        .unwrap();
    let row = t.reader.blob_work().await.unwrap().remove(0);
    assert_eq!(row.version, Some(1));
    assert_eq!(row.state, BlobWorkState::RestorePending);
    t.reader
        .run_restore(&row, Some(RestoreStep::Copied))
        .await
        .unwrap();

    // a newer takedown lands while the restoration is pending
    t.reader
        .update_blob_takedown_status(cid, takedown(true))
        .await
        .unwrap();
    assert_eq!(t.reader.moderation_version(cid).await.unwrap(), 2);
    let superseded = t.reader.blob_work().await.unwrap().remove(0);
    assert_eq!(superseded.state, BlobWorkState::Superseded);

    // the crashed restoration resumes with its stale version
    t.reader.run_restore(&row, None).await.unwrap();
    assert!(t.reader.get_blob_metadata(cid).await.is_err());
    assert_eq!(
        t.reader.blob_work().await.unwrap()[0].state,
        BlobWorkState::Superseded
    );
    assert_eq!(t.reader.blob_work().await.unwrap().len(), 1);
    // a row without a version cannot be restored
    let bad = BlobWork {
        version: None,
        ..row
    };
    assert!(t.reader.run_restore(&bad, None).await.is_err());
    assert_eq!(t.store.destructive_calls(), 0);
}

/// The reference behaviour stays when this server owns the objects.
#[tokio::test]
async fn takedown_moves_objects_when_not_coexisting() {
    let t = test_reader().await;
    let blob = upload(&t, b"owned object").await;
    let cid = blob.get_cid().unwrap();
    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&blob))
        .await
        .unwrap();
    t.reader
        .update_blob_takedown_status(cid, takedown(true))
        .await
        .unwrap();
    assert!(t.store.has_quarantined(&cid));
    assert_eq!(t.reader.moderation_version(cid).await.unwrap(), 1);
    t.reader
        .update_blob_takedown_status(cid, takedown(false))
        .await
        .unwrap();
    assert!(t.store.has_stored(cid).await.unwrap());
    assert!(t.reader.blob_work().await.unwrap().is_empty());
}

/// While another implementation may still serve the store's objects,
/// dereferencing a blob journals it instead of deleting it.
#[tokio::test]
async fn coexistence_defers_dereferenced_blobs_instead_of_deleting() {
    let mut t = test_reader().await;
    t.reader.coexistence = true;
    let blob = upload(&t, b"keep me around").await;
    let cid = blob.get_cid().unwrap();
    let record_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a".to_owned();
    let create = PreparedWrite::Create(PreparedCreateOrUpdate {
        action: WriteOpAction::Create,
        uri: record_uri.clone(),
        cid,
        swap_cid: None,
        record: serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": "with blob",
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap(),
        blobs: vec![prepared_ref(&blob)],
    });
    t.reader.process_write_blobs(vec![create]).await.unwrap();
    let delete = PreparedWrite::Delete(PreparedDelete {
        action: WriteOpAction::Delete,
        uri: record_uri,
        swap_cid: None,
    });
    t.reader.process_write_blobs(vec![delete]).await.unwrap();
    t.reader.background_queue.process_all().await;
    assert_eq!(t.reader.blob_count().await.unwrap(), 0);
    assert!(t.store.has_stored(cid).await.unwrap());
    let work = t.reader.blob_work().await.unwrap();
    assert_eq!(work.len(), 2);
    assert_eq!(work[0].kind, BlobWorkKind::Temp);
    assert_eq!(work[1].kind, BlobWorkKind::Permanent);
    assert_eq!(work[1].key, cid.to_string());
    assert_eq!(work[1].state, BlobWorkState::GcDeferred);
    assert_eq!(t.reader.nonterminal_blob_work().await.unwrap(), 0);
}

/// Outside coexistence the deletion is journaled first and runs after
/// the write's transaction; a failing store leaves the row pending.
#[tokio::test]
async fn dereferenced_blob_deletion_is_journaled() {
    let t = test_reader().await;
    let blob = upload(&t, b"journal me").await;
    let cid = blob.get_cid().unwrap();
    let record_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a".to_owned();
    let create = PreparedWrite::Create(PreparedCreateOrUpdate {
        action: WriteOpAction::Create,
        uri: record_uri.clone(),
        cid,
        swap_cid: None,
        record: serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": "with blob",
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap(),
        blobs: vec![prepared_ref(&blob)],
    });
    t.reader.process_write_blobs(vec![create]).await.unwrap();
    t.reader.background_queue.process_all().await;
    let delete = PreparedWrite::Delete(PreparedDelete {
        action: WriteOpAction::Delete,
        uri: record_uri,
        swap_cid: None,
    });
    let promoted = t
        .reader
        .promote_write_blobs(&[delete.clone()], false)
        .await
        .unwrap();
    assert!(promoted.is_empty());
    let writes = vec![delete];
    t.reader
        .db
        .tx(move |tx| apply_write_blobs_in(tx, &writes, &[], false, "2024-01-01T00:00:00.000Z"))
        .await
        .unwrap();
    let work = t.reader.blob_work().await.unwrap();
    assert_eq!(work[0].state, BlobWorkState::DeletePending);
    assert_eq!(t.reader.nonterminal_blob_work().await.unwrap(), 1);
    assert!(t.store.has_stored(cid).await.unwrap());

    t.reader.run_blob_work().await.unwrap();
    assert!(!t.store.has_stored(cid).await.unwrap());
    assert_eq!(
        t.reader.blob_work().await.unwrap()[0].state,
        BlobWorkState::Done
    );
    assert_eq!(t.reader.nonterminal_blob_work().await.unwrap(), 0);
    // nothing pending is a no-op
    t.reader.run_blob_work().await.unwrap();
}

#[tokio::test]
async fn promotion_requires_the_blob_unless_tolerated() {
    let t = test_reader().await;
    let missing = PreparedBlobRef {
        cid: sha256_to_cid(Sha256::digest(b"never uploaded").to_vec()),
        mime_type: "text/plain".to_owned(),
        constraints: BlobConstraint {
            max_size: None,
            accept: None,
        },
    };
    let write = PreparedWrite::Create(PreparedCreateOrUpdate {
        action: WriteOpAction::Create,
        uri: "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a".to_owned(),
        cid: missing.cid,
        swap_cid: None,
        record: serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": "with missing blob",
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap(),
        blobs: vec![missing],
    });
    let err = t
        .reader
        .promote_write_blobs(std::slice::from_ref(&write), false)
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("Could not find blob"));
    assert!(t
        .reader
        .promote_write_blobs(&[write], true)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn process_write_blobs_deletes_dereferenced() {
    let t = test_reader().await;
    let blob = upload(&t, b"dereference me").await;
    let cid = blob.get_cid().unwrap();
    let record_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a".to_owned();

    let create = PreparedWrite::Create(PreparedCreateOrUpdate {
        action: WriteOpAction::Create,
        uri: record_uri.clone(),
        cid,
        swap_cid: None,
        record: serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": "with blob",
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap(),
        blobs: vec![prepared_ref(&blob)],
    });
    t.reader.process_write_blobs(vec![create]).await.unwrap();
    assert!(t.store.has_stored(cid).await.unwrap());
    assert_eq!(
        t.reader.get_records_for_blob(cid).await.unwrap(),
        [record_uri.clone()]
    );

    // deleting the record dereferences and deletes the blob
    let delete = PreparedWrite::Delete(PreparedDelete {
        action: WriteOpAction::Delete,
        uri: record_uri.clone(),
        swap_cid: None,
    });
    t.reader.process_write_blobs(vec![delete]).await.unwrap();
    t.reader.background_queue.process_all().await;
    assert_eq!(t.reader.blob_count().await.unwrap(), 0);
    assert!(!t.store.has_stored(cid).await.unwrap());
    assert!(t.reader.get_records_for_blob(cid).await.unwrap().is_empty());

    // deleting a record with no blobs is a no-op
    let delete_again = PreparedWrite::Delete(PreparedDelete {
        action: WriteOpAction::Delete,
        uri: record_uri,
        swap_cid: None,
    });
    t.reader
        .process_write_blobs(vec![delete_again])
        .await
        .unwrap();
}

#[tokio::test]
async fn process_write_blobs_handles_updates() {
    let t = test_reader().await;
    let old_blob = upload(&t, b"old media").await;
    let old_cid = old_blob.get_cid().unwrap();
    let record_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a".to_owned();
    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&old_blob))
        .await
        .unwrap();
    t.reader
        .associate_blob(prepared_ref(&old_blob), record_uri.clone())
        .await
        .unwrap();

    let new_blob = upload(&t, b"new media").await;
    let new_cid = new_blob.get_cid().unwrap();
    let update = PreparedWrite::Update(PreparedCreateOrUpdate {
        action: WriteOpAction::Update,
        uri: record_uri.clone(),
        cid: new_cid,
        swap_cid: None,
        record: serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": "updated",
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap(),
        blobs: vec![prepared_ref(&new_blob)],
    });
    t.reader.process_write_blobs(vec![update]).await.unwrap();
    t.reader.background_queue.process_all().await;

    // old blob dereferenced and deleted, new blob permanent and associated
    assert!(!t.store.has_stored(old_cid).await.unwrap());
    assert!(t.store.has_stored(new_cid).await.unwrap());
    assert_eq!(
        t.reader.get_records_for_blob(new_cid).await.unwrap(),
        [record_uri]
    );
    assert_eq!(t.reader.blob_count().await.unwrap(), 1);
}

#[tokio::test]
async fn takedown_logs_missing_blobstore_entry() {
    let t = test_reader().await;
    // row exists but blob bytes were never promoted to storage
    let blob = upload(&t, b"row only").await;
    let cid = blob.get_cid().unwrap();
    t.reader
        .update_blob_takedown_status(
            cid,
            StatusAttr {
                applied: true,
                r#ref: Some("ref-x".to_owned()),
            },
        )
        .await
        .unwrap();
    let status = t
        .reader
        .get_blob_takedown_status(cid)
        .await
        .unwrap()
        .unwrap();
    assert!(status.applied);
}

#[tokio::test]
async fn verify_blob_accepts_within_max_size() {
    let t = test_reader().await;
    let blob = upload(&t, b"small").await;
    let mut sized = prepared_ref(&blob);
    sized.constraints.max_size = Some(1024);
    t.reader
        .verify_blob_and_make_permanent(sized)
        .await
        .unwrap();
}

#[tokio::test]
async fn mixed_delete_and_create_keeps_new_blobs() {
    let t = test_reader().await;
    let old_blob = upload(&t, b"replaced media").await;
    let old_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a".to_owned();
    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&old_blob))
        .await
        .unwrap();
    t.reader
        .associate_blob(prepared_ref(&old_blob), old_uri.clone())
        .await
        .unwrap();

    // re-upload the same bytes for a new record while deleting the old one
    let new_blob = upload(&t, b"replaced media").await;
    let new_uri = "at://did:example:alice/app.bsky.feed.post/3jt5vlkorbb2b".to_owned();
    let delete = PreparedWrite::Delete(PreparedDelete {
        action: WriteOpAction::Delete,
        uri: old_uri,
        swap_cid: None,
    });
    let create = PreparedWrite::Create(PreparedCreateOrUpdate {
        action: WriteOpAction::Create,
        uri: new_uri.clone(),
        cid: new_blob.get_cid().unwrap(),
        swap_cid: None,
        record: serde_json::from_value(serde_json::json!({
            "$type": "app.bsky.feed.post",
            "text": "recreated",
            "createdAt": "2023-01-01T00:00:00.000Z",
        }))
        .unwrap(),
        blobs: vec![prepared_ref(&new_blob)],
    });
    t.reader
        .process_write_blobs(vec![delete, create])
        .await
        .unwrap();
    t.reader.background_queue.process_all().await;
    // blob is kept because the create in the same commit references it
    let cid = new_blob.get_cid().unwrap();
    assert_eq!(t.reader.blob_count().await.unwrap(), 1);
    assert!(t.store.has_stored(cid).await.unwrap());
    assert_eq!(t.reader.get_records_for_blob(cid).await.unwrap(), [new_uri]);
}

#[tokio::test]
async fn keeps_blobs_still_referenced_elsewhere() {
    let t = test_reader().await;
    let blob = upload(&t, b"shared blob").await;
    let cid = blob.get_cid().unwrap();
    let uri_one = "at://did:example:alice/app.bsky.feed.post/3jt5vlkoraa2a".to_owned();
    let uri_two = "at://did:example:alice/app.bsky.feed.post/3jt5vlkorbb2b".to_owned();
    t.reader
        .verify_blob_and_make_permanent(prepared_ref(&blob))
        .await
        .unwrap();
    t.reader
        .associate_blob(prepared_ref(&blob), uri_one.clone())
        .await
        .unwrap();
    t.reader
        .associate_blob(prepared_ref(&blob), uri_two)
        .await
        .unwrap();

    let delete = PreparedWrite::Delete(PreparedDelete {
        action: WriteOpAction::Delete,
        uri: uri_one,
        swap_cid: None,
    });
    t.reader.process_write_blobs(vec![delete]).await.unwrap();
    t.reader.background_queue.process_all().await;
    // still referenced by uri_two, so blob row and bytes remain
    assert_eq!(t.reader.blob_count().await.unwrap(), 1);
    assert!(t.store.has_stored(cid).await.unwrap());
}
