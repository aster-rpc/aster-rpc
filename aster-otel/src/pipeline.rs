//! `init` / `flush` / `shutdown`: the OTel providers, installed as the OTel
//! globals, behind a refcounted [`ObservabilityHandle`] (design §2.4).
//!
//! Every SDK builder gets an explicit value for every setting it would
//! otherwise read from the environment (resource, sampler, span limits, batch
//! config, reader interval, exporter endpoint/protocol/timeout/headers/
//! temporality), so `ResolvedConfig` is the only source of truth.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::metrics::NoopMeterProvider;
use opentelemetry::propagation::{TextMapCompositePropagator, TextMapPropagator};
use opentelemetry::trace::noop::NoopTracerProvider;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::resource::{Resource, TelemetryResourceDetector};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanLimits};

use crate::config::{
    with_exporter_targets_off, ExporterKind, ObservabilityConfig, Propagator, ResolvedConfig,
    SamplerConfig, Temporality,
};
use crate::error::{Error, Result};
use crate::subscriber::{self, env_filter, SlotFilter, SubscriberStatus};
use crate::{fork, tracer};

/// The instrumentation scope of Aster's own spans and instruments.
pub const SCOPE_NAME: &str = "aster";
/// How long `shutdown` gives each provider to flush and stop.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The provider generation (design §4.1a): bumped by every `init` that builds a
/// pipeline and by every shutdown. Instruments cached by generation are stale
/// when this differs from the generation they were built in. One relaxed load.
#[inline]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

#[derive(Clone, Default)]
struct Providers {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    #[cfg(feature = "otlp-logs")]
    logger: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
    #[cfg(feature = "prometheus")]
    prometheus: Option<prometheus::Registry>,
}

impl Providers {
    fn force_flush(&self) -> Result<()> {
        let mut errors = Vec::new();
        if let Some(p) = &self.tracer {
            if let Err(e) = p.force_flush() {
                errors.push(format!("traces: {e}"));
            }
        }
        if let Some(p) = &self.meter {
            if let Err(e) = p.force_flush() {
                errors.push(format!("metrics: {e}"));
            }
        }
        #[cfg(feature = "otlp-logs")]
        if let Some(p) = &self.logger {
            if let Err(e) = p.force_flush() {
                errors.push(format!("logs: {e}"));
            }
        }
        sdk_result(errors)
    }

    fn shutdown(&self) -> Result<()> {
        let mut errors = Vec::new();
        if let Some(p) = &self.tracer {
            if let Err(e) = p.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
                errors.push(format!("traces: {e}"));
            }
        }
        if let Some(p) = &self.meter {
            if let Err(e) = p.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
                errors.push(format!("metrics: {e}"));
            }
        }
        #[cfg(feature = "otlp-logs")]
        if let Some(p) = &self.logger {
            if let Err(e) = p.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
                errors.push(format!("logs: {e}"));
            }
        }
        sdk_result(errors)
    }
}

fn sdk_result(errors: Vec<String>) -> Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Sdk(errors.join("; ")))
    }
}

struct Active {
    resolved: ResolvedConfig,
    test_exporters: bool,
    refs: usize,
    generation: u64,
    providers: Providers,
    subscriber: SubscriberStatus,
}

static PIPELINE: Mutex<Option<Active>> = Mutex::new(None);

fn pipeline() -> MutexGuard<'static, Option<Active>> {
    PIPELINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A reference to the running pipeline. Clones count as references; when the
/// last one is dropped the providers are shut down (flushing what they hold).
/// A handle outlived by an explicit [`shutdown`] is inert.
#[must_use = "dropping the last handle shuts the pipeline down"]
pub struct ObservabilityHandle {
    generation: u64,
    live: bool,
    noop: bool,
    subscriber: SubscriberStatus,
    warnings: Vec<String>,
}

impl ObservabilityHandle {
    fn inert() -> Self {
        ObservabilityHandle {
            generation: generation(),
            live: false,
            noop: true,
            subscriber: SubscriberStatus::Foreign,
            warnings: Vec::new(),
        }
    }

    /// The pipeline generation this handle belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether Aster's subscriber is the global one, or another was there first.
    pub fn subscriber(&self) -> SubscriberStatus {
        self.subscriber
    }

    /// True when nothing is exported: `OTEL_SDK_DISABLED`, or a process forked
    /// after init.
    pub fn is_noop(&self) -> bool {
        self.noop
    }

    /// Configuration warnings from the `init` that created the pipeline.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// [`flush`].
    pub fn flush(&self, timeout: Duration) -> Result<()> {
        flush(timeout)
    }
}

impl Clone for ObservabilityHandle {
    fn clone(&self) -> Self {
        let mut live = false;
        if self.live && !fork::forked() {
            if let Some(a) = pipeline().as_mut() {
                if a.generation == self.generation {
                    a.refs += 1;
                    live = true;
                }
            }
        }
        ObservabilityHandle {
            generation: self.generation,
            live,
            noop: self.noop,
            subscriber: self.subscriber,
            warnings: self.warnings.clone(),
        }
    }
}

impl Drop for ObservabilityHandle {
    fn drop(&mut self) {
        if !self.live || fork::forked() {
            return;
        }
        let mut g = pipeline();
        let last = match g.as_mut() {
            Some(a) if a.generation == self.generation => {
                a.refs -= 1;
                a.refs == 0
            }
            _ => false,
        };
        if last {
            let active = g.take();
            drop(g);
            if let Some(a) = active {
                let _ = teardown(a);
            }
        }
    }
}

impl std::fmt::Debug for ObservabilityHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservabilityHandle")
            .field("generation", &self.generation)
            .field("live", &self.live)
            .field("noop", &self.noop)
            .field("subscriber", &self.subscriber)
            .finish()
    }
}

/// Start the pipeline, or join the running one.
///
/// * First call: resolve `cfg` (programmatic > env > TOML > defaults), build
///   the providers, install them as the OTel globals, install Aster's global
///   `tracing` subscriber if no subscriber exists yet, and fill its slots.
/// * Later calls with an equal resolved configuration: refcount + 1.
/// * Later calls with a different one: [`Error::AlreadyInitialized`] naming the
///   differing settings.
/// * A foreign global subscriber: the providers are still installed and the
///   handle reports [`SubscriberStatus::Foreign`]; compose
///   [`crate::layers`] into that subscriber.
/// * In a child forked after init: an inert handle and a one-time warning.
pub fn init(cfg: ObservabilityConfig) -> Result<ObservabilityHandle> {
    if fork::check() {
        return Ok(ObservabilityHandle::inert());
    }
    let resolved = cfg.resolve()?;
    let config = resolved.config;
    let mut warnings = resolved.warnings;
    #[cfg(feature = "testing")]
    let test = cfg.test_exporters.clone();
    #[cfg(not(feature = "testing"))]
    let test: Option<()> = None;

    let mut g = pipeline();
    if let Some(a) = g.as_mut() {
        let mut diff = a.resolved.diff(&config);
        if a.test_exporters != test.is_some() {
            diff.push("test_exporters");
        }
        if !diff.is_empty() {
            return Err(Error::AlreadyInitialized(format!(
                "differs in: {}",
                diff.join(", ")
            )));
        }
        a.refs += 1;
        return Ok(ObservabilityHandle {
            generation: a.generation,
            live: true,
            noop: config.disabled,
            subscriber: a.subscriber,
            warnings,
        });
    }

    // Build everything before touching any global, so a failure leaves the
    // process as it was.
    let providers = if config.disabled {
        Providers::default()
    } else {
        build_providers(&config, test.as_ref(), &mut warnings)?
    };

    fork::register();
    let slots = subscriber::install();
    let status = if slots.is_some() {
        SubscriberStatus::Installed
    } else {
        SubscriberStatus::Foreign
    };

    if let Some(tp) = &providers.tracer {
        global::set_tracer_provider(tp.clone());
        tracer::set(Some(BoxedTracer::new(Box::new(
            tp.tracer_with_scope(scope()),
        ))));
        if let Some(s) = slots {
            let _ = s
                .spans
                .reload(SlotFilter::spans(env_filter(&with_exporter_targets_off(
                    &config.span_filter,
                ))));
        }
    }
    if let Some(mp) = &providers.meter {
        global::set_meter_provider(mp.clone());
    }
    if !config.disabled {
        global::set_text_map_propagator(propagator(&config.propagators));
    }
    #[cfg(feature = "otlp-logs")]
    if let (Some(lp), Some(s)) = (&providers.logger, slots) {
        let _ = s.otlp_logs.reload(Some(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(lp),
        ));
        let _ = s.otlp_logs_filter.reload(SlotFilter::exported(env_filter(
            &with_exporter_targets_off(&config.log_filter),
        )));
    }
    if config.log_stderr {
        if let Some(s) = slots {
            let _ = s
                .stderr
                .reload(SlotFilter::events(env_filter(&config.log_filter)));
        }
    }

    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let noop = config.disabled;
    *g = Some(Active {
        resolved: config,
        test_exporters: test.is_some(),
        refs: 1,
        generation,
        providers,
        subscriber: status,
    });
    drop(g);

    for w in &warnings {
        tracing::warn!(target: "aster::otel", "{w}");
    }
    Ok(ObservabilityHandle {
        generation,
        live: true,
        noop,
        subscriber: status,
        warnings,
    })
}

/// Flush every provider of the running pipeline, waiting at most `timeout`.
/// No pipeline: `Ok`. The flush runs on a helper thread, so a timeout returns
/// promptly even if an exporter is stuck.
pub fn flush(timeout: Duration) -> Result<()> {
    if fork::check() {
        return Ok(());
    }
    let providers = match pipeline().as_ref() {
        Some(a) => a.providers.clone(),
        None => return Ok(()),
    };
    with_timeout("flush", timeout, move || providers.force_flush())
}

/// Shut the running pipeline down regardless of outstanding handles (which
/// become inert): stop feeding the providers, restore no-op OTel globals, bump
/// the generation, then flush and stop the exporters. No pipeline: `Ok`.
pub fn shutdown() -> Result<()> {
    if fork::check() {
        return Ok(());
    }
    let active = pipeline().take();
    match active {
        Some(a) => teardown(a),
        None => Ok(()),
    }
}

fn teardown(a: Active) -> Result<()> {
    if let Some(s) = subscriber::slots() {
        let _ = s.spans.reload(SlotFilter::off());
        #[cfg(feature = "otlp-logs")]
        {
            let _ = s.otlp_logs_filter.reload(SlotFilter::off());
            let _ = s.otlp_logs.reload(None);
        }
    }
    tracer::set(None);
    if a.providers.tracer.is_some() {
        global::set_tracer_provider(NoopTracerProvider::new());
    }
    if a.providers.meter.is_some() {
        global::set_meter_provider(NoopMeterProvider::new());
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    a.providers.shutdown()
}

fn with_timeout(
    what: &'static str,
    timeout: Duration,
    f: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(format!("aster-otel-{what}"))
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| Error::Sdk(format!("could not spawn the {what} thread: {e}")))?;
    rx.recv_timeout(timeout).map_err(|_| Error::Timeout(what))?
}

#[cfg(feature = "otlp-logs")]
pub(crate) fn current_logger_provider() -> Option<opentelemetry_sdk::logs::SdkLoggerProvider> {
    pipeline().as_ref().and_then(|a| a.providers.logger.clone())
}

/// The running pipeline's Prometheus reader rendered as text exposition
/// (version 0.0.4), or `None` when no pipeline has a `prometheus` metrics
/// exporter.
#[cfg(feature = "prometheus")]
pub fn render_prometheus() -> Option<String> {
    let registry = pipeline().as_ref()?.providers.prometheus.clone()?;
    prometheus::TextEncoder::new()
        .encode_to_string(&registry.gather())
        .ok()
}

// ── building ────────────────────────────────────────────────────────────────

fn scope() -> InstrumentationScope {
    InstrumentationScope::builder(SCOPE_NAME)
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

fn resource(c: &ResolvedConfig) -> Resource {
    let mut attrs: Vec<KeyValue> = c
        .resource_attributes
        .iter()
        .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
        .collect();
    attrs.push(KeyValue::new("aster.binding", c.binding.as_str()));
    attrs.push(KeyValue::new("aster.version", env!("CARGO_PKG_VERSION")));
    // builder_empty: no EnvResourceDetector, whose OTEL_SERVICE_NAME /
    // OTEL_RESOURCE_ATTRIBUTES reads were already resolved above.
    Resource::builder_empty()
        .with_detector(Box::new(TelemetryResourceDetector))
        .with_attributes(attrs)
        .with_service_name(c.service_name.clone())
        .build()
}

fn sampler(s: SamplerConfig) -> Sampler {
    match s {
        SamplerConfig::AlwaysOn => Sampler::AlwaysOn,
        SamplerConfig::AlwaysOff => Sampler::AlwaysOff,
        SamplerConfig::TraceIdRatio(r) => Sampler::TraceIdRatioBased(r),
        SamplerConfig::ParentBasedAlwaysOn => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
        SamplerConfig::ParentBasedAlwaysOff => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
        SamplerConfig::ParentBasedTraceIdRatio(r) => {
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(r)))
        }
    }
}

/// An empty composite propagator injects and extracts nothing (`none`).
fn propagator(list: &[Propagator]) -> TextMapCompositePropagator {
    let parts: Vec<Box<dyn TextMapPropagator + Send + Sync>> = list
        .iter()
        .map(|p| -> Box<dyn TextMapPropagator + Send + Sync> {
            match p {
                Propagator::TraceContext => Box::new(TraceContextPropagator::new()),
                Propagator::Baggage => Box::new(BaggagePropagator::new()),
            }
        })
        .collect();
    TextMapCompositePropagator::new(parts)
}

#[cfg(any(feature = "otlp", feature = "testing"))]
fn span_batch(b: &crate::config::ResolvedBatch) -> opentelemetry_sdk::trace::BatchConfig {
    // `default()` reads OTEL_BSP_*; every field the thread-based processor
    // uses is then overwritten. (max_export_timeout / max_concurrent_exports
    // only exist for the experimental async-runtime processor.)
    opentelemetry_sdk::trace::BatchConfigBuilder::default()
        .with_max_queue_size(b.max_queue_size)
        .with_max_export_batch_size(b.max_export_batch_size)
        .with_scheduled_delay(b.scheduled_delay)
        .build()
}

#[cfg(all(feature = "otlp-logs", any(feature = "otlp", feature = "testing")))]
fn log_batch(b: &crate::config::ResolvedBatch) -> opentelemetry_sdk::logs::BatchConfig {
    // As for spans: `default()` reads OTEL_BLRP_*, then every field the
    // thread-based processor uses is overwritten (its export timeout setter
    // is async-runtime-only).
    opentelemetry_sdk::logs::BatchConfigBuilder::default()
        .with_max_queue_size(b.max_queue_size)
        .with_max_export_batch_size(b.max_export_batch_size)
        .with_scheduled_delay(b.scheduled_delay)
        .build()
}

fn sdk_temporality(t: Temporality) -> opentelemetry_sdk::metrics::Temporality {
    match t {
        Temporality::Cumulative => opentelemetry_sdk::metrics::Temporality::Cumulative,
        Temporality::Delta => opentelemetry_sdk::metrics::Temporality::Delta,
        Temporality::LowMemory => opentelemetry_sdk::metrics::Temporality::LowMemory,
    }
}

#[cfg(feature = "testing")]
type TestExporters = crate::testing::TestExporters;
#[cfg(not(feature = "testing"))]
type TestExporters = ();

// Which pushes into `warnings` (and which bindings are mutated) depends on the
// feature set.
#[allow(unused_variables, unused_mut, clippy::ptr_arg)]
fn build_providers(
    c: &ResolvedConfig,
    test: Option<&TestExporters>,
    warnings: &mut Vec<String>,
) -> Result<Providers> {
    let resource = resource(c);
    let mut providers = Providers::default();
    #[cfg(feature = "otlp")]
    let mut clients = otlp::Clients::default();

    // Traces.
    let mut tb = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(sampler(c.sampler))
        .with_span_limits(SpanLimits {
            max_attributes_per_span: c.span_max_attributes,
            max_events_per_span: c.span_max_events,
            max_links_per_span: c.span_max_links,
            ..SpanLimits::default()
        });
    let mut any_span_exporter = false;
    #[cfg(feature = "testing")]
    if let Some(t) = test {
        tb = tb.with_span_processor(
            opentelemetry_sdk::trace::BatchSpanProcessor::builder(t.spans.clone())
                .with_batch_config(span_batch(&c.span_batch))
                .build(),
        );
        any_span_exporter = true;
    }
    if !any_span_exporter {
        for kind in &c.traces_exporter {
            if *kind == ExporterKind::Otlp {
                #[cfg(feature = "otlp")]
                {
                    let exporter = otlp::span_exporter(&c.otlp_traces, &mut clients)?;
                    tb = tb.with_span_processor(
                        opentelemetry_sdk::trace::BatchSpanProcessor::builder(exporter)
                            .with_batch_config(span_batch(&c.span_batch))
                            .build(),
                    );
                    any_span_exporter = true;
                }
                #[cfg(not(feature = "otlp"))]
                warnings.push(
                    "OTLP trace export needs aster-otel feature `otlp`; traces are not exported"
                        .into(),
                );
            }
        }
    }
    if any_span_exporter {
        providers.tracer = Some(tb.build());
    }

    // Metrics.
    let mut mb = SdkMeterProvider::builder().with_resource(resource.clone());
    let mut any_reader = false;
    #[cfg(feature = "testing")]
    if let Some(t) = test {
        mb = mb.with_reader(
            opentelemetry_sdk::metrics::PeriodicReader::builder(t.metrics.clone())
                .with_interval(c.metric_export_interval)
                .build(),
        );
        any_reader = true;
    }
    if !any_reader {
        for kind in &c.metrics_exporter {
            match kind {
                ExporterKind::Otlp => {
                    #[cfg(feature = "otlp")]
                    {
                        let exporter = otlp::metric_exporter(
                            &c.otlp_metrics,
                            sdk_temporality(c.metrics_temporality),
                            &mut clients,
                        )?;
                        mb = mb.with_reader(
                            opentelemetry_sdk::metrics::PeriodicReader::builder(exporter)
                                .with_interval(c.metric_export_interval)
                                .build(),
                        );
                        any_reader = true;
                    }
                    #[cfg(not(feature = "otlp"))]
                    {
                        let _ = sdk_temporality(c.metrics_temporality);
                        warnings.push("OTLP metric export needs aster-otel feature `otlp`; metrics are not pushed".into());
                    }
                }
                ExporterKind::Prometheus => {
                    #[cfg(feature = "prometheus")]
                    {
                        let registry = prometheus::Registry::new();
                        let exporter = opentelemetry_prometheus::exporter()
                            .with_registry(registry.clone())
                            .build()
                            .map_err(|e| Error::Exporter(format!("prometheus reader: {e}")))?;
                        mb = mb.with_reader(exporter);
                        providers.prometheus = Some(registry);
                        any_reader = true;
                    }
                    #[cfg(not(feature = "prometheus"))]
                    warnings.push("the prometheus metrics exporter needs aster-otel feature `prometheus`; ignoring it".into());
                }
            }
        }
    }
    if any_reader {
        providers.meter = Some(mb.build());
    }

    // Logs (opt-in, Q4).
    #[cfg(feature = "otlp-logs")]
    {
        let mut lb = opentelemetry_sdk::logs::SdkLoggerProvider::builder().with_resource(resource);
        let mut any_log_exporter = false;
        #[cfg(feature = "testing")]
        if let Some(t) = test {
            lb = lb.with_log_processor(
                opentelemetry_sdk::logs::BatchLogProcessor::builder(t.logs.clone())
                    .with_batch_config(log_batch(&c.log_batch))
                    .build(),
            );
            any_log_exporter = true;
        }
        if !any_log_exporter && c.logs_exporter.contains(&ExporterKind::Otlp) {
            #[cfg(feature = "otlp")]
            {
                let exporter = otlp::log_exporter(&c.otlp_logs, &mut clients)?;
                lb = lb.with_log_processor(
                    opentelemetry_sdk::logs::BatchLogProcessor::builder(exporter)
                        .with_batch_config(log_batch(&c.log_batch))
                        .build(),
                );
                any_log_exporter = true;
            }
            #[cfg(not(feature = "otlp"))]
            warnings.push(
                "OTLP log export needs aster-otel feature `otlp`; logs are not exported".into(),
            );
        }
        if any_log_exporter {
            providers.logger = Some(lb.build());
        }
    }
    #[cfg(not(feature = "otlp-logs"))]
    if !c.logs_exporter.is_empty() {
        warnings.push(
            "OTLP log export needs aster-otel feature `otlp-logs`; logs are not exported".into(),
        );
    }
    Ok(providers)
}

#[cfg(feature = "otlp")]
mod otlp {
    use std::time::Duration;

    use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};

    use crate::config::{redact_url, OtlpProtocol, OtlpTarget};
    use crate::error::{Error, Result};
    use crate::http::{blocking_client, PinnedHeaders};

    /// One reqwest client per distinct timeout (the blocking client applies its
    /// own timeout to each request, and each client owns a runtime thread).
    #[derive(Default)]
    pub(super) struct Clients(Vec<(Duration, reqwest::blocking::Client)>);

    impl Clients {
        fn for_target(&mut self, t: &OtlpTarget) -> Result<PinnedHeaders> {
            let client = match self.0.iter().find(|(d, _)| *d == t.timeout) {
                Some((_, c)) => c.clone(),
                None => {
                    let c = blocking_client(t.timeout)?;
                    self.0.push((t.timeout, c.clone()));
                    c
                }
            };
            PinnedHeaders::new(client, &t.headers)
        }
    }

    fn check(t: &OtlpTarget, signal: &str) -> Result<()> {
        match t.protocol {
            OtlpProtocol::HttpProtobuf => Ok(()),
            OtlpProtocol::Grpc => Err(Error::Config(format!(
                "OTLP {signal}: protocol grpc is not supported yet (the tonic exporter needs a Tokio \
                 runtime that the SDK's export threads do not provide); use http/protobuf"
            ))),
        }
    }

    fn build_err(signal: &str, t: &OtlpTarget, e: impl std::fmt::Display) -> Error {
        let msg = e.to_string().replace(&t.endpoint, &redact_url(&t.endpoint));
        Error::Exporter(format!(
            "OTLP {signal} exporter for {}: {msg}",
            redact_url(&t.endpoint)
        ))
    }

    pub(super) fn span_exporter(
        t: &OtlpTarget,
        clients: &mut Clients,
    ) -> Result<opentelemetry_otlp::SpanExporter> {
        check(t, "traces")?;
        opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(t.endpoint.clone())
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(t.timeout)
            .with_http_client(clients.for_target(t)?)
            .build()
            .map_err(|e| build_err("traces", t, e))
    }

    pub(super) fn metric_exporter(
        t: &OtlpTarget,
        temporality: opentelemetry_sdk::metrics::Temporality,
        clients: &mut Clients,
    ) -> Result<opentelemetry_otlp::MetricExporter> {
        check(t, "metrics")?;
        opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_temporality(temporality)
            .with_endpoint(t.endpoint.clone())
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(t.timeout)
            .with_http_client(clients.for_target(t)?)
            .build()
            .map_err(|e| build_err("metrics", t, e))
    }

    #[cfg(feature = "otlp-logs")]
    pub(super) fn log_exporter(
        t: &OtlpTarget,
        clients: &mut Clients,
    ) -> Result<opentelemetry_otlp::LogExporter> {
        check(t, "logs")?;
        opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(t.endpoint.clone())
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(t.timeout)
            .with_http_client(clients.for_target(t)?)
            .build()
            .map_err(|e| build_err("logs", t, e))
    }
}
