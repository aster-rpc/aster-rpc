//! `fork()` after init is unsupported (design §2.4, Q14): the child inherits
//! the statics but not the exporter threads, and a lock held at fork time
//! could deadlock it. A `pthread_atfork` child handler sets a flag; the first
//! pipeline call in the child prints one warning to stderr, and from then on
//! every telemetry path is a no-op. Fork *before* init is fine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

static FORKED: AtomicBool = AtomicBool::new(false);
static WARNED: AtomicBool = AtomicBool::new(false);
static REGISTER: Once = Once::new();

#[cfg(unix)]
extern "C" fn on_fork_child() {
    // Async-signal-safe: a single atomic store.
    FORKED.store(true, Ordering::Relaxed);
}

/// Register the child handler (once per process; called by `init`).
pub(crate) fn register() {
    REGISTER.call_once(|| {
        #[cfg(unix)]
        // SAFETY: `on_fork_child` is a plain `extern "C" fn()` that only does
        // an atomic store, which is async-signal-safe as a fork handler must be.
        unsafe {
            libc::pthread_atfork(None, None, Some(on_fork_child));
        }
    });
}

/// True in a child forked after `init`. Cheap: one relaxed load.
#[inline]
pub(crate) fn forked() -> bool {
    FORKED.load(Ordering::Relaxed)
}

/// [`forked`], plus the one-time warning. For the API entry points.
pub(crate) fn check() -> bool {
    if !forked() {
        return false;
    }
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "aster: WARN telemetry is disabled in this process: it was forked after \
             aster_otel::init, which is unsupported (initialise in the child instead)"
        );
    }
    true
}
