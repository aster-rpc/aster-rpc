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
fn prometheus_reader_renders_text() {
    run("prometheus", &[]);
}
