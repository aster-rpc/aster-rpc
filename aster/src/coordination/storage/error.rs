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
    #[error(
        "coordination storage path {path} has mode {mode:04o}, which grants \
         write access beyond its owner; production requires 0700 directories \
         and 0600 files"
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

    /// A stored value did not decode, or decoded to something invalid.
    #[error("coordination storage {db}: could not decode {what}: {detail}")]
    Codec {
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

    /// Build a [`StorageError::Codec`] from anything displayable.
    pub(crate) fn codec(db: Db, what: &'static str, detail: impl std::fmt::Display) -> Self {
        StorageError::Codec {
            db,
            what,
            detail: detail.to_string(),
        }
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
