//! Record-path cost (design §4.1a disabled cost, §4.2 contention), measured
//! with a counting allocator. The pipeline and the `tracing` subscriber are
//! process-global, so each case runs in a fresh copy of this test binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use super::super::{CallInfo, ClientObs, Telemetry};
use crate::rpc::codec::StreamHeader;
use crate::rpc::status::StatusCode;

// ── counting allocator ──────────────────────────────────────────────────────

/// Counts allocations made by threads that armed it; everything else passes
/// straight through to the system allocator.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn count() {
    // `try_with`: the slot may be gone while a thread exits.
    if ARMED.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations `f` made on this thread.
fn allocations_in(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

// ── child-process harness ───────────────────────────────────────────────────

const CASE: &str = "ASTER_OBSERVE_TEST_CASE";

fn run(case: &str) {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "rpc::observe::otel::tests::child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CASE, case);
    for (k, _) in std::env::vars() {
        if k.starts_with("OTEL_") || k.starts_with("ASTER_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("case-ok"),
        "case {case} failed: {:?}\n--- stdout\n{stdout}\n--- stderr\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn child() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    match case.as_str() {
        "disabled_cost" => case_disabled_cost(),
        "record_contention" => case_record_contention(),
        "admission_and_upcalls" => case_admission_and_upcalls(),
        "pass_none" => case_pass_through(Mode::NoPipeline),
        "pass_traces_off" => case_pass_through(Mode::TracesOff),
        "pass_span_filtered" => case_pass_through(Mode::SpanFiltered),
        "pass_client_span_only" => case_pass_through(Mode::ClientSpanOnly),
        other => panic!("unknown case {other}"),
    }
    println!("case-ok");
}

// ── the cases ───────────────────────────────────────────────────────────────

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: "test.Svc".into(),
        method: method.into(),
        metadata_keys: vec!["x-other".into()],
        metadata_values: vec!["v".into()],
        ..Default::default()
    }
}

/// One server call, one client call, one connection and one admission
/// decision through every record entry point.
fn cycle(t: &Telemetry, h: &mut StreamHeader, attrs: &HashMap<String, String>) {
    let mut obs = t.start_call();
    obs.open(h, "peer", Some("unary"));
    obs.dispatching(h, "peer", attrs);
    obs.finish(StatusCode::Ok, CallInfo::default());
    let mut client = ClientObs::start(h);
    client.finish(StatusCode::Ok);
    let conn = t.connection_opened();
    t.connection_closed(conn);
    super::admission("consumer", "admit", Duration::ZERO);
}

/// `otel` compiled in, no pipeline: no allocation anywhere on the record
/// path, and trace metadata passes through untouched (§5.1).
fn case_disabled_cost() {
    // The counter itself works.
    assert_eq!(
        allocations_in(|| drop(std::hint::black_box(Box::new(1u64)))),
        1
    );
    let t = Telemetry::default();
    let attrs = HashMap::new();
    let mut h = header("unary");
    // Warm-up: the (inactive) generation cache and `tracing` callsite
    // registration allocate once.
    cycle(&t, &mut h, &attrs);
    let n = allocations_in(|| {
        for _ in 0..1000 {
            cycle(&t, &mut h, &attrs);
        }
    });
    assert_eq!(n, 0, "the disabled record path allocated {n} times");
    assert_eq!(h.metadata_keys, ["x-other"]);
    assert_eq!(h.metadata_values, ["v"]);
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// No pipeline at all.
    NoPipeline,
    /// A metrics-only pipeline: traces off.
    TracesOff,
    /// Traces on, the `rpc` and client spans filtered out by level.
    SpanFiltered,
    /// Traces on, only the client span enabled.
    ClientSpanOnly,
}

fn tc(s: &str) -> aster_otel::TraceContext {
    aster_otel::TraceContext::parse_traceparent(s.as_bytes()).unwrap()
}

fn meta<'a>(h: &'a StreamHeader, key: &str) -> Option<&'a str> {
    h.metadata_keys
        .iter()
        .position(|k| k == key)
        .map(|i| h.metadata_values[i].as_str())
}

/// W3C trace-context passes through whatever the pipeline: the call's trace
/// context is the inbound one, the reserved keys are hidden, and a Rust-native
/// handler's outbound call carries the context on.
fn case_pass_through(mode: Mode) {
    let t = aster_otel::testing::TestExporters::new();
    let _h = match mode {
        Mode::NoPipeline => None,
        Mode::TracesOff => Some(
            aster_otel::init(aster_otel::ObservabilityConfig {
                from_env: false,
                traces_exporter: Some(vec![]),
                metrics_exporter: Some(vec![aster_otel::ExporterKind::Otlp]),
                otlp_metrics: aster_otel::OtlpSettings {
                    endpoint: Some("http://127.0.0.1:9/v1/metrics".into()),
                    ..Default::default()
                },
                metric_export_interval: Some(Duration::from_secs(3600)),
                ..Default::default()
            })
            .unwrap(),
        ),
        Mode::SpanFiltered | Mode::ClientSpanOnly => {
            let mut cfg = aster_otel::ObservabilityConfig {
                from_env: false,
                test_exporters: Some(t.clone()),
                span_filter: Some(
                    if mode == Mode::SpanFiltered {
                        "warn"
                    } else {
                        "warn,aster::rpc::client=info"
                    }
                    .into(),
                ),
                metric_export_interval: Some(Duration::from_secs(3600)),
                ..Default::default()
            };
            cfg.span_batch.scheduled_delay = Some(Duration::from_secs(3600));
            Some(aster_otel::init(cfg).unwrap())
        }
    };
    let b = super::builtins();
    match mode {
        Mode::NoPipeline => assert!(b.is_none()),
        Mode::TracesOff => assert!(b.is_some_and(|b| !b.traces)),
        Mode::SpanFiltered | Mode::ClientSpanOnly => assert!(b.is_some_and(|b| b.traces)),
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let inbound = tc(TRACEPARENT);

    let mut h = header("unary");
    for (k, v) in [
        ("traceparent", TRACEPARENT),
        ("tracestate", "vendor=a"),
        ("baggage", "k=v"),
    ] {
        h.metadata_keys.push(k.into());
        h.metadata_values.push(v.into());
    }
    let mut obs = Telemetry::default().start_call();
    obs.open(&mut h, "peer", Some("unary"));
    assert!(obs.span().is_disabled(), "the rpc span must not be enabled");
    let got = obs.trace_context().expect("trace context passes through");
    assert_eq!(got.trace_id, inbound.trace_id);
    assert_eq!(got.span_id, inbound.span_id);
    assert_eq!(got.trace_flags, inbound.trace_flags);
    assert_eq!(h.metadata_keys, ["x-other"], "reserved keys hidden (Q7)");
    let it = obs.inbound_trace().unwrap();
    assert_eq!(it.tracestate.as_deref(), Some("vendor=a"));
    assert_eq!(it.baggage.as_deref(), Some("k=v"));

    // A Rust-native handler's outbound call.
    let out = |preset: Option<&str>| {
        let mut o = header("out");
        if let Some(p) = preset {
            o.metadata_keys.insert(0, "traceparent".into());
            o.metadata_values.insert(0, p.into());
        }
        rt.block_on(super::super::scoped(obs.pass(), async {
            let mut c = ClientObs::start(&mut o);
            c.finish(StatusCode::Ok);
        }));
        o
    };
    let o = out(None);
    let sent = tc(meta(&o, "traceparent").expect("outbound carries the context"));
    assert_eq!(sent.trace_id, inbound.trace_id);
    if mode == Mode::ClientSpanOnly {
        // The client span is exported as a child of the inbound span.
        aster_otel::flush(Duration::from_secs(10)).unwrap();
        let spans = t.spans.finished_spans();
        let client = spans
            .iter()
            .find(|s| s.span_kind == aster_otel::opentelemetry::trace::SpanKind::Client)
            .expect("client span exported");
        assert_eq!(client.parent_span_id.to_bytes(), inbound.span_id);
        assert_eq!(client.span_context.span_id().to_bytes(), sent.span_id);
        assert_eq!(meta(&o, "x-other"), Some("v"));
    } else {
        assert_eq!(sent.span_id, inbound.span_id);
        assert_eq!(sent.trace_flags, inbound.trace_flags);
        assert_eq!(meta(&o, "tracestate"), Some("vendor=a"));
        assert_eq!(meta(&o, "baggage"), Some("k=v"));
        // A binding-written traceparent is left intact.
        let own = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        assert_eq!(meta(&out(Some(own)), "traceparent"), Some(own));
    }

    // Outside a handler, nothing is added.
    let mut o = header("out");
    ClientObs::start(&mut o).finish(StatusCode::Ok);
    if mode != Mode::ClientSpanOnly {
        assert!(meta(&o, "traceparent").is_none());
    }

    // A malformed traceparent is no context: a new trace.
    let mut h = header("unary");
    h.metadata_keys.push("traceparent".into());
    h.metadata_values.push("00-zz".into());
    let mut obs = Telemetry::default().start_call();
    obs.open(&mut h, "peer", Some("unary"));
    assert!(obs.trace_context().is_none());
    assert_eq!(h.metadata_keys, ["x-other"]);
    let mut o = header("out");
    rt.block_on(super::super::scoped(obs.pass(), async {
        ClientObs::start(&mut o).finish(StatusCode::Ok);
    }));
    if mode != Mode::ClientSpanOnly {
        assert!(meta(&o, "traceparent").is_none());
    }
    if matches!(mode, Mode::SpanFiltered | Mode::ClientSpanOnly) {
        aster_otel::shutdown().unwrap();
    } else {
        // Exiting skips the final export (nothing listens on the endpoint).
        std::mem::forget(_h);
    }
}

/// Metrics on (traces off, so this is the instrument record path alone):
/// eight threads record on a hot series while four others keep creating new
/// series. The hot threads never allocate after warm-up, and their p99 stays
/// bounded although the SDK takes its write lock for every new series.
fn case_record_contention() {
    let cfg = aster_otel::ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![aster_otel::ExporterKind::Otlp]),
        // Never reached: nothing is exported before the process exits.
        otlp_metrics: aster_otel::OtlpSettings {
            endpoint: Some("http://127.0.0.1:9/v1/metrics".into()),
            ..Default::default()
        },
        metric_export_interval: Some(Duration::from_secs(3600)),
        ..Default::default()
    };
    let handle = aster_otel::init(cfg).unwrap();
    assert!(
        super::builtins().is_some_and(|b| b.metrics.is_some() && !b.traces),
        "metrics-only pipeline expected"
    );

    const HOT: usize = 8;
    const CHURN: usize = 4;
    const ITER: usize = 5000;
    let t = Telemetry::default();
    let start = Arc::new(Barrier::new(HOT + CHURN));
    let hot_done = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let mut hot = Vec::new();
    for _ in 0..HOT {
        let (t, start, hot_done) = (t.clone(), start.clone(), hot_done.clone());
        hot.push(std::thread::spawn(move || {
            let attrs = HashMap::new();
            let mut h = header("hot");
            for _ in 0..100 {
                cycle(&t, &mut h, &attrs);
            }
            start.wait();
            let mut lat = Vec::with_capacity(ITER);
            let n = allocations_in(|| {
                for _ in 0..ITER {
                    let at = Instant::now();
                    cycle(&t, &mut h, &attrs);
                    lat.push(at.elapsed());
                }
            });
            hot_done.fetch_add(1, Ordering::Relaxed);
            (n, lat)
        }));
    }
    let mut churn = Vec::new();
    for c in 0..CHURN {
        let (t, start, stop) = (t.clone(), start.clone(), stop.clone());
        churn.push(std::thread::spawn(move || {
            let attrs = HashMap::new();
            start.wait();
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let mut h = header(&format!("new-{c}-{i}"));
                cycle(&t, &mut h, &attrs);
                i += 1;
            }
            i
        }));
    }

    let mut lat = Vec::new();
    for h in hot {
        let (n, l) = h.join().unwrap();
        assert_eq!(
            n, 0,
            "a hot thread allocated {n} times recording a known series"
        );
        lat.extend(l);
    }
    stop.store(true, Ordering::Relaxed);
    let created: u64 = churn.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(created > 0, "the churn threads created no series");
    assert_eq!(hot_done.load(Ordering::Relaxed), HOT as u64);

    lat.sort();
    let p99 = lat[lat.len() * 99 / 100];
    // Generous for an unoptimised build on a loaded CI runner; what matters
    // is that a writer creating series cannot stall the hot path for long.
    assert!(
        p99 < Duration::from_millis(10),
        "p99 {p99:?} recording a hot series under series churn"
    );
    eprintln!("record contention: p99 {p99:?}, {created} new series created meanwhile");
    // Exiting skips the final export (nothing listens on the endpoint).
    std::mem::forget(handle);
}

/// The Gate 0 and upcall instruments (§4.1) record under their names.
fn case_admission_and_upcalls() {
    let t = aster_otel::testing::TestExporters::new();
    let cfg = aster_otel::ObservabilityConfig {
        from_env: false,
        test_exporters: Some(t.clone()),
        metric_export_interval: Some(Duration::from_secs(3600)),
        ..Default::default()
    };
    let _h = aster_otel::init(cfg).unwrap();
    super::admission("consumer", "admit", Duration::from_millis(3));
    super::admission("consumer", "deny", Duration::from_millis(1));
    #[cfg(feature = "binding-support")]
    {
        use crate::binding_support::upcall::{observe, Observed, UpcallKind};
        observe(Observed::Issued, UpcallKind::LeaseAuthorize);
        observe(Observed::Issued, UpcallKind::LeaseAuthorize);
        observe(Observed::Timeout, UpcallKind::LeaseAuthorize);
        observe(Observed::Fallback, UpcallKind::LeaseAuthorize);
    }
    aster_otel::flush(Duration::from_secs(10)).unwrap();
    let decisions = t.metric_points("aster.admission.decisions");
    for result in ["admit", "deny"] {
        let p = decisions
            .iter()
            .find(|p| p.attr("aster.result") == Some(result))
            .unwrap_or_else(|| panic!("no {result} point in {decisions:?}"));
        assert_eq!(p.attr("aster.role"), Some("consumer"));
        assert_eq!(p.value, 1.0);
    }
    let d = &t.metric_points("aster.admission.duration")[0];
    assert_eq!(d.count, 2);
    assert!((d.value - 4.0).abs() < 0.5, "{d:?}");
    #[cfg(feature = "binding-support")]
    for (name, n) in [
        ("aster.upcall.issued", 2.0),
        ("aster.upcall.timeouts", 1.0),
        ("aster.upcall.fallbacks", 1.0),
    ] {
        let p = &t.metric_points(name)[0];
        assert_eq!(p.attr("aster.upcall.kind"), Some("lease_authorize"));
        assert_eq!(p.value, n, "{name}");
    }
    aster_otel::shutdown().unwrap();
}

#[test]
fn admission_and_upcall_instruments() {
    run("admission_and_upcalls");
}

#[test]
fn disabled_record_path_does_not_allocate() {
    run("disabled_cost");
}

#[test]
fn hot_series_record_without_allocating_under_series_churn() {
    run("record_contention");
}

#[test]
fn trace_context_passes_through_with_no_pipeline() {
    run("pass_none");
}

#[test]
fn trace_context_passes_through_with_traces_off() {
    run("pass_traces_off");
}

#[test]
fn trace_context_passes_through_with_the_rpc_span_filtered_out() {
    run("pass_span_filtered");
}

#[test]
fn client_span_under_an_unexported_rpc_span_is_a_child_of_the_inbound_span() {
    run("pass_client_span_only");
}
