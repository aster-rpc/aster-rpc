#![cfg(feature = "blobs")]
//! Opt-in blob garbage collection: `gc_interval` config + the deterministic
//! `Blobs::gc_run_once` manual sweep. Mirrors the acceptance criteria in
//! `docs/portal-blob-gc-requirements.md`.
//!
//! portal-sync drives retention entirely through **named tags** (one persistent
//! tag per referencing object) and reclaims a blob by deleting its last tag.
//! These tests assert that wiring: tagged blobs survive a sweep, untagged blobs
//! are collected, and the default (no `gc_interval`) stays grow-only.
//!
//! Note on `add_bytes`/`add_path`: those auto-create a persistent `auto-<ts>`
//! tag, so a bare-added blob is *not* collectable until that tag is removed —
//! see [`add_bytes_auto_tag_blocks_collection`]. portal-sync therefore uses
//! [`Blobs::add_path_with_named_tag`](aster::Blobs::add_path_with_named_tag),
//! which sets only the caller's named tag. The helper below mirrors that.

use std::time::Duration;

use aster::{AsterConfig, BlobFormat, Blobs, Node, RelayMode};

/// In-memory node with periodic GC enabled at a long interval. The tests never
/// wait on the interval — they call `gc_run_once` for deterministic sweeps —
/// but enabling it exercises the `gc_interval` store-construction path.
async fn gc_node() -> Node {
    Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .gc_interval(Duration::from_secs(3600))
            .build(),
    )
    .await
    .expect("start gc node")
}

/// Import `content` under a single caller-chosen persistent tag (portal-sync's
/// pattern), returning the blob hash. Unlike `add_bytes`, this sets *only* the
/// named tag — no `auto-*` tag — so deleting `tag` makes the blob collectable.
async fn add_tagged(
    blobs: &Blobs,
    dir: &std::path::Path,
    content: &[u8],
    tag: &str,
) -> aster::Hash {
    let path = dir.join(tag.replace('/', "_"));
    std::fs::write(&path, content).unwrap();
    blobs
        .add_path_with_named_tag(path.to_string_lossy().to_string(), tag.to_string())
        .await
        .unwrap()
}

/// 1. Default off: a node built without `gc_interval` keeps blobs — nothing
///    collects them automatically (current grow-only behavior preserved).
#[tokio::test]
async fn default_off_preserves_blob() {
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .build(),
    )
    .await
    .unwrap();
    let blobs = node.blobs();

    let hash = blobs.add_bytes(b"grow-only".to_vec()).await.unwrap();
    // Give any (unconfigured) background task a chance to run; it must not.
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        blobs.has(&hash).await.unwrap(),
        "without gc_interval the blob must be preserved"
    );
    node.shutdown().await;
}

/// 2. Tagged survives: a blob with a live persistent tag survives a sweep.
#[tokio::test]
async fn tagged_blob_survives_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let node = gc_node().await;
    let blobs = node.blobs();

    let hash = add_tagged(&blobs, dir.path(), b"keep-me", "portal/tree/obj").await;

    blobs.gc_run_once().await.unwrap();

    assert!(
        blobs.has(&hash).await.unwrap(),
        "a tagged blob must survive gc_run_once"
    );
    node.shutdown().await;
}

/// 3. Untagged collected: once a blob's only tag is deleted, a sweep reclaims
///    it.
#[tokio::test]
async fn untagged_blob_is_collected() {
    let dir = tempfile::tempdir().unwrap();
    let node = gc_node().await;
    let blobs = node.blobs();

    let hash = add_tagged(&blobs, dir.path(), b"collect-me", "tmp/obj").await;
    assert!(blobs.has(&hash).await.unwrap(), "blob present before sweep");

    blobs.tag_delete("tmp/obj").await.unwrap();
    blobs.gc_run_once().await.unwrap();

    assert!(
        !blobs.has(&hash).await.unwrap(),
        "an untagged blob must be collected by gc_run_once"
    );
    node.shutdown().await;
}

/// 4. Dedup / refcount: two tags pin one deduplicated blob. It is reclaimed only
///    once the last tag is gone — the behavior portal-sync relies on for its
///    tag-as-refcount scheme.
#[tokio::test]
async fn tags_act_as_refcount() {
    let dir = tempfile::tempdir().unwrap();
    let node = gc_node().await;
    let blobs = node.blobs();

    // Same content under one named tag, then a second tag on the same hash.
    let hash = add_tagged(&blobs, dir.path(), b"shared", "ref/a").await;
    blobs
        .tag_set("ref/b", &hash, BlobFormat::Raw)
        .await
        .unwrap();

    // Both tags present → survives.
    blobs.gc_run_once().await.unwrap();
    assert!(blobs.has(&hash).await.unwrap(), "survives with two tags");

    // One tag remaining → still survives.
    blobs.tag_delete("ref/a").await.unwrap();
    blobs.gc_run_once().await.unwrap();
    assert!(blobs.has(&hash).await.unwrap(), "survives with one tag");

    // Last tag gone → collected.
    blobs.tag_delete("ref/b").await.unwrap();
    blobs.gc_run_once().await.unwrap();
    assert!(
        !blobs.has(&hash).await.unwrap(),
        "collected once the last tag is deleted"
    );
    node.shutdown().await;
}

/// 5. Manual sweep determinism: `gc_run_once` reclaims within the single call —
///    no sleep, no dependence on the timed interval.
#[tokio::test]
async fn manual_sweep_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let node = gc_node().await;
    let blobs = node.blobs();

    let hash = add_tagged(&blobs, dir.path(), b"transient", "tmp/x").await;
    blobs.tag_delete("tmp/x").await.unwrap();
    blobs.gc_run_once().await.unwrap();

    assert!(
        !blobs.has(&hash).await.unwrap(),
        "reclamation must complete within the gc_run_once call"
    );
    node.shutdown().await;
}

/// `add_bytes` (and `add_path`) auto-create a persistent `auto-<ts>` tag, so a
/// bare-added blob survives GC until that tag is cleared. Documents the trap
/// portal-sync avoids by always tagging explicitly; deleting the auto-tag makes
/// the blob collectable again.
#[tokio::test]
async fn add_bytes_auto_tag_blocks_collection() {
    let node = gc_node().await;
    let blobs = node.blobs();

    let hash = blobs.add_bytes(b"auto-tagged".to_vec()).await.unwrap();
    blobs.gc_run_once().await.unwrap();
    assert!(
        blobs.has(&hash).await.unwrap(),
        "auto-tag from add_bytes must keep the blob alive through a sweep"
    );

    // Clearing the auto-tag leaves the blob unprotected → next sweep reclaims.
    let removed = blobs.tag_delete_prefix("auto-").await.unwrap();
    assert!(removed >= 1, "expected an auto-* tag to delete");
    blobs.gc_run_once().await.unwrap();
    assert!(
        !blobs.has(&hash).await.unwrap(),
        "blob collected once its auto-tag is gone"
    );
    node.shutdown().await;
}

/// Persistent path: the FsStore `load_with_opts` GC branch loads and sweeps just
/// like the in-memory store. Covers `persistent_with_alpns_and_gc`.
#[tokio::test]
async fn persistent_node_gc_collects_untagged() {
    let dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .persistent(dir.path().join("gc-node"))
            .gc_interval(Duration::from_secs(3600))
            .build(),
    )
    .await
    .unwrap();
    let blobs = node.blobs();

    let kept = add_tagged(&blobs, src.path(), b"kept", "keep").await;
    let dropped = add_tagged(&blobs, src.path(), b"dropped", "drop").await;
    blobs.tag_delete("drop").await.unwrap();

    blobs.gc_run_once().await.unwrap();

    assert!(
        blobs.has(&kept).await.unwrap(),
        "tagged blob retained on disk"
    );
    assert!(
        !blobs.has(&dropped).await.unwrap(),
        "untagged blob reclaimed from disk"
    );
    node.shutdown().await;
}
