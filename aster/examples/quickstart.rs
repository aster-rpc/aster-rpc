//! End-to-end quickstart: start a persistent node, store a blob and a doc
//! entry, shut down, then reopen the same data dir and confirm everything
//! survived. Mirrors `core/tests/blob_persistence_contract.rs`.
//!
//! Run with: `cargo run -p aster --example quickstart`

use aster::{AsterConfig, NamespaceSecret, Node, RelayMode, Result};
use std::io::Write;

#[tokio::main]
async fn main() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();

    // A deterministic config namespace — the same seed always maps to the same
    // namespace id, on any node, across restarts.
    let secret = NamespaceSecret::from_bytes([0x42u8; 32]);
    let ns_id = secret.id();
    println!("config namespace id: {ns_id}");

    // ── First boot: write some state ────────────────────────────────────────
    let (node_id, blob_hash) = {
        let node = Node::start(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .persistent(data_dir.clone())
                .build(),
        )
        .await?;
        let node_id = node.id();
        println!("node id: {node_id}");

        // Import a file as a blob, protected by a named tag.
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        f.write_all(b"portal-sync payload").unwrap();
        let blob_hash = node
            .blobs()
            .add_path_with_named_tag(f.path().to_string_lossy().to_string(), "portal-sync/demo")
            .await?;
        println!("stored blob: {blob_hash}");

        // Open the deterministic config namespace and write an entry.
        let doc = node
            .docs()
            .open_or_import_write_namespace(secret.clone())
            .await?;
        let author = node.docs().default_author().await?;
        doc.set_bytes(&author, b"hello".to_vec(), b"world".to_vec())
            .await?;
        println!("wrote doc entry under namespace {}", doc.id()?);

        node.shutdown().await; // flushes to disk
        (node_id, blob_hash)
    };

    // ── Second boot: confirm everything survived ────────────────────────────
    {
        let node = Node::start(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .persistent(data_dir.clone())
                .build(),
        )
        .await?;

        // Blob survived.
        let bytes = node.blobs().read_to_bytes(&blob_hash).await?;
        assert_eq!(bytes, b"portal-sync payload");
        println!("blob survived: {} bytes", bytes.len());

        // Namespace reopens deterministically and the entry is intact.
        let doc = node.docs().open_or_import_write_namespace(secret).await?;
        assert_eq!(doc.id()?, ns_id);
        let entry = doc
            .query_latest_exact(b"hello".to_vec())
            .await?
            .expect("entry survived");
        let val = doc.read_entry_content(&entry.content_hash).await?;
        assert_eq!(val, b"world");
        println!(
            "doc entry survived: hello = {}",
            String::from_utf8_lossy(&val)
        );

        // Node id is unchanged? Only if the secret key is reused — here we let
        // it regenerate, so just report it.
        println!("reopened node id: {}", node.id());

        node.shutdown().await;
    }

    println!("\nquickstart OK — blob + deterministic namespace survived restart");
    let _ = node_id; // (printed above)
    Ok(())
}
