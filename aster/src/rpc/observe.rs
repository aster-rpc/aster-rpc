//! Per-call telemetry for the server: the `aster::rpc::dispatch` tracing
//! events and, with the `health` feature, the R7 [`HealthMetrics`] counters.
//!
//! Every call that reaches a dispatcher gets one [`CallObs`] from
//! [`Telemetry::start_call`] and is finished exactly once with its final
//! status, whichever path ends it (refused before dispatch, a native handler's
//! trailer, a foreign call's terminal response, a deadline, a cancel).
//!
//! Cost when nothing is listening: no health handle means no counter
//! updates, and the start timestamp is taken only when the dispatch target is
//! enabled at debug level. No allocation either way.
//!
//! [`HealthMetrics`]: crate::health::HealthMetrics

use std::time::Instant;

use tracing::Level;

use super::status::StatusCode;
use crate::log_targets::DISPATCH;

/// The server's telemetry sinks, shared by every dispatcher clone.
#[derive(Clone, Default)]
pub(crate) struct Telemetry {
    #[cfg(feature = "health")]
    pub(crate) health: Option<crate::health::HealthState>,
}

impl Telemetry {
    /// A call arrived: count it and start its clock.
    #[inline]
    pub(crate) fn start_call(&self) -> CallObs {
        #[cfg(feature = "health")]
        let health = self.health.as_ref().map(|h| {
            let m = h.metrics();
            m.rpc_started();
            m.stream_opened();
            h.clone()
        });
        CallObs {
            #[cfg(feature = "health")]
            health,
            started: tracing::enabled!(target: DISPATCH, Level::DEBUG).then(Instant::now),
            done: false,
        }
    }

    /// Whether connection counters are worth tracking.
    #[cfg(feature = "health")]
    pub(crate) fn has_health(&self) -> bool {
        self.health.is_some()
    }

    /// Whether connection counters are worth tracking.
    #[cfg(not(feature = "health"))]
    pub(crate) fn has_health(&self) -> bool {
        false
    }

    pub(crate) fn connection_opened(&self) {
        #[cfg(feature = "health")]
        if let Some(h) = &self.health {
            h.metrics().connection_opened();
        }
    }

    pub(crate) fn connection_closed(&self) {
        #[cfg(feature = "health")]
        if let Some(h) = &self.health {
            h.metrics().connection_closed();
        }
    }
}

/// What a completion event reports about the call.
#[derive(Default)]
pub(crate) struct CallInfo<'a> {
    pub(crate) service: &'a str,
    pub(crate) method: &'a str,
    pub(crate) call_id: i32,
    pub(crate) peer: &'a str,
}

/// One call's telemetry. Finish it with [`finish`](CallObs::finish); one
/// dropped unfinished counts as a failed call (no event: there is nothing to
/// describe it with).
pub(crate) struct CallObs {
    #[cfg(feature = "health")]
    health: Option<crate::health::HealthState>,
    started: Option<Instant>,
    done: bool,
}

impl CallObs {
    /// Record the call's final status. Later calls are no-ops.
    pub(crate) fn finish(&mut self, code: StatusCode, info: CallInfo<'_>) {
        if !self.record(code) {
            return;
        }
        if let Some(started) = self.started {
            tracing::debug!(
                target: DISPATCH,
                service = info.service,
                method = info.method,
                call_id = info.call_id,
                peer = info.peer,
                status = ?code,
                elapsed_us = started.elapsed().as_micros() as u64,
                "rpc completed"
            );
        }
    }

    /// Counters only. Returns whether this was the first finish.
    fn record(&mut self, code: StatusCode) -> bool {
        if self.done {
            return false;
        }
        self.done = true;
        #[cfg(feature = "health")]
        if let Some(h) = self.health.take() {
            let m = h.metrics();
            m.rpc_completed(code == StatusCode::Ok);
            m.stream_closed();
        }
        #[cfg(not(feature = "health"))]
        let _ = code;
        true
    }
}

impl Drop for CallObs {
    fn drop(&mut self) {
        self.record(StatusCode::Internal);
    }
}
