use super::*;

const VIDEO_CID: &str = "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku";
const VTT_CID: &str = "bafkreibjfgx2gprinfvicegelk5kosd6y2frmqpqzwqkg7usac74l3t2v4";

fn blob_json(cid: &str, mime_type: &str, size: i64) -> JsonValue {
    json!({
        "$type": "blob",
        "ref": { "$link": cid },
        "mimeType": mime_type,
        "size": size,
    })
}

fn video_post_record() -> RepoRecord {
    serde_json::from_value(json!({
        "$type": "app.bsky.feed.post",
        "text": "video post",
        "createdAt": "2026-01-01T00:00:00.000Z",
        "embed": {
            "$type": "app.bsky.embed.video",
            "video": blob_json(VIDEO_CID, "video/mp4", 1_048_576),
            "captions": [
                { "lang": "en", "file": blob_json(VTT_CID, "text/vtt", 1024) }
            ],
        },
    }))
    .expect("record deserializes")
}

#[test]
fn finds_blob_refs_in_video_embed() {
    let refs = find_blob_refs(Lex::Map(video_post_record()), None, None);
    let mut paths = refs
        .iter()
        .map(|r| r.path.join("/"))
        .collect::<Vec<String>>();
    paths.sort();
    assert_eq!(paths, vec!["embed/captions/file", "embed/video"]);
}

#[test]
fn blobs_for_write_applies_video_constraints() {
    let prepared = blobs_for_write(video_post_record(), true).expect("blobs prepared");
    let video = prepared
        .iter()
        .find(|blob| blob.mime_type == "video/mp4")
        .expect("video blob present");
    assert_eq!(video.cid.to_string(), VIDEO_CID);
    assert_eq!(
        video.constraints.accept,
        Some(vec!["video/mp4".to_string()])
    );
    assert_eq!(video.constraints.max_size, Some(100_000_000));
    let captions = prepared
        .iter()
        .find(|blob| blob.mime_type == "text/vtt")
        .expect("captions blob present");
    assert_eq!(captions.cid.to_string(), VTT_CID);
    assert_eq!(
        captions.constraints.accept,
        Some(vec!["text/vtt".to_string()])
    );
    assert_eq!(captions.constraints.max_size, Some(20_000));
}

#[test]
fn finds_blob_refs_in_image_embed() {
    let record: RepoRecord = serde_json::from_value(json!({
        "$type": "app.bsky.feed.post",
        "text": "image post",
        "createdAt": "2026-01-01T00:00:00.000Z",
        "embed": {
            "$type": "app.bsky.embed.images",
            "images": [
                { "image": blob_json(VIDEO_CID, "image/png", 512), "alt": "" }
            ],
        },
    }))
    .expect("record deserializes");
    let refs = find_blob_refs(Lex::Map(record), None, None);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].path.join("/"), "embed/images/image");
}
