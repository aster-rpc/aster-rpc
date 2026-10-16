//! Server drain: stop taking work, let in-flight calls finish until a
//! deadline, then cancel what is left. [`AsterServer::drain`] and
//! [`ServerHandle::drain`] run it; the C ABI exposes it as
//! `aster_server_drain` and Go as `AsterServer.Drain`.
//!
//! While draining, a server:
//!
//! - closes every new inbound connection right after its handshake with the
//!   application close code [`DRAIN_CLOSE_CODE`] (503) and reason
//!   [`DRAIN_CLOSE_REASON`];
//! - answers every new call on an existing connection with `UNAVAILABLE`
//!   carrying the detail `aster.draining = "true"` ([`DRAINING_DETAIL_KEY`]),
//!   except calls on a session that already exists, which run so the session
//!   can finish;
//! - closes each connection with the drain code once it is idle (no call in
//!   flight and no live session) and has stayed so for
//!   [`idle_quiet_period`] (four round trips, 100 ms to 2 s), so the last
//!   response is acknowledged before the close discards unsent data;
//! - at the deadline, closes every remaining connection with the drain code,
//!   which cancels the calls still running (handlers see the peer cancel),
//!   and waits up to [`CANCEL_WAIT`] for them to end.
//!
//! A client sees the drain as `UNAVAILABLE` with the `aster.draining` detail,
//! whether its call was refused or its connection was closed under it
//! ([`Error::is_draining`](crate::Error::is_draining)); the default retry
//! policy does not retry it on the same connection, because the answer is to
//! reconnect to another replica.
//!
//! [`AsterServer::drain`]: super::AsterServer::drain
//! [`ServerHandle::drain`]: super::ServerHandle::drain

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aster_transport_core::reactor::ReactorControl;
pub use aster_transport_core::reactor::{
    DRAINING_DETAIL_KEY, DRAIN_CLOSE_CODE, DRAIN_CLOSE_REASON,
};
use tokio::sync::Notify;

use super::codec::RpcStatus;
use super::sessions::SessionStore;
use super::status::StatusCode;
use crate::log_targets::SERVER;

/// How long a drain waits, after closing every connection at its deadline,
/// for the cancelled calls to end before it reports them.
pub const CANCEL_WAIT: Duration = Duration::from_secs(2);

/// How often a drain re-checks for idle connections when nothing wakes it.
const POLL: Duration = Duration::from_millis(25);

/// How long a connection must have been idle before a drain closes it: four
/// round trips, at least [`IDLE_QUIET_MIN`] and at most [`IDLE_QUIET_MAX`].
/// A QUIC close discards stream data the peer has not acknowledged yet, so
/// closing the instant the last response was handed to QUIC could cut off
/// that response's trailer; the quiet period lets it be acknowledged (and
/// retransmitted once if lost).
pub fn idle_quiet_period(rtt: Duration) -> Duration {
    (rtt * 4).clamp(IDLE_QUIET_MIN, IDLE_QUIET_MAX)
}

/// Lower bound of [`idle_quiet_period`].
pub const IDLE_QUIET_MIN: Duration = Duration::from_millis(100);
/// Upper bound of [`idle_quiet_period`].
pub const IDLE_QUIET_MAX: Duration = Duration::from_secs(2);

/// What a drain did. Calls count those admitted to a handler (native or
/// binding), not those refused at routing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Calls in flight when the drain started.
    pub calls_in_flight: u64,
    /// Calls that finished before the deadline: those in flight at the start,
    /// and calls on existing sessions admitted during the drain.
    pub calls_completed: u64,
    /// Calls still running at the deadline, cancelled by closing their
    /// connection.
    pub calls_cancelled: u64,
    /// New calls answered `UNAVAILABLE` (draining) during the drain.
    pub calls_refused: u64,
    /// Connections the drain closed with [`DRAIN_CLOSE_CODE`]: idle ones as
    /// they became idle, the rest at the deadline.
    pub connections_closed: u64,
    /// New connections refused at accept during the drain.
    pub connections_refused: u64,
    /// Whether the deadline passed with work outstanding.
    pub deadline_reached: bool,
    /// How long the drain took.
    pub elapsed: Duration,
}

const SERVING: u8 = 0;
const DRAINING: u8 = 1;
const CANCELLING: u8 = 2;
const DONE: u8 = 3;

/// One server's drain state, shared by its dispatchers (which refuse new
/// calls and count admitted ones) and its [`ServerHandle`](super::ServerHandle)
/// (which runs the drain).
pub(crate) struct DrainShared {
    reactor: Arc<ReactorControl>,
    phase: AtomicU8,
    in_flight: AtomicU64,
    completed: AtomicU64,
    cancelled: AtomicU64,
    refused: AtomicU64,
    changed: Notify,
    deadline: Mutex<Option<Instant>>,
    deadline_moved: Notify,
    /// Held by the drain that runs; holds its report once it ends.
    run: tokio::sync::Mutex<Option<DrainReport>>,
}

impl Default for DrainShared {
    fn default() -> Self {
        Self {
            reactor: ReactorControl::new(),
            phase: AtomicU8::new(SERVING),
            in_flight: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            changed: Notify::new(),
            deadline: Mutex::new(None),
            deadline_moved: Notify::new(),
            run: tokio::sync::Mutex::new(None),
        }
    }
}

impl DrainShared {
    pub(crate) fn reactor(&self) -> Arc<ReactorControl> {
        self.reactor.clone()
    }

    /// Whether new calls are refused.
    #[inline]
    pub(crate) fn is_draining(&self) -> bool {
        self.phase.load(Ordering::Acquire) != SERVING
    }

    /// A new call refused because the server is draining: count it and
    /// return the status to answer it with.
    pub(crate) fn refuse(&self) -> RpcStatus {
        self.refused.fetch_add(1, Ordering::Relaxed);
        draining_status()
    }

    /// Count a call admitted to a handler until the ticket drops.
    pub(crate) fn admit(self: &Arc<Self>) -> DrainTicket {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        DrainTicket(self.clone())
    }

    fn deadline(&self) -> Instant {
        self.deadline
            .lock()
            .unwrap()
            .expect("deadline set before the drain runs")
    }

    /// Move the deadline to `at` if that is earlier.
    fn shorten(&self, at: Instant) {
        let mut deadline = self.deadline.lock().unwrap();
        if deadline.is_none_or(|current| at < current) {
            *deadline = Some(at);
            drop(deadline);
            self.deadline_moved.notify_waiters();
        }
    }

    /// Drain (see the module docs). A second call while one runs moves the
    /// deadline earlier if its own is earlier, then returns the same report;
    /// a call after a drain ended returns that drain's report.
    pub(crate) async fn drain(&self, grace: Duration, sessions: &SessionStore) -> DrainReport {
        let started = Instant::now();
        self.shorten(started + grace);
        let mut run = self.run.lock().await;
        if let Some(report) = &*run {
            return report.clone();
        }
        let in_flight = self.in_flight.load(Ordering::Acquire);
        self.phase.store(DRAINING, Ordering::Release);
        self.reactor.start_draining();
        tracing::info!(
            target: SERVER,
            calls_in_flight = in_flight,
            connections = self.reactor.live_connections(),
            grace_ms = grace.as_millis() as u64,
            "draining"
        );

        let mut closed = 0u64;
        let deadline_reached = loop {
            let reactor_changed = self.reactor.changed();
            let changed = self.changed.notified();
            let moved = self.deadline_moved.notified();
            tokio::pin!(reactor_changed, changed, moved);
            reactor_changed.as_mut().enable();
            changed.as_mut().enable();
            moved.as_mut().enable();

            for conn in self.reactor.connections() {
                if conn.active_calls == 0
                    && conn.idle_for >= idle_quiet_period(conn.rtt)
                    && !sessions.has_connection_sessions(conn.connection_id)
                    && self.reactor.close_draining(conn.connection_id)
                {
                    closed += 1;
                }
            }
            if self.reactor.live_connections() == 0 && self.in_flight.load(Ordering::Acquire) == 0 {
                break false;
            }
            let deadline = self.deadline();
            if Instant::now() >= deadline {
                break true;
            }
            tokio::select! {
                () = reactor_changed => {}
                () = changed => {}
                () = moved => {}
                () = tokio::time::sleep_until(deadline.into()) => {}
                () = tokio::time::sleep(POLL) => {}
            }
        };

        if deadline_reached {
            // Everything ending from here on was cut off by the deadline.
            self.phase.store(CANCELLING, Ordering::Release);
            for conn in self.reactor.connections() {
                if self.reactor.close_draining(conn.connection_id) {
                    closed += 1;
                }
            }
            let wait_until = Instant::now() + CANCEL_WAIT;
            while self.in_flight.load(Ordering::Acquire) > 0 && Instant::now() < wait_until {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.in_flight.load(Ordering::Acquire) == 0 {
                    break;
                }
                tokio::select! {
                    () = changed => {}
                    () = tokio::time::sleep(POLL) => {}
                }
            }
        }
        // Calls that outlive the wait are reported cancelled now; later ends
        // are not counted again.
        let cancelled = self.cancelled.load(Ordering::Acquire);
        let remaining = self.in_flight.load(Ordering::Acquire);
        self.phase.store(DONE, Ordering::Release);
        let report = DrainReport {
            calls_in_flight: in_flight,
            calls_completed: self.completed.load(Ordering::Acquire),
            calls_cancelled: cancelled + remaining,
            calls_refused: self.refused.load(Ordering::Acquire),
            connections_closed: closed,
            connections_refused: self.reactor.refused_connections(),
            deadline_reached,
            elapsed: started.elapsed(),
        };
        tracing::info!(
            target: SERVER,
            calls_completed = report.calls_completed,
            calls_cancelled = report.calls_cancelled,
            calls_refused = report.calls_refused,
            connections_closed = report.connections_closed,
            connections_refused = report.connections_refused,
            deadline_reached,
            elapsed_ms = report.elapsed.as_millis() as u64,
            "drained"
        );
        *run = Some(report.clone());
        report
    }
}

/// One admitted call, counted in flight until dropped (it lives in the
/// call's telemetry, which ends exactly when the call does).
pub(crate) struct DrainTicket(Arc<DrainShared>);

impl Drop for DrainTicket {
    fn drop(&mut self) {
        let shared = &self.0;
        let phase = shared.phase.load(Ordering::Acquire);
        match phase {
            DRAINING => {
                shared.completed.fetch_add(1, Ordering::AcqRel);
            }
            CANCELLING => {
                shared.cancelled.fetch_add(1, Ordering::AcqRel);
            }
            _ => {}
        }
        shared.in_flight.fetch_sub(1, Ordering::AcqRel);
        if phase != SERVING {
            shared.changed.notify_waiters();
        }
    }
}

/// The status a draining server answers new calls with.
pub fn draining_status() -> RpcStatus {
    RpcStatus {
        code: StatusCode::Unavailable.as_i32(),
        message: "server draining; reconnect elsewhere".into(),
        detail_keys: vec![DRAINING_DETAIL_KEY.into()],
        detail_values: vec!["true".into()],
    }
}
