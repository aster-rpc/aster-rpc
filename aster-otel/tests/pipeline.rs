//! O1 pipeline, end to end, against a real OTLP/HTTP receiver on 127.0.0.1.
//! Every case runs in a child process (see `common`), because the pipeline,
//! the OTel globals and the `tracing` subscriber are once per process.

#![cfg(feature = "otlp")]

mod common;

use std::time::Duration;

use aster_otel::{
    ExporterKind, ObservabilityConfig, OtlpHeaders, SamplerConfig, StderrInit, SubscriberStatus,
};
use common::{child_case, run, Receiver};
use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};

const FLUSH: Duration = Duration::from_secs(10);

/// A config that exports traces and metrics to `rx` and nothing from env.
fn to_receiver(rx: &Receiver) -> ObservabilityConfig {
    let mut cfg = ObservabilityConfig {
        from_env: false,
        ..Default::default()
    };
    cfg.otlp.endpoint = Some(rx.url());
    cfg.service_name = Some("aster-otel-test".into());
    cfg
}

/// A config that exports nothing (no network), for lifecycle cases.
fn quiet() -> ObservabilityConfig {
    ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![]),
        ..Default::default()
    }
}

fn probe_span(name: &'static str) {
    match name {
        "probe-a" => tracing::info_span!(target: "aster::probe", "probe-a").in_scope(|| {}),
        _ => tracing::info_span!(target: "aster::probe", "probe-b").in_scope(|| {}),
    }
}

#[test]
fn child() {
    let Some(case) = child_case() else { return };
    match case.as_str() {
        "export" => case_export(),
        "precedence" => case_precedence(),
        "refcount" => case_refcount(),
        "generation" => case_generation(),
        "disabled" => case_disabled(),
        "stderr_then_init" => case_stderr_then_init(),
        "init_then_stderr" => case_init_then_stderr(),
        "foreign" => case_foreign(),
        "in_runtime" => case_in_runtime(),
        #[cfg(unix)]
        "fork" => case_fork(),
        other => panic!("unknown case {other}"),
    }
    println!("case-ok");
}

// ── cases (child side) ──────────────────────────────────────────────────────

fn case_export() {
    let mut rx = Receiver::bind();
    rx.start();
    let h = aster_otel::init(to_receiver(&rx)).unwrap();
    assert_eq!(h.subscriber(), SubscriberStatus::Installed);
    probe_span("probe-a");
    let meter = opentelemetry::global::meter_provider().meter("aster-test");
    let counter = meter.u64_counter("probe.counter").build();
    counter.add(3, &[]);
    aster_otel::flush(FLUSH).unwrap();
    let traces = rx.posts_to("/v1/traces");
    assert!(!traces.is_empty(), "no trace export: {:?}", rx.requests());
    assert!(traces.iter().any(|r| r.body_contains("probe-a")));
    assert_eq!(
        traces[0].headers.get("content-type").map(String::as_str),
        Some("application/x-protobuf")
    );
    let metrics = rx.posts_to("/v1/metrics");
    assert!(
        metrics.iter().any(|r| r.body_contains("probe.counter")),
        "{:?}",
        rx.requests()
    );
    aster_otel::shutdown().unwrap();
}

/// Programmatic values beat conflicting `OTEL_*` env, including the variables
/// the SDK and the OTLP exporter read by themselves.
fn case_precedence() {
    let mut rx = Receiver::bind();
    // Set before any thread exists. If env won: spans would go to /env-path,
    // carry x-probe=from-env and x-env-only, name the service from-env, or be
    // dropped by the always_off sampler.
    std::env::set_var(
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        format!("{}/env-path", rx.url()),
    );
    std::env::set_var(
        "OTEL_EXPORTER_OTLP_HEADERS",
        "x-probe=from-env,x-env-only=1",
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc");
    std::env::set_var("OTEL_SERVICE_NAME", "svc-from-env");
    std::env::set_var("OTEL_TRACES_SAMPLER", "always_off");
    std::env::set_var("OTEL_BSP_MAX_QUEUE_SIZE", "0");
    rx.start();

    let mut cfg = ObservabilityConfig::default(); // from_env: true
    cfg.otlp.endpoint = Some(rx.url());
    cfg.otlp.protocol = Some(aster_otel::OtlpProtocol::HttpProtobuf);
    cfg.otlp.headers = Some(OtlpHeaders::new([("x-probe", "from-code")]));
    cfg.service_name = Some("svc-from-code".into());
    cfg.sampler = Some(SamplerConfig::AlwaysOn);
    cfg.span_batch.max_queue_size = Some(64);
    cfg.metrics_exporter = Some(vec![]);
    let _h = aster_otel::init(cfg).unwrap();
    probe_span("probe-a");
    aster_otel::flush(FLUSH).unwrap();

    assert!(rx.posts_to("/env-path").is_empty(), "env endpoint won");
    let posts = rx.posts_to("/v1/traces");
    assert!(
        !posts.is_empty(),
        "no export (sampler or queue from env?): {:?}",
        rx.requests()
    );
    let r = &posts[0];
    assert_eq!(
        r.headers.get("x-probe").map(String::as_str),
        Some("from-code")
    );
    assert!(
        !r.headers.contains_key("x-env-only"),
        "env headers merged: {:?}",
        r.headers
    );
    assert!(r.body_contains("svc-from-code") && !r.body_contains("svc-from-env"));
    assert!(r.body_contains("probe-a"));
}

fn case_refcount() {
    let g0 = aster_otel::generation();
    let a = aster_otel::init(quiet()).unwrap();
    let b = aster_otel::init(quiet()).unwrap();
    assert_eq!(a.generation(), g0 + 1);
    assert_eq!(
        b.generation(),
        a.generation(),
        "same config joins the pipeline"
    );

    let mut other = quiet();
    other.service_name = Some("someone-else".into());
    let err = aster_otel::init(other).unwrap_err();
    assert!(
        matches!(err, aster_otel::Error::AlreadyInitialized(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("service_name"), "{err}");

    // A secret difference is named, not shown.
    let mut secret = quiet();
    secret.otlp.headers = Some(OtlpHeaders::new([("authorization", "Bearer s3cr3t")]));
    let err = aster_otel::init(secret).unwrap_err().to_string();
    assert!(
        err.contains("otlp_traces.headers") && !err.contains("s3cr3t"),
        "{err}"
    );

    let c = b.clone();
    drop(a);
    drop(b);
    assert_eq!(
        aster_otel::generation(),
        g0 + 1,
        "a reference is still held"
    );
    drop(c);
    assert_eq!(
        aster_otel::generation(),
        g0 + 2,
        "last handle shut the pipeline down"
    );
    // And a fresh init with a different config is now fine.
    let mut other = quiet();
    other.service_name = Some("someone-else".into());
    let d = aster_otel::init(other).unwrap();
    assert_eq!(d.generation(), g0 + 3);
}

fn case_generation() {
    let g0 = aster_otel::generation();
    let h = aster_otel::init(quiet()).unwrap();
    assert_eq!(aster_otel::generation(), g0 + 1);
    aster_otel::shutdown().unwrap();
    assert_eq!(aster_otel::generation(), g0 + 2);
    aster_otel::shutdown().unwrap();
    assert_eq!(aster_otel::generation(), g0 + 2, "no pipeline, no bump");
    let h2 = aster_otel::init(quiet()).unwrap();
    assert_eq!(aster_otel::generation(), g0 + 3);
    drop(h); // stale handle from before the shutdown: inert
    assert_eq!(aster_otel::generation(), g0 + 3);
    drop(h2);
    assert_eq!(aster_otel::generation(), g0 + 4);
}

fn case_disabled() {
    let mut rx = Receiver::bind();
    std::env::set_var("OTEL_SDK_DISABLED", "true");
    rx.start();
    let mut cfg = to_receiver(&rx);
    cfg.from_env = true;
    let h = aster_otel::init(cfg).unwrap();
    assert!(h.is_noop());
    probe_span("probe-a");
    let span = opentelemetry::global::tracer_provider()
        .tracer("x")
        .start("y");
    assert!(
        !span.span_context().is_valid(),
        "global tracer must stay no-op"
    );
    aster_otel::flush(FLUSH).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(rx.requests().is_empty(), "{:?}", rx.requests());
}

fn case_stderr_then_init() {
    assert_eq!(aster_otel::init_stderr_from_env(), StderrInit::Installed);
    tracing::info!(target: "aster::probe", "before-init");
    let mut rx = Receiver::bind();
    rx.start();
    let h = aster_otel::init(to_receiver(&rx)).unwrap();
    assert_eq!(
        h.subscriber(),
        SubscriberStatus::Installed,
        "found O0's subscriber"
    );
    tracing::info!(target: "aster::probe", "after-init");
    probe_span("probe-a");
    aster_otel::flush(FLUSH).unwrap();
    assert!(
        rx.posts_to("/v1/traces")
            .iter()
            .any(|r| r.body_contains("probe-a")),
        "span slot not filled"
    );
}

fn case_init_then_stderr() {
    let mut rx = Receiver::bind();
    rx.start();
    let h = aster_otel::init(to_receiver(&rx)).unwrap();
    assert_eq!(h.subscriber(), SubscriberStatus::Installed);
    tracing::info!(target: "aster::probe", "before-stderr");
    assert_eq!(aster_otel::init_stderr_from_env(), StderrInit::Installed);
    tracing::info!(target: "aster::probe", "after-stderr");
    probe_span("probe-a");
    aster_otel::flush(FLUSH).unwrap();
    assert!(rx
        .posts_to("/v1/traces")
        .iter()
        .any(|r| r.body_contains("probe-a")));
}

/// A Rust app owns the global subscriber: init still installs the providers,
/// reports the subscriber as foreign, and `layers` feeds them.
fn case_foreign() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let mut rx = Receiver::bind();
    rx.start();
    let cfg = to_receiver(&rx);
    tracing_subscriber::registry()
        .with(aster_otel::layers(&cfg).unwrap())
        .init();
    let h = aster_otel::init(cfg).unwrap();
    assert_eq!(h.subscriber(), SubscriberStatus::Foreign);
    std::env::set_var("ASTER_LOG_STDERR", "1");
    assert_eq!(aster_otel::init_stderr_from_env(), StderrInit::Foreign);
    probe_span("probe-b");
    aster_otel::flush(FLUSH).unwrap();
    assert!(
        rx.posts_to("/v1/traces")
            .iter()
            .any(|r| r.body_contains("probe-b")),
        "{:?}",
        rx.requests()
    );
}

/// Bindings call init from runtime threads; the blocking HTTP client must not
/// panic there (design §9.1), and neither may the last handle's shutdown.
fn case_in_runtime() {
    let mut rx = Receiver::bind();
    rx.start();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let cfg = to_receiver(&rx);
    rt.block_on(async move {
        let h = aster_otel::init(cfg).unwrap();
        probe_span("probe-a");
        aster_otel::flush(FLUSH).unwrap();
        drop(h);
    });
    assert!(!rx.posts_to("/v1/traces").is_empty());
}

#[cfg(unix)]
fn case_fork() {
    let mut rx = Receiver::bind();
    rx.start();
    let h = aster_otel::init(to_receiver(&rx)).unwrap();
    // SAFETY: the forked child only makes Aster calls that are atomics plus
    // one eprintln, then `_exit`s without running destructors.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let ok = aster_otel::flush(Duration::from_secs(1)).is_ok()
            && aster_otel::init(to_receiver(&rx)).is_ok_and(|h| {
                let noop = h.is_noop();
                std::mem::forget(h);
                noop
            })
            && aster_otel::shutdown().is_ok();
        probe_span("probe-b");
        unsafe { libc::_exit(if ok { 0 } else { 3 }) };
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child status {status}"
    );
    // The parent is unaffected.
    probe_span("probe-a");
    aster_otel::flush(FLUSH).unwrap();
    let posts = rx.posts_to("/v1/traces");
    assert!(posts.iter().any(|r| r.body_contains("probe-a")));
    assert!(
        !posts.iter().any(|r| r.body_contains("probe-b")),
        "the forked child exported"
    );
    drop(h);
}

// ── tests (parent side) ─────────────────────────────────────────────────────

#[test]
fn otlp_http_exports_traces_and_metrics_to_a_local_receiver() {
    run("export", &[]);
}

#[test]
fn programmatic_config_beats_env_including_sdk_env_reads() {
    run("precedence", &[]);
}

#[test]
fn same_config_refcounts_different_config_is_already_initialized() {
    run("refcount", &[]);
}

#[test]
fn generation_bumps_on_init_and_shutdown() {
    run("generation", &[]);
}

#[test]
fn otel_sdk_disabled_installs_nothing() {
    run("disabled", &[]);
}

#[test]
fn init_after_o0_stderr_fills_the_existing_subscriber() {
    let c = run("stderr_then_init", &[("ASTER_LOG_STDERR", "1")]);
    assert!(
        c.stderr.contains("before-init") && c.stderr.contains("after-init"),
        "{}",
        c.stderr
    );
}

#[test]
fn o0_stderr_after_init_fills_the_existing_subscriber() {
    let c = run("init_then_stderr", &[("ASTER_LOG_STDERR", "1")]);
    assert!(
        !c.stderr.contains("before-stderr"),
        "stderr was on before O0 asked: {}",
        c.stderr
    );
    assert!(
        c.stderr.contains("stderr logging enabled") && c.stderr.contains("after-stderr"),
        "{}",
        c.stderr
    );
}

#[test]
fn foreign_subscriber_still_gets_providers_through_layers() {
    run("foreign", &[]);
}

#[test]
fn init_and_shutdown_inside_a_tokio_runtime() {
    run("in_runtime", &[]);
}

#[cfg(unix)]
#[test]
fn fork_after_init_warns_once_and_is_a_no_op() {
    let c = run("fork", &[]);
    assert_eq!(c.stderr.matches("forked after").count(), 1, "{}", c.stderr);
}

#[test]
fn exporter_kind_is_public() {
    // Keeps the import honest when cases are compiled out.
    assert_ne!(ExporterKind::Otlp, ExporterKind::Prometheus);
}
