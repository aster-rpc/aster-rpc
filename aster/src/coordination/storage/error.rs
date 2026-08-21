//! Named storage failures (§11.1).
//!
//! Every variant here is a *distinct* named failure, and that is the whole
//! point of the type. §11.1 requires several outcomes that a coarser error
//! would blur together into "storage did not open", after which an operator
//! cannot tell a second running voter from an unreadable disk, or an expected
//! crash repair from real corruption:
//!
//! - lock contention vs. an I/O error while locking;
//! - an accepted unclean-shutdown recovery vs. a fatal unclean result on a
//!   database this process just created;
//! - a genuinely fresh directory vs. a partially initialized one.
//!
//! Crate-private. Slice 3 owns the public start configuration and decides how
//! these surface to a caller; inventing that mapping here would fix a public
//! contract this slice cannot verify.

// Scoped to this module deliberately -- see the `storage` module docs for why
// a crate-root allowance would be wrong, and why these come off when Slice 3
// wires the voter runtime rather than at any Slice 2 checkpoint.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Which database a failure came from. Both files carry overlapping identity
/// metadata, so an error naming neither is ambiguous exactly when it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Db {
    /// `raft.redb` — vote, log, committed and purge markers.
    Raft,
    /// `state.redb` — the replicated state machine.
    State,
}

impl Db {
    /// The on-disk file name, which is also how the plan refers to it.
    pub(crate) fn file_name(self) -> &'static str {
        match self {
            Db::Raft => "raft.redb",
            Db::State => "state.redb",
        }
    }
}

impl std::fmt::Display for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.file_name())
    }
}

/// Which identity field disagreed. Held as a `&'static str` rather than an
/// enum because the set is fixed by §11.1's binding and every one of them is
/// fatal in the same way; the name exists for the operator, not for control
/// flow.
pub(crate) type IdentityField = &'static str;

/// A storage-layer failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StorageError {
    /// Another cooperating process holds the directory lock.
    ///
    /// This is `TryLockError::WouldBlock` and nothing else. It is the only
    /// result that means "a second voter is already running here", so it must
    /// never absorb an I/O failure — an operator who reads this and kills the
    /// other process would otherwise be chasing a phantom.
    #[error(
        "coordination storage at {path} is locked by another process; \
         only one voter may own a storage directory at a time"
    )]
    LockContended { path: PathBuf },

    /// Opening or locking `LOCK` failed for a reason that is not contention.
    #[error("could not acquire the coordination storage lock at {path}: {source}")]
    LockIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A filesystem operation failed. `what` names the operation so the
    /// message says which step failed rather than only which path.
    #[error("coordination storage: {what} failed for {path}: {source}")]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A database path is a symlink.
    ///
    /// Refused rather than followed: the directory permission and lock checks
    /// describe the directory this voter was pointed at, and a symlink moves
    /// the actual bytes somewhere those checks never looked.
    #[error(
        "coordination storage path {path} is a symbolic link; \
         the database files must live in the storage directory itself"
    )]
    SymlinkedPath { path: PathBuf },

    /// An existing path is writable beyond the owner in a production open.
    ///
    /// Refused rather than repaired. Tightening the mode here would fix the
    /// symptom, let startup continue, and erase the evidence that something
    /// outside this voter could write the file — which for `LOCK` is the file
    /// its mutual exclusion depends on.
    #[error(
        "coordination storage path {path} has mode {mode:04o}, which grants \
         write access beyond its owner; production refuses it rather than \
         changing it. New directories are created 0700 and new files 0600"
    )]
    BroadPermissions { path: PathBuf, mode: u32 },

    /// `check_integrity` reported an unclean database that this process had
    /// just created — including the database built during a snapshot install.
    ///
    /// Distinct from the accepted recovery path: a file with no prior process
    /// death behind it has nothing to recover *from*, so an unclean result is
    /// evidence about the storage stack, not about the last shutdown.
    #[error(
        "coordination storage {db} was created by this process but redb reports \
         it as unclean; this is a storage fault, not crash recovery"
    )]
    FreshDatabaseUnclean { db: Db },

    /// A transaction or database handle outlived the integrity check.
    ///
    /// A lifecycle invariant failure in this crate rather than a disk problem:
    /// `check_integrity` takes `&mut Database`, so reaching this means the
    /// open coordinator published a handle before checking.
    #[error(
        "coordination storage {db}: integrity check ran while a transaction or \
         database handle was still alive"
    )]
    TransactionInProgress { db: Db },

    /// redb reported a corrupt or unreadable database.
    #[error("coordination storage {db} failed its integrity check: {source}")]
    Integrity {
        db: Db,
        #[source]
        source: redb::DatabaseError,
    },

    /// A table exists that schema v1 does not define.
    ///
    /// Never dropped or ignored: an unknown table is either a newer schema
    /// this build cannot honour or a file that is not ours, and both are
    /// data-loss risks if opened anyway.
    #[error(
        "coordination storage {db} contains table {table:?}, which schema v1 \
         does not define"
    )]
    UnknownTable { db: Db, table: String },

    /// An initialized database is missing a table schema v1 declares.
    ///
    /// Never created as an implicit repair. A bound database that has lost a
    /// table has lost whatever was in it, and silently recreating it turns
    /// data loss into an empty table that reads as legitimate.
    #[error(
        "coordination storage {db} is initialized but has no {table:?} table; \
         schema v1 requires it and startup does not create it as a repair"
    )]
    MissingTable { db: Db, table: String },

    /// A table exists under a declared name but with different key or value
    /// types than schema v1 declares.
    ///
    /// A name-only check would accept this. The rows are then unreadable
    /// through the declared definition, which is a schema error rather than an
    /// empty table.
    #[error(
        "coordination storage {db} table {table:?} has key/value types this \
         build does not declare ({detail})"
    )]
    TableTypeMismatch {
        db: Db,
        table: String,
        detail: String,
    },

    /// A metadata key exists that schema v1 does not define.
    #[error(
        "coordination storage {db} contains metadata key {key:?}, which schema \
         v1 does not define"
    )]
    UnknownMetaKey { db: Db, key: String },

    /// A required metadata key is absent from an initialized database.
    #[error("coordination storage {db} is initialized but has no {key:?} metadata")]
    MissingMetaKey { db: Db, key: &'static str },

    /// The stored logical schema version is not the one this build implements.
    ///
    /// Separate from a redb format mismatch: a redb major upgrade can change
    /// the file format with no logical change, and a logical migration can
    /// happen with the format unchanged. Reporting one as the other sends an
    /// operator to the wrong procedure.
    #[error(
        "coordination storage {db} has logical schema version {found}, but this \
         build implements version {expected}"
    )]
    SchemaVersion { db: Db, found: u32, expected: u32 },

    /// The stored storage-codec version is not the one this build implements.
    #[error(
        "coordination storage {db} has storage codec version {found}, but this \
         build implements version {expected}"
    )]
    CodecVersion { db: Db, found: u32, expected: u32 },

    /// Stored identity disagrees with the identity this voter was opened with,
    /// or the two databases disagree with each other.
    #[error(
        "coordination storage {db}: stored {field} does not match the identity \
         this voter was opened with"
    )]
    IdentityMismatch { db: Db, field: IdentityField },

    /// One database is initialized and the other is not, and the absent side
    /// cannot be proven to have never held durable state.
    ///
    /// Fails closed. Recreating the empty side here would silently discard
    /// whatever the surviving side already promised.
    #[error(
        "coordination storage is partially initialized: {detail}. \
         Refusing to start; recreating the missing side could discard \
         acknowledged state"
    )]
    PartialInitialization { detail: String },

    /// An append would leave a hole in the log, or the batch itself has one.
    ///
    /// Raft's correctness argument examines only the last log id, so a hole is
    /// not a local inconsistency to be repaired later — it makes every
    /// subsequent "up to date" conclusion wrong. The batch is refused whole;
    /// §4.3 requires that nothing be partially written.
    #[error("coordination log append expected index {expected} but found {found}: {detail}")]
    LogGap {
        expected: u64,
        found: u64,
        detail: &'static str,
    },

    /// An install was offered a snapshot this voter has already applied past.
    ///
    /// **Fatal**, despite reading like an ordinary race. It was classified as a
    /// benign refusal first, on the reasoning that a leader had simply sent a
    /// snapshot built before this follower caught up. That reasoning does not
    /// survive contact with OpenRaft: it filters snapshots at or below
    /// `committed` before it calls the state machine at all, so reaching this
    /// branch means the durable state and the state OpenRaft believes this node
    /// has are already different things -- which is the disagreement, not a
    /// symptom of one.
    ///
    /// The nonfatal projection was unreachable in any case. Any error out of
    /// `install_snapshot` terminates OpenRaft's state-machine worker, so
    /// "refused but still serving" described nothing that could happen; the
    /// test asserting it observed only that Aster's own latch was clear.
    #[error("coordination snapshot install refused: {detail}")]
    SnapshotBehind { detail: String },

    /// The storage actor is gone, so nothing can be read or written.
    #[error(
        "the coordination storage actor for {db} has stopped; \
         this store can no longer make durability claims"
    )]
    ActorStopped { db: Db },

    /// A durability or callback-path failure already happened.
    ///
    /// Sticky by design. Once a write has failed, this store cannot say what
    /// is durable, so continuing would let a later vote be acknowledged on top
    /// of a log write that never landed.
    #[error(
        "coordination storage is in a fatal state after an earlier durability \
         failure and will not accept further operations"
    )]
    Fatal,

    /// A value already on disk did not decode, or decoded to something the
    /// log's own invariants reject.
    ///
    /// Deliberately not the same variant as an encode failure. The two are
    /// both "the codec said no", but one is a bad request that wrote nothing
    /// and the other is durable state this build can no longer read — and only
    /// the second means the store has stopped knowing what it holds. Sharing a
    /// variant made a corrupt purge marker retryable.
    #[error("coordination storage {db}: stored {what} is unusable: {detail}")]
    StoredCorruption {
        db: Db,
        what: &'static str,
        detail: String,
    },

    /// The storage actor thread could not be started.
    #[error("could not start the coordination storage actor for {db}: {source}")]
    ActorSpawn {
        db: Db,
        #[source]
        source: std::io::Error,
    },

    /// A value the caller supplied could not be encoded, or was invalid.
    ///
    /// Named for the direction on purpose. This variant is the *only*
    /// retryable failure besides [`LogGap`](StorageError::LogGap), and the
    /// previous name -- `Codec` -- read as "something to do with the codec",
    /// which is exactly how persisted-value decodes ended up classified as
    /// retryable. A decode cannot plausibly be an `EncodeFailed`.
    #[error("coordination storage {db}: could not encode {what}: {detail}")]
    EncodeFailed {
        db: Db,
        what: &'static str,
        detail: String,
    },

    /// Any other redb failure, carried rather than flattened.
    #[error("coordination storage {db}: {source}")]
    Redb {
        db: Db,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl StorageError {
    /// Build an [`StorageError::Io`] without repeating the path conversion.
    pub(crate) fn io(what: &'static str, path: impl AsRef<Path>, source: std::io::Error) -> Self {
        StorageError::Io {
            what,
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// A failure encoding a value the caller supplied. Retryable.
    ///
    /// Called only by
    /// [`codec::encode_owned`](super::codec::encode_owned), which is the one
    /// place this classification can be applied; see that module for why the
    /// direction is a function choice rather than a constructor choice.
    pub(crate) fn encode_failed(
        db: Db,
        what: &'static str,
        detail: impl std::fmt::Display,
    ) -> Self {
        StorageError::EncodeFailed {
            db,
            what,
            detail: detail.to_string(),
        }
    }

    /// A failure reading a value already on disk. Fatal.
    pub(crate) fn corrupt(db: Db, what: &'static str, detail: impl std::fmt::Display) -> Self {
        StorageError::StoredCorruption {
            db,
            what,
            detail: detail.to_string(),
        }
    }

    /// Whether this failure leaves the store unable to say what is durable.
    ///
    /// The distinction decides whether a failure is *sticky*, and it is a
    /// narrow allowlist rather than a denylist on purpose: a variant added
    /// later is fatal until someone proves it is not, which is the safe
    /// direction to be wrong in.
    ///
    /// Only two things are retryable. A [`LogGap`](StorageError::LogGap) is
    /// refused before any transaction opens, and
    /// [`EncodeFailed`](StorageError::EncodeFailed) is raised only while *encoding* a value
    /// the caller supplied — also before the transaction. Both leave the store
    /// untouched, and OpenRaft may legitimately follow either with a correct
    /// request. Making the first legal refusal permanently disable a healthy
    /// voter would be far worse than the gap it refused.
    ///
    /// Everything else is sticky, including
    /// [`StoredCorruption`](StorageError::StoredCorruption): a stored value
    /// that will not decode is not a bad request to retry, it is durable state
    /// this build can no longer interpret.
    pub(crate) fn puts_durability_in_question(&self) -> bool {
        !matches!(
            self,
            StorageError::LogGap { .. } | StorageError::EncodeFailed { .. }
        )
    }

    /// Wrap a redb error that has no more specific variant.
    pub(crate) fn redb(db: Db, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        StorageError::Redb {
            db,
            source: Box::new(source),
        }
    }
}

pub(crate) type StorageResult<T> = std::result::Result<T, StorageError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The two lock failures are separate variants with separate messages.
    /// Asserted because collapsing them is the natural "simplification" and it
    /// destroys the one signal that identifies a second running voter.
    #[test]
    fn lock_contention_and_lock_io_are_distinguishable() {
        let contended = StorageError::LockContended {
            path: PathBuf::from("/tmp/x/LOCK"),
        };
        let io = StorageError::LockIo {
            path: PathBuf::from("/tmp/x/LOCK"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };

        assert!(matches!(contended, StorageError::LockContended { .. }));
        assert!(matches!(io, StorageError::LockIo { .. }));
        assert!(
            contended.to_string().contains("locked by another process"),
            "contention must say so plainly, got: {contended}"
        );
        assert!(
            !io.to_string().contains("locked by another process"),
            "an I/O failure must not be reported as another voter, got: {io}"
        );
    }

    /// `LockIo` keeps its `io::Error` reachable through `source()`.
    #[test]
    fn lock_io_preserves_its_source() {
        let err = StorageError::LockIo {
            path: PathBuf::from("/tmp/x/LOCK"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let source = std::error::Error::source(&err).expect("a source is preserved");
        let io = source
            .downcast_ref::<std::io::Error>()
            .expect("the source is the original io::Error");
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    }

    /// Schema and codec version mismatches are separate variants: §11.2 keeps
    /// the logical schema and the physical/codec concerns independent, and an
    /// operator's next step differs between them.
    #[test]
    fn schema_and_codec_version_failures_are_separate() {
        let schema = StorageError::SchemaVersion {
            db: Db::State,
            found: 2,
            expected: 1,
        };
        let codec = StorageError::CodecVersion {
            db: Db::State,
            found: 2,
            expected: 1,
        };
        assert!(schema.to_string().contains("logical schema version"));
        assert!(codec.to_string().contains("storage codec version"));
    }

    /// Every message names the database it came from, since both files carry
    /// overlapping identity metadata.
    #[test]
    fn database_named_failures_say_which_file() {
        let raft = StorageError::UnknownTable {
            db: Db::Raft,
            table: "surprise".into(),
        };
        let state = StorageError::UnknownTable {
            db: Db::State,
            table: "surprise".into(),
        };
        assert!(raft.to_string().contains("raft.redb"), "got: {raft}");
        assert!(state.to_string().contains("state.redb"), "got: {state}");
    }
}
