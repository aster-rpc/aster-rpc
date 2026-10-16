//! The `otel` half of [`super`]: the built-in instruments (design §4.1),
//! cached per provider generation (§4.1a), the server and client spans and
//! W3C trace context on the wire (§5), and the attribute extractors (§4.4).
//!
//! Everything here goes through the OpenTelemetry API that `aster-otel`
//! re-exports. With no pipeline, [`builtins`] returns `None` after one relaxed
//! load of [`aster_otel::generation`] and nothing else runs: no span, no
//! attribute, no allocation, and trace metadata passes through untouched
//! (§5.1).
//!
//! Record path (§4.2): the per-call service and method labels are interned
//! [`KeyValue`]s (cloning one is a reference-count bump), and the status and
//! pattern are `&'static str`, so recording a known series allocates nothing;
//! the SDK takes its short read lock per update, and its write lock (plus an
//! allocation) only the first time a series is seen.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use aster_otel::opentelemetry::baggage::{BaggageExt, KeyValueMetadata};
use aster_otel::opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};
use aster_otel::opentelemetry::propagation::{Extractor, Injector};
use aster_otel::opentelemetry::trace::{Status, TraceContextExt};
use aster_otel::opentelemetry::{global, Context, KeyValue};
use aster_otel::{AttributeExtractor, ExtractorSource, OpenTelemetrySpanExt, TraceContext};
use tracing::Span;

use super::status_name;
use crate::log_targets::{CLIENT, DISPATCH};
use crate::rpc::codec::StreamHeader;
use crate::rpc::contract::MethodPattern;
use crate::rpc::foreign::InboundTrace;
use crate::rpc::status::StatusCode;

// ── names ───────────────────────────────────────────────────────────────────

const RPC_SERVICE: &str = "rpc.service";
const RPC_METHOD: &str = "rpc.method";
const PATTERN: &str = "aster.pattern";
const STATUS: &str = "aster.status";
const ROLE: &str = "aster.role";
const RESULT: &str = "aster.result";
#[cfg(feature = "binding-support")]
const UPCALL_KIND: &str = "aster.upcall.kind";
/// The label for a service or method the server does not know (refused
/// before routing found it): caller-chosen names must not mint series.
const OTHER: &str = "_OTHER";

/// The W3C keys that carry trace context in call metadata (design §5.1).
const TRACEPARENT: &str = "traceparent";
const TRACESTATE: &str = "tracestate";
const BAGGAGE: &str = "baggage";
/// W3C baggage limits: a larger header is dropped, not truncated.
const BAGGAGE_MAX_BYTES: usize = 8192;
const BAGGAGE_MAX_MEMBERS: usize = 64;

/// Bound on interned (service, method) label pairs. Past it, labels are
/// built per call (they still record; only the allocation-free path ends).
const MAX_LABELS: usize = 4096;

/// Duration buckets in seconds (the OTel semantic-convention RPC buckets);
/// the SDK default is millisecond-scaled. Shared with binding-declared
/// histograms in seconds.
use aster_otel::instruments::SECONDS_BUCKETS;
const MS_BUCKETS: [f64; 12] = [
    1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0,
];

// ── instruments, cached per generation (§4.1a) ──────────────────────────────

/// The built-in instruments (§4.1) from one provider generation.
pub(crate) struct Instruments {
    server_started: Counter<u64>,
    server_completed: Counter<u64>,
    server_duration: Histogram<f64>,
    server_in_flight: UpDownCounter<i64>,
    client_completed: Counter<u64>,
    client_duration: Histogram<f64>,
    streams_active: UpDownCounter<i64>,
    connections_active: UpDownCounter<i64>,
    connections_opened: Counter<u64>,
    admission_decisions: Counter<u64>,
    admission_duration: Histogram<f64>,
    #[cfg(feature = "binding-support")]
    upcall_issued: Counter<u64>,
    #[cfg(feature = "binding-support")]
    upcall_timeouts: Counter<u64>,
    #[cfg(feature = "binding-support")]
    upcall_fallbacks: Counter<u64>,
}

impl Instruments {
    fn new(m: &Meter) -> Self {
        Instruments {
            server_started: m
                .u64_counter("aster.rpc.server.requests_started")
                .with_description("RPC calls that reached the server's dispatcher")
                .build(),
            server_completed: m
                .u64_counter("aster.rpc.server.requests_completed")
                .with_description("RPC calls the server finished, by final status")
                .build(),
            server_duration: m
                .f64_histogram("aster.rpc.server.duration")
                .with_description("Time from a call's arrival to its final status")
                .with_unit("s")
                .with_boundaries(SECONDS_BUCKETS.to_vec())
                .build(),
            server_in_flight: m
                .i64_up_down_counter("aster.rpc.server.in_flight")
                .with_description("RPC calls routed and not yet finished")
                .build(),
            client_completed: m
                .u64_counter("aster.rpc.client.requests_completed")
                .with_description("RPC calls this process made, by final status")
                .build(),
            client_duration: m
                .f64_histogram("aster.rpc.client.duration")
                .with_description("Time from issuing a call to its final status")
                .with_unit("s")
                .with_boundaries(SECONDS_BUCKETS.to_vec())
                .build(),
            streams_active: m
                .i64_up_down_counter("aster.rpc.streams.active")
                .with_description("RPC streams the server is handling")
                .build(),
            connections_active: m
                .i64_up_down_counter("aster.server.connections.active")
                .with_description("aster/1 connections with at least one call, still open")
                .build(),
            connections_opened: m
                .u64_counter("aster.server.connections.opened")
                .with_description("aster/1 connections that delivered a call")
                .build(),
            admission_decisions: m
                .u64_counter("aster.admission.decisions")
                .with_description("Gate 0 admission decisions")
                .build(),
            admission_duration: m
                .f64_histogram("aster.admission.duration")
                .with_description("Time from a Gate 0 request to its decision")
                .with_unit("ms")
                .with_boundaries(MS_BUCKETS.to_vec())
                .build(),
            #[cfg(feature = "binding-support")]
            upcall_issued: m
                .u64_counter("aster.upcall.issued")
                .with_description("Upcalls delivered to binding code")
                .build(),
            #[cfg(feature = "binding-support")]
            upcall_timeouts: m
                .u64_counter("aster.upcall.timeouts")
                .with_description("Upcalls the binding did not answer in time")
                .build(),
            #[cfg(feature = "binding-support")]
            upcall_fallbacks: m
                .u64_counter("aster.upcall.fallbacks")
                .with_description("Decision upcalls answered by their failure mode")
                .build(),
        }
    }
}

/// What one provider generation records: its instruments (when it has a
/// meter provider), whether it traces, and its extractors.
pub(crate) struct Builtins {
    generation: u64,
    traces: bool,
    metrics: Option<Instruments>,
    extractors: Vec<AttributeExtractor>,
    propagate: Vec<String>,
}

impl Builtins {
    fn build(generation: u64) -> Self {
        match aster_otel::state() {
            Some(s) if s.generation == generation => Builtins {
                generation,
                traces: s.traces,
                metrics: s.metrics.then(|| {
                    let meter = global::meter(aster_otel::SCOPE_NAME);
                    #[cfg(feature = "health")]
                    crate::ops::register_observables(generation, &meter);
                    Instruments::new(&meter)
                }),
                extractors: s.attribute_extractors,
                propagate: s.propagate_as_baggage,
            },
            // No pipeline, or one that came or went since `generation` was
            // read (the next record sees the new generation and rebuilds).
            _ => Builtins {
                generation,
                traces: false,
                metrics: None,
                extractors: Vec::new(),
                propagate: Vec::new(),
            },
        }
    }

    fn active(&self) -> bool {
        self.traces || self.metrics.is_some()
    }
}

static CACHE: ArcSwapOption<Builtins> = ArcSwapOption::const_empty();

/// The current generation's built-ins, or `None` when there is nothing to
/// record. Steady state: one relaxed load and one `ArcSwap` load; a new
/// generation rebuilds the instruments from the current global meter once.
pub(crate) fn builtins() -> Option<Arc<Builtins>> {
    let generation = aster_otel::generation();
    {
        let cur = CACHE.load();
        if let Some(b) = cur.as_ref() {
            if b.generation == generation {
                return b.active().then(|| Arc::clone(b));
            }
        }
    }
    let b = Arc::new(Builtins::build(generation));
    CACHE.store(Some(b.clone()));
    b.active().then_some(b)
}

// ── interned labels ─────────────────────────────────────────────────────────

/// A call's `rpc.service` / `rpc.method` attributes, shared by every call of
/// that method.
pub(crate) struct Labels {
    service: KeyValue,
    method: KeyValue,
}

impl Labels {
    fn new(service: &str, method: &str) -> Self {
        Labels {
            service: KeyValue::new(RPC_SERVICE, Arc::<str>::from(service)),
            method: KeyValue::new(RPC_METHOD, Arc::<str>::from(method)),
        }
    }
}

type LabelTable = HashMap<Box<str>, HashMap<Box<str>, Arc<Labels>>>;

fn label_table() -> &'static RwLock<(LabelTable, usize)> {
    static TABLE: OnceLock<RwLock<(LabelTable, usize)>> = OnceLock::new();
    TABLE.get_or_init(|| RwLock::new((HashMap::new(), 0)))
}

fn labels(service: &str, method: &str) -> Arc<Labels> {
    let table = label_table();
    if let Some(l) = table
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .get(service)
        .and_then(|m| m.get(method))
    {
        return l.clone();
    }
    let mut g = table.write().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = g.0.get(service).and_then(|m| m.get(method)) {
        return l.clone();
    }
    let l = Arc::new(Labels::new(service, method));
    if g.1 < MAX_LABELS {
        g.1 += 1;
        g.0.entry(service.into())
            .or_default()
            .insert(method.into(), l.clone());
    }
    l
}

fn other_labels() -> Arc<Labels> {
    static OTHER_LABELS: OnceLock<Arc<Labels>> = OnceLock::new();
    OTHER_LABELS
        .get_or_init(|| {
            Arc::new(Labels {
                service: KeyValue::new(RPC_SERVICE, OTHER),
                method: KeyValue::new(RPC_METHOD, OTHER),
            })
        })
        .clone()
}

pub(crate) fn pattern_name(p: MethodPattern) -> &'static str {
    match p {
        MethodPattern::Unary => "unary",
        MethodPattern::ServerStream => "server_stream",
        MethodPattern::ClientStream => "client_stream",
        MethodPattern::BidiStream => "bidi_stream",
    }
}

// ── trace context in metadata (§5.1) ────────────────────────────────────────

fn is_reserved(key: &str) -> bool {
    key == TRACEPARENT || key == TRACESTATE || key == BAGGAGE
}

fn has_trace_keys(h: &StreamHeader) -> bool {
    h.metadata_keys.iter().any(|k| is_reserved(k))
}

/// Reads the reserved keys from a header's metadata, applying the W3C
/// baggage limits.
struct HeaderCarrier<'a>(&'a StreamHeader);

impl Extractor for HeaderCarrier<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        let v = self
            .0
            .metadata_keys
            .iter()
            .zip(self.0.metadata_values.iter())
            .find(|(k, _)| k.as_str() == key)
            .map(|(_, v)| v.as_str())?;
        if key == BAGGAGE
            && (v.len() > BAGGAGE_MAX_BYTES || v.split(',').count() > BAGGAGE_MAX_MEMBERS)
        {
            tracing::debug!(
                target: DISPATCH,
                bytes = v.len(),
                "baggage over the W3C limits (8192 bytes, 64 members); dropped"
            );
            return None;
        }
        Some(v)
    }

    fn keys(&self) -> Vec<&str> {
        self.0.metadata_keys.iter().map(String::as_str).collect()
    }
}

/// Writes the propagator's keys into a header's metadata, replacing any the
/// caller set.
struct HeaderInjector<'a>(&'a mut StreamHeader);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        let h = &mut *self.0;
        match h.metadata_keys.iter().position(|k| k == key) {
            Some(i) => h.metadata_values[i] = value,
            None => {
                h.metadata_keys.push(key.to_owned());
                h.metadata_values.push(value);
            }
        }
    }
}

/// The raw `baggage` (within the W3C limits) and `tracestate` of `h`.
fn inbound_of(h: &StreamHeader) -> Option<InboundTrace> {
    let c = HeaderCarrier(h);
    let baggage = c
        .get(BAGGAGE)
        .filter(|v| !v.is_empty())
        .map(Arc::<str>::from);
    let tracestate = c
        .get(TRACESTATE)
        .filter(|v| !v.is_empty())
        .map(Arc::<str>::from);
    (baggage.is_some() || tracestate.is_some()).then_some(InboundTrace {
        baggage,
        tracestate,
    })
}

/// A call's trace context when no recording span carries it: the inbound
/// `traceparent` parsed directly (no propagator, no pipeline needed), like an
/// OTel non-recording span, and the raw `baggage` / `tracestate`.
#[derive(Clone)]
pub(crate) struct Pass {
    pub(crate) ctx: Option<TraceContext>,
    pub(crate) inbound: Option<InboundTrace>,
}

tokio::task_local! {
    /// The pass-through context of the Rust-native handler running in this
    /// task, so the client calls it makes continue the caller's trace.
    static PASS: Pass;
}

/// Read, parse and strip a header's reserved trace keys. `None` (and the
/// header untouched) when it has none: one metadata key scan.
pub(crate) fn pass_from_header(h: &mut StreamHeader) -> Option<Pass> {
    if !has_trace_keys(h) {
        return None;
    }
    let tp = metadata(h, TRACEPARENT);
    let ctx = tp.and_then(|v| TraceContext::parse_traceparent(v.as_bytes()));
    if tp.is_some() && ctx.is_none() {
        tracing::debug!(target: DISPATCH, "malformed traceparent ignored; the call starts a new trace");
    }
    let inbound = inbound_of(h);
    strip_trace_keys(h);
    Some(Pass { ctx, inbound })
}

pub(crate) fn call_trace_context(c: &TraceContext) -> crate::rpc::foreign::CallTraceContext {
    crate::rpc::foreign::CallTraceContext {
        trace_id: c.trace_id,
        span_id: c.span_id,
        trace_flags: c.trace_flags,
    }
}

/// Run `f` with `pass` as the handler's pass-through context (when it has a
/// valid trace context).
pub(crate) async fn scoped<F: std::future::Future>(pass: Option<Pass>, f: F) -> F::Output {
    match pass {
        Some(p) if p.ctx.is_some() => PASS.scope(p, f).await,
        _ => f.await,
    }
}

/// The running handler's pass-through context, if any.
fn current_pass() -> Option<Pass> {
    PASS.try_with(Clone::clone).ok()
}

/// The trace context of the code running here: the current `tracing` span's
/// OpenTelemetry context when it has one, else the pass-through context of
/// the call a Rust-native handler is serving (the caller's inbound
/// `traceparent`, valid with no pipeline, with traces off, or with the `rpc`
/// span filtered out).
pub fn current_trace_context() -> Option<TraceContext> {
    let cx = Span::current().context();
    TraceContext::from_span_context(cx.span().span_context())
        .or_else(|| current_pass().and_then(|p| p.ctx))
}

/// Run `f` with `ctx` as its trace context: the client calls it makes carry
/// `ctx` (as a child of it when a client span is exported) unless a span with
/// a valid context is current or the call's metadata has its own
/// `traceparent`. For code that continues a trace with no pipeline.
pub async fn with_trace_context<F: std::future::Future>(ctx: TraceContext, f: F) -> F::Output {
    PASS.scope(
        Pass {
            ctx: Some(ctx),
            inbound: None,
        },
        f,
    )
    .await
}

/// Write the running Rust-native handler's pass-through context into an
/// outgoing header that carries no `traceparent` of its own.
pub(crate) fn inject_pass(h: &mut StreamHeader) {
    let Some(p) = current_pass() else { return };
    let Some(ctx) = p.ctx else { return };
    if metadata(h, TRACEPARENT).is_some() {
        return;
    }
    let mut inj = HeaderInjector(h);
    inj.set(TRACEPARENT, ctx.traceparent());
    if let Some(i) = p.inbound {
        if let Some(t) = i.tracestate {
            inj.set(TRACESTATE, t.to_string());
        }
        if let Some(b) = i.baggage {
            inj.set(BAGGAGE, b.to_string());
        }
    }
}

fn extract(h: &StreamHeader) -> Context {
    let cx = global::get_text_map_propagator(|p| p.extract(&HeaderCarrier(h)));
    if !cx.span().span_context().is_valid() && h.metadata_keys.iter().any(|k| k == TRACEPARENT) {
        tracing::debug!(target: DISPATCH, "malformed traceparent ignored; the call starts a new trace");
    }
    cx
}

/// Drop the reserved keys from what handlers and interceptors see (Q7); they
/// read the trace context through the span instead.
fn strip_trace_keys(h: &mut StreamHeader) {
    let keys = std::mem::take(&mut h.metadata_keys);
    let values = std::mem::take(&mut h.metadata_values);
    for (k, v) in keys.into_iter().zip(values) {
        if !is_reserved(&k) {
            h.metadata_keys.push(k);
            h.metadata_values.push(v);
        }
    }
}

// ── extractors (§4.4) ───────────────────────────────────────────────────────

/// What a Rust-native attribute extractor
/// ([`Server::attribute_extractor`](crate::rpc::Server::attribute_extractor))
/// sees of a call that passed every gate.
pub struct ExtractContext<'a> {
    header: &'a StreamHeader,
    peer: &'a str,
    attributes: &'a HashMap<String, String>,
    parent: Option<&'a Context>,
}

impl ExtractContext<'_> {
    pub fn service(&self) -> &str {
        &self.header.service
    }

    pub fn method(&self) -> &str {
        &self.header.method
    }

    /// The caller's peer id (or authenticated principal).
    pub fn peer(&self) -> &str {
        self.peer
    }

    /// Request metadata (the W3C trace keys are not visible here).
    pub fn metadata(&self, key: &str) -> Option<&str> {
        metadata(self.header, key)
    }

    /// The caller's Gate 3 / admission attributes.
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    /// A member of the incoming W3C baggage.
    pub fn baggage(&self, key: &str) -> Option<&str> {
        self.parent?.baggage().get(key).map(|v| v.as_str())
    }
}

type ExtractorFn = Arc<dyn Fn(&ExtractContext<'_>) -> Option<String> + Send + Sync>;

/// The Rust-native extractors registered on a server.
#[derive(Clone, Default)]
pub(crate) struct RustExtractors(Option<Arc<Vec<(String, ExtractorFn)>>>);

impl RustExtractors {
    pub(crate) fn push(&mut self, key: String, f: ExtractorFn) {
        let list = self.0.get_or_insert_with(Default::default);
        Arc::make_mut(list).push((key, f));
    }
}

fn metadata<'a>(h: &'a StreamHeader, key: &str) -> Option<&'a str> {
    h.metadata_keys
        .iter()
        .zip(h.metadata_values.iter())
        .find(|(k, _)| k.as_str() == key)
        .map(|(_, v)| v.as_str())
}

// ── server calls ────────────────────────────────────────────────────────────

/// One server call's OTel state, held by its [`CallObs`](super::CallObs).
pub(crate) struct CallOtel {
    b: Arc<Builtins>,
    rust: RustExtractors,
    labels: Option<Arc<Labels>>,
    /// The remote parent extracted at open, for `baggage:` extractors and
    /// `propagate_as_baggage`.
    parent: Option<Context>,
    /// The raw inbound `baggage` / `tracestate`, read before the reserved
    /// keys were stripped from the header.
    inbound: Option<InboundTrace>,
}

impl CallOtel {
    pub(crate) fn start(rust: &RustExtractors) -> Option<Self> {
        let b = builtins()?;
        if let Some(m) = &b.metrics {
            m.streams_active.add(1, &[]);
        }
        Some(CallOtel {
            b,
            rust: rust.clone(),
            labels: None,
            parent: None,
            inbound: None,
        })
    }

    /// See [`CallObs::inbound_trace`](super::CallObs::inbound_trace).
    pub(crate) fn inbound_trace(&self) -> Option<InboundTrace> {
        self.inbound.clone()
    }

    /// The pass-through context of a call whose span is not enabled: the
    /// remote parent extracted at open.
    pub(crate) fn pass(&self) -> Option<Pass> {
        let ctx = self
            .parent
            .as_ref()
            .and_then(|cx| TraceContext::from_span_context(cx.span().span_context()));
        (ctx.is_some() || self.inbound.is_some()).then(|| Pass {
            ctx,
            inbound: self.inbound.clone(),
        })
    }

    /// Whether the call needs its start time (for the duration histogram).
    pub(crate) fn timed(&self) -> bool {
        self.b.metrics.is_some()
    }

    /// See [`CallObs::open`](super::CallObs::open). `known`: routing found
    /// the service and method, so their names may label metrics. Returns the
    /// span when tracing is on.
    pub(crate) fn open(
        &mut self,
        header: &mut StreamHeader,
        peer: &str,
        pattern: Option<&'static str>,
        known: bool,
    ) -> Option<Span> {
        let labels = if known {
            labels(&header.service, &header.method)
        } else {
            other_labels()
        };
        if let Some(m) = &self.b.metrics {
            let (s, me) = (labels.service.clone(), labels.method.clone());
            match pattern {
                Some(p) => m
                    .server_started
                    .add(1, &[s.clone(), me, KeyValue::new(PATTERN, p)]),
                None => m.server_started.add(1, &[s.clone(), me]),
            }
            m.server_in_flight.add(1, &[s]);
        }
        self.labels = Some(labels);
        if !self.b.traces {
            return None;
        }
        let cx = if has_trace_keys(header) {
            let cx = extract(header);
            self.inbound = inbound_of(header);
            strip_trace_keys(header);
            cx
        } else {
            Context::new()
        };
        let span = tracing::info_span!(
            target: DISPATCH,
            "rpc",
            otel.name = %format_args!("{}/{}", header.service, header.method),
            otel.kind = "server",
            rpc.system = "aster",
            rpc.service = header.service.as_str(),
            rpc.method = header.method.as_str(),
            aster.call_id = header.call_id,
            aster.peer = peer,
        );
        // A call is a trace root unless the caller sent a parent: the
        // dispatching task's own span context is not the caller's.
        let _ = span.set_parent(cx.clone());
        self.parent = Some(cx);
        Some(span)
    }

    /// A forwarded call's audit fields on its OTel span.
    pub(crate) fn on_behalf_of(
        &self,
        span: &Span,
        identity: &crate::rpc::identity::ForwardedIdentity,
    ) {
        if !self.b.traces || span.is_disabled() {
            return;
        }
        span.set_attribute("aster.principal", identity.principal().to_string());
        span.set_attribute("aster.actor", identity.actor().to_string());
        span.set_attribute("aster.on_behalf_of.iss", identity.issuer().to_string());
        span.set_attribute("aster.on_behalf_of.jti", identity.jti().to_string());
    }

    /// Evaluate the extractors onto `span` (§4.4); those named in
    /// `propagate_as_baggage` also go into the context outbound calls made
    /// under the span carry.
    pub(crate) fn dispatching(
        &mut self,
        span: &Span,
        header: &StreamHeader,
        peer: &str,
        attributes: &HashMap<String, String>,
    ) {
        if !self.b.traces
            || span.is_disabled()
            || (self.b.extractors.is_empty() && self.rust.0.is_none())
        {
            return;
        }
        let parent = self.parent.as_ref();
        let mut propagate: Vec<(String, String)> = Vec::new();
        for e in &self.b.extractors {
            let value = match &e.source {
                ExtractorSource::Attribute(k) => attributes.get(k).cloned(),
                ExtractorSource::Metadata(k) => metadata(header, k).map(str::to_owned),
                ExtractorSource::Baggage(k) => {
                    parent.and_then(|cx| cx.baggage().get(k).map(|v| v.as_str().to_owned()))
                }
            };
            let Some(value) = value else { continue };
            if self.b.propagate.contains(&e.key) {
                propagate.push((e.key.clone(), value.clone()));
            }
            span.set_attribute(e.key.clone(), value);
        }
        if let Some(rust) = &self.rust.0 {
            let ctx = ExtractContext {
                header,
                peer,
                attributes,
                parent,
            };
            for (key, f) in rust.iter() {
                if let Some(value) = f(&ctx) {
                    span.set_attribute(key.clone(), value);
                }
            }
        }
        if !propagate.is_empty() {
            let cx = parent.cloned().unwrap_or_default();
            let mut bag: Vec<KeyValueMetadata> = cx
                .baggage()
                .iter()
                .filter(|(k, _)| !propagate.iter().any(|(p, _)| p.as_str() == k.as_str()))
                .map(|(k, (v, m))| KeyValueMetadata::new(k.clone(), v.clone(), m.clone()))
                .collect();
            bag.extend(
                propagate
                    .into_iter()
                    .map(|(k, v)| KeyValue::new(k, v).into()),
            );
            // Still a builder (the span has not been entered): the parent
            // can be replaced, and the span's context inherits the baggage.
            let _ = span.set_parent(cx.with_baggage(bag));
        }
    }

    pub(crate) fn finish(&self, span: &Span, code: StatusCode, started: Option<Instant>) {
        let status = status_name(code);
        if let Some(m) = &self.b.metrics {
            m.streams_active.add(-1, &[]);
            if let Some(l) = &self.labels {
                let attrs = [
                    l.service.clone(),
                    l.method.clone(),
                    KeyValue::new(STATUS, status),
                ];
                m.server_completed.add(1, &attrs);
                if let Some(s) = started {
                    m.server_duration.record(s.elapsed().as_secs_f64(), &attrs);
                }
                m.server_in_flight.add(-1, &attrs[..1]);
            }
        }
        if self.b.traces && !span.is_disabled() {
            span_status(span, code);
        }
    }
}

fn span_status(span: &Span, code: StatusCode) {
    let status = status_name(code);
    span.set_attribute(STATUS, status);
    span.set_status(if code == StatusCode::Ok {
        Status::Ok
    } else {
        Status::error(status)
    });
}

/// An OTel event on `span` (no attributes).
/// The OTel span context behind `span`, when it is valid.
pub(crate) fn span_trace_context(span: &Span) -> Option<crate::rpc::foreign::CallTraceContext> {
    let cx = span.context();
    let otel_span = cx.span();
    let sc = otel_span.span_context();
    sc.is_valid()
        .then(|| crate::rpc::foreign::CallTraceContext {
            trace_id: sc.trace_id().to_bytes(),
            span_id: sc.span_id().to_bytes(),
            trace_flags: sc.trace_flags().to_u8(),
        })
}

pub(crate) fn span_event(span: &Span, name: &'static str) {
    if !span.is_disabled() {
        span.add_event(name, Vec::new());
    }
}

// ── connections and admission ───────────────────────────────────────────────

pub(crate) fn connection_opened() -> Option<Arc<Builtins>> {
    let b = builtins()?;
    let m = b.metrics.as_ref()?;
    m.connections_active.add(1, &[]);
    m.connections_opened.add(1, &[]);
    Some(b)
}

pub(crate) fn connection_closed(b: Option<Arc<Builtins>>) {
    if let Some(m) = b.as_ref().and_then(|b| b.metrics.as_ref()) {
        m.connections_active.add(-1, &[]);
    }
}

/// A Gate 0 decision (`result`: admit / deny / error) for `role`
/// (consumer / producer), `elapsed` since the request arrived.
pub(crate) fn admission(role: &'static str, result: &'static str, elapsed: Duration) {
    let Some(b) = builtins() else { return };
    let Some(m) = &b.metrics else { return };
    m.admission_decisions.add(
        1,
        &[KeyValue::new(ROLE, role), KeyValue::new(RESULT, result)],
    );
    m.admission_duration
        .record(elapsed.as_secs_f64() * 1000.0, &[KeyValue::new(ROLE, role)]);
}

/// Count an upcall event (`binding_support::upcall`) for `kind` (its stable
/// snake_case name).
#[cfg(feature = "binding-support")]
pub(crate) fn upcall(what: crate::binding_support::upcall::Observed, kind: &'static str) {
    use crate::binding_support::upcall::Observed;
    let Some(b) = builtins() else { return };
    let Some(m) = &b.metrics else { return };
    let counter = match what {
        Observed::Issued => &m.upcall_issued,
        Observed::Timeout => &m.upcall_timeouts,
        Observed::Fallback => &m.upcall_fallbacks,
    };
    counter.add(1, &[KeyValue::new(UPCALL_KIND, kind)]);
}

// ── client calls (§5.4) ─────────────────────────────────────────────────────

/// One client call's OTel state, held by its [`ClientObs`](super::ClientObs).
pub(crate) struct ClientOtel {
    b: Arc<Builtins>,
    labels: Arc<Labels>,
    started: Instant,
    pub(crate) span: Span,
}

impl ClientOtel {
    pub(crate) fn start(header: &mut StreamHeader) -> Option<Self> {
        let b = builtins()?;
        let labels = labels(&header.service, &header.method);
        let span = if b.traces {
            let span = tracing::info_span!(
                target: CLIENT,
                "rpc.client",
                otel.name = %format_args!("{}/{}", header.service, header.method),
                otel.kind = "client",
                rpc.system = "aster",
                rpc.service = header.service.as_str(),
                rpc.method = header.method.as_str(),
            );
            // A parent the caller wrote into the metadata (a binding passing
            // its current span, §5.4) wins over the `tracing` context.
            if has_trace_keys(header) {
                let cx = extract(header);
                if cx.span().span_context().is_valid() {
                    let _ = span.set_parent(cx);
                }
            }
            // Under a Rust-native handler whose `rpc` span is not enabled the
            // tracing context is empty: parent on the pass-through context.
            if !has_trace_keys(header) && span_trace_context(&Span::current()).is_none() {
                if let Some(ctx) = current_pass().and_then(|p| p.ctx) {
                    let _ = span
                        .set_parent(Context::new().with_remote_span_context(ctx.to_span_context()));
                }
            }
            let cx = span.context();
            global::get_text_map_propagator(|p| p.inject_context(&cx, &mut HeaderInjector(header)));
            span
        } else {
            Span::none()
        };
        Some(ClientOtel {
            b,
            labels,
            started: Instant::now(),
            span,
        })
    }

    pub(crate) fn finish(self, code: StatusCode) {
        if let Some(m) = &self.b.metrics {
            let attrs = [
                self.labels.service.clone(),
                self.labels.method.clone(),
                KeyValue::new(STATUS, status_name(code)),
            ];
            m.client_completed.add(1, &attrs);
            m.client_duration
                .record(self.started.elapsed().as_secs_f64(), &attrs);
        }
        if !self.span.is_disabled() {
            span_status(&self.span, code);
        }
    }
}

#[cfg(test)]
mod tests;
