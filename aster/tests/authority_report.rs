#![cfg(feature = "docs")]
//! What a node reports it may actually do with a namespace.
//!
//! The property under test throughout is that the report comes from the
//! **store**, not from the request. That distinction is the whole reason the
//! API exists: a caller that gates serving on "did I ask for read-only?"
//! gates on its own intent, which is not a fact about the node.

use aster::{
    AsterConfig, AuthorityFingerprint, LocalAccess, NamespaceSecret, Node, RelayMode, RetireOutcome,
};

async fn mem_node() -> Node {
    Node::start(AsterConfig::builder().relay(RelayMode::Disabled).build())
        .await
        .expect("start")
}

/// Importing a write capability reports write.
#[tokio::test]
async fn write_authority_reports_write() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x31u8; 32]);
    let id = secret.id();
    let _doc = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();

    let report = node.docs().authority_report(id).await.unwrap();
    assert_eq!(report.namespace(), id);
    assert_eq!(report.access(), LocalAccess::Write);
    assert_eq!(
        report.fingerprint(),
        AuthorityFingerprint::of(id, LocalAccess::Write),
        "a caller must be able to derive the same value from its own record",
    );
    node.shutdown().await;
}

/// The `Doc`-bound report answers for the document it is called on.
///
/// This is the binding that matters for a caller gating on a specific open
/// handle: the namespace comes from the document, so the report cannot be
/// about some other namespace. `Docs::authority_report(id)` echoes the `id` it
/// was given and therefore proves nothing about any handle on its own.
#[tokio::test]
async fn the_doc_bound_report_answers_for_that_doc() {
    let node = mem_node().await;
    let write_secret = NamespaceSecret::from_bytes([0x51u8; 32]);
    let write_id = write_secret.id();
    let read_id = NamespaceSecret::from_bytes([0x52u8; 32]).id();

    let write_doc = node
        .docs()
        .open_or_import_write_namespace(write_secret)
        .await
        .unwrap();
    let read_doc = node
        .docs()
        .open_or_import_read_namespace(read_id)
        .await
        .unwrap();

    let from_write = write_doc.authority_report().await.unwrap();
    let from_read = read_doc.authority_report().await.unwrap();

    assert_eq!(from_write.namespace(), write_id);
    assert_eq!(from_write.access(), LocalAccess::Write);
    assert_eq!(from_read.namespace(), read_id);
    assert_eq!(from_read.access(), LocalAccess::Read);

    // Two handles open at once, each reporting its own namespace — the report
    // follows the document, not the most recent call or some ambient state.
    assert_ne!(from_write.namespace(), from_read.namespace());
    assert_ne!(from_write.fingerprint(), from_read.fingerprint());
    node.shutdown().await;
}

/// The `Doc`-bound report tracks the store, not the open mode, in the same way
/// the namespace-keyed one does.
#[tokio::test]
async fn a_read_opened_doc_still_reports_stored_write() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x53u8; 32]);
    let id = secret.id();

    let _write_doc = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();
    let read_doc = node.docs().open_or_import_read_namespace(id).await.unwrap();

    let report = read_doc.authority_report().await.unwrap();
    assert_eq!(
        report.access(),
        LocalAccess::Write,
        "the handle was opened read-only, the store holds write, and the \
         report follows the store",
    );
    node.shutdown().await;
}

/// A genuinely read-only namespace reports read.
#[tokio::test]
async fn read_authority_reports_read() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x32u8; 32]);
    let id = secret.id();
    // Import by public id only — the secret never reaches this node.
    let _doc = node.docs().open_or_import_read_namespace(id).await.unwrap();

    let report = node.docs().authority_report(id).await.unwrap();
    assert_eq!(report.namespace(), id);
    assert_eq!(report.access(), LocalAccess::Read);
    assert_eq!(
        report.fingerprint(),
        AuthorityFingerprint::of(id, LocalAccess::Read),
    );
    node.shutdown().await;
}

/// **The load-bearing case.** Asking for read over a namespace this node can
/// already write reports *write*, because capability merge only ever upgrades.
///
/// An implementation that echoed the requested mode would pass every other test
/// in this file and fail exactly here — and in production it would put write
/// authority behind a caller's read-only gate.
#[tokio::test]
async fn a_read_request_cannot_hide_existing_write_authority() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x33u8; 32]);
    let id = secret.id();

    let _write_doc = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();

    // Ask for read. The store still holds the secret.
    let _read_doc = node.docs().open_or_import_read_namespace(id).await.unwrap();

    let report = node.docs().authority_report(id).await.unwrap();
    assert_eq!(
        report.access(),
        LocalAccess::Write,
        "the request was read, the stored capability is write, and the report \
         follows the store",
    );
    assert_eq!(
        report.fingerprint(),
        AuthorityFingerprint::of(id, LocalAccess::Write),
        "the fingerprint follows the stored access too, not the request",
    );
    node.shutdown().await;
}

/// Retiring write authority moves the report, so the report tracks the store
/// across a narrowing rather than caching what was opened.
#[tokio::test]
async fn the_report_follows_a_retirement() {
    let node = mem_node().await;
    let secret = NamespaceSecret::from_bytes([0x34u8; 32]);
    let id = secret.id();
    let doc = node
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();

    assert_eq!(
        node.docs().authority_report(id).await.unwrap().access(),
        LocalAccess::Write,
    );

    doc.shutdown_for_role_swap(std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        node.docs().retire_local_write_authority(id).await.unwrap(),
        RetireOutcome::Retired(LocalAccess::Read),
    );

    let report = node.docs().authority_report(id).await.unwrap();
    assert_eq!(
        report.access(),
        LocalAccess::Read,
        "after retirement the node may only read, and says so",
    );
    assert_ne!(
        report.fingerprint(),
        AuthorityFingerprint::of(id, LocalAccess::Write),
        "the narrowed authority does not correlate with the widened one",
    );
    node.shutdown().await;
}

/// An unknown namespace is an error, not `Read` and not a default.
///
/// A caller asks this to decide whether it may serve. "I cannot prove anything
/// about this namespace" and "I proved this namespace is read-only" must not
/// arrive in the same shape, or the first silently becomes the second.
#[tokio::test]
async fn an_unknown_namespace_cannot_be_reported() {
    let node = mem_node().await;
    let id = NamespaceSecret::from_bytes([0x35u8; 32]).id();

    let err = node
        .docs()
        .authority_report(id)
        .await
        .expect_err("nothing is stored for this namespace");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot be reported"),
        "the error should say the authority is unprovable, got: {msg}",
    );
    node.shutdown().await;
}

/// Stored authority survives a restart, and so does its fingerprint.
#[tokio::test]
async fn authority_reporting_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let secret = NamespaceSecret::from_bytes([0x36u8; 32]);
    let id = secret.id();

    let first = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .persistent(path.clone())
            .build(),
    )
    .await
    .expect("first start");
    let _doc = first
        .docs()
        .open_or_import_write_namespace(secret)
        .await
        .unwrap();
    let before = first.docs().authority_report(id).await.unwrap();
    assert_eq!(before.access(), LocalAccess::Write);
    first.shutdown().await;

    let second = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .persistent(path)
            .build(),
    )
    .await
    .expect("second start");
    let after = second.docs().authority_report(id).await.unwrap();
    assert_eq!(
        after, before,
        "unchanged stored authority reports unchanged"
    );
    assert_eq!(after.fingerprint(), before.fingerprint());
    second.shutdown().await;
}

/// The fingerprint is a function of exactly `(namespace, access)` — nothing
/// else — and both inputs move it.
///
/// This is what lets a caller derive the value from a grant record it holds
/// rather than from something only the node can see.
#[tokio::test]
async fn the_fingerprint_covers_the_namespace_and_the_access() {
    let a = NamespaceSecret::from_bytes([0x41u8; 32]).id();
    let b = NamespaceSecret::from_bytes([0x42u8; 32]).id();

    assert_eq!(
        AuthorityFingerprint::of(a, LocalAccess::Read),
        AuthorityFingerprint::of(a, LocalAccess::Read),
        "deterministic",
    );
    assert_ne!(
        AuthorityFingerprint::of(a, LocalAccess::Read),
        AuthorityFingerprint::of(a, LocalAccess::Write),
        "access is covered",
    );
    assert_ne!(
        AuthorityFingerprint::of(a, LocalAccess::Read),
        AuthorityFingerprint::of(b, LocalAccess::Read),
        "namespace is covered",
    );
}

/// The fingerprint's exact bytes are pinned.
///
/// Not a behavioural property — a *published* one. Portal compares this value
/// against its own record, so changing the algorithm makes unchanged authority
/// look re-sealed on every node that has not upgraded in step. If this test
/// has to change, the domain string's version has to change with it.
#[tokio::test]
async fn the_fingerprint_algorithm_is_pinned() {
    let id = NamespaceSecret::from_bytes([0u8; 32]).id();
    assert_eq!(
        id.to_string(),
        "3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29",
        "the fixture namespace itself is pinned, or the digest below moves \
         for an unrelated reason",
    );

    // BLAKE3 of: b"aster-docs/authority-fingerprint/v1\0" || namespace || 0x01.
    // Hard-coded rather than recomputed, so that a change to the domain string,
    // the field order, the access byte, *or* the hash function is caught here.
    assert_eq!(
        AuthorityFingerprint::of(id, LocalAccess::Write).to_hex(),
        "9d6117a07665ee8663be3dd38ffb0dcf6111c2c305d8ef1f299adab26b422db3",
    );
    assert_eq!(
        AuthorityFingerprint::of(id, LocalAccess::Read).to_hex(),
        "210a21c7c2cf5f241999e5f4da6f99ec4c0fc16af6626796b2a7d4d8cac78c3a",
    );
}
