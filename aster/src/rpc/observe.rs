//! Per-call telemetry: the `aster::rpc::dispatch` tracing events and `rpc`
//! span, and with the `otel` feature the built-in OpenTelemetry instruments
//! and the server and client spans exported to OTel (design
//! `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §4.1, §5).
//!
//! Every call that reaches a dispatcher gets one [`CallObs`] from
//! [`Telemetry::start_call`], is [`open`](CallObs::open)ed once routing has
//! decoded its header (which creates the `rpc` span, so refused, native and
//! foreign calls all get one), and is finished exactly once with its final
//! status, whichever path ends it (refused before dispatch, a native handler's
//! trailer, a foreign call's terminal response, a deadline, a cancel). The
//! span lives in the `CallObs`, so a foreign call's span is held by its call
//! table entry and ends when the binding finishes the call.
//!
//! Cost when nothing is listening: with no OTel pipeline the cached
//! [`otel::Builtins`] say so after one relaxed load; the span is a disabled
//! `debug_span!`; and the start timestamp is taken only when the dispatch
//! target is enabled at debug level or metrics are recorded. No allocation
//! either way. (The `health` feature's `/metrics` reads these same
//! instruments through the OTel Prometheus reader.)

use std::collections::HashMap;
use std::time::Instant;

use tracing::{Level, Span};

use super::codec::StreamHeader;
use super::status::StatusCode;
use crate::log_targets::DISPATCH;

#[cfg(feature = "otel")]
pub(crate) mod otel;

/// The server's telemetry sinks, shared by every dispatcher clone.
#[derive(Clone, Default)]
pub(crate) struct Telemetry {
    /// Rust-native attribute extractors (`Server::attribute_extractor`).
    #[cfg(feature = "otel")]
    pub(crate) extractors: otel::RustExtractors,
}

impl Telemetry {
    /// A call arrived: count it and start its clock.
    #[inline]
    pub(crate) fn start_call(&self) -> CallObs {
        #[cfg(feature = "otel")]
        let otel = otel::CallOtel::start(&self.extractors);
        #[cfg(feature = "otel")]
        let timed = otel.as_ref().is_some_and(|o| o.timed());
        #[cfg(not(feature = "otel"))]
        let timed = false;
        CallObs {
            started: (timed || tracing::enabled!(target: DISPATCH, Level::DEBUG))
                .then(Instant::now),
            done: false,
            opened: false,
            span: Span::none(),
            #[cfg(feature = "otel")]
            otel,
        }
    }

    /// Whether the serve loop should track connections (for the OTel
    /// connection instruments).
    pub(crate) fn tracks_connections(&self) -> bool {
        cfg!(feature = "otel")
    }

    /// An `aster/1` connection delivered its first call. Pass the result to
    /// [`connection_closed`](Telemetry::connection_closed).
    pub(crate) fn connection_opened(&self) -> ConnObs {
        ConnObs {
            #[cfg(feature = "otel")]
            otel: otel::connection_opened(),
        }
    }

    pub(crate) fn connection_closed(&self, conn: ConnObs) {
        #[cfg(feature = "otel")]
        otel::connection_closed(conn.otel);
        #[cfg(not(feature = "otel"))]
        let _ = conn;
    }
}

/// One open connection's telemetry: the instruments that counted it open, so
/// it is counted closed on the same ones even if a pipeline came or went.
pub(crate) struct ConnObs {
    #[cfg(feature = "otel")]
    otel: Option<std::sync::Arc<otel::Builtins>>,
}

/// What a completion event reports about the call.
#[derive(Default)]
pub(crate) struct CallInfo<'a> {
    pub(crate) service: &'a str,
    pub(crate) method: &'a str,
    pub(crate) call_id: i32,
    pub(crate) peer: &'a str,
}

/// One call's telemetry. [`open`](CallObs::open) it once its header is
/// known, finish it with [`finish`](CallObs::finish); one dropped unfinished
/// counts as a failed call (no event: there is nothing to describe it with).
pub(crate) struct CallObs {
    started: Option<Instant>,
    done: bool,
    opened: bool,
    /// The `rpc` span; `Span::none()` until opened and after finishing.
    span: Span,
    #[cfg(feature = "otel")]
    otel: Option<otel::CallOtel>,
}

impl CallObs {
    /// The header is decoded: create the `rpc` span (with OTel tracing on, at
    /// INFO with its remote parent from the metadata, which then loses its
    /// reserved trace keys, design §5.1/§5.2), and count the call as started
    /// under its service and method. `pattern` is the method's pattern when
    /// routing knows it. Later calls are no-ops.
    pub(crate) fn open(
        &mut self,
        header: &mut StreamHeader,
        peer: &str,
        pattern: Option<&'static str>,
    ) {
        self.open_inner(header, peer, pattern, true);
    }

    /// [`open`](CallObs::open) for a call refused before routing found its
    /// service and method: the names are caller-chosen, so metrics label it
    /// `_OTHER` (the span and log still show them).
    pub(crate) fn open_unrouted(&mut self, header: &mut StreamHeader, peer: &str) {
        self.open_inner(header, peer, None, false);
    }

    fn open_inner(
        &mut self,
        header: &mut StreamHeader,
        peer: &str,
        pattern: Option<&'static str>,
        known: bool,
    ) {
        if self.opened {
            return;
        }
        self.opened = true;
        #[cfg(feature = "otel")]
        if let Some(o) = &mut self.otel {
            if let Some(span) = o.open(header, peer, pattern, known) {
                self.span = span;
                return;
            }
        }
        let _ = (pattern, known);
        self.span = tracing::debug_span!(
            target: DISPATCH,
            "rpc",
            service = header.service.as_str(),
            method = header.method.as_str(),
            call_id = header.call_id,
            peer,
        );
    }

    /// The call passed every gate and goes to its handler: evaluate the
    /// attribute extractors (design §4.4) onto the span.
    pub(crate) fn dispatching(
        &mut self,
        header: &StreamHeader,
        peer: &str,
        attributes: &HashMap<String, String>,
    ) {
        #[cfg(feature = "otel")]
        if let Some(o) = &mut self.otel {
            o.dispatching(&self.span, header, peer, attributes);
        }
        #[cfg(not(feature = "otel"))]
        let _ = (header, peer, attributes);
    }

    /// Whether opening needs the decoded header even when no debug log
    /// wants it (OTel records the call).
    pub(crate) fn wants_header(&self) -> bool {
        #[cfg(feature = "otel")]
        return self.otel.is_some();
        #[cfg(not(feature = "otel"))]
        false
    }

    /// The call's `rpc` span (disabled when nothing records it).
    pub(crate) fn span(&self) -> &Span {
        &self.span
    }

    /// The OTel context of the call's span when it is exported (sampled or
    /// recorded), for a foreign call's binding (design §5.2).
    pub(crate) fn trace_context(&self) -> Option<super::foreign::CallTraceContext> {
        #[cfg(feature = "otel")]
        if self.otel.is_some() && !self.span.is_disabled() {
            return otel::span_trace_context(&self.span);
        }
        None
    }

    /// Add an OTel event to the span (cancel and deadline, design §5.2).
    pub(crate) fn event(&self, name: &'static str) {
        #[cfg(feature = "otel")]
        if self.otel.is_some() {
            otel::span_event(&self.span, name);
        }
        #[cfg(not(feature = "otel"))]
        let _ = name;
    }

    /// Record the call's final status and end its span. Later calls are
    /// no-ops.
    pub(crate) fn finish(&mut self, code: StatusCode, info: CallInfo<'_>) {
        if !self.record(code) {
            return;
        }
        if let Some(started) = self.started {
            self.span.in_scope(|| {
                tracing::debug!(
                    target: DISPATCH,
                    service = info.service,
                    method = info.method,
                    call_id = info.call_id,
                    peer = info.peer,
                    status = ?code,
                    elapsed_us = started.elapsed().as_micros() as u64,
                    "rpc completed"
                )
            });
        }
        self.span = Span::none();
    }

    /// Counters, OTel instruments and span status. Returns whether this was
    /// the first finish.
    fn record(&mut self, code: StatusCode) -> bool {
        if self.done {
            return false;
        }
        self.done = true;
        #[cfg(feature = "otel")]
        if let Some(o) = &self.otel {
            o.finish(&self.span, code, self.started);
        }
        #[cfg(not(feature = "otel"))]
        let _ = code;
        true
    }
}

impl Drop for CallObs {
    fn drop(&mut self) {
        self.record(StatusCode::Internal);
    }
}

/// One client call's telemetry (design §4.1 client instruments, §5.4): the
/// client span and its trace context injected into the outgoing header.
/// Finish it with [`finish`](ClientObs::finish); dropped unfinished, the call
/// counts as cancelled. Without the `otel` feature, or with no pipeline, it
/// is empty and the header goes out unchanged (§5.1 pass-through).
pub(crate) struct ClientObs {
    #[cfg(feature = "otel")]
    inner: Option<otel::ClientOtel>,
}

impl ClientObs {
    /// Start a call whose header (options already applied) is `header`.
    #[inline]
    pub(crate) fn start(header: &mut StreamHeader) -> Self {
        #[cfg(not(feature = "otel"))]
        let _ = header;
        ClientObs {
            #[cfg(feature = "otel")]
            inner: otel::ClientOtel::start(header),
        }
    }

    /// The client span, or a disabled one.
    pub(crate) fn span(&self) -> Span {
        #[cfg(feature = "otel")]
        if let Some(i) = &self.inner {
            return i.span.clone();
        }
        Span::none()
    }

    /// Record the call's final status (first one wins).
    pub(crate) fn finish(&mut self, code: StatusCode) {
        #[cfg(feature = "otel")]
        if let Some(i) = self.inner.take() {
            i.finish(code);
        }
        #[cfg(not(feature = "otel"))]
        let _ = code;
    }
}

impl Drop for ClientObs {
    fn drop(&mut self) {
        self.finish(StatusCode::Cancelled);
    }
}

/// The status a client call ended with, from its result.
pub(crate) fn client_status<T>(r: &crate::error::Result<T>) -> StatusCode {
    match r {
        Ok(_) => StatusCode::Ok,
        Err(e) => error_status(e),
    }
}

/// The status a client call that failed with `e` ended with.
pub(crate) fn error_status(e: &crate::error::Error) -> StatusCode {
    use crate::error::Error;
    match e {
        Error::Rpc { code, .. } => StatusCode::from_i32(*code),
        Error::Connection(_) | Error::Transport(_) => StatusCode::Unavailable,
        _ => StatusCode::Unknown,
    }
}

/// The gRPC-style name of a status code (the `aster.status` attribute).
#[cfg(feature = "otel")]
pub(crate) fn status_name(code: StatusCode) -> &'static str {
    match code {
        StatusCode::Ok => "OK",
        StatusCode::Cancelled => "CANCELLED",
        StatusCode::Unknown => "UNKNOWN",
        StatusCode::InvalidArgument => "INVALID_ARGUMENT",
        StatusCode::DeadlineExceeded => "DEADLINE_EXCEEDED",
        StatusCode::NotFound => "NOT_FOUND",
        StatusCode::AlreadyExists => "ALREADY_EXISTS",
        StatusCode::PermissionDenied => "PERMISSION_DENIED",
        StatusCode::ResourceExhausted => "RESOURCE_EXHAUSTED",
        StatusCode::FailedPrecondition => "FAILED_PRECONDITION",
        StatusCode::Aborted => "ABORTED",
        StatusCode::OutOfRange => "OUT_OF_RANGE",
        StatusCode::Unimplemented => "UNIMPLEMENTED",
        StatusCode::Internal => "INTERNAL",
        StatusCode::Unavailable => "UNAVAILABLE",
        StatusCode::DataLoss => "DATA_LOSS",
        StatusCode::Unauthenticated => "UNAUTHENTICATED",
        StatusCode::ContractViolation => "CONTRACT_VIOLATION",
    }
}
