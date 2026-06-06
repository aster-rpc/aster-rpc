//! Contract tests for the portal-sync persistence surface:
//!
//! * `CoreNode::persistent_with_alpns` keeps iroh-docs state (namespace +
//!   author + entry) across a close/reopen cycle — not just blobs.
//! * `CoreBlobsClient::add_path` imports a file (Copy mode) and the blob
//!   survives `CoreNode::close()` + reopen, proving `close()` flushes the store.
//! * `CoreBlobsClient::add_path_with_named_tag` sets a deterministic, caller-keyed
//!   tag that also survives reopen.
//!
//! All nodes run with relay disabled so the test stays offline and fast.

use std::path::Path;
use std::time::Duration;

use aster_transport_core::{CoreEndpointConfig, CoreNode};

/// A persistent node with relay/discovery off, loaded from `dir`.
async fn open_node(dir: &Path) -> CoreNode {
    let cfg = CoreEndpointConfig {
        relay_mode: Some("disabled".to_string()),
        ..Default::default()
    };
    CoreNode::persistent_with_alpns(dir.to_string_lossy().into_owned(), Vec::new(), Some(cfg))
        .await
        .expect("open persistent node")
}

/// Like [`open_node`], but with blob GC enabled. The interval is long enough
/// that the background loop never fires during a test — reclamation is driven
/// deterministically via `gc_run_once`. Enabling GC at all still exercises the
/// `GcConfig.add_protected` wiring at construction.
async fn open_node_with_gc(dir: &Path) -> CoreNode {
    let cfg = CoreEndpointConfig {
        relay_mode: Some("disabled".to_string()),
        ..Default::default()
    };
    CoreNode::persistent_with_alpns_and_gc(
        dir.to_string_lossy().into_owned(),
        Vec::new(),
        Some(cfg),
        Some(Duration::from_secs(3600)),
    )
    .await
    .expect("open persistent node with gc")
}

/// Unique temp directory for one test run; removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aster-persist-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_docs_and_blobs_survive_close_and_reopen() {
    let dir = TempDir::new("docs-blobs");

    let key = b"portal-sync/entry".to_vec();
    let value = b"slice-4 payload".to_vec();
    let file_contents = b"file imported via add_path".to_vec();

    let file_path = dir.path().join("import-me.bin");
    std::fs::write(&file_path, &file_contents).expect("write source file");
    let file_path_str = file_path.to_string_lossy().into_owned();

    // ── Phase 1: write blob + docs state, then close cleanly. ──────────────
    let (namespace_id, author, blob_hash) = {
        let node = open_node(dir.path()).await;

        let blobs = node.blobs_client();
        let blob_hash = blobs
            .add_path(file_path_str.clone())
            .await
            .expect("add_path");

        let docs = node.docs_client();
        let author = docs.create_author().await.expect("create_author");
        let doc = docs.create().await.expect("create doc");
        let namespace_id = doc.doc_id();
        doc.set_bytes(author.clone(), key.clone(), value.clone())
            .await
            .expect("set_bytes");

        // Clean shutdown: must flush the redb metadata + blob store to disk.
        node.close().await;
        (namespace_id, author, blob_hash)
    };

    // The persistent docs store should have been written to disk.
    assert!(
        dir.path().join("docs.redb").exists(),
        "Docs::persistent should write docs.redb under the node directory"
    );

    // ── Phase 2: reopen the same directory and assert everything survived. ─
    let node = open_node(dir.path()).await;

    // Blob: present and byte-identical (proves add_path durability + close() flush).
    let blobs = node.blobs_client();
    assert!(
        blobs.blob_has(blob_hash.clone()).await.expect("blob_has"),
        "imported blob should survive close/reopen"
    );
    let read_back = blobs
        .read_to_bytes(blob_hash.clone())
        .await
        .expect("read_to_bytes");
    assert_eq!(
        read_back, file_contents,
        "blob contents changed across reopen"
    );

    // Docs: namespace + author + entry all reloaded from docs.redb.
    let docs = node.docs_client();
    let doc = docs
        .open(namespace_id.clone())
        .await
        .expect("open namespace")
        .expect("namespace should still exist after reopen");
    let got = doc
        .get_exact(author, key)
        .await
        .expect("get_exact")
        .expect("entry should still exist after reopen");
    assert_eq!(got, value, "doc entry value changed across reopen");

    // Opening an unknown namespace must return Ok(None), not an error — the
    // documented contract (upstream DocsApi::open errors "Replica not found").
    let unknown_ns = "ff".repeat(32);
    let missing = docs
        .open(unknown_ns)
        .await
        .expect("open of an unknown namespace must not error");
    assert!(missing.is_none(), "unknown namespace should open to None");

    node.close().await;
}

/// Regression: blob GC must not reclaim the blobs backing iroh-docs entry
/// content. iroh-docs stores each entry's value as an *untagged* content blob in
/// the shared store and protects it via the docs engine's GC protect callback
/// (not via tags). If Aster enables GC without wiring that callback into both
/// the periodic loop and `gc_run_once`, the first sweep deletes synced docs
/// content and subsequent reads fail with `NotFound`. This reproduced portal's
/// "reading root policy record … Io(Kind(NotFound))" control-plane failure.
///
/// The control assertion (an untagged non-docs blob *is* collected) proves the
/// callback protects docs content specifically rather than disabling GC
/// wholesale — and that the docs `protect_handler` is actually wired, since an
/// unwired callback would `Abort` every sweep and leave the untagged blob alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docs_entry_content_survives_blob_gc() {
    let dir = TempDir::new("docs-gc");
    let node = open_node_with_gc(dir.path()).await;

    let key = b"portal/root-policy".to_vec();
    let value = b"root policy record payload".to_vec();

    // A doc entry: its content is stored as an untagged blob in the shared store.
    let docs = node.docs_client();
    let author = docs.create_author().await.expect("create_author");
    let doc = docs.create().await.expect("create doc");
    let namespace_id = doc.doc_id();
    let content_hash = doc
        .set_bytes(author.clone(), key.clone(), value.clone())
        .await
        .expect("set_bytes");

    // A plain blob with no protection: this is the control — GC must reclaim it.
    // `add_bytes` creates a persistent auto-tag, so drop every tag pointing at
    // the blob to leave it genuinely unprotected.
    let blobs = node.blobs_client();
    let untagged_hash = blobs
        .add_bytes(b"collectable untagged blob".to_vec())
        .await
        .expect("add_bytes");
    for tag in blobs.tag_list().await.expect("tag_list") {
        if tag.hash == untagged_hash {
            blobs.tag_delete(tag.name).await.expect("tag_delete");
        }
    }
    assert!(
        blobs
            .blob_has(untagged_hash.clone())
            .await
            .expect("blob_has (pre-gc)"),
        "untagged blob should exist before GC"
    );

    // Deterministic sweep.
    blobs.gc_run_once().await.expect("gc_run_once");

    // Docs content survived: the backing blob is still present …
    assert!(
        blobs
            .blob_has(content_hash.clone())
            .await
            .expect("blob_has docs content"),
        "docs content blob {content_hash} was collected by GC"
    );
    // … and is still readable through the docs API.
    let got = doc
        .get_exact(author, key)
        .await
        .expect("get_exact after gc")
        .expect("doc entry should survive blob GC");
    assert_eq!(got, value, "doc entry content changed across GC");

    // And the namespace is still readable after reopening it fresh.
    let reopened = docs
        .open(namespace_id)
        .await
        .expect("open namespace after gc")
        .expect("namespace should survive blob GC");
    drop(reopened);

    // Control: the untagged, unprotected blob was actually reclaimed — proving
    // GC ran and only the docs-protected content was spared.
    assert!(
        !blobs
            .blob_has(untagged_hash)
            .await
            .expect("blob_has (post-gc)"),
        "untagged blob should be collected by GC"
    );

    node.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_path_with_named_tag_sets_deterministic_tag_that_persists() {
    let dir = TempDir::new("named-tag");

    let file_contents = b"tagged file".to_vec();
    let file_path = dir.path().join("tagged.bin");
    std::fs::write(&file_path, &file_contents).expect("write source file");
    let file_path_str = file_path.to_string_lossy().into_owned();

    // Phase 1: import with a caller-chosen tag keyed like portal-sync would.
    let (blob_hash, tag_name) = {
        let node = open_node(dir.path()).await;
        let blobs = node.blobs_client();
        let blob_hash = blobs
            .add_path(file_path_str.clone())
            .await
            .expect("add_path (for hash)");
        // delete the anonymous auto-tag add_path created so only our named tag remains
        let tag_name = format!("portal-sync/t1/{blob_hash}");
        let tagged_hash = blobs
            .add_path_with_named_tag(file_path_str.clone(), tag_name.clone())
            .await
            .expect("add_path_with_named_tag");
        assert_eq!(tagged_hash, blob_hash, "same bytes must hash identically");
        node.close().await;
        (blob_hash, tag_name)
    };

    // Phase 2: reopen — the named tag and its target must still be there.
    let node = open_node(dir.path()).await;
    let blobs = node.blobs_client();
    let tag = blobs
        .tag_get(tag_name.clone())
        .await
        .expect("tag_get")
        .expect("named tag should survive reopen");
    assert_eq!(tag.name, tag_name);
    assert_eq!(tag.hash, blob_hash);
    assert!(blobs.blob_has(blob_hash).await.expect("blob_has"));

    node.close().await;
}
