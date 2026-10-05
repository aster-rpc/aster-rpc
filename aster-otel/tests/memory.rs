//! O1 pipeline against the in-memory exporters (feature `testing`): batching,
//! flush on shutdown, what the span layer sees, OTLP logs, Prometheus. Each
//! case runs in a child process (see `common`).

#![cfg(feature = "testing")]

mod common;

use std::time::Duration;

use aster_otel::testing::TestExporters;
use aster_otel::ObservabilityConfig;
use common::{child_case, run};

fn with_test_exporters(t: &TestExporters) -> ObservabilityConfig {
    let mut cfg = ObservabilityConfig {
        from_env: false,
        test_exporters: Some(t.clone()),
        ..Default::default()
    };
    // Nothing leaves the batch processor on its own during the test.
    cfg.span_batch.scheduled_delay = Some(Duration::from_secs(3600));
    cfg.log_batch.scheduled_delay = Some(Duration::from_secs(3600));
    cfg.metric_export_interval = Some(Duration::from_secs(3600));
    cfg
}

fn three_spans() {
    tracing::info_span!(target: "aster::probe", "span-1").in_scope(|| {
        tracing::info!(target: "aster::probe", "event-inside-span");
        tracing::info_span!(target: "aster::probe", "span-2").in_scope(|| {});
    });
    tracing::info_span!(target: "aster::probe", "span-3").in_scope(|| {});
    // Below the default filter (warn,aster=info): never exported.
    tracing::debug_span!(target: "aster::probe", "span-debug").in_scope(|| {});
    tracing::info_span!(target: "other::probe", "span-foreign-info").in_scope(|| {});
}

#[test]
fn child() {
    let Some(case) = child_case() else { return };
    match case.as_str() {
        "flush_on_shutdown" => case_flush_on_shutdown(),
        "flush_on_last_drop" => case_flush_on_last_drop(),
        "spans_only" => case_spans_only(),
        "reinit" => case_reinit(),
        #[cfg(feature = "otlp-logs")]
        "otlp_logs" => case_otlp_logs(),
        #[cfg(feature = "prometheus")]
        "prometheus" => case_prometheus(),
        #[cfg(feature = "prometheus")]
        "prometheus_implicit" => case_prometheus_implicit(),
        #[cfg(feature = "prometheus")]
        "histogram_buckets" => case_histogram_buckets(),
        #[cfg(feature = "prometheus")]
        "prometheus_reinit" => case_prometheus_reinit(),
        #[cfg(feature = "prometheus")]
        "prometheus_disabled" => case_prometheus_disabled(),
        #[cfg(feature = "prometheus")]
        "prometheus_reinit_no_provider" => case_prometheus_reinit_no_provider(),
        #[cfg(feature = "prometheus")]
        "prometheus_late_keeps_values" => case_prometheus_late_keeps_values(),
        other => panic!("unknown case {other}"),
    }
    println!("case-ok");
}

fn case_flush_on_shutdown() {
    let t = TestExporters::new();
    let _h = aster_otel::init(with_test_exporters(&t)).unwrap();
    three_spans();
    assert!(t.span_names().is_empty(), "spans should still be buffered");
    aster_otel::shutdown().unwrap();
    let mut names = t.span_names();
    names.sort();
    assert_eq!(names, ["span-1", "span-2", "span-3"]);
}

fn case_flush_on_last_drop() {
    let t = TestExporters::new();
    let h = aster_otel::init(with_test_exporters(&t)).unwrap();
    let h2 = h.clone();
    three_spans();
    drop(h);
    assert!(
        t.span_names().is_empty(),
        "a clone still holds the pipeline"
    );
    drop(h2);
    assert_eq!(t.span_names().len(), 3);
}

/// The span layer sees spans only: events inside a span are not span events
/// (they belong to the log paths, design §3.1). Parent links survive.
fn case_spans_only() {
    let t = TestExporters::new();
    let _h = aster_otel::init(with_test_exporters(&t)).unwrap();
    three_spans();
    aster_otel::flush(Duration::from_secs(10)).unwrap();
    let spans = t.spans.finished_spans();
    let s1 = spans.iter().find(|s| s.name == "span-1").unwrap();
    let s2 = spans.iter().find(|s| s.name == "span-2").unwrap();
    assert!(
        s1.events.is_empty(),
        "events leaked into the span: {:?}",
        s1.events
    );
    assert_eq!(s2.parent_span_id, s1.span_context.span_id());
    assert_eq!(s2.span_context.trace_id(), s1.span_context.trace_id());
    let res = t
        .spans
        .resource()
        .expect("the provider sets the exporter's resource");
    let attr = |k: &str| {
        res.get(&opentelemetry::Key::from(k.to_owned()))
            .map(|v| v.to_string())
    };
    assert_eq!(attr("aster.binding").as_deref(), Some("rust"));
    assert!(attr("telemetry.sdk.name").is_some());
    assert!(attr("service.name").is_some_and(|s| s.starts_with("unknown_service:")));
}

/// Shutdown, then a new init with a different config: the long-lived span
/// layer exports to the new provider (generation swap of the tracer).
fn case_reinit() {
    let a = TestExporters::new();
    let h = aster_otel::init(with_test_exporters(&a)).unwrap();
    tracing::info_span!(target: "aster::probe", "first").in_scope(|| {});
    drop(h);
    let b = TestExporters::new();
    let mut cfg = with_test_exporters(&b);
    cfg.service_name = Some("second".into());
    let _h = aster_otel::init(cfg).unwrap();
    tracing::info_span!(target: "aster::probe", "second").in_scope(|| {});
    aster_otel::shutdown().unwrap();
    assert_eq!(a.span_names(), ["first"]);
    assert_eq!(b.span_names(), ["second"]);
}

#[cfg(feature = "otlp-logs")]
fn case_otlp_logs() {
    let t = TestExporters::new();
    let _h = aster_otel::init(with_test_exporters(&t)).unwrap();
    tracing::warn!(target: "aster::probe", answer = 42, "log-probe");
    tracing::debug!(target: "aster::probe", "log-filtered");
    // Exporter-internal targets are off even at WARN.
    tracing::warn!(target: "reqwest::connect", "log-exporter-internal");
    tracing::warn!(target: "opentelemetry_sdk::x", "log-exporter-internal");
    aster_otel::flush(Duration::from_secs(10)).unwrap();
    let logs = t.logs.get_emitted_logs().unwrap();
    let bodies: Vec<String> = logs
        .iter()
        .filter_map(|l| l.record.body().map(|b| format!("{b:?}")))
        .collect();
    assert!(bodies.iter().any(|b| b.contains("log-probe")), "{bodies:?}");
    assert!(
        !bodies
            .iter()
            .any(|b| b.contains("log-filtered") || b.contains("log-exporter-internal")),
        "{bodies:?}"
    );
}

#[cfg(feature = "prometheus")]
fn case_prometheus() {
    let cfg = ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![aster_otel::ExporterKind::Prometheus]),
        ..Default::default()
    };
    let _h = aster_otel::init(cfg).unwrap();
    let counter = opentelemetry::global::meter_provider()
        .meter("aster-test")
        .u64_counter("probe.requests")
        .build();
    counter.add(2, &[opentelemetry::KeyValue::new("rpc.method", "Ping")]);
    let text = aster_otel::render_prometheus().expect("prometheus reader");
    assert!(
        text.contains("probe_requests_total") && text.contains("rpc_method=\"Ping\""),
        "{text}"
    );
}

/// Binding-declared histograms: a seconds unit gets the RPC seconds buckets,
/// explicit boundaries win, other units keep the SDK default.
#[cfg(feature = "prometheus")]
fn case_histogram_buckets() {
    use aster_otel::instruments::{self, InstrumentDesc, InstrumentKind, Measurement};

    let _h = aster_otel::init_with_prometheus(ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![]),
        ..Default::default()
    })
    .unwrap();
    let desc = |name: &str, unit: &str, boundaries: Vec<f64>| InstrumentDesc {
        kind: InstrumentKind::HistogramF64,
        name: name.into(),
        description: String::new(),
        unit: unit.into(),
        auto_attrs: vec![],
        cardinality_limit: 0,
        boundaries,
    };
    let secs = instruments::instrument_new(desc("probe.secs", "s", vec![])).unwrap();
    let custom = instruments::instrument_new(desc("probe.custom", "s", vec![1.0, 5.0])).unwrap();
    let default = instruments::instrument_new(desc("probe.default", "By", vec![])).unwrap();
    for h in [secs, custom, default] {
        instruments::record(h, Measurement::F64(0.3), 0, &[], 0).unwrap();
    }
    let text = aster_otel::render_prometheus().expect("prometheus reader");
    let les = |metric: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.starts_with(&format!("{metric}_bucket")))
            .map(|l| {
                let i = l.find("le=\"").unwrap() + 4;
                l[i..i + l[i..].find('"').unwrap()].to_string()
            })
            .collect()
    };
    let want_secs: Vec<String> = [
        "0.005", "0.01", "0.025", "0.05", "0.075", "0.1", "0.25", "0.5", "0.75", "1", "2.5", "5",
        "7.5", "10", "+Inf",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(les("probe_secs_seconds"), want_secs, "{text}");
    assert_eq!(les("probe_custom_seconds"), ["1", "5", "+Inf"], "{text}");
    let d = les("probe_default_bytes");
    assert!(
        d.contains(&"1000".to_string()) && d.contains(&"0".to_string()),
        "{d:?}"
    );
    // Bad boundaries are refused.
    assert!(instruments::instrument_new(desc("probe.bad", "s", vec![2.0, 1.0])).is_err());
    assert!(instruments::instrument_new(desc("probe.nan", "s", vec![f64::NAN])).is_err());
    let mut counter = desc("probe.ctr", "", vec![1.0]);
    counter.kind = InstrumentKind::CounterU64;
    assert!(instruments::instrument_new(counter).is_err());
}

#[cfg(feature = "prometheus")]
fn probe_counter(n: u64) {
    opentelemetry::global::meter_provider()
        .meter("aster-test")
        .u64_counter("probe.requests")
        .build()
        .add(n, &[]);
}

/// No pipeline yet: `init_with_prometheus` starts one (metrics exporter
/// `none` notwithstanding) with the pull reader.
#[cfg(feature = "prometheus")]
fn case_prometheus_implicit() {
    let g0 = aster_otel::generation();
    let h = aster_otel::init_with_prometheus(ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![]),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(aster_otel::generation(), g0 + 1);
    probe_counter(3);
    let text = aster_otel::render_prometheus().expect("pull reader");
    assert!(text.contains("probe_requests_total"), "{text}");
    // A second caller joins without another generation.
    let h2 = aster_otel::init_with_prometheus(ObservabilityConfig::default()).unwrap();
    assert_eq!(aster_otel::generation(), g0 + 1);
    drop(h);
    assert!(
        aster_otel::render_prometheus().is_some(),
        "h2 still holds it"
    );
    drop(h2);
    assert!(
        aster_otel::render_prometheus().is_none(),
        "last handle dropped"
    );
}

/// A pipeline that records metrics already has the pull reader (every meter
/// provider is built with it), so joining it keeps the generation and the
/// push reader keeps exporting.
#[cfg(feature = "prometheus")]
fn case_prometheus_reinit() {
    let t = TestExporters::new();
    let h = aster_otel::init(with_test_exporters(&t)).unwrap();
    let g1 = aster_otel::generation();
    assert!(
        aster_otel::render_prometheus().is_some(),
        "the reader comes with the provider"
    );
    // Joins whatever config is running (this one differs from `h`'s).
    let h2 = aster_otel::init_with_prometheus(ObservabilityConfig::default()).unwrap();
    assert_eq!(aster_otel::generation(), g1, "no provider rebuild");
    assert_eq!(h2.generation(), g1);
    probe_counter(5);
    let text = aster_otel::render_prometheus().expect("pull reader");
    assert!(text.contains("probe_requests_total"), "{text}");
    aster_otel::flush(Duration::from_secs(10)).unwrap();
    let pts = t.metric_points("probe.requests");
    assert_eq!(pts.len(), 1, "push reader still exports: {pts:?}");
    assert_eq!(pts[0].value, 5.0);
    // The original config still joins.
    let h3 = aster_otel::init(with_test_exporters(&t)).unwrap();
    drop(h);
    drop(h3);
    assert!(
        aster_otel::render_prometheus().is_some(),
        "h2 still holds it"
    );
    drop(h2);
    assert!(aster_otel::render_prometheus().is_none());
    assert_eq!(aster_otel::generation(), g1 + 1, "shut down once");
}

/// A pipeline with no metrics exporter has no meter provider (nothing was
/// recorded); joining with the pull reader builds one under a new generation.
#[cfg(feature = "prometheus")]
fn case_prometheus_reinit_no_provider() {
    let h = aster_otel::init(ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![]),
        ..Default::default()
    })
    .unwrap();
    let g1 = aster_otel::generation();
    assert!(aster_otel::render_prometheus().is_none());
    let h2 = aster_otel::init_with_prometheus(ObservabilityConfig::default()).unwrap();
    assert_eq!(aster_otel::generation(), g1 + 1);
    assert_eq!(h2.generation(), g1 + 1);
    probe_counter(5);
    let text = aster_otel::render_prometheus().expect("pull reader");
    assert!(text.contains("probe_requests_total"), "{text}");
    drop(h);
    drop(h2);
    assert!(aster_otel::render_prometheus().is_none());
}

/// Values recorded before the pull reader starts are still there when it
/// does: a binding instrument, an instrument off the global meter, and the
/// push reader's copy too.
#[cfg(feature = "prometheus")]
fn case_prometheus_late_keeps_values() {
    use aster_otel::instruments::{self, InstrumentDesc, InstrumentKind, Measurement};

    let t = TestExporters::new();
    let h = aster_otel::init(with_test_exporters(&t)).unwrap();
    let bound = instruments::instrument_new(InstrumentDesc {
        kind: InstrumentKind::CounterU64,
        name: "probe.bound".into(),
        description: String::new(),
        unit: String::new(),
        auto_attrs: vec![],
        cardinality_limit: 0,
        boundaries: vec![],
    })
    .unwrap();
    instruments::record(bound, Measurement::U64(4), 0, &[], 0).unwrap();
    probe_counter(5);

    // The pull reader joins late (what a health listener does).
    let h2 = aster_otel::init_with_prometheus(ObservabilityConfig::default()).unwrap();
    // More after the join, on the same instruments.
    instruments::record(bound, Measurement::U64(1), 0, &[], 0).unwrap();
    probe_counter(2);

    let text = aster_otel::render_prometheus().expect("pull reader");
    let value = |name: &str| -> f64 {
        text.lines()
            .find(|l| l.starts_with(name) && !l.starts_with('#'))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{name} missing:\n{text}"))
    };
    assert_eq!(value("probe_bound_total"), 5.0, "{text}");
    assert_eq!(value("probe_requests_total"), 7.0, "{text}");
    drop(h);
    drop(h2);
}

/// `OTEL_SDK_DISABLED` wins: a no-op handle and no reader.
#[cfg(feature = "prometheus")]
fn case_prometheus_disabled() {
    let h = aster_otel::init_with_prometheus(ObservabilityConfig {
        from_env: false,
        disabled: Some(true),
        ..Default::default()
    })
    .unwrap();
    assert!(h.is_noop());
    assert!(aster_otel::render_prometheus().is_none());
}

#[test]
fn shutdown_flushes_buffered_spans() {
    run("flush_on_shutdown", &[]);
}

#[test]
fn dropping_the_last_handle_flushes() {
    run("flush_on_last_drop", &[]);
}

#[test]
fn span_layer_sees_spans_only_with_parents_and_resource() {
    run("spans_only", &[]);
}

#[test]
fn reinit_after_shutdown_exports_to_the_new_provider() {
    run("reinit", &[]);
}

#[cfg(feature = "otlp-logs")]
#[test]
fn otlp_logs_layer_exports_filtered_events() {
    run("otlp_logs", &[]);
}

#[cfg(feature = "prometheus")]
#[test]
fn binding_histograms_get_seconds_or_explicit_buckets() {
    run("histogram_buckets", &[]);
}

#[cfg(feature = "prometheus")]
#[test]
fn prometheus_reader_renders_text() {
    run("prometheus", &[]);
}

#[cfg(feature = "prometheus")]
#[test]
fn init_with_prometheus_starts_a_pipeline_with_the_pull_reader() {
    run("prometheus_implicit", &[]);
}

#[cfg(feature = "prometheus")]
#[test]
fn init_with_prometheus_joins_a_pipeline_that_already_has_the_reader() {
    run("prometheus_reinit", &[]);
}

#[cfg(feature = "prometheus")]
#[test]
fn init_with_prometheus_builds_a_provider_for_a_pipeline_without_one() {
    run("prometheus_reinit_no_provider", &[]);
}

#[cfg(feature = "prometheus")]
#[test]
fn a_late_pull_reader_keeps_values_recorded_before_it_started() {
    run("prometheus_late_keeps_values", &[]);
}

#[cfg(feature = "prometheus")]
#[test]
fn init_with_prometheus_honours_the_kill_switch() {
    run("prometheus_disabled", &[]);
}
