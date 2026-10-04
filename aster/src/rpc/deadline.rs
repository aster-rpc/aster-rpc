//! One timer facility per server for call deadlines.
//!
//! Every call that carries a deadline (the header's relative `deadline`, a
//! method default, or the server's `handler_timeout` cap) registers one entry
//! here instead of arming its own timer. A single task sleeps until the
//! earliest entry, fires everything that has expired, and goes back to sleep.
//! Entries are removed when their [`DeadlineGuard`] drops, so a call that
//! finishes in time leaves nothing behind.
//!
//! The expiry action is a plain closure. Native services use it to wake the
//! dispatch future, which then emits `DEADLINE_EXCEEDED` and drops the
//! handler. Foreign (binding) calls, which have no Rust future to race, can
//! register an action that emits the trailer and a cancel event directly.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;
use tokio::time::Instant;

type Action = Box<dyn FnOnce() + Send>;

/// Shared handle to the server's deadline queue. Cheap to clone.
#[derive(Clone, Default)]
pub(crate) struct DeadlineQueue {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    /// Held by the timer task too, so it can wait without keeping `Inner`
    /// alive. `Inner::drop` signals it so the task notices and exits.
    wake: Arc<Notify>,
    started: AtomicBool,
}

#[derive(Default)]
struct State {
    /// Ordered by expiry; the `u64` makes equal instants distinct.
    entries: BTreeMap<(Instant, u64), Action>,
    next_seq: u64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

/// Removes its entry from the queue when dropped. Dropping after the entry
/// fired is a no-op.
pub(crate) struct DeadlineGuard {
    inner: Weak<Inner>,
    key: (Instant, u64),
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.state.lock().unwrap().entries.remove(&self.key);
        }
    }
}

impl DeadlineQueue {
    /// Run `action` at `at` unless the returned guard is dropped first. Must
    /// be called from within a tokio runtime (the timer task starts lazily on
    /// first use).
    pub(crate) fn register(
        &self,
        at: Instant,
        action: impl FnOnce() + Send + 'static,
    ) -> DeadlineGuard {
        self.ensure_started();
        let key;
        let earliest;
        {
            let mut st = self.inner.state.lock().unwrap();
            key = (at, st.next_seq);
            st.next_seq = st.next_seq.wrapping_add(1);
            st.entries.insert(key, Box::new(action));
            earliest = st.entries.keys().next() == Some(&key);
        }
        if earliest {
            // `notify_one` stores a permit when the task is not waiting, so a
            // wake-up between its scan and its wait is not lost.
            self.inner.wake.notify_one();
        }
        DeadlineGuard {
            inner: Arc::downgrade(&self.inner),
            key,
        }
    }

    /// Number of pending entries (tests and diagnostics).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.state.lock().unwrap().entries.len()
    }

    fn ensure_started(&self) {
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let wake = self.inner.wake.clone();
        tokio::spawn(run(weak, wake));
    }
}

async fn run(weak: Weak<Inner>, wake: Arc<Notify>) {
    loop {
        let next = {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let now = Instant::now();
            let mut fired = Vec::new();
            let next = {
                let mut st = inner.state.lock().unwrap();
                while let Some(entry) = st.entries.first_entry() {
                    if entry.key().0 > now {
                        break;
                    }
                    fired.push(entry.remove());
                }
                st.entries.keys().next().map(|k| k.0)
            };
            // Release the strong reference before running actions or waiting,
            // so dropping the last dispatcher ends this task.
            drop(inner);
            for action in fired {
                action();
            }
            next
        };
        match next {
            Some(at) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(at) => {}
                    _ = wake.notified() => {}
                }
            }
            None => wake.notified().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn fires_in_order_and_guards_cancel() {
        let q = DeadlineQueue::default();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let now = Instant::now();
        let h = hits.clone();
        let _a = q.register(now + Duration::from_millis(30), move || {
            h.lock().unwrap().push(3)
        });
        let h = hits.clone();
        let _b = q.register(now + Duration::from_millis(10), move || {
            h.lock().unwrap().push(1)
        });
        let h = hits.clone();
        let cancelled = q.register(now + Duration::from_millis(20), move || {
            h.lock().unwrap().push(2)
        });
        drop(cancelled);
        assert_eq!(q.len(), 2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*hits.lock().unwrap(), vec![1, 3]);
        assert_eq!(q.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_earlier_entry_wakes_a_sleeping_task() {
        let q = DeadlineQueue::default();
        let n = Arc::new(AtomicUsize::new(0));
        let now = Instant::now();
        let _far = q.register(now + Duration::from_secs(3600), || {});
        tokio::task::yield_now().await;
        let c = n.clone();
        let _near = q.register(now + Duration::from_millis(5), move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }
}
