//! The process-wide `tracing` subscriber.
//!
//! There is exactly one, installed with `set_global_default` at most once per
//! process, by whichever of [`init_stderr_from_env`] (O0) or [`crate::init`]
//! (O1) runs first. Every layer sits in a fixed slot whose per-layer filter
//! (and, where the layer itself must change, the layer) is behind
//! `tracing_subscriber::reload`, so a later call fills slots in instead of
//! installing a second subscriber:
//!
//! ```text
//! Registry ─ Vec[
//!   OTel span layer   tracing-opentelemetry + SwapTracer   filter: reload(spans only)
//!   stderr fmt layer  O0 / log_stderr                      filter: reload
//!   OTLP logs layer   reload(Option<bridge>) (otlp-logs)   filter: reload
//!   binding log slot  reload(Option<BindingLogLayer>) (O4)  filter: reload
//! ]
//! ```
//!
//! A slot whose filter is off costs what a disabled callsite costs: its
//! interest is `never`, cached per callsite.

use std::sync::OnceLock;

use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Filter, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, reload, EnvFilter, Registry};

use crate::config::with_exporter_targets_off;
use crate::tracer::SwapTracer;

/// Enables the stderr sink when truthy (`1`, `true`, `yes`, `on`).
pub const ENV_STDERR: &str = "ASTER_LOG_STDERR";
/// `EnvFilter` directives for the stderr sink (and, in O1, the log filter).
pub const ENV_FILTER: &str = "ASTER_LOG";
/// The filter when [`ENV_FILTER`] is unset or empty.
pub const DEFAULT_FILTER: &str = "warn,aster=info";

/// What [`init_stderr_from_env`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrInit {
    /// The stderr sink is enabled in the global subscriber.
    Installed,
    /// [`ENV_STDERR`] is not set: nothing was installed and events cost what
    /// they cost with no subscriber.
    Disabled,
    /// Another global subscriber was already installed (a Rust application
    /// embedding Aster); it is left alone.
    Foreign,
}

/// Whether Aster's subscriber is the process's global one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriberStatus {
    /// Aster's reloadable subscriber is the global default.
    Installed,
    /// Something else installed a global subscriber first. The OTel providers
    /// still work; compose [`layers`] into that subscriber to feed them.
    Foreign,
}

pub(crate) type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// A per-layer filter that can be off, restricted to spans, and guarded
/// against use in a forked child.
#[derive(Debug)]
pub(crate) struct SlotFilter {
    filter: Option<EnvFilter>,
    spans_only: bool,
    /// Of the spans, only those named `rpc` (the binding log bridge, which
    /// needs the call's span for its fields and nothing else).
    rpc_spans: bool,
    fork_guard: bool,
}

impl SlotFilter {
    pub(crate) fn off() -> Self {
        SlotFilter {
            filter: None,
            spans_only: false,
            rpc_spans: false,
            fork_guard: false,
        }
    }
    /// Events and spans matching `directives` (stderr).
    pub(crate) fn events(filter: EnvFilter) -> Self {
        SlotFilter {
            filter: Some(filter),
            spans_only: false,
            rpc_spans: false,
            fork_guard: false,
        }
    }
    /// Spans only (the OTel span layer, design §3.1: events go through the
    /// log paths, not as span events), no-op in a forked child.
    pub(crate) fn spans(filter: EnvFilter) -> Self {
        SlotFilter {
            filter: Some(filter),
            spans_only: true,
            rpc_spans: false,
            fork_guard: true,
        }
    }
    /// Events and spans, no-op in a forked child (exporting slots).
    #[cfg_attr(not(feature = "otlp-logs"), allow(dead_code))]
    pub(crate) fn exported(filter: EnvFilter) -> Self {
        SlotFilter {
            filter: Some(filter),
            spans_only: false,
            rpc_spans: false,
            fork_guard: true,
        }
    }
    /// The binding log bridge (O4): events matching `filter`, plus the `rpc`
    /// spans it enables (their fields go on the records), no-op in a forked
    /// child (the queue's locks may be held by a thread that did not fork).
    pub(crate) fn bridge(filter: EnvFilter) -> Self {
        SlotFilter {
            filter: Some(filter),
            spans_only: false,
            rpc_spans: true,
            fork_guard: true,
        }
    }

    /// Whether `meta` is a span this filter never looks at.
    fn skips_span(&self, meta: &Metadata<'_>) -> bool {
        meta.is_span() && self.rpc_spans && meta.name() != "rpc"
    }
}

impl<S: Subscriber> Filter<S> for SlotFilter {
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        let Some(f) = &self.filter else { return false };
        if (self.spans_only && !meta.is_span()) || self.skips_span(meta) {
            return false;
        }
        if self.fork_guard && crate::fork::forked() {
            return false;
        }
        Filter::<S>::enabled(f, meta, cx)
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        let Some(f) = &self.filter else {
            return Interest::never();
        };
        if (self.spans_only && !meta.is_span()) || self.skips_span(meta) {
            return Interest::never();
        }
        let interest = Filter::<S>::callsite_enabled(f, meta);
        // `always` would skip `enabled`, and with it the fork check.
        if self.fork_guard && interest.is_always() {
            Interest::sometimes()
        } else {
            interest
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        match &self.filter {
            None => Some(LevelFilter::OFF),
            Some(f) => Filter::<S>::max_level_hint(f),
        }
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if let Some(f) = &self.filter {
            Filter::<S>::on_new_span(f, attrs, id, ctx);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        if let Some(f) = &self.filter {
            Filter::<S>::on_record(f, id, values, ctx);
        }
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        if let Some(f) = &self.filter {
            Filter::<S>::on_enter(f, id, ctx);
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        if let Some(f) = &self.filter {
            Filter::<S>::on_exit(f, id, ctx);
        }
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        if let Some(f) = &self.filter {
            Filter::<S>::on_close(f, id, ctx);
        }
    }
}

type FilterHandle = reload::Handle<SlotFilter, Registry>;

#[cfg(feature = "otlp-logs")]
pub(crate) type LogsBridge = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge<
    opentelemetry_sdk::logs::SdkLoggerProvider,
    opentelemetry_sdk::logs::SdkLogger,
>;

/// Reload handles into the global subscriber's slots.
pub(crate) struct Slots {
    pub(crate) spans: FilterHandle,
    pub(crate) stderr: FilterHandle,
    #[cfg(feature = "otlp-logs")]
    pub(crate) otlp_logs: reload::Handle<Option<LogsBridge>, Registry>,
    #[cfg(feature = "otlp-logs")]
    pub(crate) otlp_logs_filter: FilterHandle,
    /// The binding log layer (O4, [`crate::bridge`]).
    pub(crate) bridge: reload::Handle<Option<BoxedLayer>, Registry>,
    pub(crate) bridge_filter: FilterHandle,
}

enum Global {
    Ours(Slots),
    Foreign,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

/// Install Aster's subscriber as the global default (first call only) and
/// return its slots, or `None` when another subscriber got there first.
pub(crate) fn install() -> Option<&'static Slots> {
    let g = GLOBAL.get_or_init(|| {
        let (spans_filter, spans) = reload::Layer::new(SlotFilter::off());
        let span_layer = tracing_opentelemetry::layer::<Registry>()
            .with_tracer(SwapTracer)
            .with_filter(spans_filter);

        let (stderr_filter, stderr) = reload::Layer::new(SlotFilter::off());
        let stderr_layer = fmt::layer::<Registry>()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_target(true)
            .with_filter(stderr_filter);

        let (bridge_layer, bridge) = reload::Layer::new(None::<BoxedLayer>);
        let (bridge_filter_layer, bridge_filter) = reload::Layer::new(SlotFilter::off());
        let bridge_slot = bridge_layer.with_filter(bridge_filter_layer);

        let mut layers: Vec<BoxedLayer> = vec![Box::new(span_layer), Box::new(stderr_layer)];

        #[cfg(feature = "otlp-logs")]
        let (otlp_logs, otlp_logs_filter) = {
            let (layer, handle) = reload::Layer::new(None::<LogsBridge>);
            let (filter, filter_handle) = reload::Layer::new(SlotFilter::off());
            layers.push(Box::new(layer.with_filter(filter)));
            (handle, filter_handle)
        };
        layers.push(Box::new(bridge_slot));

        match tracing_subscriber::registry().with(layers).try_init() {
            Ok(()) => Global::Ours(Slots {
                spans,
                stderr,
                #[cfg(feature = "otlp-logs")]
                otlp_logs,
                #[cfg(feature = "otlp-logs")]
                otlp_logs_filter,
                bridge,
                bridge_filter,
            }),
            Err(_) => Global::Foreign,
        }
    });
    match g {
        Global::Ours(s) => Some(s),
        Global::Foreign => None,
    }
}

/// The slots, if Aster's subscriber is installed (without installing it).
pub(crate) fn slots() -> Option<&'static Slots> {
    match GLOBAL.get() {
        Some(Global::Ours(s)) => Some(s),
        _ => None,
    }
}

/// Parse directives that were validated during resolution.
pub(crate) fn env_filter(directives: &str) -> EnvFilter {
    EnvFilter::try_new(directives).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// The layers [`crate::init`] would put in Aster's own subscriber, for a Rust
/// application that installed its own: the OTel span layer (fed by whichever
/// pipeline is current, so it can be composed before or after `init`) and,
/// with `otlp-logs`, the OTLP logs layer when the current pipeline exports
/// logs (so call `init` first if you want it).
///
/// ```no_run
/// use tracing_subscriber::prelude::*;
/// let cfg = aster_otel::ObservabilityConfig::default();
/// let handle = aster_otel::init(cfg.clone()).unwrap();
/// tracing_subscriber::registry()
///     .with(aster_otel::layers(&cfg).unwrap())
///     .with(tracing_subscriber::fmt::layer())
///     .init();
/// # drop(handle);
/// ```
pub fn layers<S>(cfg: &crate::ObservabilityConfig) -> crate::Result<Box<dyn Layer<S> + Send + Sync>>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let resolved = cfg.resolve()?.config;
    let mut out: Vec<Box<dyn Layer<S> + Send + Sync>> = Vec::new();
    if !resolved.disabled {
        let spans = tracing_opentelemetry::layer::<S>()
            .with_tracer(SwapTracer)
            .with_filter(SlotFilter::spans(env_filter(&with_exporter_targets_off(
                &resolved.span_filter,
            ))));
        out.push(Box::new(spans));
        #[cfg(feature = "otlp-logs")]
        if let Some(provider) = crate::pipeline::current_logger_provider() {
            let bridge =
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider)
                    .with_filter(SlotFilter::exported(env_filter(
                        &with_exporter_targets_off(&resolved.log_filter),
                    )));
            out.push(Box::new(bridge));
        }
    }
    Ok(Box::new(out))
}

/// Install the stderr sink as the global subscriber if [`ENV_STDERR`] asks for
/// it. Idempotent: the environment is read on the first call only, and every
/// later call returns the first call's outcome.
///
/// An invalid [`ENV_FILTER`] falls back to [`DEFAULT_FILTER`] with a note on
/// stderr. When installed, one `INFO` event on target `aster::otel` records the
/// filter in effect.
///
/// Cooperates with [`crate::init`] in either order: whichever runs first
/// installs the one global subscriber, and the other fills in its slot.
pub fn init_stderr_from_env() -> StderrInit {
    static STDERR: OnceLock<StderrInit> = OnceLock::new();
    *STDERR.get_or_init(|| {
        if !truthy(std::env::var(ENV_STDERR).ok().as_deref()) {
            return StderrInit::Disabled;
        }
        let directives = std::env::var(ENV_FILTER)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_FILTER.to_owned());
        let filter = EnvFilter::try_new(&directives).unwrap_or_else(|e| {
            eprintln!("aster: ignoring invalid {ENV_FILTER}={directives:?} ({e}); using {DEFAULT_FILTER:?}");
            EnvFilter::new(DEFAULT_FILTER)
        });
        let effective = filter.to_string();
        match install() {
            Some(slots) => {
                let _ = slots.stderr.reload(SlotFilter::events(filter));
                tracing::info!(target: "aster::otel", filter = %effective, "stderr logging enabled");
                StderrInit::Installed
            }
            None => StderrInit::Foreign,
        }
    })
}

pub(crate) fn truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::truthy;

    #[test]
    fn truthy_accepts_the_usual_spellings_only() {
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(truthy(Some(v)), "{v:?}");
        }
        for v in ["", "0", "false", "no", "off", "2", "enabled"] {
            assert!(!truthy(Some(v)), "{v:?}");
        }
        assert!(!truthy(None));
    }
}
