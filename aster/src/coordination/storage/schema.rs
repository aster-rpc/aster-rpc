//! Directory layout, the lifetime lock, schema creation, and the open
//! coordinator (§11.1 to §11.3).
//!
//! ## Why the open coordinator owns the databases by value
//!
//! `Database::check_integrity` takes `&mut Database` and returns
//! `TransactionInProgress` while any transaction or shared handle is alive. So
//! the order here is not stylistic: this module creates each database, owns it
//! outright while checking and validating it, and only then hands it on. A
//! handle published before the check makes the check impossible to run at all.
//!
//! ## Why `codec_version` sits outside the Fory row
//!
//! Both meta tables carry [`KEY_CODEC_VERSION`] as a fixed-width scalar
//! alongside the encoded values, per §11.2 and §11.3. In `state.redb` that
//! means it sits beside the Fory `"meta"` row rather than inside it, and it is
//! checked first: a build that cannot read the row's encoding must still be
//! able to refuse it *by name*, instead of reporting a decode failure on the
//! very value whose codec is in question. The value is a compiled constant,
//! never a caller's choice.

// Scoped to this module deliberately -- see the `storage` module docs for why
// a crate-root allowance would be wrong, and why these come off when Slice 3
// wires the voter runtime rather than at any Slice 2 checkpoint.
#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use redb::{
    Database, DatabaseError, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};

use super::codec::{
    copy_stored_bounded, decode_array32, decode_persisted_bounded, decode_string, decode_u32,
    encode_owned,
};
use super::error::{Db, StorageError, StorageResult};
use super::identity::{OpenPolicy, StorageIdentity, LOGICAL_SCHEMA_VERSION, STORAGE_CODEC_VERSION};
use crate::coordination::raft::CoordinationMetaV1;

// ── raft.redb (§11.2) ────────────────────────────────────────────────────────

/// Identity, vote, committed and purge markers.
pub(crate) const RAFT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");

/// Log entries keyed by index.
///
/// redb stores `u64` keys little-endian, but `Key::compare` decodes both
/// operands and compares numerically, so `range`, `pop_first` and `pop_last`
/// iterate in true log order regardless of byte layout. Truncate, purge and
/// replay depend on that; do not infer ordering from the stored bytes.
pub(crate) const RAFT_LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");

/// Exactly the tables schema v1 defines in `raft.redb`. Anything else present
/// is a schema error, never something to drop.
pub(crate) const RAFT_TABLES: &[&str] = &["raft_meta", "raft_log"];

// ── state.redb (§11.3) ───────────────────────────────────────────────────────

pub(crate) const STATE_META: TableDefinition<&str, &[u8]> =
    TableDefinition::new("coordination_meta");
pub(crate) const RESOURCES: TableDefinition<(&str, &[u8]), &[u8]> =
    TableDefinition::new("resources");
pub(crate) const ACL: TableDefinition<(&str, &[u8], [u8; 32]), u64> =
    TableDefinition::new("resource_acl");
pub(crate) const ADMINS: TableDefinition<[u8; 32], ()> = TableDefinition::new("group_admins");
pub(crate) const SESSIONS: TableDefinition<([u8; 32], [u8; 16]), &[u8]> =
    TableDefinition::new("client_sessions");
pub(crate) const INTENTS: TableDefinition<[u8; 16], &[u8]> =
    TableDefinition::new("membership_intents");
pub(crate) const ROUTES: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("node_routes");
pub(crate) const AUDIT: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("audit");

/// Exactly the tables schema v1 defines in `state.redb`.
pub(crate) const STATE_TABLES: &[&str] = &[
    "coordination_meta",
    "resources",
    "resource_acl",
    "group_admins",
    "client_sessions",
    "membership_intents",
    "node_routes",
    "audit",
];

/// The `state.redb` data tables, in canonical section order. Used to prove a
/// state database holds no durable rows.
const STATE_DATA_TABLES: &[&str] = &[
    "resources",
    "resource_acl",
    "group_admins",
    "client_sessions",
    "membership_intents",
    "node_routes",
    "audit",
];

// ── Value ceilings (§4.2) ────────────────────────────────────────────────────

/// Ceiling on an identity scalar: a version, a 32-byte id, or a domain.
///
/// Generous against the real sizes (4 to 32 bytes, plus a hostname) and tight
/// against a corrupt row: the point is that a damaged length cannot be
/// allocated, not that it is measured precisely.
pub(crate) const MAX_META_SCALAR_BYTES: usize = 4 * 1024;

/// Ceiling on the stored `CoordinationMetaV1` row.
///
/// The invariant this has to satisfy is *not* "equals the snapshot header
/// bound". §11.4's `SnapshotHeaderV1` embeds this root **plus** two version
/// fields, seven row counts, and Fory envelope, so a meta row at exactly
/// `MAX_HEADER_BYTES` produces a header above it: the voter would accept state
/// it could never snapshot, and would discover that only when a follower
/// needed it. An earlier version of this constant was that equality, and the
/// test that "proved" it pinned the same false oracle.
///
/// What it must satisfy is that anything this ceiling *admits* still fits in a
/// header once wrapped. The maximal legal meta measures **1849 bytes**: a joint
/// membership at `MAX_VOTERS` with the whole §9 learner envelope outside it —
/// thirteen node entries — plus a 128-byte authority domain and every optional
/// present. The header envelope around it is measured separately in the same
/// test. 16 KiB leaves close to ninefold headroom for fields added later while
/// keeping `16384 + envelope` far below the 64 KiB header bound.
///
/// The figure moved twice and both moves are worth recording: it was 5607 when
/// the domain was sized at the generic `MAX_WIRE_STRING` rather than §7.1's
/// 128 bytes, and it grew again when the learner envelope widened from two to
/// six. Neither moved the ceiling, which is the point of leaving this much
/// room.
///
/// Both directions are proved by measurement in
/// `the_meta_ceiling_admits_only_snapshottable_rows`, not asserted here.
pub(crate) const MAX_COORDINATION_META_BYTES: usize = 16 * 1024;

// ── Metadata keys ────────────────────────────────────────────────────────────

pub(crate) const KEY_SCHEMA_VERSION: &str = "schema_version";
pub(crate) const KEY_CODEC_VERSION: &str = "codec_version";
pub(crate) const KEY_GROUP_ID: &str = "group_id";
pub(crate) const KEY_GENESIS_HASH: &str = "genesis_hash";
pub(crate) const KEY_AUTHORITY_PUBLIC_KEY: &str = "authority_public_key";
pub(crate) const KEY_AUTHORITY_DOMAIN: &str = "authority_domain";
pub(crate) const KEY_LOCAL_NODE_ID: &str = "local_node_id";
pub(crate) const KEY_VOTE: &str = "vote";
pub(crate) const KEY_COMMITTED: &str = "committed";
pub(crate) const KEY_LAST_PURGED: &str = "last_purged";

/// The singleton replicated-meta row (§11.3).
pub(crate) const KEY_META: &str = "meta";

/// Exactly the meta keys `raft.redb` may contain.
pub(crate) const RAFT_META_KEYS: &[&str] = &[
    KEY_SCHEMA_VERSION,
    KEY_CODEC_VERSION,
    KEY_GROUP_ID,
    KEY_GENESIS_HASH,
    KEY_AUTHORITY_PUBLIC_KEY,
    KEY_AUTHORITY_DOMAIN,
    KEY_LOCAL_NODE_ID,
    KEY_VOTE,
    KEY_COMMITTED,
    KEY_LAST_PURGED,
];

/// Exactly the meta keys `state.redb` may contain. The replicated binding
/// lives inside the Fory `CoordinationMetaV1` under [`KEY_META`]; the codec
/// version sits beside it because a value that cannot be decoded cannot report
/// its own codec.
pub(crate) const STATE_META_KEYS: &[&str] = &[KEY_META, KEY_CODEC_VERSION];

/// The `raft.redb` meta keys that carry durable Raft state rather than
/// identity. Their presence is what makes a database non-empty.
const RAFT_DURABLE_KEYS: &[&str] = &[KEY_VOTE, KEY_COMMITTED, KEY_LAST_PURGED];

// ── Layout ───────────────────────────────────────────────────────────────────

/// The §11.1 directory layout, resolved once.
#[derive(Debug, Clone)]
pub(crate) struct StorageLayout {
    root: PathBuf,
}

impl StorageLayout {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn database(&self, db: Db) -> PathBuf {
        self.root.join(db.file_name())
    }

    pub(crate) fn snapshots(&self) -> PathBuf {
        self.root.join("snapshots")
    }

    pub(crate) fn incoming(&self) -> PathBuf {
        self.root.join("incoming")
    }

    pub(crate) fn lock(&self) -> PathBuf {
        self.root.join("LOCK")
    }

    /// The durable current-snapshot marker. Absent before the first snapshot.
    pub(crate) fn current_snapshot_marker(&self) -> PathBuf {
        self.snapshots().join("CURRENT")
    }
}

// ── The lifetime lock ────────────────────────────────────────────────────────

/// An exclusive lock on the storage directory, held for the runtime lifetime.
///
/// The lock is released by dropping this value, which closes the file. It is
/// advisory against cooperating processes; arbitrary code already running as
/// the storage owner is inside the host threat boundary and this does not
/// pretend otherwise.
#[derive(Debug)]
pub(crate) struct DirectoryLock {
    path: PathBuf,
    file: File,
}

impl DirectoryLock {
    /// Acquire the lock, or fail with the reason it could not be acquired.
    ///
    /// The two failure modes are kept apart deliberately. `WouldBlock` is the
    /// only result that means another voter owns this directory; the standard
    /// library guarantees it never hides a `WouldBlock` inside
    /// `TryLockError::Error`, so the split is exact rather than heuristic.
    pub(crate) fn acquire(path: impl Into<PathBuf>, policy: &OpenPolicy) -> StorageResult<Self> {
        let path = path.into();
        reject_symlink(&path)?;

        // Whether the lock file already existed decides what happens to its
        // permissions, and the two are not interchangeable. A pre-existing
        // group-writable `LOCK` is a production policy violation to *refuse*;
        // chmodding it would repair the symptom, let startup continue, and
        // erase the evidence that something else could write the file this
        // voter's mutual exclusion depends on.
        let existed = path.exists();
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| StorageError::LockIo {
                path: path.clone(),
                source,
            })?;
        if existed {
            check_permissions(&path, policy)?;
        } else {
            restrict_permissions(&path, FILE_MODE)?;
        }

        match file.try_lock() {
            Ok(()) => Ok(Self { path, file }),
            Err(failure) => Err(classify_lock_failure(path, failure)),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Release the lock explicitly during an orderly shutdown.
    pub(crate) fn unlock(self) -> StorageResult<()> {
        self.file.unlock().map_err(|source| StorageError::LockIo {
            path: self.path.clone(),
            source,
        })
    }
}

/// Map a `try_lock` failure to its named storage failure.
///
/// A pure function, for the same reason [`classify_integrity`] is one: a real
/// `TryLockError::Error` needs a filesystem that fails the lock syscall while
/// still opening the file, which is not something a test can arrange on a
/// normal volume. Fused into [`DirectoryLock::acquire`], the `Error` arm had
/// no test at all — an earlier version of this module collapsed both arms into
/// contention and every lock test still passed, because the only I/O failure
/// being exercised happened at `File::open`, one step earlier.
fn classify_lock_failure(path: PathBuf, failure: std::fs::TryLockError) -> StorageError {
    match failure {
        std::fs::TryLockError::WouldBlock => StorageError::LockContended { path },
        std::fs::TryLockError::Error(source) => StorageError::LockIo { path, source },
    }
}

// ── Permissions ──────────────────────────────────────────────────────────────

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Set owner-only permissions on a path we just created. A no-op off Unix,
/// where §11.1 relies on the application directory's inherited user ACL and a
/// platform test/runbook instead.
#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> StorageResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|source| StorageError::io("set permissions", path, source))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> StorageResult<()> {
    Ok(())
}

/// Refuse an existing path that is writable beyond its owner.
#[cfg(unix)]
fn check_permissions(path: &Path, policy: &OpenPolicy) -> StorageResult<()> {
    use std::os::unix::fs::PermissionsExt;
    if !policy.enforces_permissions() {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|source| StorageError::io("read metadata", path, source))?;
    let mode = meta.permissions().mode() & 0o777;
    // Group- or other-writable. Read access is not the concern here: §11.1
    // provides no at-rest encryption and says so, but another writer can
    // corrupt state the lock is supposed to protect.
    if mode & 0o022 != 0 {
        return Err(StorageError::BroadPermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path, _policy: &OpenPolicy) -> StorageResult<()> {
    Ok(())
}

/// Refuse a symlinked path.
fn reject_symlink(path: &Path) -> StorageResult<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(StorageError::SymlinkedPath {
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StorageError::io("read metadata", path, source)),
    }
}

/// Create a directory with owner-only permissions if it is absent, and
/// validate it if it is not.
fn ensure_directory(path: &Path, policy: &OpenPolicy) -> StorageResult<()> {
    reject_symlink(path)?;
    if path.exists() {
        check_permissions(path, policy)?;
        return Ok(());
    }
    std::fs::create_dir_all(path)
        .map_err(|source| StorageError::io("create directory", path, source))?;
    restrict_permissions(path, DIR_MODE)
}

// ── Write transactions ───────────────────────────────────────────────────────

/// The one place a write transaction is created in this subsystem.
///
/// Centralised so the §11.1 durability rule is a property of the code rather
/// than of reviewer attention. Selecting the deferred-fsync level is
/// unobservable in any behavioural test — every read in this process and a
/// clean reopen both still see the data — so the only way to hold the rule is
/// to leave exactly one place where it can be got wrong, and assert that in
/// [`durability_guard`](super::durability_guard).
///
/// An earlier guard counted occurrences per file, which meant removing the
/// setting from one of two write paths in the same file still passed.
pub(crate) fn durable_write(database: &Database, db: Db) -> StorageResult<redb::WriteTransaction> {
    let mut write = database
        .begin_write()
        .map_err(|source| StorageError::redb(db, source))?;
    write
        .set_durability(redb::Durability::Immediate)
        .map_err(|source| StorageError::redb(db, source))?;
    Ok(write)
}

// ── Integrity ────────────────────────────────────────────────────────────────

/// What `check_integrity` reported, interpreted per §11.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrityOutcome {
    /// `Ok(true)`: the physical database was clean.
    Clean,
    /// `Ok(false)` on a pre-existing database: the accepted unclean-shutdown
    /// recovery path. The caller still runs every logical check before
    /// serving; this is emitted, not swallowed.
    RecoveredFromUncleanShutdown,
}

/// Run the integrity check and classify its result.
///
/// `freshly_created` is not cosmetic. A database this process just made has no
/// prior crash to recover from, so `Ok(false)` there is evidence about the
/// storage stack rather than about the last shutdown — and after a snapshot
/// install it would mean the replacement database was already damaged.
fn check_integrity(
    database: &mut Database,
    db: Db,
    freshly_created: bool,
) -> StorageResult<IntegrityOutcome> {
    classify_integrity(database.check_integrity(), db, freshly_created)
}

/// The §11.1 classification rule, separated from the redb call that produces
/// the input.
///
/// Split deliberately. `Ok(false)` cannot be provoked on demand — it needs a
/// process killed mid-commit, which is Checkpoint E's fault harness — so
/// leaving the rule fused to the I/O would mean the most consequential branch
/// here had no test until then. As a pure function every arm is directly
/// evidenced now.
fn classify_integrity(
    result: Result<bool, DatabaseError>,
    db: Db,
    freshly_created: bool,
) -> StorageResult<IntegrityOutcome> {
    match result {
        Ok(true) => Ok(IntegrityOutcome::Clean),
        Ok(false) if freshly_created => Err(StorageError::FreshDatabaseUnclean { db }),
        Ok(false) => Ok(IntegrityOutcome::RecoveredFromUncleanShutdown),
        Err(DatabaseError::TransactionInProgress) => {
            Err(StorageError::TransactionInProgress { db })
        }
        Err(source) => Err(StorageError::Integrity { db, source }),
    }
}

// ── Scalar meta encoding ─────────────────────────────────────────────────────
//
// Identity meta values are fixed-width scalars, not declared Fory roots: §4.2
// names the roots for OpenRaft values and state rows, and a group id is
// neither. Encoding is explicit and length-checked in both directions, so a
// truncated or overlong value is a named codec failure rather than a silent
// reinterpretation.

fn encode_u32(value: u32) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

// ── Open ─────────────────────────────────────────────────────────────────────

/// Whether a database file existed before this open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Fresh,
    Existing,
}

/// How an open resolved.
///
/// Reported rather than reduced to a boolean because a recreation is an event
/// an operator should see: it means this node crashed between its two
/// first-open commits, and the side that was rebuilt is provably empty rather
/// than merely assumed so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenOutcome {
    /// Nothing was bound here; both databases were created and bound.
    Fresh,
    /// Both databases were already bound and were verified.
    Initialized,
    /// `raft.redb` was bound and pristine; `state.redb` was rebuilt.
    RecreateState,
    /// `state.redb` was bound and pristine; `raft.redb` was rebuilt.
    RecreateRaft,
}

/// The pieces of a validated open, handed on to the per-database actors.
#[derive(Debug)]
pub(crate) struct OpenedParts {
    pub(crate) layout: StorageLayout,
    pub(crate) identity: StorageIdentity,
    pub(crate) lock: DirectoryLock,
    pub(crate) raft: Database,
    pub(crate) state: Database,
    pub(crate) outcome: OpenOutcome,
    /// Carried, not discarded. §11.1 requires the unclean-shutdown recovery
    /// path to *emit* an event, and a value dropped here cannot be emitted by
    /// anyone: Slice 3 owns the health surface but can only report what
    /// storage hands it.
    pub(crate) integrity: (IntegrityOutcome, IntegrityOutcome),
}

/// A validated, locked, open storage directory.
///
/// Holds both databases by value. Checkpoints B, C and D take them from here; the
/// lock lives as long as this value does.
#[derive(Debug)]
pub(crate) struct OpenedStorage {
    layout: StorageLayout,
    identity: StorageIdentity,
    lock: DirectoryLock,
    raft: Database,
    state: Database,
    raft_integrity: IntegrityOutcome,
    state_integrity: IntegrityOutcome,
    outcome: OpenOutcome,
}

impl OpenedStorage {
    pub(crate) fn layout(&self) -> &StorageLayout {
        &self.layout
    }

    pub(crate) fn identity(&self) -> &StorageIdentity {
        &self.identity
    }

    pub(crate) fn lock(&self) -> &DirectoryLock {
        &self.lock
    }

    pub(crate) fn raft(&self) -> &Database {
        &self.raft
    }

    pub(crate) fn state(&self) -> &Database {
        &self.state
    }

    /// How each database's integrity check resolved. Exposed so a caller can
    /// emit the §11.1 recovery event rather than discovering it in a log line.
    pub(crate) fn integrity(&self) -> (IntegrityOutcome, IntegrityOutcome) {
        (self.raft_integrity, self.state_integrity)
    }

    /// How this open resolved, including whether a pristine side was rebuilt.
    pub(crate) fn outcome(&self) -> OpenOutcome {
        self.outcome
    }

    /// Break the validated open into its parts so each database can be handed
    /// to its own single-writer actor.
    ///
    /// By value, and only after every check has passed: `check_integrity`
    /// needs `&mut Database`, so a database published to an actor can never be
    /// checked again. The lock travels with the parts because releasing it
    /// here would let a second voter in behind the actors' backs.
    pub(crate) fn into_parts(self) -> OpenedParts {
        OpenedParts {
            layout: self.layout,
            identity: self.identity,
            lock: self.lock,
            raft: self.raft,
            state: self.state,
            outcome: self.outcome,
            integrity: (self.raft_integrity, self.state_integrity),
        }
    }

    /// Whether this open found an existing binding rather than creating both
    /// sides from nothing. A recreation counts: one side's binding was already
    /// there and was verified.
    pub(crate) fn was_initialized(&self) -> bool {
        !matches!(self.outcome, OpenOutcome::Fresh)
    }
}

/// Open the storage directory: create or validate the layout, take the
/// lifetime lock, create or validate both schemas, and bind or verify the
/// identity.
///
/// Fails closed at every ambiguity. In particular, one initialized database
/// beside one that cannot be proven empty is
/// [`StorageError::PartialInitialization`] rather than a recreation, because
/// recreating the missing side would discard whatever the surviving side has
/// already acknowledged.
pub(crate) fn open(
    layout: StorageLayout,
    identity: StorageIdentity,
    policy: OpenPolicy,
) -> StorageResult<OpenedStorage> {
    ensure_directory(layout.root(), &policy)?;
    ensure_directory(&layout.snapshots(), &policy)?;
    ensure_directory(&layout.incoming(), &policy)?;

    // The lock comes before either database is opened, so a losing opener
    // never touches the files at all.
    let lock = DirectoryLock::acquire(layout.lock(), &policy)?;

    let raft_path = layout.database(Db::Raft);
    let state_path = layout.database(Db::State);
    reject_symlink(&raft_path)?;
    reject_symlink(&state_path)?;

    let raft_presence = presence(&raft_path);
    let state_presence = presence(&state_path);
    if raft_presence == Presence::Existing {
        check_permissions(&raft_path, &policy)?;
    }
    if state_presence == Presence::Existing {
        check_permissions(&state_path, &policy)?;
    }

    let (mut raft, raft_integrity) = open_database(&raft_path, Db::Raft, raft_presence)?;
    let (mut state, state_integrity) = open_database(&state_path, Db::State, state_presence)?;

    let raft_side = classify_raft(&raft, raft_presence)?;
    let state_side = classify_state(&state, state_presence)?;

    use SideState::{BoundPristine, Unbound};
    let outcome = match (raft_side, state_side) {
        // Nothing has ever been bound here.
        (Unbound, Unbound) => OpenOutcome::Fresh,

        // Both bound. Durable state on either side is ordinary: a
        // `raft.redb` ahead of `state.redb` is normal crash recovery, and
        // OpenRaft replays its committed log after the durable `last_applied`
        // marker.
        (raft_state, state_state) if raft_state.is_bound() && state_state.is_bound() => {
            OpenOutcome::Initialized
        }

        // One side bound with nothing behind it: the crash landed between the
        // two first-open identity commits. Recoverable, because there is
        // provably nothing to discard. The surviving binding is verified
        // against the requested identity *before* the empty side is written,
        // so a foreign survivor cannot seed a new partner.
        (BoundPristine, Unbound) => OpenOutcome::RecreateState,
        (Unbound, BoundPristine) => OpenOutcome::RecreateRaft,

        // One side has acknowledged something and the other is gone. Writing
        // a fresh partner here would discard state the group was already told
        // about, so this fails closed.
        (raft_state, state_state) => {
            return Err(StorageError::PartialInitialization {
                detail: format!(
                    "{} is {}, {} is {}",
                    Db::Raft,
                    raft_state.describe(),
                    Db::State,
                    state_state.describe()
                ),
            })
        }
    };

    match outcome {
        OpenOutcome::Fresh => {
            write_raft_identity(&mut raft, &identity)?;
            write_state_identity(&mut state, &identity)?;
        }
        OpenOutcome::Initialized => {
            verify_raft_identity(&raft, &identity)?;
            verify_state_identity(&state, &identity)?;
        }
        OpenOutcome::RecreateState => {
            verify_raft_identity(&raft, &identity)?;
            write_state_identity(&mut state, &identity)?;
        }
        OpenOutcome::RecreateRaft => {
            verify_state_identity(&state, &identity)?;
            write_raft_identity(&mut raft, &identity)?;
        }
    }

    Ok(OpenedStorage {
        layout,
        identity,
        lock,
        raft,
        state,
        raft_integrity,
        state_integrity,
        outcome,
    })
}

fn presence(path: &Path) -> Presence {
    if path.exists() {
        Presence::Existing
    } else {
        Presence::Fresh
    }
}

/// Create or open one database and check its integrity while owning it.
fn open_database(
    path: &Path,
    db: Db,
    presence: Presence,
) -> StorageResult<(Database, IntegrityOutcome)> {
    let mut database = Database::create(path).map_err(|source| match source {
        DatabaseError::TransactionInProgress => StorageError::TransactionInProgress { db },
        other => StorageError::Integrity { db, source: other },
    })?;
    if presence == Presence::Fresh {
        restrict_permissions(path, FILE_MODE)?;
    }
    let outcome = check_integrity(&mut database, db, presence == Presence::Fresh)?;
    Ok((database, outcome))
}

/// What one side of the storage pair proved about itself.
///
/// The distinction that matters is *pristine versus durable*, not merely
/// bound versus unbound. A crash between the two first-open identity commits
/// leaves one bound side with nothing in it, and that is recoverable; a bound
/// side that has acknowledged a vote or applied an entry is not, because
/// recreating its partner would discard state the group was already told
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideState {
    /// Absent, or present with no identity binding and provably no durable
    /// state of any kind.
    Unbound,
    /// Identity bound, and nothing durable behind it.
    BoundPristine,
    /// Identity bound, with durable state.
    BoundDurable,
}

impl SideState {
    fn is_bound(self) -> bool {
        !matches!(self, SideState::Unbound)
    }

    fn describe(self) -> &'static str {
        match self {
            SideState::Unbound => "absent or provably empty",
            SideState::BoundPristine => "bound with no durable state",
            SideState::BoundDurable => "bound and carrying durable state",
        }
    }
}

/// Validate `raft.redb`'s schema and classify it.
fn classify_raft(database: &Database, presence: Presence) -> StorageResult<SideState> {
    if presence == Presence::Fresh {
        return Ok(SideState::Unbound);
    }
    let read = database
        .begin_read()
        .map_err(|source| StorageError::redb(Db::Raft, source))?;
    reject_unknown_tables(&read, Db::Raft, RAFT_TABLES)?;
    // Metadata is validated whether or not the identity row is there. An
    // unknown key in an otherwise-unbound database is still a foreign or newer
    // file, and skipping the check when the binding is absent is exactly the
    // window a crashed first open leaves open.
    check_meta_keys(&read, Db::Raft, RAFT_META, RAFT_META_KEYS)?;

    let bound = meta_bytes(
        &read,
        Db::Raft,
        RAFT_META,
        KEY_GROUP_ID,
        MAX_META_SCALAR_BYTES,
    )?
    .is_some();
    if bound {
        require_raft_tables(&read)?;
    }

    let mut durable = !table_is_empty_u64(&read, Db::Raft, RAFT_LOG)?;
    for key in RAFT_DURABLE_KEYS {
        if meta_bytes(&read, Db::Raft, RAFT_META, key, MAX_META_SCALAR_BYTES)?.is_some() {
            durable = true;
        }
    }

    Ok(match (bound, durable) {
        (true, true) => SideState::BoundDurable,
        (true, false) => SideState::BoundPristine,
        (false, false) => {
            require_no_metadata(&read, Db::Raft, RAFT_META)?;
            SideState::Unbound
        }
        // Durable Raft state with no binding to check it against. Nothing can
        // prove which group that vote or log belongs to, so it is neither
        // recoverable nor safe to adopt.
        (false, true) => {
            return Err(StorageError::PartialInitialization {
                detail: format!(
                    "{} carries vote/log/marker state but no identity binding",
                    Db::Raft
                ),
            })
        }
    })
}

/// An unbound side must hold no metadata at all before it is treated as
/// pristine and rebound.
///
/// Checking key *names* is not enough. A database holding only a
/// `codec_version` — wrong, truncated, or simply unexplained — passes the
/// allowlist, classifies as unbound, and is then overwritten by the recreation
/// path, which is the opposite of failing closed. Since first-open metadata is
/// written in one immediate-durability transaction, any metadata without the
/// binding means this file did not come from a crash of ours.
fn require_no_metadata(
    read: &redb::ReadTransaction,
    db: Db,
    definition: TableDefinition<&'static str, &'static [u8]>,
) -> StorageResult<()> {
    let rows = meta_row_count(read, db, definition)?;
    if rows > 0 {
        return Err(StorageError::PartialInitialization {
            detail: format!(
                "{db} has no identity binding but holds {rows} metadata row(s); \
                 first-open metadata commits atomically, so this did not come \
                 from an interrupted first open"
            ),
        });
    }
    Ok(())
}

/// Validate `state.redb`'s schema and classify it.
fn classify_state(database: &Database, presence: Presence) -> StorageResult<SideState> {
    if presence == Presence::Fresh {
        return Ok(SideState::Unbound);
    }
    let read = database
        .begin_read()
        .map_err(|source| StorageError::redb(Db::State, source))?;
    reject_unknown_tables(&read, Db::State, STATE_TABLES)?;
    check_meta_keys(&read, Db::State, STATE_META, STATE_META_KEYS)?;

    let meta_row = meta_bytes(
        &read,
        Db::State,
        STATE_META,
        KEY_META,
        MAX_COORDINATION_META_BYTES,
    )?;
    let bound = meta_row.is_some();
    if bound {
        require_state_tables(&read)?;
        // Before decoding anything. `codec_version` sits outside the Fory row
        // precisely so an incompatible codec is refused by name rather than
        // surfacing as a decode failure on the value whose codec is the thing
        // in question -- which would send an operator looking for corruption.
        verify_state_codec_version(&read)?;
    }

    let mut durable = state_data_tables_have_rows(&read)?;
    if let Some(bytes) = &meta_row {
        let meta: CoordinationMetaV1 = decode_persisted_bounded(
            Db::State,
            "CoordinationMetaV1",
            bytes,
            MAX_COORDINATION_META_BYTES,
        )?;
        if replicated_state_is_durable(&meta) {
            durable = true;
        }
    }

    Ok(match (bound, durable) {
        (true, true) => SideState::BoundDurable,
        (true, false) => SideState::BoundPristine,
        (false, false) => {
            require_no_metadata(&read, Db::State, STATE_META)?;
            SideState::Unbound
        }
        (false, true) => {
            return Err(StorageError::PartialInitialization {
                detail: format!("{} carries rows but no replicated meta row", Db::State),
            })
        }
    })
}

/// Whether a decoded replicated meta row records anything beyond the identity
/// binding a first open writes.
///
/// Every mutable field is listed rather than only `last_applied`. A field that
/// advanced without `last_applied` would mean apply and its metadata had
/// already diverged, and treating that as pristine would let the recovery path
/// overwrite the evidence.
fn replicated_state_is_durable(meta: &CoordinationMetaV1) -> bool {
    meta.last_applied.is_some()
        || meta.membership != Default::default()
        || meta.admin_revision != 0
        || meta.aggregate_growth_bytes != 0
        || meta.aggregate_reserve_bytes != 0
        || meta.audit_floor.is_some()
        || meta.recovery_export_hash.is_some()
        || meta.recovered_from.is_some()
}

/// Every table present must be one schema v1 defines.
fn reject_unknown_tables(
    read: &redb::ReadTransaction,
    db: Db,
    allowed: &[&str],
) -> StorageResult<()> {
    let tables = read
        .list_tables()
        .map_err(|source| StorageError::redb(db, source))?;
    for handle in tables {
        let name = redb::TableHandle::name(&handle).to_string();
        if !allowed.contains(&name.as_str()) {
            return Err(StorageError::UnknownTable { db, table: name });
        }
    }
    Ok(())
}

/// Every declared `raft.redb` table must exist under its exact redb types.
///
/// Rejecting extras is only half of an exact schema check: without this, a
/// bound database that had lost `raft_log`, or that carried a `raft_log` under
/// different key/value types, opened cleanly and read as empty.
fn require_raft_tables(read: &redb::ReadTransaction) -> StorageResult<()> {
    require_table(read, Db::Raft, RAFT_META)?;
    require_table(read, Db::Raft, RAFT_LOG)
}

/// The same for `state.redb`'s eight tables.
fn require_state_tables(read: &redb::ReadTransaction) -> StorageResult<()> {
    require_table(read, Db::State, STATE_META)?;
    require_table(read, Db::State, RESOURCES)?;
    require_table(read, Db::State, ACL)?;
    require_table(read, Db::State, ADMINS)?;
    require_table(read, Db::State, SESSIONS)?;
    require_table(read, Db::State, INTENTS)?;
    require_table(read, Db::State, ROUTES)?;
    require_table(read, Db::State, AUDIT)
}

/// Open one declared table for reading purely to prove it exists with the
/// declared key and value types.
fn require_table<K, V>(
    read: &redb::ReadTransaction,
    db: Db,
    definition: TableDefinition<'static, K, V>,
) -> StorageResult<()>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    match read.open_table(definition) {
        Ok(_) => Ok(()),
        Err(redb::TableError::TableDoesNotExist(table)) => {
            Err(StorageError::MissingTable { db, table })
        }
        Err(redb::TableError::TableTypeMismatch { table, key, value }) => {
            Err(StorageError::TableTypeMismatch {
                db,
                table,
                detail: format!("found key {key:?}, value {value:?}"),
            })
        }
        Err(redb::TableError::TableIsMultimap(table)) => Err(StorageError::TableTypeMismatch {
            db,
            table,
            detail: "found a multimap table where schema v1 declares a plain table".into(),
        }),
        Err(source) => Err(StorageError::redb(db, source)),
    }
}

/// Open a metadata table for reading, or report why it could not be opened.
///
/// `Ok(None)` means the table is absent, which is legitimate for a database
/// nothing has written yet. A same-named table under different key/value types
/// is *not* absence, and is reported as [`StorageError::TableTypeMismatch`]
/// rather than a generic redb error — the distinction is what tells an
/// operator "this file is not ours" apart from "this file is damaged".
fn open_meta_table<'a>(
    read: &'a redb::ReadTransaction,
    db: Db,
    definition: TableDefinition<&'static str, &'static [u8]>,
) -> StorageResult<Option<redb::ReadOnlyTable<&'static str, &'static [u8]>>>
where
    'a: 'a,
{
    match read.open_table(definition) {
        Ok(table) => Ok(Some(table)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(redb::TableError::TableTypeMismatch { table, key, value }) => {
            Err(StorageError::TableTypeMismatch {
                db,
                table,
                detail: format!("found key {key:?}, value {value:?}"),
            })
        }
        Err(redb::TableError::TableIsMultimap(table)) => Err(StorageError::TableTypeMismatch {
            db,
            table,
            detail: "found a multimap table where schema v1 declares a plain table".into(),
        }),
        Err(source) => Err(StorageError::redb(db, source)),
    }
}

/// Every metadata key present must be one schema v1 defines.
fn check_meta_keys(
    read: &redb::ReadTransaction,
    db: Db,
    definition: TableDefinition<&'static str, &'static [u8]>,
    allowed: &[&str],
) -> StorageResult<()> {
    let Some(table) = open_meta_table(read, db, definition)? else {
        return Ok(());
    };
    let iter = table
        .iter()
        .map_err(|source| StorageError::redb(db, source))?;
    for entry in iter {
        let (key, _) = entry.map_err(|source| StorageError::redb(db, source))?;
        let key = key.value().to_string();
        if !allowed.contains(&key.as_str()) {
            return Err(StorageError::UnknownMetaKey { db, key });
        }
    }
    Ok(())
}

/// How many metadata rows a database holds.
///
/// Used to prove an unbound side is genuinely pristine. A first open writes
/// every metadata key in one immediate-durability transaction, so redb commits
/// all of them or none: metadata present *without* the identity binding is not
/// a crash state this code can produce, it is a foreign or damaged file.
fn meta_row_count(
    read: &redb::ReadTransaction,
    db: Db,
    definition: TableDefinition<&'static str, &'static [u8]>,
) -> StorageResult<u64> {
    let Some(table) = open_meta_table(read, db, definition)? else {
        return Ok(0);
    };
    table.len().map_err(|source| StorageError::redb(db, source))
}

/// Copy one metadata value out, refusing anything over `max_bytes`.
///
/// The ceiling is checked against the *borrowed* redb value, before `to_vec`.
/// Checking after the copy would have already allocated whatever length a
/// corrupt row claimed, which is the allocation §4.2 exists to prevent.
fn meta_bytes(
    read: &redb::ReadTransaction,
    db: Db,
    definition: TableDefinition<&'static str, &'static [u8]>,
    key: &'static str,
    max_bytes: usize,
) -> StorageResult<Option<Vec<u8>>> {
    let Some(table) = open_meta_table(read, db, definition)? else {
        return Ok(None);
    };
    let value = table
        .get(key)
        .map_err(|source| StorageError::redb(db, source))?;
    match value {
        // One call, so the check cannot end up after the copy.
        Some(v) => Ok(Some(copy_stored_bounded(db, key, v.value(), max_bytes)?)),
        None => Ok(None),
    }
}

fn table_is_empty_u64(
    read: &redb::ReadTransaction,
    db: Db,
    definition: TableDefinition<u64, &'static [u8]>,
) -> StorageResult<bool> {
    match read.open_table(definition) {
        Ok(table) => Ok(table
            .is_empty()
            .map_err(|source| StorageError::redb(db, source))?),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(true),
        Err(source) => Err(StorageError::redb(db, source)),
    }
}

/// Whether any `state.redb` data table holds a row.
///
/// Each table is opened under its own declared key/value types. An earlier
/// version opened them all as `&str -> &[u8]` to avoid repeating the list, and
/// redb rejected every table whose real types differ — which the helper then
/// read as "not empty". Every freshly bound store looked durable, and the
/// recovery path could never fire.
fn state_data_tables_have_rows(read: &redb::ReadTransaction) -> StorageResult<bool> {
    Ok(!is_empty(read, RESOURCES)?
        || !is_empty(read, ACL)?
        || !is_empty(read, ADMINS)?
        || !is_empty(read, SESSIONS)?
        || !is_empty(read, INTENTS)?
        || !is_empty(read, ROUTES)?
        || !is_empty(read, AUDIT)?)
}

/// Row count emptiness for one declared table. An absent table is empty; a
/// type mismatch is a schema error rather than an emptiness answer, and is
/// reported as one by [`require_table`].
fn is_empty<K, V>(
    read: &redb::ReadTransaction,
    definition: TableDefinition<'static, K, V>,
) -> StorageResult<bool>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    match read.open_table(definition) {
        Ok(table) => table
            .is_empty()
            .map_err(|source| StorageError::redb(Db::State, source)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(true),
        Err(redb::TableError::TableTypeMismatch { table, key, value }) => {
            Err(StorageError::TableTypeMismatch {
                db: Db::State,
                table,
                detail: format!("found key {key:?}, value {value:?}"),
            })
        }
        Err(source) => Err(StorageError::redb(Db::State, source)),
    }
}

// ── Identity binding ─────────────────────────────────────────────────────────

fn write_raft_identity(database: &mut Database, identity: &StorageIdentity) -> StorageResult<()> {
    let write = durable_write(database, Db::Raft)?;
    {
        let mut meta = write
            .open_table(RAFT_META)
            .map_err(|source| StorageError::redb(Db::Raft, source))?;
        let mut put = |key: &str, value: &[u8]| -> StorageResult<()> {
            meta.insert(key, value)
                .map(|_| ())
                .map_err(|source| StorageError::redb(Db::Raft, source))
        };
        put(KEY_SCHEMA_VERSION, &encode_u32(LOGICAL_SCHEMA_VERSION))?;
        put(KEY_CODEC_VERSION, &encode_u32(STORAGE_CODEC_VERSION))?;
        put(KEY_GROUP_ID, identity.group_id().as_bytes())?;
        put(KEY_GENESIS_HASH, identity.genesis_hash())?;
        put(KEY_AUTHORITY_PUBLIC_KEY, identity.authority_public_key())?;
        put(KEY_AUTHORITY_DOMAIN, identity.authority_domain().as_bytes())?;
        put(KEY_LOCAL_NODE_ID, identity.local_node_id().as_bytes())?;
        // The log table is created here so an initialized database always has
        // both tables, and the allowlist check does not have to special-case
        // a database that has been bound but never appended to.
        write
            .open_table(RAFT_LOG)
            .map_err(|source| StorageError::redb(Db::Raft, source))?;
    }
    write
        .commit()
        .map_err(|source| StorageError::redb(Db::Raft, source))
}

fn write_state_identity(database: &mut Database, identity: &StorageIdentity) -> StorageResult<()> {
    let meta_row = CoordinationMetaV1 {
        schema_version: LOGICAL_SCHEMA_VERSION,
        group_id: identity.group_id().as_bytes().to_vec(),
        genesis_hash: identity.genesis_hash().to_vec(),
        authority_public_key: identity.authority_public_key().to_vec(),
        authority_domain: identity.authority_domain().to_string(),
        deployment_profile_code: identity.deployment_profile_code(),
        ..Default::default()
    };
    let write = durable_write(database, Db::State)?;
    // Every data table exists from the start, so an initialized state database
    // always has the full §11.3 shape and a later missing table is a real
    // schema error rather than "not written to yet".
    write_state_binding(&write, &meta_row)?;
    write
        .commit()
        .map_err(|source| StorageError::redb(Db::State, source))
}

/// Everything a fresh state database must carry before it is a state database.
///
/// The full §11.1 binding, in one place, so a caller that builds a candidate
/// cannot get a *part* of it right. `KEY_CODEC_VERSION` is the one that goes
/// missing: nothing before [`classify_state`] needs it, `rows::load` is happy
/// without it, and the database is then unopenable by the very reopen an
/// install performs after replacing `state.redb` — which is a failure at the
/// least recoverable moment there is.
///
/// The meta row comes from the caller because an install's is the snapshot
/// header's, verbatim, and a bootstrap's is derived from its identity.
pub(crate) fn write_state_binding(
    write: &redb::WriteTransaction,
    meta: &CoordinationMetaV1,
) -> StorageResult<()> {
    create_state_tables(write)?;
    let encoded = encode_owned(Db::State, "CoordinationMetaV1", meta)?;
    let mut table = write
        .open_table(STATE_META)
        .map_err(|source| StorageError::redb(Db::State, source))?;
    table
        .insert(KEY_META, encoded.as_slice())
        .map_err(|source| StorageError::redb(Db::State, source))?;
    table
        .insert(
            KEY_CODEC_VERSION,
            encode_u32(STORAGE_CODEC_VERSION).as_slice(),
        )
        .map_err(|source| StorageError::redb(Db::State, source))?;
    Ok(())
}

/// A newly-created state candidate whose path did not exist at creation.
///
/// Kept as a distinct type because candidate integrity is checked with
/// `freshly_created = true`. Accepting an arbitrary [`Database`] there would
/// make that assertion a caller convention and let an old candidate retain
/// rows the rebuild never visited.
///
/// **Removed on drop unless kept.** The two rules compose into a trap
/// otherwise: creation refuses an existing path, so a rebuild that failed
/// after creating the file makes every retry at that path fail too — with
/// `AlreadyExists`, naming the previous attempt's leftovers rather than the
/// reason the first one failed. Cleanup rides on the value that owns the file
/// so no error path has to remember it, and [`StateCandidate::keep`] is the
/// single deliberate way to end it.
#[derive(Debug)]
pub(crate) struct StateCandidate {
    /// `None` once a caller has taken ownership of the file.
    path: Option<PathBuf>,
    /// `None` once verification has closed the builder handle.
    database: Option<Database>,
}

impl StateCandidate {
    pub(crate) fn database(&self) -> &Database {
        self.database
            .as_ref()
            .expect("a candidate holds its database until it is verified")
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("a candidate holds its path until it is kept")
    }

    /// Close the builder handle. Verification reopens the file instead.
    fn close(&mut self) {
        drop(self.database.take());
    }

    /// Take the file, and with it the responsibility for removing it.
    ///
    /// The only way out of cleanup-on-drop, and it consumes the candidate: what
    /// follows is the replacement, after which the file is `state.redb` and
    /// removing it would be the opposite of the intent.
    pub(crate) fn keep(mut self) -> PathBuf {
        self.path.take().expect("a candidate is kept once")
    }
}

impl Drop for StateCandidate {
    fn drop(&mut self) {
        drop(self.database.take());
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A candidate that has passed production's complete open, and the state it
/// was proved to hold.
///
/// The candidate stays armed: an install that fails *after* validation — while
/// quiescing, or replacing — must leave nothing behind either.
#[derive(Debug)]
pub(crate) struct VerifiedCandidate {
    candidate: StateCandidate,
    state: super::rows::DurableState,
}

impl VerifiedCandidate {
    pub(crate) fn state(&self) -> &super::rows::DurableState {
        &self.state
    }

    /// The validated state and the file, with cleanup handed to the caller.
    pub(crate) fn keep(self) -> (PathBuf, super::rows::DurableState) {
        let VerifiedCandidate { candidate, state } = self;
        (candidate.keep(), state)
    }
}

/// Create an absent state candidate with the production file permissions.
///
/// `Database::create(path)` also opens an existing database. A rebuild needs
/// the stronger operation: every row in the candidate must come from this
/// rebuild, so an existing path is refused atomically rather than reused.
pub(crate) fn create_state_candidate(path: &Path) -> StorageResult<StateCandidate> {
    reject_symlink(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| StorageError::io("creating a state candidate", path, source))?;

    // Armed from the moment the file exists, so every failure below unwinds
    // through `Drop` rather than through its own removal. The two that used to
    // be written by hand are exactly the two a third failure would have been
    // added without.
    let mut candidate = StateCandidate {
        path: Some(path.to_path_buf()),
        database: None,
    };
    restrict_permissions(path, FILE_MODE)?;

    candidate.database = Some(Database::builder().create_file(file).map_err(
        |source| match source {
            DatabaseError::TransactionInProgress => {
                StorageError::TransactionInProgress { db: Db::State }
            }
            other => StorageError::Integrity {
                db: Db::State,
                source: other,
            },
        },
    )?);
    Ok(candidate)
}

/// Open a state database exactly as production would, and refuse it if
/// production could not.
///
/// What an install's post-replacement reopen runs, hoisted so a candidate can
/// be held to it *before* it replaces anything. The [`StateCandidate`] type is
/// the construction proof for `freshly_created = true`, so `Ok(false)` from
/// `check_integrity` is evidence about the storage stack rather than about a
/// previous shutdown.
///
/// Returning the loaded state makes the complete production-open validation
/// inseparable from rebuild: schema classification alone cannot detect valid
/// individual rows whose counters or cross-table relationships disagree.
pub(crate) fn verify_state_database(
    mut candidate: StateCandidate,
) -> StorageResult<VerifiedCandidate> {
    // The builder handle goes, but the candidate does not: it still owns the
    // file, so a refusal below removes it rather than leaving a rejected
    // database under a name the next attempt cannot create.
    candidate.close();
    let path = candidate.path();

    // A distinct open is intentional: this is the exact boundary the install
    // will cross after replacement, not validation through the builder handle
    // that happened to write the bytes.
    reject_symlink(path)?;
    let mut database = Database::open(path).map_err(|source| match source {
        DatabaseError::TransactionInProgress => {
            StorageError::TransactionInProgress { db: Db::State }
        }
        other => StorageError::Integrity {
            db: Db::State,
            source: other,
        },
    })?;
    check_integrity(&mut database, Db::State, true)?;
    match classify_state(&database, Presence::Existing)? {
        SideState::BoundDurable | SideState::BoundPristine => {}
        SideState::Unbound => Err(StorageError::PartialInitialization {
            detail: format!(
                "{} at {} carries no identity binding",
                Db::State,
                path.display()
            ),
        })?,
    }
    let state = super::rows::load(&database)?;
    drop(database);
    Ok(VerifiedCandidate { candidate, state })
}

pub(crate) fn create_state_tables(write: &redb::WriteTransaction) -> StorageResult<()> {
    let map = |source: redb::TableError| StorageError::redb(Db::State, source);
    write.open_table(RESOURCES).map_err(map)?;
    write.open_table(ACL).map_err(map)?;
    write.open_table(ADMINS).map_err(map)?;
    write.open_table(SESSIONS).map_err(map)?;
    write.open_table(INTENTS).map_err(map)?;
    write.open_table(ROUTES).map_err(map)?;
    write.open_table(AUDIT).map_err(map)?;
    Ok(())
}

fn verify_raft_identity(database: &Database, identity: &StorageIdentity) -> StorageResult<()> {
    let read = database
        .begin_read()
        .map_err(|source| StorageError::redb(Db::Raft, source))?;

    let required = |key: &'static str| -> StorageResult<Vec<u8>> {
        meta_bytes(&read, Db::Raft, RAFT_META, key, MAX_META_SCALAR_BYTES)?
            .ok_or(StorageError::MissingMetaKey { db: Db::Raft, key })
    };

    let schema = decode_u32(Db::Raft, "schema_version", &required(KEY_SCHEMA_VERSION)?)?;
    if schema != LOGICAL_SCHEMA_VERSION {
        return Err(StorageError::SchemaVersion {
            db: Db::Raft,
            found: schema,
            expected: LOGICAL_SCHEMA_VERSION,
        });
    }
    let codec = decode_u32(Db::Raft, "codec_version", &required(KEY_CODEC_VERSION)?)?;
    if codec != STORAGE_CODEC_VERSION {
        return Err(StorageError::CodecVersion {
            db: Db::Raft,
            found: codec,
            expected: STORAGE_CODEC_VERSION,
        });
    }

    let mismatch = |field| StorageError::IdentityMismatch {
        db: Db::Raft,
        field,
    };

    if decode_array32(Db::Raft, "group_id", &required(KEY_GROUP_ID)?)?
        != *identity.group_id().as_bytes()
    {
        return Err(mismatch("group_id"));
    }
    if decode_array32(Db::Raft, "genesis_hash", &required(KEY_GENESIS_HASH)?)?
        != *identity.genesis_hash()
    {
        return Err(mismatch("genesis_hash"));
    }
    if decode_array32(
        Db::Raft,
        "authority_public_key",
        &required(KEY_AUTHORITY_PUBLIC_KEY)?,
    )? != *identity.authority_public_key()
    {
        return Err(mismatch("authority_public_key"));
    }
    if decode_string(
        Db::Raft,
        "authority_domain",
        &required(KEY_AUTHORITY_DOMAIN)?,
    )? != identity.authority_domain()
    {
        return Err(mismatch("authority_domain"));
    }
    if decode_array32(Db::Raft, "local_node_id", &required(KEY_LOCAL_NODE_ID)?)?
        != *identity.local_node_id().as_bytes()
    {
        return Err(mismatch("local_node_id"));
    }
    Ok(())
}

/// Check `state.redb`'s stored codec version against this build's.
///
/// Called before any Fory value from this database is decoded. The key is a
/// fixed-width scalar outside the Fory row for exactly that reason: a build
/// that cannot read the row's encoding must still be able to say so by name.
fn verify_state_codec_version(read: &redb::ReadTransaction) -> StorageResult<()> {
    let codec_bytes = meta_bytes(
        read,
        Db::State,
        STATE_META,
        KEY_CODEC_VERSION,
        MAX_META_SCALAR_BYTES,
    )?
    .ok_or(StorageError::MissingMetaKey {
        db: Db::State,
        key: KEY_CODEC_VERSION,
    })?;
    let codec = decode_u32(Db::State, "codec_version", &codec_bytes)?;
    if codec != STORAGE_CODEC_VERSION {
        return Err(StorageError::CodecVersion {
            db: Db::State,
            found: codec,
            expected: STORAGE_CODEC_VERSION,
        });
    }
    Ok(())
}

fn verify_state_identity(database: &Database, identity: &StorageIdentity) -> StorageResult<()> {
    let read = database
        .begin_read()
        .map_err(|source| StorageError::redb(Db::State, source))?;

    verify_state_codec_version(&read)?;

    let bytes = meta_bytes(
        &read,
        Db::State,
        STATE_META,
        KEY_META,
        MAX_COORDINATION_META_BYTES,
    )?
    .ok_or(StorageError::MissingMetaKey {
        db: Db::State,
        key: KEY_META,
    })?;
    let meta: CoordinationMetaV1 = decode_persisted_bounded(
        Db::State,
        "CoordinationMetaV1",
        &bytes,
        MAX_COORDINATION_META_BYTES,
    )?;

    if meta.schema_version != LOGICAL_SCHEMA_VERSION {
        return Err(StorageError::SchemaVersion {
            db: Db::State,
            found: meta.schema_version,
            expected: LOGICAL_SCHEMA_VERSION,
        });
    }

    let mismatch = |field| StorageError::IdentityMismatch {
        db: Db::State,
        field,
    };

    if meta.group_id.as_slice() != identity.group_id().as_bytes() {
        return Err(mismatch("group_id"));
    }
    if meta.genesis_hash.as_slice() != identity.genesis_hash() {
        return Err(mismatch("genesis_hash"));
    }
    if meta.authority_public_key.as_slice() != identity.authority_public_key() {
        return Err(mismatch("authority_public_key"));
    }
    if meta.authority_domain != identity.authority_domain() {
        return Err(mismatch("authority_domain"));
    }
    // The local NodeId is deliberately *not* checked here: it is bound in
    // `raft.redb` only. `state.redb` is replicated and a snapshot moves
    // between nodes, so binding a local id into it would make every installed
    // snapshot look like an identity mismatch.
    if meta.deployment_profile_code != identity.deployment_profile_code() {
        return Err(mismatch("deployment_profile_code"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::payload_fory;
    use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
    use crate::coordination::model::shape;
    use crate::coordination::storage::fixture::{
        maximal_legal_meta, maximal_legal_row_counts, oversized_meta_for_envelope,
        widest_admitted_membership,
    };
    use redb::TableHandle;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh directory per test. Not `tempfile`: these tests deliberately
    /// inspect and mutate the layout between opens, so the path has to outlive
    /// any one handle.
    fn scratch() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aster-coordination-storage-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn identity() -> StorageIdentity {
        StorageIdentity::new(
            CoordinationGroupId::from_array([1u8; 32]),
            [2u8; 32],
            [3u8; 32],
            "authority.test",
            CoordinationNodeId::from_array([4u8; 32]),
            shape::PROFILE_PROD,
        )
    }

    fn open_at(root: &Path, id: StorageIdentity) -> StorageResult<OpenedStorage> {
        open(StorageLayout::new(root), id, OpenPolicy::production())
    }

    // ── Layout, lock, and first open ─────────────────────────────────────────

    /// A fresh open builds the whole §11.1 layout and binds both databases.
    #[test]
    fn a_fresh_open_creates_the_layout_and_binds_both_databases() {
        let root = scratch();
        let opened = open_at(&root, identity()).expect("fresh open");

        assert!(
            !opened.was_initialized(),
            "a fresh directory is not initialized"
        );
        let layout = opened.layout();
        assert!(layout.database(Db::Raft).exists());
        assert!(layout.database(Db::State).exists());
        assert!(layout.snapshots().is_dir());
        assert!(layout.incoming().is_dir());
        assert!(layout.lock().exists());
        assert!(
            !layout.current_snapshot_marker().exists(),
            "CURRENT is absent before the first snapshot"
        );
        assert_eq!(
            opened.integrity(),
            (IntegrityOutcome::Clean, IntegrityOutcome::Clean),
            "a database this process just created must be clean"
        );

        drop(opened);
        let reopened = open_at(&root, identity()).expect("reopen");
        assert!(
            reopened.was_initialized(),
            "the second open finds the binding the first wrote"
        );
    }

    /// A second opener reports contention, and specifically not an I/O error.
    #[test]
    fn a_second_opener_reports_lock_contention() {
        let root = scratch();
        let first = open_at(&root, identity()).expect("first open");

        let err = open_at(&root, identity()).expect_err("a second voter must not open");
        assert!(
            matches!(err, StorageError::LockContended { .. }),
            "expected contention, got: {err:?}"
        );

        // And the lock is a lifetime lock: releasing the first opener lets the
        // next one in. Asserted because a lock that is never released looks
        // identical to a correct one until an operator restarts a voter.
        drop(first);
        open_at(&root, identity()).expect("the lock is released with its holder");
    }

    /// Both `try_lock` arms map to their own failure, and only `WouldBlock`
    /// means another voter.
    ///
    /// Tested through the classifier rather than through `acquire`, because a
    /// genuine `TryLockError::Error` needs a filesystem that opens the file
    /// and then fails the lock syscall. That is not arrangeable on a normal
    /// volume, and pretending otherwise is how this arm went uncovered:
    /// collapsing both into contention left every lock test green, since the
    /// only I/O failure being exercised was the `File::open` one below.
    #[test]
    fn each_try_lock_failure_maps_to_its_own_error() {
        let path = PathBuf::from("/tmp/example/LOCK");

        let contended = classify_lock_failure(path.clone(), std::fs::TryLockError::WouldBlock);
        assert!(
            matches!(contended, StorageError::LockContended { .. }),
            "WouldBlock is the only result that means another voter: {contended:?}"
        );

        let io = classify_lock_failure(
            path,
            std::fs::TryLockError::Error(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            )),
        );
        let StorageError::LockIo { source, .. } = &io else {
            panic!("a lock syscall failure must not be reported as contention: {io:?}");
        };
        assert_eq!(
            source.kind(),
            std::io::ErrorKind::PermissionDenied,
            "and it must keep the original error rather than a summary"
        );
    }

    /// Failing to *open* `LOCK` is also `LockIo`, not contention.
    ///
    /// Injected by making `LOCK` a directory: a real filesystem state rather
    /// than a mocked error. Distinct from the classifier test above — this one
    /// covers the step before `try_lock` is ever reached.
    #[test]
    fn a_lock_open_failure_is_not_reported_as_contention() {
        let root = scratch();
        std::fs::create_dir_all(root.join("LOCK")).expect("occupy LOCK with a directory");

        let err = open_at(&root, identity()).expect_err("an unusable LOCK must fail the open");
        let StorageError::LockIo { source, .. } = &err else {
            panic!("expected StorageLockIo, got: {err:?}");
        };
        assert_ne!(
            source.kind(),
            std::io::ErrorKind::WouldBlock,
            "the standard library never hides WouldBlock inside TryLockError::Error"
        );
        assert!(
            !err.to_string().contains("locked by another process"),
            "an operator must not be sent looking for a second voter: {err}"
        );
    }

    /// An explicit unlock releases the directory.
    #[test]
    fn an_orderly_unlock_releases_the_directory() {
        let root = scratch();
        let lock =
            DirectoryLock::acquire(root.join("LOCK"), &OpenPolicy::production()).expect("acquire");
        assert_eq!(lock.path(), root.join("LOCK"));
        lock.unlock().expect("unlock");
        DirectoryLock::acquire(root.join("LOCK"), &OpenPolicy::production())
            .expect("reacquire after an orderly unlock");
    }

    // ── Integrity classification ─────────────────────────────────────────────

    /// A clean database is clean regardless of who created it.
    #[test]
    fn a_clean_integrity_result_is_clean_either_way() {
        for freshly_created in [true, false] {
            assert_eq!(
                classify_integrity(Ok(true), Db::Raft, freshly_created).expect("clean"),
                IntegrityOutcome::Clean
            );
        }
    }

    /// `Ok(false)` on a pre-existing database is the accepted recovery path.
    ///
    /// This is the *common* case in the Checkpoint E fault matrix, not an
    /// anomaly: every process-death test reopens a database redb had to
    /// recover. Rejecting it would make the crash matrix unrunnable; silently
    /// treating it as `Ok(true)` would erase the one signal that a crash
    /// happened.
    #[test]
    fn an_unclean_pre_existing_database_is_the_named_recovery_path() {
        assert_eq!(
            classify_integrity(Ok(false), Db::Raft, false).expect("recovery is accepted"),
            IntegrityOutcome::RecoveredFromUncleanShutdown
        );
    }

    /// `Ok(false)` on a database this process just created is fatal.
    ///
    /// It had no prior crash to recover from, so the result is evidence about
    /// the storage stack. The same rule covers the database built during a
    /// snapshot install, which must never be acknowledged unclean.
    #[test]
    fn an_unclean_freshly_created_database_is_fatal() {
        let err = classify_integrity(Ok(false), Db::State, true)
            .expect_err("a fresh database has nothing to recover from");
        assert!(
            matches!(err, StorageError::FreshDatabaseUnclean { db: Db::State }),
            "got: {err:?}"
        );
    }

    /// A live handle during the check is a lifecycle failure in this crate,
    /// reported as itself rather than as corruption.
    #[test]
    fn a_live_handle_during_the_check_is_a_lifecycle_failure() {
        let err = classify_integrity(Err(DatabaseError::TransactionInProgress), Db::Raft, false)
            .expect_err("a live handle must not be reported as clean");
        assert!(
            matches!(err, StorageError::TransactionInProgress { .. }),
            "got: {err:?}"
        );
    }

    // ── Identity ─────────────────────────────────────────────────────────────

    /// Every identity field is checked on reopen, by name.
    ///
    /// Table-driven because the failure mode being guarded against is a field
    /// that is *persisted* but never *compared* — which no single-field test
    /// would catch.
    #[test]
    fn every_identity_field_is_rechecked_on_reopen() {
        let base = identity();
        let cases: Vec<(&str, StorageIdentity)> = vec![
            (
                "group_id",
                StorageIdentity::new(
                    CoordinationGroupId::from_array([9u8; 32]),
                    *base.genesis_hash(),
                    *base.authority_public_key(),
                    base.authority_domain(),
                    *base.local_node_id(),
                    base.deployment_profile_code(),
                ),
            ),
            (
                "genesis_hash",
                StorageIdentity::new(
                    *base.group_id(),
                    [9u8; 32],
                    *base.authority_public_key(),
                    base.authority_domain(),
                    *base.local_node_id(),
                    base.deployment_profile_code(),
                ),
            ),
            (
                "authority_public_key",
                StorageIdentity::new(
                    *base.group_id(),
                    *base.genesis_hash(),
                    [9u8; 32],
                    base.authority_domain(),
                    *base.local_node_id(),
                    base.deployment_profile_code(),
                ),
            ),
            (
                "authority_domain",
                StorageIdentity::new(
                    *base.group_id(),
                    *base.genesis_hash(),
                    *base.authority_public_key(),
                    "someone-else.test",
                    *base.local_node_id(),
                    base.deployment_profile_code(),
                ),
            ),
            (
                "local_node_id",
                StorageIdentity::new(
                    *base.group_id(),
                    *base.genesis_hash(),
                    *base.authority_public_key(),
                    base.authority_domain(),
                    CoordinationNodeId::from_array([9u8; 32]),
                    base.deployment_profile_code(),
                ),
            ),
            (
                "deployment_profile_code",
                StorageIdentity::new(
                    *base.group_id(),
                    *base.genesis_hash(),
                    *base.authority_public_key(),
                    base.authority_domain(),
                    *base.local_node_id(),
                    shape::PROFILE_DEV_SINGLE,
                ),
            ),
        ];

        for (field, wrong) in cases {
            let root = scratch();
            drop(open_at(&root, base.clone()).expect("bind"));

            let err = open_at(&root, wrong)
                .err()
                .unwrap_or_else(|| panic!("a wrong {field} must not open"));
            match err {
                StorageError::IdentityMismatch { field: got, .. } => {
                    assert_eq!(got, field, "the failure must name the field that disagreed")
                }
                other => panic!("a wrong {field} must be an identity mismatch, got: {other:?}"),
            }
        }
    }

    /// The local NodeId is bound in `raft.redb` only.
    ///
    /// `state.redb` is replicated and a snapshot moves between nodes, so a
    /// local id inside it would make every installed snapshot look like an
    /// identity mismatch.
    #[test]
    fn the_local_node_id_is_absent_from_replicated_state() {
        let root = scratch();
        let opened = open_at(&root, identity()).expect("bind");
        let read = opened.state().begin_read().expect("read");
        let bytes = meta_bytes(
            &read,
            Db::State,
            STATE_META,
            KEY_META,
            MAX_COORDINATION_META_BYTES,
        )
        .expect("meta")
        .expect("meta row present");
        let meta: CoordinationMetaV1 = payload_fory::<CoordinationMetaV1>()
            .deserialize(&bytes)
            .expect("decode");

        assert_eq!(meta.group_id, vec![1u8; 32]);
        assert_eq!(meta.deployment_profile_code, shape::PROFILE_PROD);
        // The replicated meta has no NodeId field at all; this asserts the
        // binding did not smuggle one in via another field.
        assert!(meta.last_applied.is_none());
        assert_eq!(meta.aggregate_growth_bytes, 0);
    }

    /// A wrong logical schema version is refused by name.
    #[test]
    fn a_wrong_schema_version_is_refused() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        overwrite_raft_meta(&root, KEY_SCHEMA_VERSION, &99u32.to_be_bytes());

        match open_at(&root, identity()) {
            Err(StorageError::SchemaVersion {
                found, expected, ..
            }) => {
                assert_eq!((found, expected), (99, LOGICAL_SCHEMA_VERSION));
            }
            other => panic!("expected a schema-version failure, got: {other:?}"),
        }
    }

    /// A wrong storage codec version is refused by name, and separately from
    /// the schema version.
    #[test]
    fn a_wrong_codec_version_is_refused_separately() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        overwrite_raft_meta(&root, KEY_CODEC_VERSION, &77u32.to_be_bytes());

        match open_at(&root, identity()) {
            Err(StorageError::CodecVersion {
                found, expected, ..
            }) => {
                assert_eq!((found, expected), (77, STORAGE_CODEC_VERSION));
            }
            other => panic!("expected a codec-version failure, got: {other:?}"),
        }
    }

    /// A truncated meta value is named stored corruption, not a panic and not
    /// a silent reinterpretation.
    ///
    /// Classified as corruption rather than a retryable codec failure: these
    /// bytes are already durable. The test previously pinned the wrong
    /// classification, which is how it survived being wrong.
    #[test]
    fn a_malformed_meta_value_is_stored_corruption() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        overwrite_raft_meta(&root, KEY_SCHEMA_VERSION, &[0u8, 1]);

        match open_at(&root, identity()) {
            Err(StorageError::StoredCorruption { what, .. }) => {
                assert_eq!(what, "schema_version")
            }
            other => panic!("expected stored corruption, got: {other:?}"),
        }
    }

    // ── Schema allowlists ────────────────────────────────────────────────────

    /// A table schema v1 does not define fails the open rather than being
    /// dropped: it is either a newer schema or a file that is not ours.
    #[test]
    fn an_unknown_table_fails_the_open() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        {
            let db = Database::create(root.join("raft.redb")).expect("open raft");
            let write = db.begin_write().expect("write");
            let surprise: TableDefinition<u64, u64> = TableDefinition::new("surprise");
            write.open_table(surprise).expect("create");
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::UnknownTable { table, .. }) => assert_eq!(table, "surprise"),
            other => panic!("expected an unknown-table failure, got: {other:?}"),
        }
    }

    /// The same for a metadata key.
    #[test]
    fn an_unknown_meta_key_fails_the_open() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        overwrite_raft_meta(&root, "not_in_the_allowlist", b"x");

        match open_at(&root, identity()) {
            Err(StorageError::UnknownMetaKey { key, .. }) => {
                assert_eq!(key, "not_in_the_allowlist")
            }
            other => panic!("expected an unknown-meta-key failure, got: {other:?}"),
        }
    }

    /// The allowlists and the table definitions cannot drift apart.
    #[test]
    fn the_allowlists_match_the_declared_tables() {
        assert!(RAFT_TABLES.contains(&RAFT_META.name()));
        assert!(RAFT_TABLES.contains(&RAFT_LOG.name()));
        assert_eq!(RAFT_TABLES.len(), 2, "raft.redb defines exactly two tables");

        for definition_name in [
            STATE_META.name(),
            RESOURCES.name(),
            ACL.name(),
            ADMINS.name(),
            SESSIONS.name(),
            INTENTS.name(),
            ROUTES.name(),
            AUDIT.name(),
        ] {
            assert!(
                STATE_TABLES.contains(&definition_name),
                "{definition_name} is defined but not allowlisted"
            );
        }
        assert_eq!(
            STATE_TABLES.len(),
            8,
            "state.redb defines exactly eight tables"
        );
    }

    // ── The pristine-versus-durable pairing matrix ───────────────────────────
    //
    // The distinction is not "bound versus unbound". A crash between the two
    // first-open identity commits leaves a bound side with nothing behind it,
    // and rebuilding its partner discards nothing. A bound side that has
    // acknowledged a vote or applied an entry is the opposite case: rebuilding
    // its partner throws away state the group was already told about.

    /// A pristine `raft.redb` beside a deleted `state.redb` is *recovered*,
    /// not refused. This is the crash-between-commits case.
    #[test]
    fn a_pristine_log_beside_a_missing_state_database_is_recreated() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        std::fs::remove_file(root.join("state.redb")).expect("remove state");

        let opened = open_at(&root, identity()).expect("a pristine survivor is recoverable");
        assert_eq!(opened.outcome(), OpenOutcome::RecreateState);
        assert!(
            root.join("state.redb").exists(),
            "the empty side is rebuilt"
        );

        // And the rebuilt pair is a normal initialized store afterwards.
        drop(opened);
        let reopened = open_at(&root, identity()).expect("reopen");
        assert_eq!(reopened.outcome(), OpenOutcome::Initialized);
    }

    /// The mirror case.
    #[test]
    fn a_pristine_state_beside_a_missing_log_database_is_recreated() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        std::fs::remove_file(root.join("raft.redb")).expect("remove raft");

        let opened = open_at(&root, identity()).expect("a pristine survivor is recoverable");
        assert_eq!(opened.outcome(), OpenOutcome::RecreateRaft);
        assert!(root.join("raft.redb").exists());
    }

    /// A surviving side carrying durable Raft state fails closed.
    #[test]
    fn a_durable_log_beside_a_missing_state_database_fails_closed() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        overwrite_raft_meta(&root, KEY_VOTE, b"an acknowledged vote");
        std::fs::remove_file(root.join("state.redb")).expect("remove state");

        match open_at(&root, identity()) {
            Err(StorageError::PartialInitialization { detail }) => {
                assert!(detail.contains("durable state"), "got: {detail}");
                assert!(detail.contains("raft.redb"), "got: {detail}");
                assert!(detail.contains("state.redb"), "got: {detail}");
            }
            other => panic!("expected partial initialization, got: {other:?}"),
        }
    }

    /// A log entry alone is durable state, even with no vote or marker.
    #[test]
    fn a_log_entry_alone_makes_the_log_side_durable() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        {
            let db = Database::create(root.join("raft.redb")).expect("open raft");
            let write = db.begin_write().expect("write");
            {
                let mut log = write.open_table(RAFT_LOG).expect("open log");
                log.insert(1u64, b"entry".as_slice()).expect("insert");
            }
            write.commit().expect("commit");
        }
        std::fs::remove_file(root.join("state.redb")).expect("remove state");

        assert!(matches!(
            open_at(&root, identity()),
            Err(StorageError::PartialInitialization { .. })
        ));
    }

    /// A surviving side that has applied something fails closed.
    ///
    /// `last_applied` is inside the Fory meta row, so this also proves the
    /// durability test decodes that row rather than treating any present
    /// binding as pristine.
    #[test]
    fn an_applied_state_database_beside_a_missing_log_fails_closed() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        advance_state_last_applied(&root);
        std::fs::remove_file(root.join("raft.redb")).expect("remove raft");

        match open_at(&root, identity()) {
            Err(StorageError::PartialInitialization { detail }) => {
                assert!(detail.contains("durable state"), "got: {detail}");
            }
            other => panic!("expected partial initialization, got: {other:?}"),
        }
    }

    /// A durable log beside a pristine state machine opens normally.
    ///
    /// This is `raft.redb`-ahead-of-`state.redb`: exactly what a crash after
    /// commit but before apply looks like, and OpenRaft replays from the
    /// durable `last_applied` marker.
    #[test]
    fn a_durable_log_ahead_of_a_pristine_state_opens_normally() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        overwrite_raft_meta(&root, KEY_VOTE, b"an acknowledged vote");

        let opened = open_at(&root, identity()).expect("a log ahead of state is normal recovery");
        assert_eq!(opened.outcome(), OpenOutcome::Initialized);
    }

    /// Both sides durable is the steady state of a running voter, and the
    /// matrix case the two "ahead" tests do not cover between them.
    ///
    /// Each of those leaves the other side pristine, so
    /// `(BoundDurable, BoundDurable)` was reachable only in production. A
    /// classifier that refused it would pass every other test here and refuse
    /// to start any voter that had ever applied an entry.
    #[test]
    fn durable_state_on_both_sides_opens_normally() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        overwrite_raft_meta(&root, KEY_VOTE, b"an acknowledged vote");
        advance_state_last_applied(&root);

        let opened = open_at(&root, identity()).expect("an ordinary running voter must restart");
        assert_eq!(opened.outcome(), OpenOutcome::Initialized);
    }

    /// A state machine ahead of the log side opens normally when both are
    /// bound.
    ///
    /// The inverse of the case above, and equally legitimate: a crash during
    /// snapshot install can leave `last_applied` ahead of a log that has been
    /// truncated or purged. OpenRaft's `StorageHelper::get_initial_state`
    /// closes that hole by purging the log through `last_applied` before
    /// serving. Refusing it here would make a successfully installed snapshot
    /// unstartable.
    #[test]
    fn an_applied_state_ahead_of_a_pristine_log_opens_normally() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        advance_state_last_applied(&root);

        let opened =
            open_at(&root, identity()).expect("a snapshot-install crash state must still start");
        assert_eq!(opened.outcome(), OpenOutcome::Initialized);
    }

    /// That reconciliation does **not** extend to an absent log side.
    ///
    /// Stated as its own case because the two look similar and the difference
    /// decides whether acknowledged state survives: OpenRaft can purge a log
    /// it still has, and can do nothing about one that is gone.
    #[test]
    fn state_ahead_does_not_license_recreating_an_absent_log() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        advance_state_last_applied(&root);
        std::fs::remove_file(root.join("raft.redb")).expect("remove raft");

        assert!(
            matches!(
                open_at(&root, identity()),
                Err(StorageError::PartialInitialization { .. })
            ),
            "durable applied state beside a missing log must fail closed"
        );
    }

    /// An incompatible codec is refused before the Fory row it governs is
    /// decoded.
    ///
    /// The row is left deliberately unreadable as well. If classification
    /// decoded first, this would surface as a corruption-looking decode
    /// failure on the value whose codec is exactly what is in question.
    #[test]
    fn a_wrong_state_codec_is_refused_before_the_meta_row_is_decoded() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        {
            let db = Database::create(root.join("state.redb")).expect("open state");
            let write = db.begin_write().expect("write");
            {
                let mut meta = write.open_table(STATE_META).expect("open meta");
                meta.insert(KEY_CODEC_VERSION, 42u32.to_be_bytes().as_slice())
                    .expect("insert codec");
                meta.insert(KEY_META, b"not a valid Fory CoordinationMetaV1".as_slice())
                    .expect("corrupt meta");
            }
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::CodecVersion {
                found, expected, ..
            }) => {
                assert_eq!((found, expected), (42, STORAGE_CODEC_VERSION));
            }
            other => panic!("expected a codec-version failure, got: {other:?}"),
        }
    }

    /// The recovery path verifies the surviving binding *before* writing the
    /// empty side.
    ///
    /// Otherwise a foreign survivor would seed a brand-new partner bound to
    /// the requested identity, manufacturing a store that never existed.
    #[test]
    fn recovery_verifies_the_surviving_binding_first() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        std::fs::remove_file(root.join("state.redb")).expect("remove state");

        let foreign = StorageIdentity::new(
            CoordinationGroupId::from_array([0xAB; 32]),
            *identity().genesis_hash(),
            *identity().authority_public_key(),
            identity().authority_domain(),
            *identity().local_node_id(),
            identity().deployment_profile_code(),
        );
        assert!(
            matches!(
                open_at(&root, foreign),
                Err(StorageError::IdentityMismatch { .. })
            ),
            "a foreign identity must not seed a partner from someone else's survivor"
        );

        // The file itself is recreated by `Database::create` before anything
        // can be classified, so its existence proves nothing. What must not
        // have happened is a *binding* being written into it — otherwise the
        // rejected identity would have manufactured a store that never
        // existed. Proven by the real identity still finding a recoverable
        // pristine survivor rather than an initialized pair.
        let opened = open_at(&root, identity()).expect("the real identity still recovers");
        assert_eq!(
            opened.outcome(),
            OpenOutcome::RecreateState,
            "the refused open must not have bound the empty side"
        );
    }

    // ── Exact schema validation ──────────────────────────────────────────────

    /// A bound database missing a required table fails closed rather than
    /// having it recreated.
    ///
    /// Rejecting extras is only half an exact check: a lost `raft_log` used to
    /// open cleanly and read as an empty log.
    #[test]
    fn a_missing_required_table_fails_the_open() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        {
            let db = Database::create(root.join("raft.redb")).expect("open raft");
            let write = db.begin_write().expect("write");
            write.delete_table(RAFT_LOG).expect("delete log table");
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::MissingTable { table, .. }) => assert_eq!(table, "raft_log"),
            other => panic!("expected a missing-table failure, got: {other:?}"),
        }
    }

    /// A declared name carrying different redb types is a schema error, not an
    /// empty table.
    #[test]
    fn a_declared_table_with_the_wrong_types_fails_the_open() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        {
            let db = Database::create(root.join("raft.redb")).expect("open raft");
            let write = db.begin_write().expect("write");
            write.delete_table(RAFT_LOG).expect("delete log table");
            // Same name, different key and value types.
            let impostor: TableDefinition<&str, u64> = TableDefinition::new("raft_log");
            write.open_table(impostor).expect("create impostor");
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::TableTypeMismatch { table, .. }) => assert_eq!(table, "raft_log"),
            other => panic!("expected a table-type mismatch, got: {other:?}"),
        }
    }

    /// Unknown metadata is rejected even when the identity row is absent.
    ///
    /// The unbound window is precisely where a crashed first open leaves a
    /// database, so skipping validation there checks the file least when it is
    /// least trustworthy.
    #[test]
    fn unknown_metadata_is_rejected_in_an_unbound_database() {
        let root = scratch();
        std::fs::create_dir_all(&root).expect("root");
        {
            let db = Database::create(root.join("raft.redb")).expect("create raft");
            let write = db.begin_write().expect("write");
            {
                let mut meta = write.open_table(RAFT_META).expect("open meta");
                meta.insert("smuggled", b"x".as_slice()).expect("insert");
            }
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::UnknownMetaKey { key, .. }) => assert_eq!(key, "smuggled"),
            other => panic!("expected an unknown-meta-key failure, got: {other:?}"),
        }
    }

    /// An unbound side holding *allowlisted* metadata is refused too.
    ///
    /// The name check passes here, which is the whole problem: a lone
    /// `codec_version` — wrong, truncated, or simply unexplained — used to
    /// classify as unbound and then be overwritten by the recreation path,
    /// which is the opposite of failing closed. First-open metadata commits
    /// atomically, so metadata without a binding did not come from a crash of
    /// ours.
    #[test]
    fn an_unbound_database_holding_known_metadata_is_refused() {
        for value in [
            77u32.to_be_bytes().to_vec(), // a wrong codec version
            vec![0u8, 1],                 // and a truncated one
        ] {
            let root = scratch();
            {
                let db = Database::create(root.join("raft.redb")).expect("create raft");
                let write = db.begin_write().expect("write");
                {
                    let mut meta = write.open_table(RAFT_META).expect("open meta");
                    meta.insert(KEY_CODEC_VERSION, value.as_slice())
                        .expect("insert");
                }
                write.commit().expect("commit");
            }

            match open_at(&root, identity()) {
                Err(StorageError::PartialInitialization { detail }) => {
                    assert!(detail.contains("metadata row"), "got: {detail}");
                }
                other => panic!("expected partial initialization, got: {other:?}"),
            }

            // And the refusal left the value alone rather than rebinding over
            // it, which is what makes the failure diagnosable.
            let db = Database::create(root.join("raft.redb")).expect("reopen raft");
            let read = db.begin_read().expect("read");
            let stored = meta_bytes(
                &read,
                Db::Raft,
                RAFT_META,
                KEY_CODEC_VERSION,
                MAX_META_SCALAR_BYTES,
            )
            .expect("meta")
            .expect("the value survives the refusal");
            assert_eq!(stored, value, "a refused open must not overwrite metadata");
        }
    }

    /// The same rule on the state side.
    ///
    /// Both call sites share one helper, but only the Raft one was
    /// mutation-protected: removing the `state.redb` call left every test
    /// green. A production call being correct is not evidence that it stays
    /// correct.
    #[test]
    fn an_unbound_state_database_holding_known_metadata_is_refused() {
        let root = scratch();
        {
            // Bind the log side so classification actually reaches the state
            // side rather than stopping at a fresh pair.
            drop(open_at(&root, identity()).expect("bind"));
            std::fs::remove_file(root.join("state.redb")).expect("remove state");
            let db = Database::create(root.join("state.redb")).expect("create state");
            let write = db.begin_write().expect("write");
            {
                let mut meta = write.open_table(STATE_META).expect("open meta");
                meta.insert(KEY_CODEC_VERSION, 55u32.to_be_bytes().as_slice())
                    .expect("insert");
            }
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::PartialInitialization { detail }) => {
                assert!(detail.contains("state.redb"), "got: {detail}");
                assert!(detail.contains("metadata row"), "got: {detail}");
            }
            other => panic!("expected partial initialization, got: {other:?}"),
        }

        let db = Database::create(root.join("state.redb")).expect("reopen state");
        let read = db.begin_read().expect("read");
        let stored = meta_bytes(
            &read,
            Db::State,
            STATE_META,
            KEY_CODEC_VERSION,
            MAX_META_SCALAR_BYTES,
        )
        .expect("meta")
        .expect("the value survives the refusal");
        assert_eq!(
            stored,
            55u32.to_be_bytes().to_vec(),
            "a refused open must not overwrite state metadata either"
        );
    }

    /// An over-ceiling stored meta row is refused before it is copied.
    ///
    /// `state.redb`'s metadata used to be copied with `to_vec()` and decoded
    /// with no ceiling at all, so a corrupt row allocated whatever length it
    /// claimed before anything looked. The bound is now checked against the
    /// borrowed redb value.
    #[test]
    fn an_over_ceiling_stored_meta_row_is_refused() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));
        {
            let db = Database::create(root.join("state.redb")).expect("open state");
            let write = db.begin_write().expect("write");
            {
                let mut meta = write.open_table(STATE_META).expect("open meta");
                let oversized = vec![0xAAu8; MAX_COORDINATION_META_BYTES + 1];
                meta.insert(KEY_META, oversized.as_slice()).expect("insert");
            }
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::StoredCorruption { what, detail, .. }) => {
                assert_eq!(what, KEY_META);
                assert!(
                    detail.contains("ceiling"),
                    "the ceiling must be what refused it: {detail}"
                );
            }
            other => panic!("expected an over-ceiling refusal, got: {other:?}"),
        }
    }

    /// The maximal membership is one Aster can actually reach, and survives
    /// the OpenRaft round trip.
    #[test]
    fn the_widest_admitted_membership_round_trips_and_bounds_both_reachable_peaks() {
        use crate::coordination::model::shape::{
            profile_permits_voters, MAX_LEARNER_ENVELOPE, MAX_NEW_VOTERS, MAX_VOTERS, PROFILE_PROD,
        };
        use crate::coordination::typeconfig::{CoordinationNode, CoordinationTypeConfig};
        use openraft::Membership;
        use std::collections::BTreeSet;

        // The two shapes a single transition actually reaches, stated before
        // the fixture is used so the gap between "admitted" and "reachable" is
        // written down rather than implied. The fixture combines both peaks --
        // seven voters *and* six learners -- which no transition does: the two
        // new voters are learners in the first and voters in the second.
        let before_joint = MAX_VOTERS + MAX_LEARNER_ENVELOPE;
        let during_joint = (MAX_VOTERS + MAX_NEW_VOTERS) + (MAX_LEARNER_ENVELOPE - MAX_NEW_VOTERS);
        assert_eq!(
            before_joint, during_joint,
            "both reachable peaks are the same node count; the new voters change role, \
             not the total"
        );

        let mirror = widest_admitted_membership();
        let ids = |set: &crate::coordination::raft::NodeIdSetV1| -> BTreeSet<Vec<u8>> {
            set.ids.iter().map(|i| i.node_id.clone()).collect()
        };

        let old = ids(&mirror.configs[0]);
        let new = ids(&mirror.configs[1]);

        // Aster legality, which is the claim the old fixture could not make.
        //
        // `profile_permits_voters` returns `true` when the count is *not*
        // permitted -- it matches the `Option` idiom of the other shape
        // checks, so it reads backwards here. Written the wrong way round
        // first, and caught only because the fixture is legal.
        assert!(!profile_permits_voters(PROFILE_PROD, old.len()));
        assert!(!profile_permits_voters(PROFILE_PROD, new.len()));
        assert_eq!(old.len(), MAX_VOTERS);
        assert_eq!(
            new.difference(&old).count(),
            MAX_NEW_VOTERS,
            "a single step may introduce at most MAX_NEW_VOTERS voters"
        );

        // The node map covers every voter, and its remainder is learners
        // within the retention cap. Not an exact union: OpenRaft represents
        // learners as nodes outside every config, so requiring equality would
        // have excluded them and understated the maximum.
        let nodes: BTreeSet<Vec<u8>> = mirror.nodes.iter().map(|n| n.node_id.clone()).collect();
        let voters: BTreeSet<Vec<u8>> = old.union(&new).cloned().collect();
        assert!(
            voters.is_subset(&nodes),
            "every voter must appear in the node map"
        );
        let learners: BTreeSet<Vec<u8>> = nodes.difference(&voters).cloned().collect();
        assert_eq!(
            learners.len(),
            MAX_LEARNER_ENVELOPE,
            "the maximum carries the whole §9 learner envelope outside the configs"
        );
        assert!(
            nodes.len() > before_joint,
            "the sizing fixture must be a strict superset of what a transition reaches \
             ({} nodes against {before_joint}), or the ceiling is measured against the \
             narrower shape",
            nodes.len()
        );

        // And it is a real OpenRaft membership, through the same mirror
        // conversion storage uses.
        let _ = std::marker::PhantomData::<CoordinationTypeConfig>;
        let membership: Membership<CoordinationNodeId, CoordinationNode> = (&mirror)
            .try_into()
            .expect("the maximal membership converts");
        let back = crate::coordination::raft::MembershipV1::from(&membership);
        let round_tripped: BTreeSet<Vec<u8>> =
            back.nodes.iter().map(|n| n.node_id.clone()).collect();
        assert_eq!(
            round_tripped, nodes,
            "the node map survives the round trip storage performs"
        );
    }

    /// The ceiling admits only rows that can still be snapshotted, and rejects
    /// no legal row.
    ///
    /// Both directions matter, and neither is the constant equality this used
    /// to assert. `MAX_COORDINATION_META_BYTES == MAX_HEADER_BYTES` proved
    /// nothing: `SnapshotHeaderV1` wraps the meta in two version fields, seven
    /// row counts and Fory envelope, so a row at exactly the header bound
    /// yields a header *over* it.
    ///
    /// The two halves use different fixtures on purpose. The lower bound —
    /// "does not reject legal state" — must use genuinely legal state, built
    /// from the declared caps, or it proves nothing about a real voter. The
    /// envelope is measured against a deliberately oversized meta so the
    /// wrapper allowance cannot be understated.
    #[test]
    fn the_meta_ceiling_admits_only_snapshottable_rows() {
        use crate::coordination::contract::MAX_WIRE_STRING;
        use crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES;
        use crate::coordination::model::limits::{GROWTH_BUDGET, RESERVE_BUDGET};
        use crate::coordination::model::shape::PROFILE_PROD;
        use crate::coordination::rowstream::{
            RowCountsV1, SnapshotHeaderV1, MAX_HEADER_BYTES, SNAPSHOT_FORMAT_VERSION,
        };

        // ── The fixture is still maximal, and still legal ────────────────────
        //
        // Every assertion below is an upper bound, so a token row satisfies
        // all of them: shrinking a field would leave this green while proving
        // nothing. Checked against the declared caps, not literals.
        let legal = maximal_legal_meta();
        assert_eq!(legal.authority_domain.len(), MAX_AUTHORITY_DOMAIN_BYTES);
        assert!(legal.authority_domain.is_ascii());
        assert_eq!(legal.schema_version, LOGICAL_SCHEMA_VERSION);
        assert_eq!(legal.deployment_profile_code, PROFILE_PROD);
        assert_eq!(legal.aggregate_growth_bytes, GROWTH_BUDGET);
        assert_eq!(legal.aggregate_reserve_bytes, RESERVE_BUDGET);
        // Demonstrated by *use*, not by parsing. `VerifyingKey::from_bytes`
        // does decompress immediately -- it is a real check -- but `[0xFF; 32]`
        // happens to decode to a point, so `is_ok()` passed against it. The
        // property that matters is not "these bytes parse" but "these bytes
        // correspond to the signer", and only a verification shows that.
        {
            use ed25519_dalek::{Signer, Verifier};

            let signing = crate::coordination::model::authority::testing::signing_key();
            let signature = signing.sign(b"aster-coordination/fixture-probe");
            let verifying = ed25519_dalek::VerifyingKey::from_bytes(
                &legal
                    .authority_public_key
                    .clone()
                    .try_into()
                    .expect("32 bytes"),
            )
            .expect("decode");
            verifying
                .verify(b"aster-coordination/fixture-probe", &signature)
                .expect("the authority key must actually verify what its signer produced");
        }
        assert!(
            legal.last_applied.is_some()
                && legal.audit_floor.is_some()
                && legal.recovery_export_hash.is_some()
                && legal.recovered_from.is_some(),
            "every optional must be present in the maximal case"
        );

        let encoded_legal = payload_fory::<CoordinationMetaV1>()
            .serialize(&legal)
            .expect("encode the maximal legal meta");

        // 1. The ceiling does not reject legal state. A voter that refused its
        //    own maximal membership would fail to start rather than fail to
        //    snapshot.
        assert!(
            encoded_legal.len() <= MAX_COORDINATION_META_BYTES,
            "the maximal legal meta encodes to {} bytes, over the \
             {MAX_COORDINATION_META_BYTES}-byte ceiling",
            encoded_legal.len()
        );

        // 2. The maximal legal header fits, directly. Its format version is
        //    the snapshot body format, not the storage codec version.
        //
        //    Those are different compatibility concerns that happen to share
        //    the value 1 today, which is exactly why substituting one for the
        //    other read as correct -- and why no test here can catch the
        //    substitution: the constants are equal, so a mutation swapping
        //    them is a no-op. Verified. The guard that can catch it belongs
        //    with Checkpoint D's production header construction, where there
        //    is a call site to constrain.
        let legal_header = SnapshotHeaderV1 {
            format_version: SNAPSHOT_FORMAT_VERSION,
            schema_version: LOGICAL_SCHEMA_VERSION,
            meta: legal,
            row_counts: maximal_legal_row_counts(),
        };
        let encoded_legal_header = payload_fory::<SnapshotHeaderV1>()
            .serialize(&legal_header)
            .expect("encode the maximal legal header");
        assert!(
            (encoded_legal_header.len() as u64) <= MAX_HEADER_BYTES,
            "the maximal legal header encodes to {} bytes, over the \
             {MAX_HEADER_BYTES}-byte bound",
            encoded_legal_header.len()
        );

        // ── The envelope, measured conservatively ────────────────────────────
        let oversized = oversized_meta_for_envelope();
        assert_eq!(oversized.authority_domain.len(), MAX_WIRE_STRING);
        let encoded_oversized = payload_fory::<CoordinationMetaV1>()
            .serialize(&oversized)
            .expect("encode");
        let oversized_header = SnapshotHeaderV1 {
            format_version: u32::MAX,
            schema_version: u32::MAX,
            meta: oversized,
            row_counts: RowCountsV1 {
                resources: u64::MAX,
                grants: u64::MAX,
                admins: u64::MAX,
                sessions: u64::MAX,
                intents: u64::MAX,
                routes: u64::MAX,
                audit: u64::MAX,
            },
        };
        let encoded_oversized_header = payload_fory::<SnapshotHeaderV1>()
            .serialize(&oversized_header)
            .expect("encode");
        let envelope = encoded_oversized_header
            .len()
            .checked_sub(encoded_oversized.len())
            .expect("the header cannot be smaller than the meta it embeds");

        // 3. The invariant that actually constrains the ceiling: anything it
        //    admits must still fit once wrapped. This is what fails if the
        //    ceiling is raised toward the header bound.
        assert!(
            (MAX_COORDINATION_META_BYTES + envelope) as u64 <= MAX_HEADER_BYTES,
            "a meta row at the {MAX_COORDINATION_META_BYTES}-byte ceiling plus a \
             {envelope}-byte header envelope exceeds the {MAX_HEADER_BYTES}-byte \
             header bound: such a row could be accepted from disk but never snapshotted"
        );
    }

    /// A wrong-typed metadata table is a schema error, not a generic redb
    /// failure.
    ///
    /// "This file is not ours" and "this file is damaged" send an operator to
    /// different procedures, so they must not arrive as the same error.
    #[test]
    fn a_wrong_typed_meta_table_reports_a_type_mismatch() {
        let root = scratch();
        {
            let db = Database::create(root.join("raft.redb")).expect("create raft");
            let write = db.begin_write().expect("write");
            let impostor: TableDefinition<u64, u64> = TableDefinition::new("raft_meta");
            write.open_table(impostor).expect("create impostor");
            write.commit().expect("commit");
        }

        match open_at(&root, identity()) {
            Err(StorageError::TableTypeMismatch { table, .. }) => assert_eq!(table, "raft_meta"),
            other => panic!("expected a table-type mismatch, got: {other:?}"),
        }
    }

    /// A failed open leaves nothing a voter could start from.
    ///
    /// The specific hazard: an open that creates and binds `raft.redb`, then
    /// fails on `state.redb`, must not leave a directory that looks bound on
    /// the next attempt. Here the second open fails on identity, and a third
    /// open with the *original* identity must still succeed unchanged — the
    /// failed attempt wrote nothing.
    #[test]
    fn a_failed_open_never_half_initializes_a_usable_voter() {
        let root = scratch();
        drop(open_at(&root, identity()).expect("bind"));

        let wrong = StorageIdentity::new(
            CoordinationGroupId::from_array([0xEE; 32]),
            *identity().genesis_hash(),
            *identity().authority_public_key(),
            identity().authority_domain(),
            *identity().local_node_id(),
            identity().deployment_profile_code(),
        );
        assert!(
            open_at(&root, wrong).is_err(),
            "a foreign group must not open"
        );

        let reopened = open_at(&root, identity()).expect("the original identity still opens");
        assert!(reopened.was_initialized());

        // And the failed attempt did not leave a lock behind.
        drop(reopened);
        open_at(&root, identity()).expect("no lock leaked from the failed open");
    }

    // ── Filesystem policy ────────────────────────────────────────────────────

    /// A symlinked database path is refused rather than followed: the
    /// permission and lock checks describe this directory, and a symlink moves
    /// the bytes somewhere they never looked.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_database_path_is_refused() {
        let root = scratch();
        let elsewhere = scratch();
        let target = elsewhere.join("real.redb");
        std::fs::write(&target, b"").expect("create target");
        std::os::unix::fs::symlink(&target, root.join("raft.redb")).expect("symlink");

        match open_at(&root, identity()) {
            Err(StorageError::SymlinkedPath { path }) => {
                assert!(path.ends_with("raft.redb"), "got: {}", path.display());
            }
            other => panic!("expected a symlink refusal, got: {other:?}"),
        }
    }

    /// A group- or world-writable storage directory fails a production open,
    /// and is accepted only under the explicit development-only override.
    #[cfg(unix)]
    #[test]
    fn broad_permissions_fail_production_but_pass_the_explicit_override() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777))
            .expect("widen permissions");

        let err = open(
            StorageLayout::new(&root),
            identity(),
            OpenPolicy::production(),
        )
        .expect_err("a world-writable storage directory must fail production startup");
        assert!(
            matches!(err, StorageError::BroadPermissions { .. }),
            "got: {err:?}"
        );

        open(
            StorageLayout::new(&root),
            identity(),
            OpenPolicy::production().allowing_broad_permissions(),
        )
        .expect("the development-only override accepts it");
    }

    /// A pre-existing broadly writable `LOCK` is refused, not repaired.
    ///
    /// Chmodding it would fix the symptom, let startup continue, and erase the
    /// evidence that something outside this voter could write the file its
    /// mutual exclusion depends on. Production policy refuses; the explicit
    /// development override still accepts.
    #[cfg(unix)]
    #[test]
    fn an_existing_world_writable_lock_is_refused_not_chmodded() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let lock = root.join("LOCK");
        std::fs::write(&lock, b"").expect("create lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666))
            .expect("widen lock");

        let err = open_at(&root, identity()).expect_err("a writable LOCK must fail production");
        assert!(
            matches!(err, StorageError::BroadPermissions { .. }),
            "got: {err:?}"
        );

        let mode = std::fs::metadata(&lock)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o666,
            "the refusal must leave the evidence in place, not silently repair it"
        );

        open(
            StorageLayout::new(&root),
            identity(),
            OpenPolicy::production().allowing_broad_permissions(),
        )
        .expect("the development-only override accepts it");
    }

    /// New directories and files are owner-only.
    #[cfg(unix)]
    #[test]
    fn a_fresh_layout_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        // The scratch helper creates the root with the process umask, so
        // check the paths this module itself creates.
        let opened = open_at(&root, identity()).expect("open");
        for path in [opened.layout().snapshots(), opened.layout().incoming()] {
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode,
                DIR_MODE,
                "{} should be 0700, is {mode:04o}",
                path.display()
            );
        }
        let lock_mode = std::fs::metadata(opened.layout().lock())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            lock_mode, FILE_MODE,
            "LOCK should be 0600, is {lock_mode:04o}"
        );
    }

    // ── Test helpers ─────────────────────────────────────────────────────────

    /// Give `state.redb` a `last_applied`, making it durable rather than
    /// pristine. Written through the real Fory root so the durability test has
    /// to decode the row rather than pattern-match bytes.
    fn advance_state_last_applied(root: &Path) {
        let db = Database::create(root.join("state.redb")).expect("open state");
        let bytes = {
            let read = db.begin_read().expect("read");
            meta_bytes(
                &read,
                Db::State,
                STATE_META,
                KEY_META,
                MAX_COORDINATION_META_BYTES,
            )
            .expect("meta")
            .expect("meta row present")
        };
        let mut meta: CoordinationMetaV1 = payload_fory::<CoordinationMetaV1>()
            .deserialize(&bytes)
            .expect("decode");
        meta.last_applied = Some(crate::coordination::contract::LogIdV1 {
            term: 1,
            index: 1,
            leader_node_id: vec![4u8; 32],
        });
        let encoded = payload_fory::<CoordinationMetaV1>()
            .serialize(&meta)
            .expect("encode");

        let write = db.begin_write().expect("write");
        {
            let mut table = write.open_table(STATE_META).expect("open meta");
            table
                .insert(KEY_META, encoded.as_slice())
                .expect("insert meta");
        }
        write.commit().expect("commit");
    }

    /// Write a raw value into `raft_meta`, bypassing the open coordinator.
    ///
    /// Deliberately not going through this module's own writer: a corruption
    /// test that can only produce states the writer produces proves nothing
    /// about states a crash or a foreign build can produce.
    fn overwrite_raft_meta(root: &Path, key: &str, value: &[u8]) {
        let db = Database::create(root.join("raft.redb")).expect("open raft");
        let write = db.begin_write().expect("write");
        {
            let mut meta = write.open_table(RAFT_META).expect("open meta");
            meta.insert(key, value).expect("insert");
        }
        write.commit().expect("commit");
    }
}
