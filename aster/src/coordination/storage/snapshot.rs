//! Snapshot files: the role-safe handle, the catalog, and the §11.4 create /
//! digest / publish path.
//!
//! The rows themselves are [`super::rows`]'s: it owns the §11.3 tables and
//! streams them into the canonical body. This module owns everything about the
//! *file* — where it is written, what its name is derived from, when it becomes
//! discoverable, and when it stops being.
//!
//! ## Why the body decode is not `codec`'s
//!
//! `codec` is the one decode site for values persisted *by this store*, and
//! its ceiling is a property of the row type. A snapshot body arrives from
//! another node, is far larger than any row, and is bounded section by section
//! by [`crate::coordination::rowstream`] — which refuses a declared count or
//! row length before allocating for it. Routing it through `codec` would mean
//! either a 512 MiB ceiling on every row decode or a body read that could not
//! be streamed at all.
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

// See the `storage` module docs: nothing in a shipping build reaches
// `VoterStorage::open` until Slice 3 supplies a verified identity. Scoped to
// this module deliberately -- a crate-root allowance would cover every storage
// module added after it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::error::{Db, StorageError, StorageResult};
use super::schema::{StorageLayout, MAX_COORDINATION_META_BYTES};
use crate::coordination::raft::SnapshotMetaV1;
use crate::coordination::rowstream::{
    read_body, BodyError, BodyVisitor, SnapshotBodyRowV1, SnapshotHeaderV1,
};

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

/// The digest-only file name a body is published under.
///
/// The outbound counterpart of [`file_stem_for`], and deliberately a different
/// function: this one takes a digest we computed, that one narrows a string
/// another node chose. Sharing them would put a foreign string one refactor
/// away from naming a path.
pub(crate) fn file_name_for(digest: &[u8; 32]) -> String {
    let id = snapshot_id_for(digest);
    let mut name = String::from(&id[SNAPSHOT_ID_PREFIX.len()..]);
    name.push_str(SNAPSHOT_FILE_SUFFIX);
    name
}

// ── Files ────────────────────────────────────────────────────────────────────

fn io<'a>(what: &'static str, path: &'a Path) -> impl Fn(std::io::Error) -> StorageError + 'a {
    move |source| StorageError::io(what, path, source)
}

fn corrupt(detail: impl std::fmt::Display) -> StorageError {
    StorageError::corrupt(Db::State, "snapshot", detail)
}

/// A file that is removed unless it is promoted.
///
/// Every path here writes to a scratch name first, and every one of them has a
/// failure between the create and the rename. Without this, each of those
/// failures leaks a partial body into `snapshots/` that looks like nothing in
/// particular — it has no digest name, so nothing sweeps it and nothing can say
/// what it was.
struct Scratch {
    path: Option<PathBuf>,
}

impl Scratch {
    /// A name no digest can collide with: digests are 64 hex characters, and
    /// this is neither hex nor 64 characters.
    fn new(dir: &Path, tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: Some(dir.join(format!(".{tag}-{}-{n}.partial", std::process::id()))),
        }
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("a scratch file has a path")
    }

    /// Give up the unlink-on-drop guarantee, returning the path to rename.
    fn keep(mut self) -> PathBuf {
        self.path.take().expect("a scratch file has a path")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Make `to` name `from`'s content, atomically and durably.
///
/// Two implementations, because the platforms give the guarantee in different
/// places and only one of them is `std::fs::rename`.
///
/// On Unix the rename is atomic and the *directory entry* still has to be
/// fsynced, which [`fsync_dir`] does at each call site. On Windows
/// `std::fs::rename` is `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` and
/// nothing else — the replacement is atomic but not write-through, and there is
/// no directory handle to synchronize instead, so the durability half was
/// simply missing. `MOVEFILE_WRITE_THROUGH` is what supplies it, and it has to
/// be asked for.
///
/// Replacing `state.redb` while redb still has it open is the harder case and
/// is not this — that one needs the handles closed first, which is why it lives
/// with install.
#[cfg(not(windows))]
fn atomic_replace(from: PathBuf, to: &Path) -> StorageResult<()> {
    // The error names the destination, not the scratch file: the scratch name
    // carries a pid and a counter and means nothing to whoever reads the log,
    // while the destination is the snapshot that failed to appear.
    std::fs::rename(&from, to).map_err(io("replacing a snapshot file", to))
}

#[cfg(windows)]
fn atomic_replace(from: PathBuf, to: &Path) -> StorageResult<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let (source, destination) = (wide(&from), wide(to));
    // SAFETY: both buffers are NUL-terminated and outlive the call, which is
    // the whole of `MoveFileExW`'s contract on its two path arguments.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(StorageError::io(
            "replacing a snapshot file",
            to,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// fsync a directory so a rename within it is durable.
///
/// Unix only, and now genuinely not an omission on Windows: there is no
/// directory handle to synchronize there, and the write-through flag on the
/// replacement above is the platform's equivalent guarantee — which is why it
/// had to be implemented before this comment could claim it.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> StorageResult<()> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(io("fsyncing the snapshot directory", dir))
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> StorageResult<()> {
    Ok(())
}

/// A pass-through that digests everything crossing it.
///
/// One type for both directions on purpose: the digest a build computes and the
/// digest a load recomputes have to be over the same bytes in the same way, and
/// two implementations of "BLAKE3 as it goes" is exactly the kind of pair that
/// agrees until it does not.
struct Digesting<T> {
    inner: T,
    hasher: blake3::Hasher,
    len: u64,
}

impl<T> Digesting<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            len: 0,
        }
    }

    fn into_parts(self) -> (T, [u8; 32], u64) {
        let Digesting { inner, hasher, len } = self;
        (inner, *hasher.finalize().as_bytes(), len)
    }
}

impl<W: Write> Write for Digesting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<R: Read> Read for Digesting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
}

/// BLAKE3 of a file's bytes, or `None` if it is not there.
///
/// Streamed rather than read whole: a snapshot body is bounded at 512 MiB, and
/// the point of this whole path is that no step holds one in memory.
fn digest_of(path: &Path) -> StorageResult<Option<[u8; 32]>> {
    let handle = match File::open(path) {
        Ok(handle) => handle,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(StorageError::io("hashing a snapshot body", path, err)),
    };
    let mut reader = Digesting::new(BufReader::new(handle));
    std::io::copy(&mut reader, &mut std::io::sink())
        .map_err(io("hashing a snapshot body", path))?;
    let (_, digest, _) = reader.into_parts();
    Ok(Some(digest))
}

// ── Build ────────────────────────────────────────────────────────────────────

/// An immutable snapshot file and the metadata derived from its body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SnapshotFile {
    /// The OpenRaft metadata, derived from the body rather than supplied.
    pub(crate) meta: SnapshotMetaV1,
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
}

/// Derive the OpenRaft metadata a body implies.
///
/// `last_log_id` and `last_membership` come from the header's replicated meta,
/// and `snapshot_id` from the digest — so the metadata cannot describe a body
/// other than the one it was computed from. §11.4 requires the reverse check on
/// install, and this is the function that makes the two comparable.
fn meta_for(header: &SnapshotHeaderV1, digest: &[u8; 32]) -> SnapshotMetaV1 {
    SnapshotMetaV1 {
        last_log_id: header.meta.last_applied.clone(),
        last_membership: header.meta.membership.clone(),
        snapshot_id: snapshot_id_for(digest),
    }
}

/// Stream `read`'s state into an immutable snapshot file under `snapshots/`.
///
/// Writes to a scratch name, fsyncs it, renames it to the digest-only published
/// name, and fsyncs the directory — in that order, because a directory entry
/// naming a body that is not yet on disk is a snapshot another node can ask for
/// and this one cannot serve.
///
/// Does **not** publish `CURRENT`. Building and publishing are separate because
/// §11.4's monotonicity rule is about publication: a builder that finishes below
/// the current snapshot has still produced a valid file, and the decision to
/// discard it is not the writer's to make.
pub(crate) fn build(
    layout: &StorageLayout,
    read: &redb::ReadTransaction,
) -> StorageResult<SnapshotFile> {
    let dir = layout.snapshots();
    let scratch = Scratch::new(&dir, "building");

    let handle =
        File::create(scratch.path()).map_err(io("creating a snapshot body", scratch.path()))?;
    let (digesting, header) =
        super::rows::stream_body(read, scratch.path(), Digesting::new(BufWriter::new(handle)))?;
    let (buffered, digest, len) = digesting.into_parts();
    let handle = buffered
        .into_inner()
        .map_err(|err| StorageError::io("flushing a snapshot body", scratch.path(), err.into()))?;
    handle
        .sync_all()
        .map_err(io("fsyncing a snapshot body", scratch.path()))?;
    drop(handle);

    let path = dir.join(file_name_for(&digest));
    // A file already under this name is *claimed* to have this content, and a
    // claim is what the digest exists to replace. Rebuilding an identical body
    // is the ordinary case after a lost acknowledgement, so the file is usually
    // exactly right — but "usually" is how a corrupted or half-written orphan
    // from an earlier run gets published under a name that vouches for it.
    //
    // So the existing bytes are hashed and compared against the body just
    // written. Equal means reuse; anything else means the name is a lie and the
    // freshly fsynced scratch — whose digest *is* this name — replaces it. That
    // is a repair rather than a risk: no reader can be served worse by bytes
    // that hash to the name they are under than by bytes that do not.
    match digest_of(&path)? {
        Some(existing) if existing == digest => drop(scratch),
        _ => {
            atomic_replace(scratch.keep(), &path)?;
            fsync_dir(&dir)?;
        }
    }

    Ok(SnapshotFile {
        meta: meta_for(&header, &digest),
        path,
        len,
    })
}

// ── The durable current marker ───────────────────────────────────────────────

/// What publishing a candidate against the current marker resolves to (§11.4).
///
/// Written as a decision an independent test can drive, rather than as branches
/// inside the publish path: the rule is "monotonic by applied index, with full
/// identity at an equal index", and a test that has to construct two real stores
/// to exercise the fatal case would exercise it once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Publication {
    /// Newer than the current marker: replace it.
    Publish,
    /// The same snapshot arriving again. Republishing the marker is legal and
    /// is how a lost acknowledgement is repaired; the body is already there.
    Idempotent,
    /// Behind the current marker. The file stays, the marker does not move.
    Discard,
    /// The same index describing different history. Not recoverable.
    Fatal(String),
}

/// Compare a candidate against the current marker by **applied index**, never by
/// a derived `(term, index)` ordering.
pub(crate) fn decide_publication(
    candidate: &SnapshotMetaV1,
    current: Option<&SnapshotMetaV1>,
) -> Publication {
    let Some(current) = current else {
        return Publication::Publish;
    };
    let candidate_index = candidate.last_log_id.as_ref().map(|id| id.index);
    let current_index = current.last_log_id.as_ref().map(|id| id.index);

    if candidate_index > current_index {
        return Publication::Publish;
    }
    if candidate_index < current_index {
        return Publication::Discard;
    }
    // Equal index. Two voters at the same complete `last_applied` must emit
    // byte-identical bodies (§11.4), so anything but full equality here is
    // evidence of nondeterminism or corruption rather than a leader change.
    if candidate.last_log_id != current.last_log_id {
        return Publication::Fatal(format!(
            "two snapshots at index {candidate_index:?} name different log ids: \
             {:?} and {:?}",
            candidate.last_log_id, current.last_log_id
        ));
    }
    if candidate.snapshot_id != current.snapshot_id {
        return Publication::Fatal(format!(
            "two snapshots at the same log id have different content: {} and {}",
            candidate.snapshot_id, current.snapshot_id
        ));
    }
    Publication::Idempotent
}

/// Write `snapshots/CURRENT` atomically.
pub(crate) fn publish_current(layout: &StorageLayout, meta: &SnapshotMetaV1) -> StorageResult<()> {
    let dir = layout.snapshots();
    let scratch = Scratch::new(&dir, "current");
    let bytes = super::codec::encode_committed(
        Db::State,
        "SnapshotMetaV1",
        meta,
        MAX_COORDINATION_META_BYTES,
    )?;

    {
        let mut handle = File::create(scratch.path())
            .map_err(io("creating the current marker", scratch.path()))?;
        handle
            .write_all(&bytes)
            .map_err(io("writing the current marker", scratch.path()))?;
        handle
            .sync_all()
            .map_err(io("fsyncing the current marker", scratch.path()))?;
    }

    atomic_replace(scratch.keep(), &layout.current_snapshot_marker())?;
    fsync_dir(&dir)
}

/// Everything that must hold between a snapshot body and the store it sits
/// beside (§11.4, §11.1).
///
/// Internal agreement is not enough. A marker and a body that agree with each
/// other are a *complete and consistent snapshot of some group*, and a foreign
/// one satisfies every check in that family — same file format, same digest,
/// same membership as its own header claims. Dropping another voter's
/// `snapshots/` directory in beside this `state.redb` would then be accepted
/// and served to followers as this group's state.
///
/// So the body is also checked against the identity `state.redb` is bound to,
/// and against how far that database has actually applied. The comparison is
/// against the loaded state rather than against the store's `StorageIdentity`
/// because the profile and applied index are replicated facts that only the
/// state carries.
pub(crate) fn check_binding(
    header: &SnapshotHeaderV1,
    state: &super::rows::DurableState,
) -> StorageResult<()> {
    use crate::coordination::rowstream::SNAPSHOT_FORMAT_VERSION;

    let mismatch = |what: &str, found: String, expected: String| {
        corrupt(format!(
            "the current snapshot's {what} is {found}, and this store's is {expected}"
        ))
    };

    if header.format_version != SNAPSHOT_FORMAT_VERSION {
        return Err(mismatch(
            "body format version",
            header.format_version.to_string(),
            SNAPSHOT_FORMAT_VERSION.to_string(),
        ));
    }
    if header.schema_version != super::identity::LOGICAL_SCHEMA_VERSION {
        return Err(mismatch(
            "schema version",
            header.schema_version.to_string(),
            super::identity::LOGICAL_SCHEMA_VERSION.to_string(),
        ));
    }

    let authority = &state.model.authority;
    let bindings: [(&str, &[u8], &[u8]); 3] = [
        ("group id", &header.meta.group_id, &authority.group_id),
        (
            "genesis hash",
            &header.meta.genesis_hash,
            &authority.genesis_hash,
        ),
        (
            "authority key",
            &header.meta.authority_public_key,
            &authority.public_key,
        ),
    ];
    for (what, found, expected) in bindings {
        if found != expected {
            return Err(mismatch(what, hex(found), hex(expected)));
        }
    }
    if header.meta.authority_domain != authority.domain {
        return Err(mismatch(
            "authority domain",
            format!("{:?}", header.meta.authority_domain),
            format!("{:?}", authority.domain),
        ));
    }
    if header.meta.deployment_profile_code != state.model.deployment_profile_code {
        return Err(mismatch(
            "deployment profile",
            header.meta.deployment_profile_code.to_string(),
            state.model.deployment_profile_code.to_string(),
        ));
    }

    // A snapshot ahead of the database it describes cannot have been produced
    // by this store: the body is streamed from a committed read transaction, so
    // its applied index is one this `state.redb` had already reached. Ahead
    // means the state was replaced by an older one, or the snapshot came from
    // somewhere else. Behind is ordinary — the state has applied entries since.
    let snapshot_index = header.meta.last_applied.as_ref().map(|id| id.index);
    let applied_index = state.model.last_applied.as_ref().map(|id| id.index);
    if snapshot_index > applied_index {
        return Err(corrupt(format!(
            "the current snapshot is at applied index {snapshot_index:?}, ahead of \
             state.redb at {applied_index:?}; a snapshot this store built could not be"
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Read `snapshots/CURRENT`, prove it describes the body it names, and prove
/// the body belongs to this store.
///
/// A missing marker is `Ok(None)` — a store that has never snapshotted. A
/// marker that is present and wrong is `DATA_LOSS`: §11.4 is explicit that a
/// missing file, malformed id, digest mismatch, or metadata mismatch is not an
/// absent snapshot, because treating it as one would silently offer a follower
/// a full log replay instead of reporting that this voter's snapshot is gone.
pub(crate) fn load_current(
    layout: &StorageLayout,
    state: &super::rows::DurableState,
) -> StorageResult<Option<SnapshotFile>> {
    let marker = layout.current_snapshot_marker();
    let stat = match std::fs::metadata(&marker) {
        Ok(stat) => stat,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(StorageError::io("reading the current marker", &marker, err)),
    };
    // Bounded before the read allocates, not after.
    if stat.len() > MAX_COORDINATION_META_BYTES as u64 {
        return Err(corrupt(format!(
            "the current marker is {} bytes, over the {MAX_COORDINATION_META_BYTES}-byte ceiling",
            stat.len()
        )));
    }
    let bytes = std::fs::read(&marker).map_err(io("reading the current marker", &marker))?;
    let meta: SnapshotMetaV1 = super::codec::decode_persisted_bounded(
        Db::State,
        "SnapshotMetaV1",
        &bytes,
        MAX_COORDINATION_META_BYTES,
    )?;

    let stem = file_stem_for(&meta.snapshot_id).ok_or_else(|| {
        corrupt(format!(
            "the current marker names {:?}, which is not a content-derived snapshot id",
            meta.snapshot_id
        ))
    })?;
    let path = layout
        .snapshots()
        .join(format!("{stem}{SNAPSHOT_FILE_SUFFIX}"));

    let (header, digest, len) = read_and_verify(&path)?;
    let derived = meta_for(&header, &digest);
    if derived.snapshot_id != meta.snapshot_id {
        return Err(corrupt(format!(
            "the body under {} hashes to {}, not the id the marker names",
            path.display(),
            derived.snapshot_id
        )));
    }
    if derived.last_log_id != meta.last_log_id {
        return Err(corrupt(format!(
            "the marker names last log id {:?} and the body carries {:?}",
            meta.last_log_id, derived.last_log_id
        )));
    }
    if derived.last_membership != meta.last_membership {
        return Err(corrupt(
            "the marker's membership is not the membership the body carries",
        ));
    }
    check_binding(&header, state)?;

    Ok(Some(SnapshotFile { meta, path, len }))
}

/// Read a body end to end, digesting it and validating every row.
///
/// Not a header-only read. §11.4 says "hash and decode the complete body", and
/// the difference is the whole point: a header names counts, and only reading
/// the rows proves the counts are true, the sections are complete, and each row
/// still decodes under this build.
fn read_and_verify(path: &Path) -> StorageResult<(SnapshotHeaderV1, [u8; 32], u64)> {
    let handle = match File::open(path) {
        Ok(handle) => handle,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(corrupt(format!(
                "the current marker names {}, which does not exist",
                path.display()
            )))
        }
        Err(err) => return Err(StorageError::io("opening a snapshot body", path, err)),
    };

    let mut reader = Digesting::new(BufReader::new(handle));
    let mut visitor = CapturingHeader { header: None };
    read_body::<_, SnapshotBodyRowV1>(&mut reader, &mut visitor).map_err(|err| match err {
        BodyError::Io => StorageError::io(
            "reading a snapshot body",
            path,
            std::io::Error::other("the snapshot body reader failed"),
        ),
        other => corrupt(format!(
            "{} is not a readable body: {other:?}",
            path.display()
        )),
    })?;
    let (_, digest, len) = reader.into_parts();

    let header = visitor
        .header
        .ok_or_else(|| corrupt("a body was read without a header"))?;
    Ok((header, digest, len))
}

/// Keeps the header and forgets every row, which is what makes the read a
/// streaming one: retaining rows here would put the whole state back in memory
/// and undo the property [`super::rows::stream_body`] exists to have.
struct CapturingHeader {
    header: Option<SnapshotHeaderV1>,
}

impl BodyVisitor<SnapshotBodyRowV1> for CapturingHeader {
    fn header(&mut self, header: &SnapshotHeaderV1) -> Result<(), BodyError> {
        self.header = Some(header.clone());
        Ok(())
    }

    fn row(&mut self, _section: u64, _row: SnapshotBodyRowV1) -> Result<(), BodyError> {
        Ok(())
    }
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

    // ── Naming ───────────────────────────────────────────────────────────────

    /// The name a body is published under is the id's digest and nothing else.
    #[test]
    fn a_published_file_is_named_by_its_digest_alone() {
        let digest = [0x9au8; 32];
        let name = file_name_for(&digest);
        assert_eq!(name, format!("{}{SNAPSHOT_FILE_SUFFIX}", "9a".repeat(32)));
        assert_eq!(
            file_stem_for(&snapshot_id_for(&digest)),
            Some(name.trim_end_matches(SNAPSHOT_FILE_SUFFIX)),
            "the outbound name and the inbound stem must agree, or a body this \
             node published could not be found by the id it advertised"
        );
    }

    /// The Windows replacement asks for write-through.
    ///
    /// A statement about the source, because this host cannot execute that arm
    /// and CI's Windows job would compile a version without the flag perfectly
    /// happily. The flag is the entire durability guarantee on that platform:
    /// `std::fs::rename` is `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`
    /// alone, and there is no directory handle to fsync instead, so dropping it
    /// silently reduces every snapshot publication to "atomic but not durable"
    /// — which is exactly the state this replaced.
    #[test]
    fn the_windows_replacement_is_write_through() {
        let source = include_str!("snapshot.rs");
        let end = source.find("\n#[cfg(test)]\n").unwrap_or(source.len());
        let flat: String = source[..end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let marker = "#[cfg(windows)] fn atomic_replace";
        let arm = flat
            .split(marker)
            .nth(1)
            .expect("a Windows replacement implementation");
        assert!(
            arm.contains("MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH"),
            "the Windows replacement must be both atomic and write-through"
        );
    }

    // ── Publication (§11.4) ──────────────────────────────────────────────────

    fn meta(index: u64, term: u64, digest: u8) -> SnapshotMetaV1 {
        SnapshotMetaV1 {
            last_log_id: Some(crate::coordination::contract::LogIdV1 {
                term,
                leader_node_id: vec![0x11; 32],
                index,
            }),
            last_membership: Default::default(),
            snapshot_id: snapshot_id_for(&[digest; 32]),
        }
    }

    /// The rule is monotonic by **applied index**, with full identity required
    /// at an equal index.
    ///
    /// Driven as a table against the decision rather than through two real
    /// stores, because the two outcomes that matter most — a stale builder and
    /// a divergent one — are the ones a pair of healthy stores will not
    /// produce on demand.
    #[test]
    fn publication_is_monotonic_by_index_and_exact_at_an_equal_one() {
        let current = meta(10, 2, 0xaa);

        assert_eq!(decide_publication(&current, None), Publication::Publish);
        assert_eq!(
            decide_publication(&meta(11, 2, 0xbb), Some(&current)),
            Publication::Publish
        );
        assert_eq!(
            decide_publication(&meta(9, 2, 0xbb), Some(&current)),
            Publication::Discard,
            "a builder that finished behind the current snapshot is discarded"
        );
        assert_eq!(
            decide_publication(&meta(10, 2, 0xaa), Some(&current)),
            Publication::Idempotent,
            "the same log id and the same content is the same snapshot"
        );

        // A different leader at the same index is two histories, and a
        // different body at the same log id is nondeterminism. §11.4 makes both
        // fatal rather than letting either overwrite the other.
        assert!(matches!(
            decide_publication(&meta(10, 3, 0xaa), Some(&current)),
            Publication::Fatal(_)
        ));
        assert!(matches!(
            decide_publication(&meta(10, 2, 0xbb), Some(&current)),
            Publication::Fatal(_)
        ));
    }

    /// A snapshot of a store with nothing applied sorts before every other one,
    /// and is still exact against itself.
    ///
    /// `None` is not a missing value here — it is index "before entry one",
    /// which a bootstrapping voter really does snapshot at.
    #[test]
    fn an_unapplied_snapshot_is_behind_every_applied_one() {
        let empty = SnapshotMetaV1 {
            last_log_id: None,
            last_membership: Default::default(),
            snapshot_id: snapshot_id_for(&[0u8; 32]),
        };
        assert_eq!(
            decide_publication(&meta(1, 1, 0xaa), Some(&empty)),
            Publication::Publish
        );
        assert_eq!(
            decide_publication(&empty, Some(&meta(1, 1, 0xaa))),
            Publication::Discard
        );
        assert_eq!(
            decide_publication(&empty, Some(&empty)),
            Publication::Idempotent
        );
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
