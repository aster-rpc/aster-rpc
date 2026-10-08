//! Shared local durable-storage primitives. Callers retain their own policy and errors.

use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub(crate) enum StorageError {
    #[error("storage directory is locked: {path}")]
    LockContended { path: PathBuf },
    #[error("storage lock failed at {path}: {source}")]
    LockIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{what} failed at {path}: {source}")]
    Io {
        what: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("storage path is a symlink: {path}")]
    SymlinkedPath { path: PathBuf },
    #[error("storage path {path} has writable permissions {mode:04o}")]
    BroadPermissions { path: PathBuf, mode: u32 },
}

impl StorageError {
    fn io(what: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            what,
            path: path.to_owned(),
            source,
        }
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
    /// only result that means another owner holds this directory; the standard
    /// library guarantees it never hides a `WouldBlock` inside
    /// `TryLockError::Error`, so the split is exact rather than heuristic.
    pub(crate) fn acquire(
        path: impl Into<PathBuf>,
        enforce_permissions: bool,
    ) -> Result<Self, StorageError> {
        let path = path.into();
        reject_symlink(&path)?;

        // Whether the lock file already existed decides what happens to its
        // permissions, and the two are not interchangeable. A pre-existing
        // group-writable `LOCK` is a production policy violation to *refuse*;
        // chmodding it would repair the symptom, let startup continue, and
        // erase the evidence that something else could write the file the
        // owner’s mutual exclusion depends on.
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
            check_permissions(&path, enforce_permissions)?;
        } else {
            restrict_permissions(&path, FILE_MODE)?;
        }

        match file.try_lock() {
            Ok(()) => Ok(Self { path, file }),
            Err(failure) => Err(classify_lock_failure(path, failure)),
        }
    }

    #[cfg(any(feature = "coordination", test))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Release the lock explicitly during an orderly shutdown.
    pub(crate) fn unlock(self) -> Result<(), StorageError> {
        self.file.unlock().map_err(|source| StorageError::LockIo {
            path: self.path.clone(),
            source,
        })
    }
}

/// Map a `try_lock` failure to its named storage failure.
///
/// A pure function, for the same reason the integrity classifier is one: a real
/// `TryLockError::Error` needs a filesystem that fails the lock syscall while
/// still opening the file, which is not something a test can arrange on a
/// normal volume. Fused into [`DirectoryLock::acquire`], the `Error` arm had
/// no test at all — an earlier version of this module collapsed both arms into
/// contention and every lock test still passed, because the only I/O failure
/// being exercised happened at `File::open`, one step earlier.
pub(crate) fn classify_lock_failure(path: PathBuf, failure: std::fs::TryLockError) -> StorageError {
    match failure {
        std::fs::TryLockError::WouldBlock => StorageError::LockContended { path },
        std::fs::TryLockError::Error(source) => StorageError::LockIo { path, source },
    }
}

// ── Permissions ──────────────────────────────────────────────────────────────

pub(crate) const DIR_MODE: u32 = 0o700;
pub(crate) const FILE_MODE: u32 = 0o600;

/// Set owner-only permissions on a path we just created. A no-op off Unix,
/// where storage relies on the application directory's inherited user ACL and a
/// platform test/runbook instead.
#[cfg(unix)]
pub(crate) fn restrict_permissions(path: &Path, mode: u32) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|source| StorageError::io("set permissions", path, source))
}

#[cfg(not(unix))]
pub(crate) fn restrict_permissions(_path: &Path, _mode: u32) -> Result<(), StorageError> {
    Ok(())
}

/// Refuse an existing path that is writable beyond its owner.
#[cfg(unix)]
pub(crate) fn check_permissions(
    path: &Path,
    enforce_permissions: bool,
) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;
    if !enforce_permissions {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|source| StorageError::io("read metadata", path, source))?;
    let mode = meta.permissions().mode() & 0o777;
    // Group- or other-writable. Read access is not the concern here: §11.1
    // provides no at-rest encryption, but another writer can
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
pub(crate) fn check_permissions(
    _path: &Path,
    _enforce_permissions: bool,
) -> Result<(), StorageError> {
    Ok(())
}

/// Refuse a symlinked path.
pub(crate) fn reject_symlink(path: &Path) -> Result<(), StorageError> {
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
pub(crate) fn ensure_directory(path: &Path, enforce_permissions: bool) -> Result<(), StorageError> {
    reject_symlink(path)?;
    if path.exists() {
        check_permissions(path, enforce_permissions)?;
        return Ok(());
    }
    std::fs::create_dir_all(path)
        .map_err(|source| StorageError::io("create directory", path, source))?;
    restrict_permissions(path, DIR_MODE)
}

/// The only production write-transaction constructor for local durable stores.
pub(crate) fn durable_write(
    database: &redb::Database,
) -> Result<redb::WriteTransaction, redb::Error> {
    let mut write = database.begin_write()?;
    write.set_durability(redb::Durability::Immediate)?;
    Ok(write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::ReadableDatabase;

    #[test]
    fn exclusive_lock_is_released_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("LOCK");
        let owner = DirectoryLock::acquire(&path, true).unwrap();
        assert_eq!(owner.path(), path);
        assert!(matches!(
            DirectoryLock::acquire(&path, true),
            Err(StorageError::LockContended { .. })
        ));
        drop(owner);
        DirectoryLock::acquire(&path, true)
            .unwrap()
            .unlock()
            .unwrap();
    }

    #[test]
    fn lock_io_is_not_reported_as_contention() {
        let path = PathBuf::from("LOCK");
        let error = classify_lock_failure(
            path,
            std::fs::TryLockError::Error(std::io::Error::other("injected")),
        );
        assert!(matches!(error, StorageError::LockIo { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_writable_lock_without_repairing_it() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"evidence").unwrap();
        let path = directory.path().join("LOCK");
        symlink(&target, &path).unwrap();
        assert!(matches!(
            DirectoryLock::acquire(&path, true),
            Err(StorageError::SymlinkedPath { .. })
        ));
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"evidence").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            DirectoryLock::acquire(&path, true),
            Err(StorageError::BroadPermissions { .. })
        ));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o666
        );
        assert_eq!(std::fs::read(path).unwrap(), b"evidence");
    }

    #[cfg(unix)]
    #[test]
    fn new_directory_and_lock_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("outbox");
        ensure_directory(&directory, true).unwrap();
        let path = directory.join("LOCK");
        let _owner = DirectoryLock::acquire(&path, true).unwrap();
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            DIR_MODE
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            FILE_MODE
        );
    }

    #[test]
    fn immediate_transaction_reopens_with_committed_data() {
        // Reopen is a persistence regression, not evidence of power-loss durability.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.redb");
        const ROWS: redb::TableDefinition<u64, &[u8]> = redb::TableDefinition::new("rows");
        {
            let database = redb::Database::create(&path).unwrap();
            let write = durable_write(&database).unwrap();
            {
                write
                    .open_table(ROWS)
                    .unwrap()
                    .insert(1, b"committed".as_slice())
                    .unwrap();
            }
            write.commit().unwrap();
        }
        let database = redb::Database::open(path).unwrap();
        let read = database.begin_read().unwrap();
        assert_eq!(
            read.open_table(ROWS)
                .unwrap()
                .get(1)
                .unwrap()
                .unwrap()
                .value(),
            b"committed"
        );
    }
}
