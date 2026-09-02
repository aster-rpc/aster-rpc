//! A storage scratch directory the production open policy will accept.
//!
//! ## Why this is not `tempfile::tempdir()`
//!
//! `OpenPolicy::production()` refuses a storage directory that is writable
//! beyond its owner, and refuses it rather than repairing it -- tightening the
//! mode would fix the symptom and erase the evidence that something outside
//! this voter could write the files its mutual exclusion depends on. That
//! policy is right and is not what this helper works around.
//!
//! What it works around is that `tempfile` asks for `0o777` and lets the
//! process umask subtract from it, so the mode a scratch directory ends up
//! with is a property of the host rather than of the test. Under macOS's usual
//! `022` it is `0755` and opens; under a umask of `000` it is `0777` and is
//! **correctly refused**. A fixture that hands production a world-writable
//! directory is asserting the host's umask, not the code.
//!
//! A umask of `000` is not hypothetical. Forgejo runs each workflow step with
//! `docker exec`, which does not apply the image's umask the way `docker run`
//! does: inside the same container `docker run` reports `0022` while
//! `docker exec` reports `0000`. Every storage-opening integration test
//! therefore failed the first time this suite ran on the Linux runner, while
//! the same assertions passed locally.
//!
//! ## Why the umask and not only the mode
//!
//! `aster/src/coordination/storage/scratch.rs` already solved this for the
//! library's own tests and documents why chmod-ing the root is not sufficient:
//! a fixture that creates a store beneath the root, or seeds a database by
//! hand before handing the directory to production, creates that path at the
//! ambient umask no matter what the root was set to. That module is `cfg(test)`
//! and so covers the lib target only; an integration test is a separate binary
//! and inherits none of it. This is the same fix at the same altitude, for the
//! targets that were left out: narrow the process once, then pin the root.

#![allow(dead_code)]

/// A scratch directory owned by the caller, at `0700`, with the rest of this
/// process's creations narrowed to match.
pub fn storage_dir() -> tempfile::TempDir {
    narrow_process_umask();
    let dir = tempfile::tempdir().expect("a storage directory");
    make_owner_only(dir.path());
    dir
}

/// Narrow this process's umask to `0o077`, once, for the whole test binary.
///
/// Process-wide and never restored, so there is no window in which one test's
/// creation races another's change. It cannot affect production behaviour:
/// `restrict_permissions` uses `set_permissions`, which ignores the umask.
#[cfg(unix)]
fn narrow_process_umask() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `umask` always succeeds, cannot fail and cannot alias. It is
        // `unsafe` only because it is a raw libc call.
        unsafe { libc::umask(0o077) };
    });
}

#[cfg(not(unix))]
fn narrow_process_umask() {}

/// Pin the scratch root itself to `0700`.
///
/// The umask above governs what this process creates from here on; the root was
/// created by `tempfile` before it, so it is set explicitly.
#[cfg(unix)]
fn make_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("narrow the scratch root");
}

#[cfg(not(unix))]
fn make_owner_only(_path: &std::path::Path) {}
