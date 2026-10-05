//! Spans a binding creates (design §5.3): live spans by `u64` handle in the
//! Rust SDK tracer (scope `aster.app`), exported with Aster's own spans and
//! sharing their sampling, plus finished spans a binding recorded itself
//! ([`span_record`], the .NET `Activity` bridge). The C ABI's `aster_span_*`.
//!
//! The live-span table is bounded ([`MAX_LIVE_SPANS`]); while it is full,
//! [`span_start`] fails with [`Error::QueueFull`]. The pipeline's teardown
//! ends every span still open (status unset) and clears the table (§2.4): C
//! has no RAII, so a leaked handle ends there at the latest.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::global::{self, BoxedSpan, BoxedTracer};
use opentelemetry::trace::noop::NoopTracer;
use opentelemetry::trace::{
    Event, Span as _, SpanBuilder, SpanId, SpanKind as OtelKind, Status, TraceContextExt, TraceId,
    Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator};

use crate::error::{Error, Result};
use crate::propagation::TraceContext;

/// Live binding spans at most; [`span_start`] fails with
/// [`Error::QueueFull`] beyond it.
pub const MAX_LIVE_SPANS: usize = 65_536;

/// Span kind, numbered as the C ABI's `aster_span_kind_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum SpanKind {
    #[default]
    Internal = 0,
    Server = 1,
    Client = 2,
    Producer = 3,
    Consumer = 4,
}

impl SpanKind {
    /// From the C ABI value.
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => SpanKind::Internal,
            1 => SpanKind::Server,
            2 => SpanKind::Client,
            3 => SpanKind::Producer,
            4 => SpanKind::Consumer,
            _ => return None,
        })
    }

    fn otel(self) -> OtelKind {
        match self {
            SpanKind::Internal => OtelKind::Internal,
            SpanKind::Server => OtelKind::Server,
            SpanKind::Client => OtelKind::Client,
            SpanKind::Producer => OtelKind::Producer,
            SpanKind::Consumer => OtelKind::Consumer,
        }
    }
}

/// A span status code: 0 unset, 1 ok, 2 error (the C ABI's numbering).
fn status(code: u32, message: &str) -> Result<Status> {
    Ok(match code {
        0 => Status::Unset,
        1 => Status::Ok,
        2 => Status::error(message.to_owned()),
        other => {
            return Err(Error::InvalidArgument(format!(
                "span status code {other} (0 unset, 1 ok, 2 error)"
            )))
        }
    })
}

fn kvs(pairs: &[(String, String)]) -> Vec<KeyValue> {
    pairs
        .iter()
        .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
        .collect()
}

// ── the tracer ──────────────────────────────────────────────────────────────

/// The `aster.app` tracer of the current generation's global provider.
static TRACER: RwLock<Option<(u64, std::sync::Arc<BoxedTracer>)>> = RwLock::new(None);

fn tracer() -> std::sync::Arc<BoxedTracer> {
    if crate::fork::forked() {
        return std::sync::Arc::new(BoxedTracer::new(Box::new(NoopTracer::new())));
    }
    let generation = crate::generation();
    if let Some((g, t)) = &*TRACER.read().unwrap_or_else(|e| e.into_inner()) {
        if *g == generation {
            return t.clone();
        }
    }
    let t = std::sync::Arc::new(
        global::tracer_provider().tracer_with_scope(crate::instruments::app_scope()),
    );
    *TRACER.write().unwrap_or_else(|e| e.into_inner()) = Some((generation, t.clone()));
    t
}

fn parent_context(parent: Option<&TraceContext>) -> Context {
    match parent {
        Some(p) if p.is_valid() => Context::new().with_remote_span_context(p.to_span_context()),
        _ => Context::new(),
    }
}

// ── the live-span table ─────────────────────────────────────────────────────

const SHARDS: usize = 16;

struct LiveSpans {
    shards: [Mutex<HashMap<u64, BoxedSpan>>; SHARDS],
    live: AtomicUsize,
}

fn live() -> &'static LiveSpans {
    static LIVE: std::sync::OnceLock<LiveSpans> = std::sync::OnceLock::new();
    LIVE.get_or_init(|| LiveSpans {
        shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
        live: AtomicUsize::new(0),
    })
}

static NEXT: AtomicU64 = AtomicU64::new(1);

fn shard(handle: u64) -> std::sync::MutexGuard<'static, HashMap<u64, BoxedSpan>> {
    live().shards[(handle as usize) % SHARDS]
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn with_span<R>(handle: u64, f: impl FnOnce(&mut BoxedSpan) -> R) -> Result<R> {
    let mut g = shard(handle);
    let span = g.get_mut(&handle).ok_or(Error::NotFound("span"))?;
    Ok(f(span))
}

/// Start a span named `name` under `parent` (`None` or an invalid context: a
/// root), with string attributes. Returns its handle (never 0, never reused).
/// Without a pipeline the span is a no-op whose context is invalid.
pub fn span_start(
    name: &str,
    kind: SpanKind,
    parent: Option<&TraceContext>,
    attributes: &[(String, String)],
) -> Result<u64> {
    let l = live();
    if l.live.fetch_add(1, Ordering::AcqRel) >= MAX_LIVE_SPANS {
        l.live.fetch_sub(1, Ordering::AcqRel);
        return Err(Error::QueueFull("live span table"));
    }
    let builder = SpanBuilder::from_name(name.to_owned())
        .with_kind(kind.otel())
        .with_attributes(kvs(attributes));
    let span = tracer().build_with_context(builder, &parent_context(parent));
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    shard(id).insert(id, span);
    Ok(id)
}

/// Add string attributes to a live span.
pub fn span_set_attributes(handle: u64, attributes: &[(String, String)]) -> Result<()> {
    with_span(handle, |s| s.set_attributes(kvs(attributes)))
}

/// Add an event (now) to a live span.
pub fn span_add_event(handle: u64, name: &str, attributes: &[(String, String)]) -> Result<()> {
    with_span(handle, |s| s.add_event(name.to_owned(), kvs(attributes)))
}

/// Set a live span's status: 0 unset, 1 ok, 2 error with `message`.
pub fn span_set_status(handle: u64, code: u32, message: &str) -> Result<()> {
    let st = status(code, message)?;
    with_span(handle, |s| s.set_status(st))
}

/// Record an error on a live span: an `exception` event (`exception.type`,
/// `exception.message`) and status error.
pub fn span_record_error(handle: u64, error_type: &str, message: &str) -> Result<()> {
    with_span(handle, |s| {
        let mut attrs = Vec::with_capacity(2);
        if !error_type.is_empty() {
            attrs.push(KeyValue::new("exception.type", error_type.to_owned()));
        }
        attrs.push(KeyValue::new("exception.message", message.to_owned()));
        s.add_event("exception", attrs);
        s.set_status(Status::error(message.to_owned()));
    })
}

/// A live span's context (invalid for a no-op span).
pub fn span_context(handle: u64) -> Result<TraceContext> {
    with_span(handle, |s| {
        TraceContext::from_span_context(s.span_context()).unwrap_or_default()
    })
}

/// End a live span and free its handle.
pub fn span_end(handle: u64) -> Result<()> {
    let span = shard(handle).remove(&handle);
    let mut span = span.ok_or(Error::NotFound("span"))?;
    live().live.fetch_sub(1, Ordering::AcqRel);
    span.end();
    Ok(())
}

/// Live spans (tests, diagnostics).
pub fn live_spans() -> usize {
    live().live.load(Ordering::Acquire)
}

/// End every live span (status unset) and clear the table: pipeline
/// teardown (§2.4).
pub(crate) fn end_all() {
    for s in &live().shards {
        let spans: Vec<BoxedSpan> = {
            let mut g = s.lock().unwrap_or_else(|e| e.into_inner());
            g.drain().map(|(_, v)| v).collect()
        };
        live().live.fetch_sub(spans.len(), Ordering::AcqRel);
        for mut span in spans {
            span.end();
        }
    }
}

// ── finished spans recorded by the binding ──────────────────────────────────

/// One event of a [`SpanRecord`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanEventRecord {
    pub name: String,
    pub time_unix_nanos: u64,
    pub attributes: Vec<(String, String)>,
}

/// A finished span whose ids, times and content the binding owns (the .NET
/// `Activity` bridge, design §5.3). `context` carries its trace id, span id
/// and flags; an invalid `parent` makes it a root.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanRecord {
    pub context: TraceContext,
    pub parent: TraceContext,
    pub start_unix_nanos: u64,
    pub end_unix_nanos: u64,
    pub kind: SpanKind,
    /// 0 unset, 1 ok, 2 error.
    pub status_code: u32,
    pub status_message: String,
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub events: Vec<SpanEventRecord>,
}

thread_local! {
    /// Ids the next span built on this thread takes (`span_record`).
    static FORCED: Cell<Option<([u8; 16], [u8; 8])>> = const { Cell::new(None) };
}

/// The pipeline's id generator: random ids, except while [`span_record`]
/// builds a span on this thread with the binding's own ids.
#[derive(Debug, Default)]
pub(crate) struct AsterIdGenerator {
    random: RandomIdGenerator,
}

impl IdGenerator for AsterIdGenerator {
    fn new_trace_id(&self) -> TraceId {
        match FORCED.with(Cell::get) {
            Some((t, _)) => TraceId::from_bytes(t),
            None => self.random.new_trace_id(),
        }
    }

    fn new_span_id(&self) -> SpanId {
        match FORCED.with(Cell::get) {
            Some((_, s)) => SpanId::from_bytes(s),
            None => self.random.new_span_id(),
        }
    }
}

fn system_time(unix_nanos: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(unix_nanos)
}

/// Export a finished span recorded by the binding, with its own ids and
/// times. Copies everything; never blocks (the batch processor drops when
/// full). A span whose sampled flag is clear is not exported. The pipeline's
/// sampler still runs (with the default parent-based sampler it follows the
/// parent's flag). [`Error::InvalidArgument`] for an invalid context, a
/// parent from another trace, an end before the start, or a bad status code.
pub fn span_record(rec: &SpanRecord) -> Result<()> {
    if !rec.context.is_valid() {
        return Err(Error::InvalidArgument(
            "span record: context has an all-zero trace or span id".into(),
        ));
    }
    if rec.parent.is_valid() && rec.parent.trace_id != rec.context.trace_id {
        return Err(Error::InvalidArgument(
            "span record: parent belongs to another trace".into(),
        ));
    }
    if rec.end_unix_nanos < rec.start_unix_nanos {
        return Err(Error::InvalidArgument(
            "span record: end before start".into(),
        ));
    }
    let st = status(rec.status_code, &rec.status_message)?;
    if !rec.context.is_sampled() {
        return Ok(());
    }
    let events = rec
        .events
        .iter()
        .map(|e| {
            Event::new(
                e.name.clone(),
                system_time(e.time_unix_nanos),
                kvs(&e.attributes),
                0,
            )
        })
        .collect();
    let builder = SpanBuilder::from_name(rec.name.clone())
        .with_kind(rec.kind.otel())
        .with_start_time(system_time(rec.start_unix_nanos))
        .with_attributes(kvs(&rec.attributes))
        .with_events(events);
    let cx = parent_context(Some(&rec.parent));
    let tracer = tracer();
    FORCED.with(|f| f.set(Some((rec.context.trace_id, rec.context.span_id))));
    let mut span = tracer.build_with_context(builder, &cx);
    FORCED.with(|f| f.set(None));
    span.set_status(st);
    span.end_with_timestamp(system_time(rec.end_unix_nanos));
    Ok(())
}
