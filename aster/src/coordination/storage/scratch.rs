//! One owning scratch directory, for every test in this module tree.
//!
//! ## Why this exists rather than five copies of it
//!
//! Each of `schema`, `log`, `rows`, `fixture` and `machine` grew its own
//! `fn scratch_dir() -> PathBuf`: same three lines, same name shape, and the
//! same non-ownership -- the caller received a path and nothing ever removed
//! the tree behind it. A store here is a redb file, a log, snapshot bodies and
//! whatever a case published, so on a machine that runs the suite regularly
//! they accumulate. They had: 61,549 directories across the five families,
//! roughly 30 GiB, on a laptop whose `$TMPDIR` a reboot sweeps and on a
//! long-lived CI host that never would.
//!
//! Cleaning them without fixing the helpers would reset a recurring leak, and
//! fixing one helper would leave four. So the guard is one type, and the five
//! families differ only in the name they pass.
//!
//! ## Best-effort, and deliberately silent about it
//!
//! [`ScratchDir::drop`] cannot report a failure -- a panicking destructor
//! during unwind aborts the process, which would turn a tidy-up problem into a
//! lost test result. It removes what it can and returns. What it must not do is
//! fail *predictably*, which is what the permission pass below is for.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes concurrent directories within one process.
///
/// Shared across families rather than one counter each: the name already
/// carries the family and the process id, so a single sequence is enough to
/// make it unique and has the advantage of being unique *between* families
/// too.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory that removes itself when the test drops it.
///
/// Deliberately not conditional on the test passing. A harness that kept its
/// evidence only when something failed would fill the disk exactly when a run
/// is being repeated, and the directory contents are reproducible by rerunning
/// the case under a debugger.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScratchDir(PathBuf);

impl ScratchDir {
    /// A fresh, empty directory named for `family`, this process and a counter.
    ///
    /// The process id is in the name because two `cargo test` runs can overlap
    /// -- the crash matrix spawns children that create their own -- and two
    /// runs sharing a directory would be a test failure with no cause visible
    /// in either.
    pub(crate) fn new(family: &str) -> Self {
        narrow_process_umask();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aster-coordination-{family}-{}-{n}",
            std::process::id()
        ));
        // A leftover under this exact name would be a previous run of this
        // process id, which is only possible after a pid wrap. Cheap to
        // tolerate, confusing to inherit.
        remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        make_owner_only(&dir);
        Self(dir)
    }

    /// A copy of the path, for the few callers that need to own one.
    ///
    /// The removal stays with the guard rather than travelling with the copy,
    /// so a caller that keeps a `PathBuf` past the guard's life keeps a path to
    /// nothing -- which is the right failure, and a loud one.
    pub(crate) fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        remove_tree(&self.0);
    }
}

impl std::ops::Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Borrowed, never consumed. `From<ScratchDir>` is deliberately absent: it
/// would let a caller move the path out of the guard, and the directory would
/// then outlive the only thing that removes it.
impl From<&ScratchDir> for PathBuf {
    fn from(dir: &ScratchDir) -> PathBuf {
        dir.to_path_buf()
    }
}

/// Remove `root` and everything under it, as far as permissions allow.
fn remove_tree(root: &Path) {
    restore_owner_write(root);
    let _ = std::fs::remove_dir_all(root);
}

/// Give the owner write permission back over the whole tree, on Unix.
///
/// Several tests take a directory to `0o555` to force a real I/O failure out of
/// production code, and restore it immediately afterwards. When the assertion
/// *between* those two lines fails, the restore never runs, and the tree is
/// left unremovable: `unlink` needs write permission on the containing
/// directory, not on the file. Two such directories were found in `$TMPDIR`,
/// still there weeks later, having survived every attempt to clean up around
/// them.
///
/// So the guard does not assume the tests left things tidy. It walks down
/// adding `u+wx` to directories and `u+w` to files, top-down because a
/// directory missing `x` cannot be listed until it has been fixed.
///
/// Symlinks are skipped rather than followed. `remove_dir_all` unlinks a
/// symlink without descending, and a `chmod` that followed one would change
/// permissions on a file outside the tree -- in a helper whose whole purpose is
/// to touch nothing but its own directory.
#[cfg(unix)]
fn restore_owner_write(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }

    let mode = meta.permissions().mode();
    // Directories need `x` as well: without it the entries cannot be reached to
    // be unlinked, however writable the directory itself is.
    let wanted = if meta.is_dir() { 0o300 } else { 0o200 };
    if mode & wanted != wanted {
        let mut permissions = meta.permissions();
        permissions.set_mode(mode | wanted);
        let _ = std::fs::set_permissions(path, permissions);
    }

    if meta.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            restore_owner_write(&entry.path());
        }
    }
}

/// Nothing to do: Windows removability is not a permission bit in this sense,
/// and `remove_dir_all` handles the read-only attribute itself.
#[cfg(not(unix))]
fn restore_owner_write(_path: &Path) {}

/// Narrow a freshly created scratch directory to `0700`.
///
/// `create_dir_all` requests `0777` and lets the process umask subtract from
/// it, so what a scratch directory ends up as is a property of the host and not
/// of this helper. That is a difference between platforms with teeth:
/// `OpenPolicy::production()` refuses a storage directory writable beyond its
/// owner, so under macOS's usual `022` a scratch directory is `0755` and opens,
/// while under Ubuntu's default `002` it is `0775` and is **correctly refused**
/// with `BroadPermissions { mode: 509 }`.
///
/// Found by running the fault matrix natively on Linux for the first time, which
/// is the sort of thing that gate exists for. The production policy is right and
/// is not what changed here; a test fixture that hands it a group-writable
/// directory was simply asserting the host's umask.
///
/// This pins the root. [`narrow_process_umask`] is what covers everything
/// created *inside* it.
#[cfg(unix)]
fn make_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

/// Narrow this process's umask to `0o077`, once, for the whole test binary.
///
/// Chmod-ing the scratch root is not enough on its own, and the first Linux
/// run showed exactly why: a fixture that creates `store-0` beneath the root,
/// or a test that seeds a `raft.redb` by hand before handing the directory to
/// production, creates that path at the ambient umask no matter what the root
/// is set to. Under Ubuntu's default `002` those came out `0775` and `0664`
/// and were refused — eight further failures beyond the one the root fixed,
/// spread across `fixture` and `schema`, every one of them a fixture asserting
/// the host's umask rather than the code.
///
/// Patching each creation site would fix today's and leave the next fixture to
/// reintroduce it, so the narrowing goes here: every test directory in this
/// subsystem comes through [`ScratchDir::new`].
///
/// **This is deliberately process-wide, having first been rejected as such.**
/// The objection was that the crash matrix's spawned children inherit it —
/// which is true, and is the reason to want it: those children open storage
/// through the production path and need the same modes. It is set once, before
/// any scratch directory exists, and never restored, so there is no window in
/// which one thread's creation races another's change. It cannot affect
/// production behaviour: `restrict_permissions` uses `set_permissions`, which
/// ignores the umask, and this module is `cfg(test)`.
#[cfg(unix)]
fn narrow_process_umask() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `umask` is always successful and cannot fail or alias. It is
        // `unsafe` only because it is a raw libc call.
        unsafe { libc::umask(0o077) };
    });
}

#[cfg(not(unix))]
fn narrow_process_umask() {}

/// Windows has no mode bits to narrow, and the production permission check is
/// itself Unix-only.
#[cfg(not(unix))]
fn make_owner_only(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A scratch directory is `0700` whatever the host's umask is.**
    ///
    /// The regression this pins is a platform difference that hid on macOS for
    /// the whole of Slices 2–5: `create_dir_all` subtracts the ambient umask,
    /// so the mode was the host's opinion rather than this helper's. macOS's
    /// usual `022` gives `0755`, which `OpenPolicy::production()` accepts;
    /// Ubuntu's default `002` gives `0775`, which it refuses — correctly, since
    /// a group-writable storage directory is exactly what that check is for.
    /// The first native Linux run of the fault matrix failed on it.
    ///
    /// Asserted as the whole low nine bits, not as "no group write": a fixture
    /// that drifted to `0777` would still have to be caught, and a mask that
    /// only looked at one bit would miss `o+w`.
    #[cfg(unix)]
    #[test]
    fn a_scratch_directory_is_owner_only_whatever_the_hosts_umask_is() {
        use std::os::unix::fs::PermissionsExt as _;

        // Force the process-wide narrowing to have already happened, so this
        // test's own `umask(0)` below is not silently undone by it. Without
        // this the test passes whether or not `make_owner_only` runs, but only
        // when it happens to be the first in the binary to build a scratch
        // directory — an ordering-dependent gate, which is no gate.
        drop(ScratchDir::new("umask-independence-warmup"));

        // Now widen, so the assertion is about this helper and not about
        // whatever the host happens to run with. A test that only passed under
        // a strict umask would be asserting the host again, which is the defect
        // being fixed.
        let previous = unsafe { libc::umask(0) };
        let dir = ScratchDir::new("umask-independence");
        let mode = std::fs::metadata(&*dir)
            .expect("the scratch directory exists")
            .permissions()
            .mode()
            & 0o777;
        unsafe { libc::umask(previous) };

        assert_eq!(
            mode, 0o700,
            "a scratch directory must be owner-only regardless of umask; \
             production storage refuses anything writable beyond its owner, \
             so a broader one asserts the host rather than the code (got {mode:o})"
        );
    }

    /// The ordinary case: a directory with contents, gone when its guard is.
    #[test]
    fn a_scratch_directory_is_removed_when_its_guard_drops() {
        let path = {
            let dir = ScratchDir::new("guard-ordinary");
            std::fs::create_dir_all(dir.join("nested")).expect("nested");
            std::fs::write(dir.join("nested/file"), b"contents").expect("write");
            assert!(dir.exists(), "the guard must create its directory");
            dir.to_path_buf()
        };
        assert!(!path.exists(), "{} survived its guard", path.display());
    }

    /// The case that leaked.
    ///
    /// A test that chmods its root to `0o555` and fails before restoring it
    /// leaves a tree `remove_dir_all` cannot touch: the file inside is
    /// writable, and the directory it lives in is not, so it cannot be
    /// unlinked. Without the permission pass this assertion fails and the
    /// directory is still on disk afterwards.
    #[test]
    fn a_read_only_scratch_directory_is_still_removed() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = {
            let dir = ScratchDir::new("guard-read-only");
            std::fs::create_dir_all(dir.join("inner")).expect("inner");
            std::fs::write(dir.join("inner/file"), b"contents").expect("write");
            // Inner first, then the root: the same order a test's own cleanup
            // would have restored them in, left half-done.
            std::fs::set_permissions(dir.join("inner"), std::fs::Permissions::from_mode(0o555))
                .expect("chmod inner");
            std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o555))
                .expect("chmod root");
            dir.to_path_buf()
        };
        assert!(
            !path.exists(),
            "{} survived its guard; the permission pass did not run or did not \
             reach far enough",
            path.display()
        );
    }

    /// Two guards never name the same directory.
    ///
    /// The counter is shared across families, so this checks the property that
    /// matters rather than the mechanism: distinct names, within a family and
    /// across two.
    #[test]
    fn every_scratch_directory_has_its_own_name() {
        let first = ScratchDir::new("guard-unique");
        let second = ScratchDir::new("guard-unique");
        let other = ScratchDir::new("guard-unique-other");
        assert_ne!(first, second);
        assert_ne!(first.to_path_buf(), other.to_path_buf());
        assert_ne!(second.to_path_buf(), other.to_path_buf());
    }
}
