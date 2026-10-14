#![cfg(all(feature = "blobs", feature = "docs"))]
//! Public-facade witness for `Doc::set_bytes_durable`.
//!
//! The discriminating store-level crash contract lives in iroh-docs. This
//! test crosses Aster's two forwarding layers and then kills the writer as
//! soon as the public method acknowledges, so either wrapper accidentally
//! calling ordinary `set_bytes` reopens the original crash window.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

use aster::{AsterConfig, Node, RelayMode};

// The full test name inside the shared `it` binary (tests/it/main.rs), which
// `--exact` requires.
const CHILD_TEST: &str = "durable_docs::durable_write_child";
const READY_PREFIX: &str = "ASTER_DURABLE_READY ";
const PAYLOAD_LEN: usize = 8 * 1024 * 1024;

fn persistent_config(path: impl Into<std::path::PathBuf>) -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .persistent(path)
        .build()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "child role for the hard-kill witness"]
async fn durable_write_child() {
    let dir = std::env::var_os("ASTER_DURABLE_DIR").expect("parent supplies data directory");
    let node = Node::start(persistent_config(dir))
        .await
        .expect("start child node");
    let author = node.docs().default_author().await.expect("default author");
    let doc = node.docs().create().await.expect("create document");
    let hash = doc
        .set_bytes_durable(
            &author,
            b"manifest/object".to_vec(),
            vec![0xA5; PAYLOAD_LEN],
        )
        .await
        .expect("durable write through public facade");

    println!("{READY_PREFIX}{}", hash.as_str());
    std::io::stdout().flush().expect("flush READY line");
    std::future::pending::<()>().await;
}

/// The writer child waits forever after its acknowledgement; kill and reap it
/// however the round ends, so a failed `expect` cannot orphan it.
struct KillOnDrop(std::process::Child);
impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn public_durable_write_survives_an_immediate_hard_kill() {
    let exe = std::env::current_exe().expect("current test executable");

    for round in 0..5 {
        let dir = tempfile::tempdir().expect("temporary node directory");
        let mut child = KillOnDrop(
            Command::new(&exe)
                .args([CHILD_TEST, "--exact", "--ignored", "--nocapture"])
                .env("ASTER_DURABLE_DIR", dir.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn writer child"),
        );

        let stdout = child.stdout.take().expect("child stdout");
        let hash = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find_map(|line| line.strip_prefix(READY_PREFIX).map(str::to_owned))
            .expect("child acknowledged its durable write");

        child.kill().expect("hard-kill writer child");
        let status = child.wait().expect("reap writer child");
        assert!(
            !status.success(),
            "round {round}: killed child exited cleanly"
        );

        let runtime = tokio::runtime::Runtime::new().expect("reopen runtime");
        runtime.block_on(async {
            use aster::native::iroh_blobs::{
                api::blobs::BlobStatus,
                store::fs::{options::Options, FsStore},
                Hash as NativeHash,
            };

            // Reopen the blob store directly. A hard-killed first start may
            // legitimately lose unrelated docs/default-author metadata; the
            // contract under test is that the acknowledged value's blob
            // metadata is already committed.
            let hash: NativeHash = hash.parse().expect("hash printed by child");
            let store =
                FsStore::load_with_opts(dir.path().join("blobs.db"), Options::new(dir.path()))
                    .await
                    .expect("reopen blob store after hard kill");
            let status = store.blobs().status(hash).await.expect("blob status");
            assert_eq!(
                status,
                BlobStatus::Complete {
                    size: PAYLOAD_LEN as u64
                },
                "round {round}: acknowledged value is not complete after reopen"
            );
            let recovered = store
                .blobs()
                .get_bytes(hash)
                .await
                .expect("read acknowledged content after hard kill");
            assert_eq!(
                recovered.as_ref(),
                vec![0xA5; PAYLOAD_LEN].as_slice(),
                "round {round}: acknowledged content changed after reopen"
            );
            store.shutdown().await.expect("shutdown reopened store");
        });
    }
}
