//! Aster observability: the Rust-owned `tracing` / OpenTelemetry pipeline that
//! every binding (Python, TypeScript, Java, .NET, Go) shares.
//!
//! Design: `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md`.
//!
//! * O0: an opt-in stderr sink, [`init_stderr_from_env`], called by each
//!   binding's native crate when it loads.
//!
//!   ```text
//!   ASTER_LOG_STDERR=1              enable (also: true, yes, on)
//!   ASTER_LOG=warn,aster=debug      EnvFilter directives (default: warn,aster=info)
//!   ```
//!
//! * O1: [`ObservabilityConfig`] (programmatic > `OTEL_*`/`ASTER_*` env >
//!   `[observability]` TOML > defaults), [`init`] / [`flush`] / [`shutdown`]
//!   with a refcounted [`ObservabilityHandle`], the OTel tracer, meter and
//!   (feature `otlp-logs`) logger providers installed as globals, OTLP over
//!   http/protobuf, and [`generation`] for instruments cached per provider.
//!
//! There is one global `tracing` subscriber, installed by whichever of
//! [`init_stderr_from_env`] and [`init`] runs first; every layer in it sits
//! behind `reload`, so the other fills its slot instead of competing for the
//! process-wide default. A Rust application that installs its own subscriber
//! composes [`layers`] into it instead.
//!
//! Features: `otlp` (default), `otlp-grpc`, `otlp-logs`, `prometheus`,
//! `testing`.

mod config;
mod error;
mod fork;
#[cfg(feature = "otlp")]
mod http;
mod pipeline;
mod subscriber;
#[cfg(feature = "testing")]
pub mod testing;
mod tracer;

pub use config::{
    redact_url, AttributeExtractor, BatchSettings, Binding, ExporterKind, ExtractorSource,
    ObservabilityConfig, OtlpHeaders, OtlpProtocol, OtlpSettings, OtlpTarget, Propagator, Resolved,
    ResolvedBatch, ResolvedConfig, SamplerConfig, Secret, SpanLimitSettings, Temporality,
    DEFAULT_LOG_QUEUE, DEFAULT_METRIC_INTERVAL, DEFAULT_OTLP_ENDPOINT, DEFAULT_OTLP_TIMEOUT,
    EXPORTER_INTERNAL_TARGETS, OTEL_SDK_DISABLED,
};
pub use error::{Error, Result};
#[cfg(feature = "prometheus")]
pub use pipeline::render_prometheus;
pub use pipeline::{
    flush, generation, init, shutdown, state, ObservabilityHandle, PipelineState, SCOPE_NAME,
    SHUTDOWN_TIMEOUT,
};
pub use subscriber::{
    init_stderr_from_env, layers, StderrInit, SubscriberStatus, DEFAULT_FILTER, ENV_FILTER,
    ENV_STDERR,
};
pub use tracer::SwapTracer;

/// The OpenTelemetry API crate this pipeline is built on, so dependents use the
/// same (exactly pinned) version.
pub use opentelemetry;
/// Parent, attributes, status and events of the OTel span behind a `tracing`
/// span (the `rpc` spans `aster` creates), without a direct
/// `tracing-opentelemetry` dependency.
pub use tracing_opentelemetry::OpenTelemetrySpanExt;
