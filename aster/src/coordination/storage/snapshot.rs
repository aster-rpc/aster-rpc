//! The role-safe snapshot handle and its catalog (§11.4).
//!
//! Checkpoint A lands the *type* and its ownership rules, because the
//! production OpenRaft type config names it and the `Cursor<Vec<u8>>`
//! placeholder must not survive to the point where anything claims storage
//! progress. Building, installing, and publishing `CURRENT` are Checkpoint D.
//!
//! ## Why two roles rather than one path
//!
//! With `generic-snapshot-data`, OpenRaft bounds `SnapshotData` as only
//! `OptionalSend + 'static` and treats the value as opaque — it never reads or
//! writes it. So ownership is entirely ours to get right, and OpenRaft's own
//! storage suite exercises the case that gets it wrong:
//! `testing::Suite::transfer_snapshot` takes the follower's *Incoming* box and
//! assigns the leader's *Published* handle into it:
//!
//! ```text
//! let mut ss2_box = sm_f.begin_receiving_snapshot().await?;
//! *ss2_box = *ss1_cur.snapshot;
//! sm_f.install_snapshot(&ss1_cur.meta, ss2_box).await?;
//! ```
//!
//! That assignment drops the displaced Incoming value, and the moved Published
//! value is dropped later inside the follower. With one delete-on-drop role,
//! the second drop deletes the *leader's* current snapshot — a live node
//! losing its snapshot because a different node finished installing one.
//!
//! Hence: Incoming owns a receiver-local temporary and deletes it on drop;
//! Published names an immutable file and never unlinks on drop. A predecessor
//! is retired logically, and the catalog unlinks it only after its last lease
//! is released, which is also what makes replacement work on Windows where an
//! open file cannot be removed.
//!
//! ## No blanket `Clone`
//!
//! Cloning an Incoming handle would create two owners that each believe they
//! should delete one temporary. `generic-snapshot-data` does not require
//! `Clone`, so the type does not have it; an additional Published reader comes
//! from an explicit [`SnapshotCatalog::lease`] instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Snapshot ids are content-derived (§11.4): the domain prefix plus the
/// lowercase hex BLAKE3 of the canonical body.
pub(crate) const SNAPSHOT_ID_PREFIX: &str = "aster-coordination-v1:";

/// The published file name is the digest plus this suffix — the digest only,
/// never a caller-supplied string, so no external value becomes a path.
pub(crate) const SNAPSHOT_FILE_SUFFIX: &str = ".snapshot";

/// Derive the content-addressed snapshot id for a body digest.
pub(crate) fn snapshot_id_for(digest: &[u8; 32]) -> String {
    let mut id = String::with_capacity(SNAPSHOT_ID_PREFIX.len() + 64);
    id.push_str(SNAPSHOT_ID_PREFIX);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// Recover the 64-character digest file stem from a snapshot id.
///
/// Returns `None` for anything that is not exactly the domain prefix followed
/// by 64 lowercase hex digits. This is the only path from an id to a file
/// name, so an id arriving from another node cannot name a path of its own
/// choosing.
pub(crate) fn file_stem_for(snapshot_id: &str) -> Option<&str> {
    let stem = snapshot_id.strip_prefix(SNAPSHOT_ID_PREFIX)?;
    let valid = stem.len() == 64
        && stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then_some(stem)
}

// ── Catalog ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct CatalogInner {
    /// Outstanding leases per snapshot id.
    leases: HashMap<String, usize>,
    /// Snapshots superseded by a successor: logically gone, physically waiting
    /// on their last reader.
    retired: HashMap<String, PathBuf>,
}

impl CatalogInner {
    /// Unlink a retired snapshot once nothing is reading it.
    ///
    /// Deliberately best-effort: failing to remove a superseded file is a
    /// disk-hygiene problem, not a correctness one — the marker already names
    /// its successor, and startup sweeps retired orphans after validating
    /// `CURRENT`. Turning it into a hard error would fail an install that has
    /// already durably succeeded.
    fn collect(&mut self, snapshot_id: &str) {
        let outstanding = self.leases.get(snapshot_id).copied().unwrap_or(0);
        if outstanding > 0 {
            return;
        }
        self.leases.remove(snapshot_id);
        if let Some(path) = self.retired.remove(snapshot_id) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Tracks which published snapshots exist, which are retired, and who is
/// still reading them.
///
/// Cloning shares one catalog: the handle is `Arc`-based precisely because a
/// Published lease can cross into another store's actor and be dropped there.
#[derive(Debug, Clone, Default)]
pub(crate) struct SnapshotCatalog {
    inner: Arc<Mutex<CatalogInner>>,
}

impl SnapshotCatalog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Take a reader lease on a published snapshot, pinning its file until the
    /// returned handle is dropped.
    pub(crate) fn lease(
        &self,
        snapshot_id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> CoordinationSnapshotFile {
        let snapshot_id = snapshot_id.into();
        {
            let mut inner = self.inner.lock().expect("snapshot catalog mutex");
            *inner.leases.entry(snapshot_id.clone()).or_insert(0) += 1;
        }
        CoordinationSnapshotFile::Published(PublishedSnapshot {
            snapshot_id,
            path: path.into(),
            catalog: Arc::clone(&self.inner),
        })
    }

    /// Retire a snapshot: it stops being discoverable now, and its file is
    /// removed as soon as the last outstanding lease is released.
    pub(crate) fn retire(&self, snapshot_id: &str, path: impl Into<PathBuf>) {
        let mut inner = self.inner.lock().expect("snapshot catalog mutex");
        inner.retired.insert(snapshot_id.to_string(), path.into());
        inner.collect(snapshot_id);
    }

    /// Outstanding lease count, for tests that assert a superseded file is
    /// pinned rather than removed under a live reader.
    #[cfg(test)]
    pub(crate) fn outstanding(&self, snapshot_id: &str) -> usize {
        self.inner
            .lock()
            .expect("snapshot catalog mutex")
            .leases
            .get(snapshot_id)
            .copied()
            .unwrap_or(0)
    }
}

// ── The handle ───────────────────────────────────────────────────────────────

/// A receiver-owned temporary being filled in, under the receiver's
/// `incoming/`.
#[derive(Debug)]
pub struct IncomingSnapshot {
    path: PathBuf,
}

impl IncomingSnapshot {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IncomingSnapshot {
    /// Removes the temporary. Best-effort for the same reason as
    /// [`CatalogInner::collect`]: an orphan under `incoming/` is swept at
    /// startup, and a failed unlink must not poison an unwind.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// An immutable published snapshot, with a lease pinning its file.
#[derive(Debug)]
pub struct PublishedSnapshot {
    snapshot_id: String,
    path: PathBuf,
    catalog: Arc<Mutex<CatalogInner>>,
}

impl PublishedSnapshot {
    pub(crate) fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PublishedSnapshot {
    /// Releases the lease. Never unlinks directly: this handle may be the
    /// leader's current snapshot, moved into a follower by OpenRaft's suite
    /// and dropped there. Only the catalog removes a file, and only one that
    /// has been retired.
    fn drop(&mut self) {
        let mut inner = self.catalog.lock().expect("snapshot catalog mutex");
        if let Some(count) = inner.leases.get_mut(&self.snapshot_id) {
            *count = count.saturating_sub(1);
        }
        inner.collect(&self.snapshot_id);
    }
}

/// The production `SnapshotData`.
///
/// Not `Clone` by design; see the module documentation.
#[derive(Debug)]
pub enum CoordinationSnapshotFile {
    /// Receiver-owned, delete-on-drop.
    Incoming(IncomingSnapshot),
    /// Immutable and leased; drop never unlinks.
    Published(PublishedSnapshot),
}

impl CoordinationSnapshotFile {
    /// Start receiving into a receiver-owned temporary.
    pub(crate) fn incoming(path: impl Into<PathBuf>) -> Self {
        CoordinationSnapshotFile::Incoming(IncomingSnapshot::new(path))
    }

    /// The file this handle names, whichever role it holds.
    ///
    /// Install reads through this without caring which role produced it: the
    /// source may be another store's Published handle, and validation is what
    /// grants trust, not provenance.
    pub(crate) fn path(&self) -> &Path {
        match self {
            CoordinationSnapshotFile::Incoming(incoming) => incoming.path(),
            CoordinationSnapshotFile::Published(published) => published.path(),
        }
    }

    /// Whether dropping this handle removes its file.
    pub(crate) fn deletes_on_drop(&self) -> bool {
        matches!(self, CoordinationSnapshotFile::Incoming(_))
    }
}

/// `SnapshotData: OptionalSend + 'static`, and the production graph leaves
/// OpenRaft's `singlethreaded` feature off, so `OptionalSend` is `Send`.
///
/// Pinned as a compile assertion rather than left to the first trait-bound
/// error in Checkpoint D: a Published lease crosses from one store into
/// another store's actor and is dropped there, so an `Rc` catalog or a
/// borrowed actor reference would not merely be slower — it would not compile,
/// and it should not compile *here*, next to the reason.
const _: fn() = || {
    fn assert_send<T: Send + 'static>() {}
    assert_send::<CoordinationSnapshotFile>();
    assert_send::<SnapshotCatalog>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aster-snapshot-role-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write(path: &Path) {
        std::fs::write(path, b"body").expect("write");
    }

    /// The content-addressed id is the domain prefix plus lowercase hex.
    #[test]
    fn a_snapshot_id_is_its_prefixed_lowercase_digest() {
        let id = snapshot_id_for(&[0xab; 32]);
        assert!(id.starts_with(SNAPSHOT_ID_PREFIX));
        assert_eq!(id.len(), SNAPSHOT_ID_PREFIX.len() + 64);
        assert_eq!(&id[SNAPSHOT_ID_PREFIX.len()..], &"ab".repeat(32));
    }

    /// An id maps to a file name only when it is exactly the expected shape.
    /// This is the guard that keeps a foreign id from naming a path: a
    /// traversal attempt has no valid digest, so it never becomes one.
    #[test]
    fn only_a_well_formed_id_maps_to_a_file_name() {
        let good = snapshot_id_for(&[1u8; 32]);
        assert_eq!(file_stem_for(&good), Some("01".repeat(32).as_str()));

        assert_eq!(file_stem_for("01".repeat(32).as_str()), None, "no prefix");
        assert_eq!(
            file_stem_for(&format!("{SNAPSHOT_ID_PREFIX}../../etc/passwd")),
            None,
            "a traversal attempt is not a digest"
        );
        assert_eq!(
            file_stem_for(&format!("{SNAPSHOT_ID_PREFIX}{}", "AB".repeat(32))),
            None,
            "uppercase hex is not the canonical form"
        );
        assert_eq!(
            file_stem_for(&format!("{SNAPSHOT_ID_PREFIX}{}", "ab".repeat(31))),
            None,
            "a short digest is refused"
        );
    }

    /// Dropping an Incoming handle removes its temporary.
    #[test]
    fn dropping_an_incoming_handle_removes_its_temporary() {
        let dir = temp_dir();
        let path = dir.join("incoming-drop.tmp");
        write(&path);

        let handle = CoordinationSnapshotFile::incoming(&path);
        assert!(handle.deletes_on_drop());
        drop(handle);

        assert!(!path.exists(), "an unpromoted temporary is removed on drop");
    }

    /// Dropping a Published handle does **not** remove its file.
    ///
    /// This is the `transfer_snapshot` case: the leader's current snapshot is
    /// dropped inside the follower after install, and it must survive.
    #[test]
    fn dropping_a_published_handle_leaves_its_file_alone() {
        let dir = temp_dir();
        let path = dir.join("published-drop.snapshot");
        write(&path);

        let catalog = SnapshotCatalog::new();
        let id = snapshot_id_for(&[2u8; 32]);
        let handle = catalog.lease(&id, &path);
        assert!(!handle.deletes_on_drop());
        drop(handle);

        assert!(
            path.exists(),
            "a published snapshot must survive its handle; the owner still has it"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Assigning a Published handle over an Incoming one removes only the
    /// displaced temporary — exactly what `transfer_snapshot` does.
    #[test]
    fn replacing_an_incoming_value_removes_only_the_displaced_temporary() {
        let dir = temp_dir();
        let incoming_path = dir.join("displaced.tmp");
        let published_path = dir.join("survivor.snapshot");
        write(&incoming_path);
        write(&published_path);

        let catalog = SnapshotCatalog::new();
        let id = snapshot_id_for(&[3u8; 32]);

        let mut receiving = Box::new(CoordinationSnapshotFile::incoming(&incoming_path));
        let source = Box::new(catalog.lease(&id, &published_path));
        *receiving = *source;

        assert!(
            !incoming_path.exists(),
            "the displaced receiving temporary is removed"
        );
        assert!(
            published_path.exists(),
            "the source snapshot is untouched by the assignment"
        );

        drop(receiving);
        assert!(
            published_path.exists(),
            "and it survives the moved handle being dropped by the receiver"
        );
        std::fs::remove_file(&published_path).ok();
    }

    /// A retired snapshot with a live reader is pinned until that reader
    /// finishes. Required on Windows, where an open file cannot be removed,
    /// and correct everywhere else.
    #[test]
    fn a_retired_snapshot_is_pinned_until_its_last_lease_ends() {
        let dir = temp_dir();
        let path = dir.join("retired.snapshot");
        write(&path);

        let catalog = SnapshotCatalog::new();
        let id = snapshot_id_for(&[4u8; 32]);

        let reader = catalog.lease(&id, &path);
        catalog.retire(&id, &path);
        assert!(
            path.exists(),
            "retirement is logical; a live reader still holds the bytes"
        );
        assert_eq!(catalog.outstanding(&id), 1);

        drop(reader);
        assert_eq!(catalog.outstanding(&id), 0);
        assert!(
            !path.exists(),
            "the catalog unlinks it once the last lease is released"
        );
    }

    /// Two readers both have to finish before the file goes.
    #[test]
    fn every_lease_must_end_before_a_retired_file_is_removed() {
        let dir = temp_dir();
        let path = dir.join("two-readers.snapshot");
        write(&path);

        let catalog = SnapshotCatalog::new();
        let id = snapshot_id_for(&[5u8; 32]);

        let first = catalog.lease(&id, &path);
        let second = catalog.lease(&id, &path);
        catalog.retire(&id, &path);

        drop(first);
        assert!(path.exists(), "one reader remains");
        drop(second);
        assert!(!path.exists(), "the last reader releases it");
    }

    /// Retiring a snapshot nobody is reading removes it immediately.
    #[test]
    fn an_unread_retired_snapshot_is_removed_at_once() {
        let dir = temp_dir();
        let path = dir.join("unread.snapshot");
        write(&path);

        let catalog = SnapshotCatalog::new();
        let id = snapshot_id_for(&[6u8; 32]);
        catalog.retire(&id, &path);

        assert!(!path.exists());
    }
}
