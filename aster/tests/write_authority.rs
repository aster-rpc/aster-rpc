#![cfg(feature = "docs")]
//! Retiring a node's **local** write authority over a namespace: the reference
//! accounting that gates it, the consuming shutdown that satisfies it, and the
//! two behaviours a caller must not assume away.

use std::time::{Duration, Instant};

use aster::{AsterConfig, LocalAccess, NamespaceSecret, Node, RelayMode, RetireOutcome};

async fn mem_node() -> Node {
    Node::start(AsterConfig::builder().relay(RelayMode::Disabled).build())
        .await
        .expect("start")
}

const BUDGET: Duration = Duration::from_secs(5);

/// Both references have to go: the live-sync one and the caller's own.
///
/// Asserting the 2 → 1 → 0 transition is what pins that an implementation
/// cannot quietly get away with releasing one of them and calling it done.
#[tokio::test]
async fn retirement_needs_both_references_released() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x11u8; 32]);
    let id = secret.id();
    let doc = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();

    doc.start_sync(&[]).await.unwrap();
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Busy { handles: 2 },
        "live sync and the caller's handle are separate references",
    );

    doc.leave().await.unwrap();
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Busy { handles: 1 },
        "leaving releases the sync reference and nothing else",
    );

    doc.shutdown_for_role_swap(BUDGET).await.unwrap();
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Retired(LocalAccess::Read),
        "only at zero references may write authority be retired",
    );
    node.shutdown().await;
}

/// The consuming shutdown releases both references in one call, and does it
/// within a single budget rather than one per step.
#[tokio::test]
async fn shutdown_for_role_swap_releases_both_within_one_budget() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x22u8; 32]);
    let id = secret.id();
    let doc = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();
    doc.start_sync(&[]).await.unwrap();

    let budget = Duration::from_millis(500);
    let started = Instant::now();
    doc.shutdown_for_role_swap(budget).await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed < budget,
        "one absolute deadline covers leave and close; two sequential budgets \
         would silently double what the caller allowed ({elapsed:?})",
    );
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Retired(LocalAccess::Read),
        "both references must be gone after the consuming shutdown",
    );
    node.shutdown().await;
}

/// An independent open the caller did not account for keeps it `Busy` — and is
/// **not** closed on its behalf.
///
/// Retiring under an unexpected handle would leave a writable reference behind
/// an operation reporting success. Reporting `Busy` says the caller does not
/// own what it thought it owned.
#[tokio::test]
async fn an_unexpected_open_stays_busy_and_is_not_closed() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x33u8; 32]);
    let id = secret.id();
    let owned = node
        .docs()
        .open_or_import_write_namespace(secret.clone())
        .await
        .unwrap();
    // Somebody else's handle on the same namespace.
    let stranger = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();

    owned.shutdown_for_role_swap(BUDGET).await.unwrap();
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Busy { handles: 1 },
        "the reference we did not own must still count",
    );
    // And it was left alone: it still works.
    let author = node.docs().default_author().await.unwrap();
    stranger
        .set_bytes(&author, b"k".to_vec(), b"v".to_vec())
        .await
        .expect("an unexpected handle must not be closed on the caller's behalf");
    node.shutdown().await;
}

/// A handle put through the role-swap shutdown stays unusable — including
/// after write is imported again.
///
/// The close flag is shared with every clone, so this is the one invalidation
/// the layer really does provide, and it is permanent.
#[tokio::test]
async fn a_shut_down_handle_stays_dead_across_reimport() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x44u8; 32]);
    let id = secret.id();
    let doc = node
        .docs()
        .open_or_import_write_namespace(secret.clone())
        .await
        .unwrap();
    let author = node.docs().default_author().await.unwrap();
    doc.set_bytes(&author, b"k".to_vec(), b"v1".to_vec())
        .await
        .unwrap();

    let clone = doc.clone();
    doc.shutdown_for_role_swap(BUDGET).await.unwrap();
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Retired(LocalAccess::Read),
    );

    // Write comes back, and the clone is still dead — the flag is shared.
    let _regranted = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();
    assert!(
        clone
            .set_bytes(&author, b"k".to_vec(), b"v2".to_vec())
            .await
            .is_err(),
        "a clone of a shut-down handle must never revive",
    );
    node.shutdown().await;
}

/// A handle opened *after* retirement widens in place when write is imported —
/// which is exactly why Read→Write must close the old read handle first.
///
/// A handle addresses a namespace; it is not a snapshot of the capability it
/// was opened under. Nothing here is a defect to hide: it is the reason the
/// widening direction is as ordered as the narrowing one.
#[tokio::test]
async fn a_post_retirement_read_handle_widens_on_write_import() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x55u8; 32]);
    let id = secret.id();
    let doc = node
        .docs()
        .open_or_import_write_namespace(secret.clone())
        .await
        .unwrap();
    let author = node.docs().default_author().await.unwrap();
    doc.set_bytes(&author, b"k".to_vec(), b"v1".to_vec())
        .await
        .unwrap();
    doc.shutdown_for_role_swap(BUDGET).await.unwrap();
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Retired(LocalAccess::Read),
    );

    // A read handle taken while the namespace is retired.
    let reader = node.docs().open_or_import_read_namespace(id).await.unwrap();
    assert!(
        reader
            .set_bytes(&author, b"k".to_vec(), b"v2".to_vec())
            .await
            .is_err(),
        "while retired, no handle may write",
    );

    // Import write: that same handle now writes, without being reopened.
    let _regranted = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();
    reader
        .set_bytes(&author, b"k".to_vec(), b"v3".to_vec())
        .await
        .expect("a read handle widens in place when its namespace does");
    node.shutdown().await;
}
