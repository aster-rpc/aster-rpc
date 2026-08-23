//! `wait_ready()` and the published lifecycle view (§7.6).
//!
//! ## Level-triggered, and not a latch
//!
//! [`Readiness::wait_ready`] returns immediately when the node is **already**
//! `Ready`, and otherwise waits for the next transition into `Ready`. Both
//! halves matter. Edge-triggered would hang forever on a node that became
//! `Ready` before the caller asked; a latch would answer `Ready` for a node
//! that has since been partitioned away.
//!
//! It promises nothing about the future. A node may be `Ready` when this
//! returns and `NoQuorum` a moment later — that is not a defect in the
//! signature, it is what readiness *is*. A caller needing a durable guarantee
//! holds a fence.
//!
//! ## It terminates on `Fatal`
//!
//! `Fatal` is latched and no later message repairs it, so a `wait_ready()`
//! against a fatal node can never succeed. Blocking forever would turn a
//! diagnosable startup failure into a hang with no error anywhere, so this
//! returns the cause instead. The same applies when the runtime has shut down
//! and the publisher is gone.
//!
//! ## One publisher
//!
//! The view is written by the runtime's metrics loop and by nothing else. A
//! second writer would let two sources of truth disagree about a node's own
//! state, which is the failure [`lifecycle::classify`] already exists to
//! prevent one level down.

use tokio::sync::watch;

use super::lifecycle::{FatalCause, LifecycleCode, LifecycleView};

/// Why waiting for readiness ended without readiness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NotReady {
    /// The node latched `Fatal`. No later message repairs it.
    Fatal(FatalCause),
    /// The runtime shut down while waiting.
    ShutDown,
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotReady::Fatal(cause) => write!(f, "the voter will not become ready: {cause}"),
            NotReady::ShutDown => f.write_str("the coordination runtime shut down while waiting"),
        }
    }
}

impl std::error::Error for NotReady {}

impl From<NotReady> for crate::error::Error {
    fn from(err: NotReady) -> Self {
        // Neither cause is an invalid argument: the configuration was fine and
        // the node reached a state it cannot leave. `Transport` is the facade's
        // pass-through for exactly that, and it preserves the `Display` text
        // that names which of the two it was.
        crate::error::Error::Transport(anyhow::anyhow!(err.to_string()))
    }
}

/// The write side. Held by the runtime's metrics loop; dropping it is what
/// makes an outstanding [`Readiness::wait_ready`] return [`NotReady::ShutDown`].
#[derive(Debug)]
pub(crate) struct LifecyclePublisher {
    tx: watch::Sender<LifecycleView>,
}

impl LifecyclePublisher {
    /// Publish a newly classified view.
    ///
    /// `send_replace` rather than `send`: `watch::Sender::send` fails when
    /// there are no receivers, and a runtime whose observers have all gone away
    /// must keep classifying itself — its own shutdown path reads the view.
    pub(crate) fn publish(&self, view: LifecycleView) {
        self.tx.send_replace(view);
    }

    /// A reader for this runtime's lifecycle.
    pub(crate) fn subscribe(&self) -> Readiness {
        Readiness {
            rx: self.tx.subscribe(),
        }
    }
}

/// The read side: the current lifecycle view, and the level-triggered wait.
#[derive(Clone, Debug)]
pub(crate) struct Readiness {
    rx: watch::Receiver<LifecycleView>,
}

/// Create the publisher/reader pair for one runtime.
///
/// `initial` is the state `start()` may legitimately return in —
/// `Bootstrapping`, `Joining` or `NoQuorum` per §7.6, and never `Ready`, which
/// is observed rather than asserted by construction.
pub(crate) fn channel(initial: LifecycleView) -> (LifecyclePublisher, Readiness) {
    debug_assert_ne!(
        initial.code,
        LifecycleCode::Ready,
        "readiness is observed, never asserted at construction (§7.6)"
    );
    let (tx, rx) = watch::channel(initial);
    (LifecyclePublisher { tx }, Readiness { rx })
}

impl Readiness {
    /// The current view, without waiting.
    pub(crate) fn view(&self) -> LifecycleView {
        self.rx.borrow().clone()
    }

    /// The current lifecycle code, without waiting.
    pub(crate) fn code(&self) -> LifecycleCode {
        self.rx.borrow().code
    }

    /// Wait until this node is `Ready`.
    ///
    /// Returns immediately if it already is. Returns [`NotReady`] if the node
    /// latches `Fatal` or the runtime shuts down; otherwise it waits.
    pub(crate) async fn wait_ready(&mut self) -> Result<(), NotReady> {
        loop {
            // Checked before awaiting a change, which is what makes this
            // level-triggered: a node that was already `Ready` returns here.
            {
                let view = self.rx.borrow_and_update();
                match view.code {
                    LifecycleCode::Ready => return Ok(()),
                    LifecycleCode::Fatal => {
                        return Err(NotReady::Fatal(view.fatal.clone().unwrap_or_else(|| {
                            // `classify` sets a cause on every `Fatal`; this
                            // arm keeps the function total rather than
                            // asserting, because a hang is a worse failure than
                            // a vague error.
                            FatalCause::Raft("fatal with no recorded cause".into())
                        })));
                    }
                    _ => {}
                }
            }
            if self.rx.changed().await.is_err() {
                return Err(NotReady::ShutDown);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::lifecycle::{LifecycleView, LocalRole};
    use super::*;

    fn view(code: LifecycleCode) -> LifecycleView {
        LifecycleView {
            code,
            local_role: LocalRole::Voter,
            quorum_available: Some(code == LifecycleCode::Ready),
            fatal: None,
        }
    }

    fn fatal(cause: FatalCause) -> LifecycleView {
        LifecycleView {
            code: LifecycleCode::Fatal,
            local_role: LocalRole::Voter,
            quorum_available: None,
            fatal: Some(cause),
        }
    }

    /// The level half: a node that was already `Ready` before anyone asked
    /// returns immediately.
    ///
    /// Driven with a zero-length timeout, so a wait that blocked at all would
    /// fail rather than pass slowly.
    #[tokio::test]
    async fn a_node_that_is_already_ready_returns_without_waiting() {
        let (publisher, mut readiness) = channel(view(LifecycleCode::NoQuorum));
        publisher.publish(view(LifecycleCode::Ready));

        tokio::time::timeout(std::time::Duration::ZERO, readiness.wait_ready())
            .await
            .expect("must not wait at all")
            .expect("ready");
    }

    /// The trigger half: a waiter parked before the transition is released by
    /// it.
    #[tokio::test]
    async fn a_waiter_is_released_by_the_next_transition_into_ready() {
        let (publisher, mut readiness) = channel(view(LifecycleCode::Bootstrapping));
        let waiter = tokio::spawn(async move { readiness.wait_ready().await });

        // Intervening states must not release it.
        publisher.publish(view(LifecycleCode::NoQuorum));
        publisher.publish(view(LifecycleCode::Joining));
        assert!(!waiter.is_finished());

        publisher.publish(view(LifecycleCode::Ready));
        waiter.await.expect("join").expect("ready");
    }

    /// Not a latch: a node that was `Ready` and is no longer does not report
    /// readiness, and a fresh wait parks again.
    #[tokio::test]
    async fn readiness_is_not_latched_once_observed() {
        let (publisher, mut readiness) = channel(view(LifecycleCode::NoQuorum));
        publisher.publish(view(LifecycleCode::Ready));
        readiness.wait_ready().await.expect("ready");

        publisher.publish(view(LifecycleCode::NoQuorum));
        assert_eq!(readiness.code(), LifecycleCode::NoQuorum);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), readiness.wait_ready())
                .await
                .is_err(),
            "a node that has lost readiness must not report it from a previous observation"
        );
    }

    /// `Fatal` terminates the wait with its cause rather than blocking
    /// forever. The cause is what makes the failure diagnosable at all.
    #[tokio::test]
    async fn a_fatal_node_ends_the_wait_with_its_cause() {
        let (publisher, mut readiness) = channel(view(LifecycleCode::Bootstrapping));
        let waiter = tokio::spawn(async move { readiness.wait_ready().await });
        publisher.publish(fatal(FatalCause::Storage("out of disk".into())));

        assert_eq!(
            waiter.await.expect("join"),
            Err(NotReady::Fatal(FatalCause::Storage("out of disk".into())))
        );
    }

    /// A node that is *already* fatal ends the wait immediately, on the same
    /// pre-await check that makes readiness level-triggered.
    #[tokio::test]
    async fn an_already_fatal_node_ends_the_wait_immediately() {
        let (publisher, mut readiness) = channel(view(LifecycleCode::NoQuorum));
        publisher.publish(fatal(FatalCause::IdentityMismatch("group id".into())));
        let result = tokio::time::timeout(std::time::Duration::ZERO, readiness.wait_ready())
            .await
            .expect("must not wait at all");
        assert!(matches!(result, Err(NotReady::Fatal(_))));
    }

    /// Shutdown releases waiters rather than stranding them.
    #[tokio::test]
    async fn shutting_the_runtime_down_releases_a_waiter() {
        let (publisher, mut readiness) = channel(view(LifecycleCode::Joining));
        let waiter = tokio::spawn(async move { readiness.wait_ready().await });
        drop(publisher);
        assert_eq!(waiter.await.expect("join"), Err(NotReady::ShutDown));
    }

    /// The publisher keeps working with no observers at all: the runtime's own
    /// shutdown path reads the view, so a classification loop that stopped when
    /// the last caller went away would stop reporting its own state.
    #[test]
    fn publishing_survives_the_last_observer_leaving() {
        let (publisher, readiness) = channel(view(LifecycleCode::NoQuorum));
        drop(readiness);
        publisher.publish(view(LifecycleCode::Ready));
        assert_eq!(publisher.subscribe().code(), LifecycleCode::Ready);
    }

    /// §7.6: `start()` never returns `Ready`. Asserted as a debug invariant on
    /// the constructor, because the alternative — a runtime that publishes
    /// `Ready` before it has observed anything — is exactly the "asserted by
    /// construction" failure the rule forbids.
    #[test]
    #[should_panic(expected = "readiness is observed")]
    fn a_runtime_cannot_be_constructed_already_ready() {
        let _ = channel(view(LifecycleCode::Ready));
    }
}
