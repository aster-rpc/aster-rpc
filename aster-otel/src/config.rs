//! [`ObservabilityConfig`]: what the pipeline should do, and how it is resolved.
//!
//! Precedence, highest first (design §2.3): programmatic fields, then the
//! `OTEL_*` / `ASTER_*` environment, then the `[observability]` TOML table,
//! then defaults. Everything is parsed here, once; [`ResolvedConfig`] holds an
//! explicit value for every setting and the pipeline passes each one to its SDK
//! builder, so the SDK's and the OTLP exporter's own environment reads never
//! decide anything.
//!
//! Secrets: OTLP headers (typically `authorization=Bearer ...`) live in
//! [`OtlpHeaders`], whose `Debug` and `Display` print `<redacted>`. Endpoints are
//! printed through [`redact_url`], which drops userinfo and the query string.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

// ── env names ───────────────────────────────────────────────────────────────

/// Kill switch: when true, `init` installs nothing and every signal is a no-op.
pub const OTEL_SDK_DISABLED: &str = "OTEL_SDK_DISABLED";
const OTEL_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
const OTEL_RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";
const OTEL_TRACES_EXPORTER: &str = "OTEL_TRACES_EXPORTER";
const OTEL_METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";
const OTEL_LOGS_EXPORTER: &str = "OTEL_LOGS_EXPORTER";
const OTEL_TRACES_SAMPLER: &str = "OTEL_TRACES_SAMPLER";
const OTEL_TRACES_SAMPLER_ARG: &str = "OTEL_TRACES_SAMPLER_ARG";
const OTEL_PROPAGATORS: &str = "OTEL_PROPAGATORS";
const OTEL_METRIC_EXPORT_INTERVAL: &str = "OTEL_METRIC_EXPORT_INTERVAL";
const OTEL_METRICS_TEMPORALITY: &str = "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE";
const OTEL_EXPORTER_OTLP_INSECURE: &str = "OTEL_EXPORTER_OTLP_INSECURE";
const ASTER_LOG_LEVEL: &str = "ASTER_LOG_LEVEL";
const ASTER_LOG_QUEUE: &str = "ASTER_LOG_QUEUE";

/// Default OTLP/HTTP collector base URL (OTel spec).
pub const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4318";
/// Default per-request OTLP timeout (OTel spec: 10 s).
pub const DEFAULT_OTLP_TIMEOUT: Duration = Duration::from_secs(10);
/// Default metric export interval (OTel spec: 60 s).
pub const DEFAULT_METRIC_INTERVAL: Duration = Duration::from_secs(60);
/// Default bound of the binding log queue (O4).
pub const DEFAULT_LOG_QUEUE: usize = 8192;
/// Targets the exporters themselves log on. Appended as `=off` to the span,
/// OTLP-log and bridge filters (unless the directives already name them) so an
/// export can never produce telemetry about itself. EnvFilter targets match by
/// prefix, so `hyper` also covers `hyper_util` and `opentelemetry` covers
/// `opentelemetry_sdk` / `_otlp` / `_http`.
pub const EXPORTER_INTERNAL_TARGETS: [&str; 3] = ["reqwest", "hyper", "opentelemetry"];

// ── value types ─────────────────────────────────────────────────────────────

/// Which binding initialised the pipeline (resource attribute `aster.binding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Binding {
    #[default]
    Rust,
    Python,
    #[serde(rename = "ts")]
    TypeScript,
    Java,
    DotNet,
    Go,
}

impl Binding {
    /// The `aster.binding` attribute value.
    pub fn as_str(self) -> &'static str {
        match self {
            Binding::Rust => "rust",
            Binding::Python => "python",
            Binding::TypeScript => "ts",
            Binding::Java => "java",
            Binding::DotNet => "dotnet",
            Binding::Go => "go",
        }
    }
}

/// One entry of `OTEL_{TRACES,METRICS,LOGS}_EXPORTER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExporterKind {
    /// OTLP push.
    Otlp,
    /// Prometheus pull reader (metrics only, feature `prometheus`).
    Prometheus,
}

/// OTLP wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    /// `http/protobuf` (the default and the only one O1 exports with).
    HttpProtobuf,
    /// `grpc` (feature `otlp-grpc`; refused at init in O1, see `pipeline`).
    Grpc,
}

/// Trace sampler (`OTEL_TRACES_SAMPLER` / `_ARG`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SamplerConfig {
    AlwaysOn,
    AlwaysOff,
    TraceIdRatio(f64),
    ParentBasedAlwaysOn,
    ParentBasedAlwaysOff,
    ParentBasedTraceIdRatio(f64),
}

/// A context propagator (`OTEL_PROPAGATORS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Propagator {
    TraceContext,
    Baggage,
}

/// Metric temporality preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporality {
    Cumulative,
    Delta,
    LowMemory,
}

/// Where an attribute extractor reads its value (design §4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractorSource {
    /// `attr:<key>`: the caller's Gate 3 / admission attributes.
    Attribute(String),
    /// `meta:<key>`: request metadata.
    Metadata(String),
    /// `baggage:<key>`: a W3C baggage member of the incoming trace context.
    Baggage(String),
}

impl ExtractorSource {
    /// Parse `attr:<key>`, `meta:<key>` or `baggage:<key>`.
    pub fn parse(source: &str) -> Result<Self> {
        let (kind, key) = source.split_once(':').unwrap_or((source, ""));
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::Config(format!(
                "attribute extractor source {source:?}: expected attr:<key>, meta:<key> or baggage:<key>"
            )));
        }
        match kind.trim() {
            "attr" => Ok(Self::Attribute(key.to_owned())),
            "meta" => Ok(Self::Metadata(key.to_owned())),
            "baggage" => Ok(Self::Baggage(key.to_owned())),
            other => Err(Error::Config(format!(
                "attribute extractor source {source:?}: unknown kind {other:?} (attr, meta, baggage)"
            ))),
        }
    }
}

/// One declarative extractor: the OTel attribute `key` and where its value
/// comes from. Evaluated once per call, in Rust (design §4.4, D7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeExtractor {
    pub key: String,
    pub source: ExtractorSource,
}

/// A secret string: `Debug` and `Display` print `<redacted>`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }
    /// The secret itself. Only the exporter's HTTP client calls this.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// OTLP request headers (`OTEL_EXPORTER_OTLP_HEADERS` and friends). Header
/// values routinely carry bearer tokens, so `Debug` and `Display` print
/// `<redacted>` and nothing else, not even the header names.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct OtlpHeaders(Vec<(String, Secret)>);

impl OtlpHeaders {
    /// Build from name/value pairs. Names are lower-cased (HTTP header names
    /// are case-insensitive); a later duplicate replaces an earlier one.
    pub fn new<K: Into<String>, V: Into<String>>(pairs: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut map: BTreeMap<String, Secret> = BTreeMap::new();
        for (k, v) in pairs {
            map.insert(k.into().trim().to_ascii_lowercase(), Secret::new(v));
        }
        OtlpHeaders(map.into_iter().collect())
    }

    /// Parse the OTel env format: `k1=v1,k2=v2`, values percent-decoded.
    /// Malformed entries are skipped; the returned warnings name the entry's
    /// position only, never its content.
    pub fn parse(input: &str) -> (Self, Vec<String>) {
        let mut pairs = Vec::new();
        let mut warnings = Vec::new();
        for (i, entry) in input.split(',').enumerate() {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            match entry.split_once('=') {
                Some((k, v)) if !k.trim().is_empty() && valid_header_name(k.trim()) => {
                    pairs.push((k.trim().to_owned(), percent_decode(v.trim())));
                }
                _ => warnings.push(format!("OTLP headers: ignoring malformed entry #{}", i + 1)),
            }
        }
        (OtlpHeaders::new(pairs), warnings)
    }

    /// The headers, values exposed. For the exporter's HTTP client only.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.expose()))
    }

    /// Number of headers.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// No headers.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for OtlpHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for OtlpHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// OTLP settings for one signal, or the shared ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OtlpSettings {
    /// For the shared settings: the base URL; `/v1/<signal>` is appended. For a
    /// per-signal setting: the full URL, used as-is (OTel spec).
    pub endpoint: Option<String>,
    pub protocol: Option<OtlpProtocol>,
    pub headers: Option<OtlpHeaders>,
    pub timeout: Option<Duration>,
}

/// Batch processor knobs (traces: `OTEL_BSP_*`, logs: `OTEL_BLRP_*`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchSettings {
    pub max_queue_size: Option<usize>,
    pub max_export_batch_size: Option<usize>,
    pub scheduled_delay: Option<Duration>,
}

/// Span limits (`OTEL_SPAN_*_COUNT_LIMIT`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpanLimitSettings {
    pub max_attributes_per_span: Option<u32>,
    pub max_events_per_span: Option<u32>,
    pub max_links_per_span: Option<u32>,
}

/// What the pipeline should do. Every field is optional: `None` means "not set
/// here", and resolution falls through to the environment, then the TOML
/// table, then the default.
#[derive(Clone)]
pub struct ObservabilityConfig {
    /// Apply `OTEL_*` / `ASTER_*` env under the explicit fields (default true).
    /// The C ABI's `ASTER_OBSERVABILITY_FROM_ENV` flag maps here.
    pub from_env: bool,
    pub disabled: Option<bool>,
    pub service_name: Option<String>,
    /// Extra resource attributes; merged by key over the lower layers.
    pub resource_attributes: Vec<(String, String)>,
    pub binding: Option<Binding>,
    /// `Some(vec![])` means `none`.
    pub traces_exporter: Option<Vec<ExporterKind>>,
    pub metrics_exporter: Option<Vec<ExporterKind>>,
    pub logs_exporter: Option<Vec<ExporterKind>>,
    pub otlp: OtlpSettings,
    pub otlp_traces: OtlpSettings,
    pub otlp_metrics: OtlpSettings,
    pub otlp_logs: OtlpSettings,
    /// Accept plain `http://` to a non-loopback collector without a warning.
    pub otlp_insecure: Option<bool>,
    pub sampler: Option<SamplerConfig>,
    /// `Some(vec![])` means `none`.
    pub propagators: Option<Vec<Propagator>>,
    pub metric_export_interval: Option<Duration>,
    pub metrics_temporality: Option<Temporality>,
    pub span_batch: BatchSettings,
    pub log_batch: BatchSettings,
    pub span_limits: SpanLimitSettings,
    /// `EnvFilter` directives for Rust events (`ASTER_LOG`).
    pub log_filter: Option<String>,
    /// Shorthand level for the `aster` targets (`ASTER_LOG_LEVEL`); used only
    /// when no layer sets `log_filter`.
    pub log_level: Option<String>,
    /// `EnvFilter` directives for spans exported to OTel (programmatic / TOML
    /// only). Default: the resolved log filter.
    pub span_filter: Option<String>,
    /// Also write Rust events to stderr (`ASTER_LOG_STDERR`, as in O0).
    pub log_stderr: Option<bool>,
    /// Binding log queue bound (`ASTER_LOG_QUEUE`, used by O4).
    pub log_queue: Option<usize>,
    /// Per-call attributes, `(otel_key, source)` with `source` one of
    /// `attr:<key>`, `meta:<key>`, `baggage:<key>` (programmatic / TOML only,
    /// design §4.4). `Some(vec![])` means none.
    pub attribute_extractors: Option<Vec<(String, String)>>,
    /// Extractor keys whose values are added to the outbound baggage of calls
    /// made while handling the call (programmatic / TOML only). Never put
    /// secrets here: baggage travels to every downstream service.
    pub propagate_as_baggage: Option<Vec<String>>,
    /// The `[observability]` TOML layer, below env.
    pub file: Option<Box<ObservabilityConfig>>,
    /// In-memory exporters that replace OTLP (tests).
    #[cfg(feature = "testing")]
    pub test_exporters: Option<crate::testing::TestExporters>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        ObservabilityConfig {
            from_env: true,
            disabled: None,
            service_name: None,
            resource_attributes: Vec::new(),
            binding: None,
            traces_exporter: None,
            metrics_exporter: None,
            logs_exporter: None,
            otlp: OtlpSettings::default(),
            otlp_traces: OtlpSettings::default(),
            otlp_metrics: OtlpSettings::default(),
            otlp_logs: OtlpSettings::default(),
            otlp_insecure: None,
            sampler: None,
            propagators: None,
            metric_export_interval: None,
            metrics_temporality: None,
            span_batch: BatchSettings::default(),
            log_batch: BatchSettings::default(),
            span_limits: SpanLimitSettings::default(),
            log_filter: None,
            log_level: None,
            span_filter: None,
            log_stderr: None,
            log_queue: None,
            attribute_extractors: None,
            propagate_as_baggage: None,
            file: None,
            #[cfg(feature = "testing")]
            test_exporters: None,
        }
    }
}

impl fmt::Debug for ObservabilityConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("ObservabilityConfig");
        d.field("from_env", &self.from_env)
            .field("disabled", &self.disabled)
            .field("service_name", &self.service_name)
            .field("resource_attributes", &self.resource_attributes)
            .field("binding", &self.binding)
            .field("traces_exporter", &self.traces_exporter)
            .field("metrics_exporter", &self.metrics_exporter)
            .field("logs_exporter", &self.logs_exporter)
            .field("otlp", &RedactedSettings(&self.otlp))
            .field("otlp_traces", &RedactedSettings(&self.otlp_traces))
            .field("otlp_metrics", &RedactedSettings(&self.otlp_metrics))
            .field("otlp_logs", &RedactedSettings(&self.otlp_logs))
            .field("otlp_insecure", &self.otlp_insecure)
            .field("sampler", &self.sampler)
            .field("propagators", &self.propagators)
            .field("metric_export_interval", &self.metric_export_interval)
            .field("metrics_temporality", &self.metrics_temporality)
            .field("span_batch", &self.span_batch)
            .field("log_batch", &self.log_batch)
            .field("span_limits", &self.span_limits)
            .field("log_filter", &self.log_filter)
            .field("log_level", &self.log_level)
            .field("span_filter", &self.span_filter)
            .field("log_stderr", &self.log_stderr)
            .field("log_queue", &self.log_queue)
            .field("attribute_extractors", &self.attribute_extractors)
            .field("propagate_as_baggage", &self.propagate_as_baggage)
            .field("file", &self.file);
        #[cfg(feature = "testing")]
        d.field("test_exporters", &self.test_exporters.is_some());
        d.finish()
    }
}

/// `Debug` for [`OtlpSettings`] with the endpoint's userinfo and query removed
/// (headers are already redacted by their own type).
struct RedactedSettings<'a>(&'a OtlpSettings);

impl fmt::Debug for RedactedSettings<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpSettings")
            .field("endpoint", &self.0.endpoint.as_deref().map(redact_url))
            .field("protocol", &self.0.protocol)
            .field("headers", &self.0.headers)
            .field("timeout", &self.0.timeout)
            .finish()
    }
}

// ── the TOML table ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlTable {
    disabled: Option<bool>,
    service_name: Option<String>,
    #[serde(default)]
    resource_attributes: BTreeMap<String, String>,
    binding: Option<Binding>,
    traces_exporter: Option<StringOrList>,
    metrics_exporter: Option<StringOrList>,
    logs_exporter: Option<StringOrList>,
    otlp_endpoint: Option<String>,
    otlp_traces_endpoint: Option<String>,
    otlp_metrics_endpoint: Option<String>,
    otlp_logs_endpoint: Option<String>,
    otlp_protocol: Option<String>,
    otlp_timeout_ms: Option<u64>,
    otlp_insecure: Option<bool>,
    sampler: Option<String>,
    sampler_arg: Option<String>,
    propagators: Option<StringOrList>,
    metric_export_interval_ms: Option<u64>,
    metrics_temporality: Option<String>,
    log_filter: Option<String>,
    log_level: Option<String>,
    span_filter: Option<String>,
    log_stderr: Option<bool>,
    log_queue: Option<usize>,
    attribute_extractors: Option<BTreeMap<String, String>>,
    propagate_as_baggage: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn joined(self) -> String {
        match self {
            StringOrList::One(s) => s,
            StringOrList::Many(v) => v.join(","),
        }
    }
}

#[derive(Deserialize)]
struct TomlDoc {
    observability: Option<toml::Value>,
}

impl ObservabilityConfig {
    /// An empty programmatic layer (everything from env, TOML or defaults).
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse the `[observability]` table of an `AsterConfig` TOML document into
    /// a config layer. A document without the table yields an empty layer.
    /// Unknown keys are errors. OTLP headers are deliberately not accepted
    /// here: configuration files get copied around, so secrets come from env or
    /// the programmatic API.
    pub fn from_toml_str(doc: &str) -> Result<Self> {
        let doc: TomlDoc = toml::from_str(doc)
            .map_err(|e| Error::Config(format!("observability TOML: {}", e.message())))?;
        let Some(table) = doc.observability else {
            return Ok(Self::default());
        };
        let t: TomlTable = table.try_into().map_err(|e: toml::de::Error| {
            Error::Config(format!("[observability]: {}", e.message()))
        })?;
        let mut w = Vec::new();
        let src = "[observability]";
        let mut c = ObservabilityConfig {
            from_env: true,
            disabled: t.disabled,
            service_name: t.service_name,
            resource_attributes: t.resource_attributes.into_iter().collect(),
            binding: t.binding,
            otlp_insecure: t.otlp_insecure,
            log_filter: t.log_filter,
            log_level: t.log_level,
            span_filter: t.span_filter,
            log_stderr: t.log_stderr,
            log_queue: t.log_queue,
            attribute_extractors: t.attribute_extractors.map(|m| m.into_iter().collect()),
            propagate_as_baggage: t.propagate_as_baggage,
            metric_export_interval: t.metric_export_interval_ms.map(Duration::from_millis),
            ..Default::default()
        };
        c.traces_exporter = t
            .traces_exporter
            .map(|v| parse_exporters(&v.joined(), src, &mut w))
            .transpose()?;
        c.metrics_exporter = t
            .metrics_exporter
            .map(|v| parse_exporters(&v.joined(), src, &mut w))
            .transpose()?;
        c.logs_exporter = t
            .logs_exporter
            .map(|v| parse_exporters(&v.joined(), src, &mut w))
            .transpose()?;
        c.otlp.endpoint = t.otlp_endpoint;
        c.otlp_traces.endpoint = t.otlp_traces_endpoint;
        c.otlp_metrics.endpoint = t.otlp_metrics_endpoint;
        c.otlp_logs.endpoint = t.otlp_logs_endpoint;
        c.otlp.protocol = t
            .otlp_protocol
            .as_deref()
            .map(|p| parse_protocol(p, src))
            .transpose()?;
        c.otlp.timeout = t.otlp_timeout_ms.map(Duration::from_millis);
        c.sampler = t
            .sampler
            .as_deref()
            .map(|s| parse_sampler(s, t.sampler_arg.as_deref(), src))
            .transpose()?;
        c.propagators = t
            .propagators
            .map(|p| parse_propagators(&p.joined(), src, &mut w))
            .transpose()?;
        c.metrics_temporality = t
            .metrics_temporality
            .as_deref()
            .map(|s| parse_temporality(s, src))
            .transpose()?;
        // TOML values are authored, so unknown list entries are errors there.
        if let Some(first) = w.into_iter().next() {
            return Err(Error::Config(first));
        }
        Ok(c)
    }

    /// Put a TOML layer (from [`Self::from_toml_str`]) under this one.
    pub fn with_file_layer(mut self, file: ObservabilityConfig) -> Self {
        self.file = Some(Box::new(file));
        self
    }

    /// Resolve against the process environment.
    pub fn resolve(&self) -> Result<Resolved> {
        self.resolve_with_env(&|k| std::env::var(k).ok())
    }

    /// Resolve against an explicit environment (tests, or callers that pass a
    /// snapshot). `env` returns the value of a variable, or `None`.
    pub fn resolve_with_env(&self, env: &dyn Fn(&str) -> Option<String>) -> Result<Resolved> {
        let mut warnings = Vec::new();
        let env_layer = if self.from_env {
            Some(Self::from_env_with(env, &mut warnings)?)
        } else {
            None
        };
        let mut layers: Vec<&ObservabilityConfig> = vec![self];
        if let Some(e) = &env_layer {
            layers.push(e);
        }
        if let Some(f) = &self.file {
            layers.push(f);
        }
        let config = resolve_layers(&layers, &mut warnings)?;
        Ok(Resolved { config, warnings })
    }

    /// The environment as a config layer.
    fn from_env_with(env: &dyn Fn(&str) -> Option<String>, w: &mut Vec<String>) -> Result<Self> {
        let get = |k: &str| {
            env(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let mut c = ObservabilityConfig::default();
        if let Some(v) = get(OTEL_SDK_DISABLED) {
            c.disabled = Some(v.eq_ignore_ascii_case("true"));
        }
        c.service_name = get(OTEL_SERVICE_NAME);
        if let Some(v) = get(OTEL_RESOURCE_ATTRIBUTES) {
            c.resource_attributes = parse_resource_attributes(&v, w);
        }
        c.traces_exporter = get(OTEL_TRACES_EXPORTER)
            .map(|v| parse_exporters(&v, OTEL_TRACES_EXPORTER, w))
            .transpose()?;
        c.metrics_exporter = get(OTEL_METRICS_EXPORTER)
            .map(|v| parse_exporters(&v, OTEL_METRICS_EXPORTER, w))
            .transpose()?;
        c.logs_exporter = get(OTEL_LOGS_EXPORTER)
            .map(|v| parse_exporters(&v, OTEL_LOGS_EXPORTER, w))
            .transpose()?;
        c.otlp = otlp_from_env(&get, "OTEL_EXPORTER_OTLP", w)?;
        c.otlp_traces = otlp_from_env(&get, "OTEL_EXPORTER_OTLP_TRACES", w)?;
        c.otlp_metrics = otlp_from_env(&get, "OTEL_EXPORTER_OTLP_METRICS", w)?;
        c.otlp_logs = otlp_from_env(&get, "OTEL_EXPORTER_OTLP_LOGS", w)?;
        if let Some(v) = get(OTEL_EXPORTER_OTLP_INSECURE) {
            c.otlp_insecure = Some(v.eq_ignore_ascii_case("true"));
        }
        if let Some(s) = get(OTEL_TRACES_SAMPLER) {
            match parse_sampler(
                &s,
                get(OTEL_TRACES_SAMPLER_ARG).as_deref(),
                OTEL_TRACES_SAMPLER,
            ) {
                Ok(s) => c.sampler = Some(s),
                Err(Error::Config(m)) => w.push(format!("{m}; using the default sampler")),
                Err(e) => return Err(e),
            }
        }
        c.propagators = get(OTEL_PROPAGATORS)
            .map(|v| parse_propagators(&v, OTEL_PROPAGATORS, w))
            .transpose()?;
        c.metric_export_interval = parse_ms(
            get(OTEL_METRIC_EXPORT_INTERVAL),
            OTEL_METRIC_EXPORT_INTERVAL,
            w,
        );
        if let Some(v) = get(OTEL_METRICS_TEMPORALITY) {
            match parse_temporality(&v, OTEL_METRICS_TEMPORALITY) {
                Ok(t) => c.metrics_temporality = Some(t),
                Err(Error::Config(m)) => w.push(format!("{m}; using cumulative")),
                Err(e) => return Err(e),
            }
        }
        c.span_batch = batch_from_env(&get, "OTEL_BSP", w);
        c.log_batch = batch_from_env(&get, "OTEL_BLRP", w);
        c.span_limits = SpanLimitSettings {
            max_attributes_per_span: parse_num(
                get("OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT"),
                "OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT",
                w,
            ),
            max_events_per_span: parse_num(
                get("OTEL_SPAN_EVENT_COUNT_LIMIT"),
                "OTEL_SPAN_EVENT_COUNT_LIMIT",
                w,
            ),
            max_links_per_span: parse_num(
                get("OTEL_SPAN_LINK_COUNT_LIMIT"),
                "OTEL_SPAN_LINK_COUNT_LIMIT",
                w,
            ),
        };
        c.log_filter = get(crate::ENV_FILTER);
        c.log_level = get(ASTER_LOG_LEVEL);
        if let Some(v) = env(crate::ENV_STDERR) {
            c.log_stderr = Some(crate::subscriber::truthy(Some(&v)));
        }
        c.log_queue = parse_num(get(ASTER_LOG_QUEUE), ASTER_LOG_QUEUE, w);
        Ok(c)
    }
}

/// A resolved configuration plus the warnings resolution produced (ignored
/// env values, insecure endpoints). The pipeline logs the warnings as `WARN`
/// events on target `aster::otel`.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub config: ResolvedConfig,
    pub warnings: Vec<String>,
}

/// One signal's OTLP target, fully resolved.
#[derive(Clone, PartialEq)]
pub struct OtlpTarget {
    /// The full URL (`.../v1/traces` etc.).
    pub endpoint: String,
    pub protocol: OtlpProtocol,
    pub headers: OtlpHeaders,
    pub timeout: Duration,
}

impl fmt::Debug for OtlpTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpTarget")
            .field("endpoint", &redact_url(&self.endpoint))
            .field("protocol", &self.protocol)
            .field("headers", &self.headers)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Batch processor settings, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedBatch {
    pub max_queue_size: usize,
    pub max_export_batch_size: usize,
    pub scheduled_delay: Duration,
}

/// Every setting with an explicit value. Two `init` calls agree when their
/// `ResolvedConfig`s are equal.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConfig {
    pub disabled: bool,
    pub service_name: String,
    /// Sorted by key; excludes `service.name` (see `service_name`).
    pub resource_attributes: Vec<(String, String)>,
    pub binding: Binding,
    pub traces_exporter: Vec<ExporterKind>,
    pub metrics_exporter: Vec<ExporterKind>,
    pub logs_exporter: Vec<ExporterKind>,
    pub otlp_traces: OtlpTarget,
    pub otlp_metrics: OtlpTarget,
    pub otlp_logs: OtlpTarget,
    pub otlp_insecure: bool,
    pub sampler: SamplerConfig,
    pub propagators: Vec<Propagator>,
    pub metric_export_interval: Duration,
    pub metrics_temporality: Temporality,
    pub span_batch: ResolvedBatch,
    pub log_batch: ResolvedBatch,
    pub span_max_attributes: u32,
    pub span_max_events: u32,
    pub span_max_links: u32,
    pub log_filter: String,
    pub span_filter: String,
    pub log_stderr: bool,
    pub log_queue: usize,
    pub attribute_extractors: Vec<AttributeExtractor>,
    pub propagate_as_baggage: Vec<String>,
}

impl ResolvedConfig {
    /// The names of the settings that differ between `self` and `other`, for
    /// [`Error::AlreadyInitialized`]. Values are not included (some are secret).
    pub fn diff(&self, other: &ResolvedConfig) -> Vec<&'static str> {
        let mut d = Vec::new();
        macro_rules! cmp {
            ($($f:ident),* $(,)?) => { $( if self.$f != other.$f { d.push(stringify!($f)); } )* };
        }
        cmp!(
            disabled,
            service_name,
            resource_attributes,
            binding,
            traces_exporter,
            metrics_exporter,
            logs_exporter,
            otlp_insecure,
            sampler,
            propagators,
            metric_export_interval,
            metrics_temporality,
            span_batch,
            log_batch,
            span_max_attributes,
            span_max_events,
            span_max_links,
            log_filter,
            span_filter,
            log_stderr,
            log_queue,
            attribute_extractors,
            propagate_as_baggage,
        );
        type Names = [&'static str; 4];
        const TRACES: Names = [
            "otlp_traces.endpoint",
            "otlp_traces.protocol",
            "otlp_traces.headers",
            "otlp_traces.timeout",
        ];
        const METRICS: Names = [
            "otlp_metrics.endpoint",
            "otlp_metrics.protocol",
            "otlp_metrics.headers",
            "otlp_metrics.timeout",
        ];
        const LOGS: Names = [
            "otlp_logs.endpoint",
            "otlp_logs.protocol",
            "otlp_logs.headers",
            "otlp_logs.timeout",
        ];
        for (names, a, b) in [
            (TRACES, &self.otlp_traces, &other.otlp_traces),
            (METRICS, &self.otlp_metrics, &other.otlp_metrics),
            (LOGS, &self.otlp_logs, &other.otlp_logs),
        ] {
            let differs = [
                a.endpoint != b.endpoint,
                a.protocol != b.protocol,
                a.headers != b.headers,
                a.timeout != b.timeout,
            ];
            d.extend(
                names
                    .iter()
                    .zip(differs)
                    .filter(|(_, x)| *x)
                    .map(|(n, _)| *n),
            );
        }
        d
    }
}

// ── resolution ──────────────────────────────────────────────────────────────

fn first<'a, T: 'a>(
    layers: &[&'a ObservabilityConfig],
    f: impl Fn(&'a ObservabilityConfig) -> Option<T>,
) -> Option<T> {
    layers.iter().find_map(|l| f(l))
}

#[derive(Clone, Copy)]
enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Signal {
    fn path(self) -> &'static str {
        match self {
            Signal::Traces => "/v1/traces",
            Signal::Metrics => "/v1/metrics",
            Signal::Logs => "/v1/logs",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
            Signal::Logs => "logs",
        }
    }
    fn settings(self, c: &ObservabilityConfig) -> &OtlpSettings {
        match self {
            Signal::Traces => &c.otlp_traces,
            Signal::Metrics => &c.otlp_metrics,
            Signal::Logs => &c.otlp_logs,
        }
    }
}

/// Per field, the highest layer that sets either the per-signal or the shared
/// value wins; within one layer the per-signal value beats the shared one.
fn resolve_target(layers: &[&ObservabilityConfig], s: Signal) -> Result<(OtlpTarget, bool)> {
    let mut explicit_endpoint = false;
    let endpoint = first(layers, |l| {
        if let Some(e) = s
            .settings(l)
            .endpoint
            .as_ref()
            .filter(|e| !e.trim().is_empty())
        {
            return Some(e.trim().to_owned());
        }
        l.otlp
            .endpoint
            .as_ref()
            .filter(|e| !e.trim().is_empty())
            .map(|e| join_url(e.trim(), s.path()))
    });
    let endpoint = match endpoint {
        Some(e) => {
            explicit_endpoint = true;
            e
        }
        None => join_url(DEFAULT_OTLP_ENDPOINT, s.path()),
    };
    validate_endpoint(&endpoint, s.name())?;
    let protocol = first(layers, |l| s.settings(l).protocol.or(l.otlp.protocol))
        .unwrap_or(OtlpProtocol::HttpProtobuf);
    let headers = first(layers, |l| {
        s.settings(l)
            .headers
            .clone()
            .or_else(|| l.otlp.headers.clone())
    })
    .unwrap_or_default();
    let timeout =
        first(layers, |l| s.settings(l).timeout.or(l.otlp.timeout)).unwrap_or(DEFAULT_OTLP_TIMEOUT);
    Ok((
        OtlpTarget {
            endpoint,
            protocol,
            headers,
            timeout,
        },
        explicit_endpoint,
    ))
}

fn resolve_batch(
    layers: &[&ObservabilityConfig],
    f: fn(&ObservabilityConfig) -> &BatchSettings,
    delay: Duration,
) -> ResolvedBatch {
    let max_queue_size = first(layers, |l| f(l).max_queue_size)
        .unwrap_or(2048)
        .max(1);
    let max_export_batch_size = first(layers, |l| f(l).max_export_batch_size)
        .unwrap_or(512)
        .clamp(1, max_queue_size);
    let scheduled_delay = first(layers, |l| f(l).scheduled_delay).unwrap_or(delay);
    ResolvedBatch {
        max_queue_size,
        max_export_batch_size,
        scheduled_delay,
    }
}

fn resolve_layers(layers: &[&ObservabilityConfig], w: &mut Vec<String>) -> Result<ResolvedConfig> {
    let disabled = first(layers, |l| l.disabled).unwrap_or(false);

    // Resource: lower layers first, higher ones override by key.
    let mut attrs: BTreeMap<String, String> = BTreeMap::new();
    for l in layers.iter().rev() {
        for (k, v) in &l.resource_attributes {
            attrs.insert(k.clone(), v.clone());
        }
    }
    let attr_service = attrs.remove("service.name");
    let service_name = first(layers, |l| l.service_name.clone())
        .or(attr_service)
        .unwrap_or_else(default_service_name);
    let binding = first(layers, |l| l.binding).unwrap_or_default();

    let (otlp_traces, _) = resolve_target(layers, Signal::Traces)?;
    let (otlp_metrics, metrics_endpoint_set) = resolve_target(layers, Signal::Metrics)?;
    let (otlp_logs, _) = resolve_target(layers, Signal::Logs)?;

    let traces_exporter =
        first(layers, |l| l.traces_exporter.clone()).unwrap_or_else(|| vec![ExporterKind::Otlp]);
    // Metrics push only when someone said where to (design §2.3).
    let metrics_exporter = first(layers, |l| l.metrics_exporter.clone()).unwrap_or_else(|| {
        if metrics_endpoint_set {
            vec![ExporterKind::Otlp]
        } else {
            Vec::new()
        }
    });
    let logs_exporter = first(layers, |l| l.logs_exporter.clone()).unwrap_or_default();
    for (name, list) in [("traces", &traces_exporter), ("logs", &logs_exporter)] {
        if list.contains(&ExporterKind::Prometheus) {
            return Err(Error::Config(format!(
                "the prometheus exporter is metrics-only, not valid for {name}"
            )));
        }
    }

    let otlp_insecure = first(layers, |l| l.otlp_insecure).unwrap_or(false);
    for (sig, list, target) in [
        ("traces", &traces_exporter, &otlp_traces),
        ("metrics", &metrics_exporter, &otlp_metrics),
        ("logs", &logs_exporter, &otlp_logs),
    ] {
        if list.contains(&ExporterKind::Otlp)
            && !otlp_insecure
            && is_plaintext_remote(&target.endpoint)
        {
            w.push(format!(
                "OTLP {sig} endpoint {} is plain http to a non-loopback host: telemetry and any \
                 OTLP headers travel unencrypted (use https, or set insecure to accept this)",
                redact_url(&target.endpoint)
            ));
        }
    }

    let sampler = first(layers, |l| l.sampler).unwrap_or(SamplerConfig::ParentBasedAlwaysOn);
    let propagators = first(layers, |l| l.propagators.clone())
        .unwrap_or_else(|| vec![Propagator::TraceContext, Propagator::Baggage]);
    let metric_export_interval = first(layers, |l| l.metric_export_interval)
        .filter(|d| !d.is_zero())
        .unwrap_or(DEFAULT_METRIC_INTERVAL);
    let metrics_temporality =
        first(layers, |l| l.metrics_temporality).unwrap_or(Temporality::Cumulative);
    let span_batch = resolve_batch(layers, |l| &l.span_batch, Duration::from_secs(5));
    let log_batch = resolve_batch(layers, |l| &l.log_batch, Duration::from_secs(1));
    let span_max_attributes =
        first(layers, |l| l.span_limits.max_attributes_per_span).unwrap_or(128);
    let span_max_events = first(layers, |l| l.span_limits.max_events_per_span).unwrap_or(128);
    let span_max_links = first(layers, |l| l.span_limits.max_links_per_span).unwrap_or(128);

    let log_filter = match first(layers, |l| l.log_filter.clone()) {
        Some(f) => f,
        None => match first(layers, |l| l.log_level.clone()) {
            Some(level) => format!("warn,aster={}", level.trim()),
            None => crate::DEFAULT_FILTER.to_owned(),
        },
    };
    validate_filter(&log_filter, "log filter")?;
    let span_filter =
        first(layers, |l| l.span_filter.clone()).unwrap_or_else(|| log_filter.clone());
    validate_filter(&span_filter, "span filter")?;
    let log_stderr = first(layers, |l| l.log_stderr).unwrap_or(false);
    let log_queue = first(layers, |l| l.log_queue)
        .unwrap_or(DEFAULT_LOG_QUEUE)
        .max(1);
    let attribute_extractors = first(layers, |l| l.attribute_extractors.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|(key, source)| {
            let key = key.trim().to_owned();
            if key.is_empty() {
                return Err(Error::Config(format!(
                    "attribute extractor for {source:?}: empty attribute key"
                )));
            }
            Ok(AttributeExtractor {
                key,
                source: ExtractorSource::parse(&source)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let propagate_as_baggage =
        first(layers, |l| l.propagate_as_baggage.clone()).unwrap_or_default();
    for k in &propagate_as_baggage {
        if !attribute_extractors.iter().any(|e| &e.key == k) {
            return Err(Error::Config(format!(
                "propagate_as_baggage names {k:?}, which no attribute extractor sets"
            )));
        }
    }

    Ok(ResolvedConfig {
        disabled,
        service_name,
        resource_attributes: attrs.into_iter().collect(),
        binding,
        traces_exporter,
        metrics_exporter,
        logs_exporter,
        otlp_traces,
        otlp_metrics,
        otlp_logs,
        otlp_insecure,
        sampler,
        propagators,
        metric_export_interval,
        metrics_temporality,
        span_batch,
        log_batch,
        span_max_attributes,
        span_max_events,
        span_max_links,
        log_filter,
        span_filter,
        log_stderr,
        log_queue,
        attribute_extractors,
        propagate_as_baggage,
    })
}

/// `directives` plus `<target>=off` for each exporter-internal target the
/// directives do not already mention.
pub fn with_exporter_targets_off(directives: &str) -> String {
    let mentioned = |t: &str| {
        directives.split(',').any(|d| {
            d.trim()
                .split(['=', '['])
                .next()
                .is_some_and(|target| target.trim().starts_with(t))
        })
    };
    let mut out = directives.trim().to_owned();
    for t in EXPORTER_INTERNAL_TARGETS {
        if !mentioned(t) {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(t);
            out.push_str("=off");
        }
    }
    out
}

fn validate_filter(directives: &str, what: &str) -> Result<()> {
    tracing_subscriber::EnvFilter::builder()
        .parse(directives)
        .map(|_| ())
        .map_err(|e| Error::Config(format!("{what} {directives:?}: {e}")))
}

fn default_service_name() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    format!("unknown_service:{exe}")
}

// ── parsing helpers ─────────────────────────────────────────────────────────

fn otlp_from_env(
    get: &dyn Fn(&str) -> Option<String>,
    prefix: &str,
    w: &mut Vec<String>,
) -> Result<OtlpSettings> {
    let mut s = OtlpSettings {
        endpoint: get(&format!("{prefix}_ENDPOINT")),
        ..Default::default()
    };
    if let Some(p) = get(&format!("{prefix}_PROTOCOL")) {
        s.protocol = Some(parse_protocol(&p, &format!("{prefix}_PROTOCOL"))?);
    }
    if let Some(h) = get(&format!("{prefix}_HEADERS")) {
        let (headers, warnings) = OtlpHeaders::parse(&h);
        w.extend(
            warnings
                .into_iter()
                .map(|m| format!("{prefix}_HEADERS: {m}")),
        );
        s.headers = Some(headers);
    }
    s.timeout = parse_ms(
        get(&format!("{prefix}_TIMEOUT")),
        &format!("{prefix}_TIMEOUT"),
        w,
    );
    if let Some(c) = get(&format!("{prefix}_COMPRESSION")) {
        if !c.eq_ignore_ascii_case("none") {
            w.push(format!(
                "{prefix}_COMPRESSION={c:?} is not supported yet; exporting uncompressed"
            ));
        }
    }
    Ok(s)
}

fn batch_from_env(
    get: &dyn Fn(&str) -> Option<String>,
    prefix: &str,
    w: &mut Vec<String>,
) -> BatchSettings {
    BatchSettings {
        max_queue_size: parse_num(
            get(&format!("{prefix}_MAX_QUEUE_SIZE")),
            &format!("{prefix}_MAX_QUEUE_SIZE"),
            w,
        ),
        max_export_batch_size: parse_num(
            get(&format!("{prefix}_MAX_EXPORT_BATCH_SIZE")),
            &format!("{prefix}_MAX_EXPORT_BATCH_SIZE"),
            w,
        ),
        scheduled_delay: parse_ms(
            get(&format!("{prefix}_SCHEDULE_DELAY")),
            &format!("{prefix}_SCHEDULE_DELAY"),
            w,
        ),
    }
}

fn parse_num<T: std::str::FromStr>(
    v: Option<String>,
    name: &str,
    w: &mut Vec<String>,
) -> Option<T> {
    let v = v?;
    match v.parse() {
        Ok(n) => Some(n),
        Err(_) => {
            w.push(format!("ignoring {name}={v:?}: not a number"));
            None
        }
    }
}

fn parse_ms(v: Option<String>, name: &str, w: &mut Vec<String>) -> Option<Duration> {
    parse_num::<u64>(v, name, w).map(Duration::from_millis)
}

fn parse_exporters(v: &str, src: &str, w: &mut Vec<String>) -> Result<Vec<ExporterKind>> {
    let mut out = Vec::new();
    for item in v
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        match item.as_str() {
            "none" => return Ok(Vec::new()),
            "otlp" => out.push(ExporterKind::Otlp),
            "prometheus" => out.push(ExporterKind::Prometheus),
            other => w.push(format!(
                "{src}: exporter {other:?} is not supported; ignoring it"
            )),
        }
    }
    out.dedup();
    Ok(out)
}

fn parse_protocol(v: &str, src: &str) -> Result<OtlpProtocol> {
    match v.trim().to_ascii_lowercase().as_str() {
        "http/protobuf" => Ok(OtlpProtocol::HttpProtobuf),
        "grpc" => Ok(OtlpProtocol::Grpc),
        "http/json" => Err(Error::Config(format!(
            "{src}: http/json is not supported; use http/protobuf"
        ))),
        other => Err(Error::Config(format!(
            "{src}: unknown OTLP protocol {other:?}"
        ))),
    }
}

fn parse_sampler(name: &str, arg: Option<&str>, src: &str) -> Result<SamplerConfig> {
    let ratio = || -> Result<f64> {
        match arg.map(str::trim) {
            None | Some("") => Ok(1.0),
            Some(a) => a
                .parse::<f64>()
                .ok()
                .filter(|r| (0.0..=1.0).contains(r))
                .ok_or_else(|| {
                    Error::Config(format!(
                        "{src}: sampler argument {a:?} is not a ratio in [0, 1]"
                    ))
                }),
        }
    };
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "always_on" => SamplerConfig::AlwaysOn,
        "always_off" => SamplerConfig::AlwaysOff,
        "traceidratio" => SamplerConfig::TraceIdRatio(ratio()?),
        "parentbased_always_on" => SamplerConfig::ParentBasedAlwaysOn,
        "parentbased_always_off" => SamplerConfig::ParentBasedAlwaysOff,
        "parentbased_traceidratio" => SamplerConfig::ParentBasedTraceIdRatio(ratio()?),
        other => return Err(Error::Config(format!("{src}: unknown sampler {other:?}"))),
    })
}

fn parse_propagators(v: &str, src: &str, w: &mut Vec<String>) -> Result<Vec<Propagator>> {
    let mut out = Vec::new();
    for item in v
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        match item.as_str() {
            "none" => return Ok(Vec::new()),
            "tracecontext" => out.push(Propagator::TraceContext),
            "baggage" => out.push(Propagator::Baggage),
            other => w.push(format!(
                "{src}: propagator {other:?} is not supported; ignoring it"
            )),
        }
    }
    out.dedup();
    Ok(out)
}

fn parse_temporality(v: &str, src: &str) -> Result<Temporality> {
    match v.trim().to_ascii_lowercase().as_str() {
        "cumulative" => Ok(Temporality::Cumulative),
        "delta" => Ok(Temporality::Delta),
        "lowmemory" => Ok(Temporality::LowMemory),
        other => Err(Error::Config(format!(
            "{src}: unknown temporality {other:?}"
        ))),
    }
}

fn parse_resource_attributes(v: &str, w: &mut Vec<String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, entry) in v.split(',').enumerate() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry.split_once('=') {
            Some((k, val)) if !k.trim().is_empty() => {
                out.push((k.trim().to_owned(), percent_decode(val.trim())))
            }
            _ => w.push(format!(
                "{OTEL_RESOURCE_ATTRIBUTES}: ignoring malformed entry #{}",
                i + 1
            )),
        }
    }
    out
}

fn valid_header_name(name: &str) -> bool {
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `%XX` decoding (W3C baggage octets, as the OTel env formats use). Invalid
/// escapes are kept literally.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// Split `scheme://[userinfo@]host[:port]/...` into (scheme, host).
fn scheme_and_host(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    Some((scheme, host))
}

fn validate_endpoint(url: &str, signal: &str) -> Result<()> {
    match scheme_and_host(url) {
        Some((scheme, host))
            if (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
                && !host.is_empty() =>
        {
            Ok(())
        }
        _ => Err(Error::Config(format!(
            "OTLP {signal} endpoint {} is not an http(s) URL with a host",
            redact_url(url)
        ))),
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

fn is_plaintext_remote(url: &str) -> bool {
    matches!(scheme_and_host(url), Some((scheme, host)) if scheme.eq_ignore_ascii_case("http") && !is_loopback_host(host))
}

/// An endpoint for logs and errors: userinfo replaced by `***`, query and
/// fragment dropped.
pub fn redact_url(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or("");
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<invalid url>".to_owned();
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("***@{host}"),
        None => authority.to_owned(),
    };
    format!("{scheme}://{authority}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults() {
        let r = ObservabilityConfig::new()
            .resolve_with_env(&env(&[]))
            .unwrap();
        let c = r.config;
        assert!(!c.disabled);
        assert!(c.service_name.starts_with("unknown_service:"));
        assert_eq!(c.traces_exporter, vec![ExporterKind::Otlp]);
        assert!(c.metrics_exporter.is_empty(), "metrics need an endpoint");
        assert!(c.logs_exporter.is_empty());
        assert_eq!(c.otlp_traces.endpoint, "http://localhost:4318/v1/traces");
        assert_eq!(c.otlp_traces.protocol, OtlpProtocol::HttpProtobuf);
        assert_eq!(c.otlp_traces.timeout, DEFAULT_OTLP_TIMEOUT);
        assert_eq!(c.sampler, SamplerConfig::ParentBasedAlwaysOn);
        assert_eq!(
            c.propagators,
            vec![Propagator::TraceContext, Propagator::Baggage]
        );
        assert_eq!(c.metric_export_interval, DEFAULT_METRIC_INTERVAL);
        assert_eq!(c.log_filter, crate::DEFAULT_FILTER);
        assert_eq!(c.span_filter, crate::DEFAULT_FILTER);
        assert_eq!(c.log_queue, DEFAULT_LOG_QUEUE);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn programmatic_beats_env_beats_toml() {
        let toml = ObservabilityConfig::from_toml_str(
            r#"
            [observability]
            service_name = "from-toml"
            log_filter = "debug"
            metric_export_interval_ms = 1000
            sampler = "always_off"
            otlp_endpoint = "https://toml.example.com"
            "#,
        )
        .unwrap();
        let e = env(&[
            ("OTEL_SERVICE_NAME", "from-env"),
            ("ASTER_LOG", "info"),
            ("OTEL_TRACES_SAMPLER", "traceidratio"),
            ("OTEL_TRACES_SAMPLER_ARG", "0.25"),
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "https://env.example.com/custom",
            ),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-probe=from-env"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "1234"),
        ]);
        let mut p = ObservabilityConfig::new().with_file_layer(toml.clone());
        p.service_name = Some("from-code".into());
        p.otlp.endpoint = Some("https://code.example.com".into());
        p.otlp.headers = Some(OtlpHeaders::new([("x-probe", "from-code")]));
        let c = p.resolve_with_env(&e).unwrap().config;
        assert_eq!(c.service_name, "from-code");
        // The programmatic shared endpoint outranks env's per-signal one.
        assert_eq!(c.otlp_traces.endpoint, "https://code.example.com/v1/traces");
        assert_eq!(
            c.otlp_traces.headers,
            OtlpHeaders::new([("x-probe", "from-code")])
        );
        // Env beats TOML where code is silent.
        assert_eq!(c.log_filter, "info");
        assert_eq!(c.sampler, SamplerConfig::TraceIdRatio(0.25));
        assert_eq!(c.otlp_traces.timeout, Duration::from_millis(1234));
        // TOML beats defaults.
        assert_eq!(c.metric_export_interval, Duration::from_secs(1));

        // Without env, TOML shows through; the env per-signal endpoint is used as-is.
        let c = ObservabilityConfig::new()
            .with_file_layer(toml)
            .resolve_with_env(&env(&[]))
            .unwrap()
            .config;
        assert_eq!(c.service_name, "from-toml");
        assert_eq!(c.sampler, SamplerConfig::AlwaysOff);
        assert_eq!(
            c.otlp_metrics.endpoint,
            "https://toml.example.com/v1/metrics"
        );
        assert_eq!(
            c.metrics_exporter,
            vec![ExporterKind::Otlp],
            "an endpoint enables metrics"
        );
        let c = ObservabilityConfig::new()
            .resolve_with_env(&env(&[(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "https://env.example.com/custom",
            )]))
            .unwrap()
            .config;
        assert_eq!(c.otlp_traces.endpoint, "https://env.example.com/custom");
    }

    #[test]
    fn from_env_false_ignores_the_environment() {
        let p = ObservabilityConfig {
            from_env: false,
            ..Default::default()
        };
        let c = p
            .resolve_with_env(&env(&[
                ("OTEL_SERVICE_NAME", "from-env"),
                ("OTEL_SDK_DISABLED", "true"),
            ]))
            .unwrap()
            .config;
        assert!(c.service_name.starts_with("unknown_service:"));
        assert!(!c.disabled);
    }

    #[test]
    fn sdk_disabled_and_none_lists() {
        let c = ObservabilityConfig::new()
            .resolve_with_env(&env(&[
                ("OTEL_SDK_DISABLED", "TRUE"),
                ("OTEL_TRACES_EXPORTER", "none"),
                ("OTEL_PROPAGATORS", "none"),
            ]))
            .unwrap()
            .config;
        assert!(c.disabled);
        assert!(c.traces_exporter.is_empty());
        assert!(c.propagators.is_empty());
    }

    #[test]
    fn log_level_shorthand_and_filter_validation() {
        let c = ObservabilityConfig::new()
            .resolve_with_env(&env(&[("ASTER_LOG_LEVEL", "debug")]))
            .unwrap()
            .config;
        assert_eq!(c.log_filter, "warn,aster=debug");
        let c = ObservabilityConfig::new()
            .resolve_with_env(&env(&[
                ("ASTER_LOG_LEVEL", "debug"),
                ("ASTER_LOG", "error"),
            ]))
            .unwrap()
            .config;
        assert_eq!(c.log_filter, "error");
        let p = ObservabilityConfig {
            log_filter: Some("aster=notalevel".into()),
            ..Default::default()
        };
        assert!(matches!(
            p.resolve_with_env(&env(&[])),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn exporter_internal_targets_are_turned_off_unless_named() {
        assert_eq!(
            with_exporter_targets_off("warn,aster=info"),
            "warn,aster=info,reqwest=off,hyper=off,opentelemetry=off"
        );
        assert_eq!(
            with_exporter_targets_off("hyper=debug"),
            "hyper=debug,reqwest=off,opentelemetry=off"
        );
    }

    #[test]
    fn secrets_never_print() {
        let (h, warnings) =
            OtlpHeaders::parse("authorization=Bearer%20s3cr3t-token,bad entry,x-tenant=acme");
        assert_eq!(h.len(), 2);
        assert_eq!(warnings.len(), 1);
        assert!(!warnings[0].contains("bad entry"));
        assert_eq!(
            h.iter().find(|(k, _)| *k == "authorization").unwrap().1,
            "Bearer s3cr3t-token"
        );
        assert_eq!(format!("{h:?}"), "<redacted>");
        assert_eq!(format!("{h}"), "<redacted>");

        let mut p = ObservabilityConfig::new();
        p.otlp.headers = Some(h.clone());
        p.otlp.endpoint = Some("https://user:hunter2@collector.example.com:4318?token=abc".into());
        let dbg = format!("{p:?}");
        assert!(
            !dbg.contains("s3cr3t") && !dbg.contains("hunter2") && !dbg.contains("token=abc"),
            "{dbg}"
        );
        let resolved = p
            .resolve_with_env(&env(&[(
                "OTEL_EXPORTER_OTLP_HEADERS",
                "authorization=env-s3cr3t",
            )]))
            .unwrap();
        let dbg = format!("{resolved:?}");
        assert!(!dbg.contains("s3cr3t") && !dbg.contains("hunter2"), "{dbg}");

        // Error and warning text name the setting, never the value.
        let p = ObservabilityConfig::new();
        let e = p
            .resolve_with_env(&env(&[(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "ftp://user:hunter2@example.com",
            )]))
            .unwrap_err();
        assert!(!e.to_string().contains("hunter2"), "{e}");
        let mut other = resolved.config.clone();
        other.otlp_traces.headers = OtlpHeaders::new([("authorization", "different-s3cr3t")]);
        let d = resolved.config.diff(&other);
        assert_eq!(d, vec!["otlp_traces.headers"]);
    }

    #[test]
    fn plaintext_remote_endpoints_warn_unless_insecure() {
        let e = env(&[(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://collector.example.com:4318",
        )]);
        let r = ObservabilityConfig::new().resolve_with_env(&e).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("plain http")),
            "{:?}",
            r.warnings
        );
        let p = ObservabilityConfig {
            otlp_insecure: Some(true),
            ..Default::default()
        };
        assert!(p.resolve_with_env(&e).unwrap().warnings.is_empty());
        for local in [
            "http://127.0.0.1:4318",
            "http://localhost:4318",
            "http://[::1]:4318",
        ] {
            let r = ObservabilityConfig::new()
                .resolve_with_env(&env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", local)]))
                .unwrap();
            assert!(r.warnings.is_empty(), "{local}: {:?}", r.warnings);
        }
    }

    #[test]
    fn toml_rejects_unknown_keys_and_headers() {
        assert!(
            ObservabilityConfig::from_toml_str("[observability]\notlp_headers = \"a=b\"\n")
                .is_err()
        );
        assert!(ObservabilityConfig::from_toml_str(
            "[observability]\ntraces_exporter = \"zipkin\"\n"
        )
        .is_err());
        let c = ObservabilityConfig::from_toml_str("[other]\nx = 1\n").unwrap();
        assert!(c.service_name.is_none());
        let c = ObservabilityConfig::from_toml_str(
            "[observability]\ntraces_exporter = [\"otlp\"]\nmetrics_exporter = \"prometheus\"\n[observability.resource_attributes]\n\"deployment.environment\" = \"test\"\n",
        )
        .unwrap();
        assert_eq!(c.metrics_exporter, Some(vec![ExporterKind::Prometheus]));
        assert_eq!(
            c.resource_attributes,
            vec![("deployment.environment".to_owned(), "test".to_owned())]
        );
    }

    #[test]
    fn attribute_extractors_parse_and_validate() {
        let file = ObservabilityConfig::from_toml_str(
            "[observability]\npropagate_as_baggage = [\"tenant\"]\n[observability.attribute_extractors]\ntenant = \"attr:tenant_id\"\n\"app.region\" = \"meta:x-region\"\n",
        )
        .unwrap();
        let c = ObservabilityConfig::new()
            .with_file_layer(file)
            .resolve_with_env(&env(&[]))
            .unwrap()
            .config;
        assert_eq!(
            c.attribute_extractors,
            vec![
                AttributeExtractor {
                    key: "app.region".into(),
                    source: ExtractorSource::Metadata("x-region".into()),
                },
                AttributeExtractor {
                    key: "tenant".into(),
                    source: ExtractorSource::Attribute("tenant_id".into()),
                },
            ]
        );
        assert_eq!(c.propagate_as_baggage, ["tenant"]);

        let mut p = ObservabilityConfig::new();
        p.attribute_extractors = Some(vec![("t".into(), "cookie:x".into())]);
        assert!(p.resolve_with_env(&env(&[])).is_err());
        p.attribute_extractors = Some(vec![("t".into(), "baggage:".into())]);
        assert!(p.resolve_with_env(&env(&[])).is_err());
        p.attribute_extractors = Some(vec![("t".into(), "baggage:tenant".into())]);
        p.propagate_as_baggage = Some(vec!["other".into()]);
        assert!(p.resolve_with_env(&env(&[])).is_err());
    }

    #[test]
    fn resource_attributes_merge_by_key() {
        let mut p = ObservabilityConfig::new();
        p.resource_attributes = vec![("team".into(), "code".into())];
        let c = p
            .resolve_with_env(&env(&[(
                "OTEL_RESOURCE_ATTRIBUTES",
                "team=env,region=eu%2Dwest,service.name=svc-from-attrs",
            )]))
            .unwrap()
            .config;
        assert_eq!(
            c.resource_attributes,
            vec![
                ("region".into(), "eu-west".into()),
                ("team".into(), "code".into())
            ]
        );
        assert_eq!(c.service_name, "svc-from-attrs");
    }

    #[test]
    fn redact_url_strips_userinfo_and_query() {
        assert_eq!(
            redact_url("https://u:p@example.com:4318/v1/traces?k=v"),
            "https://***@example.com:4318/v1/traces"
        );
        assert_eq!(redact_url("http://127.0.0.1:4318"), "http://127.0.0.1:4318");
        assert_eq!(percent_decode("a%20b%zz%4"), "a b%zz%4");
    }
}
