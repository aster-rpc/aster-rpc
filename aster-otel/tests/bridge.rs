//! O4 binding log bridge (design §3): filter cost, FIFO, drop accounting,
//! the audit reservation, trace context inside an `rpc` span, no feedback
//! from the drain thread, OTLP logs beside the bridge. The bridge and the
//! subscriber are process-global, so each case runs in a child process (see
//! `common`).

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aster_otel::bridge::{self, Poll};
use aster_otel::{Error, LogLevel, LogRecord, ObservabilityConfig};
use common::{child_case, run};

// ── a counting allocator (this thread only, while armed) ────────────────────

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}
static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations `f` makes on this thread.
fn allocations(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

// ── plumbing ────────────────────────────────────────────────────────────────

fn bridge_cfg(filter: &str) -> ObservabilityConfig {
    ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        log_bridge: Some(true),
        log_filter: Some(filter.into()),
        ..Default::default()
    }
}

/// Poll until `n` records arrived (or 10 s), leaving out the pipeline's own
/// `aster::otel` notes (init reports, e.g. OTLP logs beside the bridge).
fn collect(d: &mut aster_otel::LogDrain, n: usize) -> Vec<LogRecord> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while out.len() < n && Instant::now() < deadline {
        if d.poll_into(&mut out, 256, Duration::from_millis(100)) == Poll::Closed {
            break;
        }
        out.retain(|r| r.target != "aster::otel");
    }
    out
}

/// `LogDrain::try_drain` without the `aster::otel` notes.
fn take(d: &mut aster_otel::LogDrain, max: usize) -> Vec<LogRecord> {
    let mut v = d.try_drain(max);
    v.retain(|r| r.target != "aster::otel");
    v
}

fn messages(recs: &[LogRecord]) -> Vec<&str> {
    recs.iter().map(|r| r.message.as_str()).collect()
}

#[test]
fn child() {
    let Some(case) = child_case() else { return };
    match case.as_str() {
        "filter_cost" => case_filter_cost(),
        "fifo_and_fields" => case_fifo_and_fields(),
        "drops" => case_drops(),
        "audit_reserved" => case_audit_reserved(),
        "audit_block" => case_audit_block(),
        "no_feedback" => case_no_feedback(),
        "lifecycle" => case_lifecycle(),
        "rpc_span_context" => case_rpc_span_context(),
        #[cfg(feature = "testing")]
        "trace_ids" => case_trace_ids(),
        #[cfg(all(feature = "testing", feature = "otlp-logs"))]
        "otlp_logs_beside" => case_otlp_logs_beside(),
        other => panic!("unknown case {other}"),
    }
    println!("case-ok");
}

// ── cases ───────────────────────────────────────────────────────────────────

/// A callsite the bridge filter disables costs no allocation, even with
/// arguments that would allocate if formatted; an enabled one is delivered.
fn case_filter_cost() {
    let _h = aster_otel::init(bridge_cfg("warn")).unwrap();
    let mut d = bridge::drain().unwrap();
    let payload = String::from("synthetic-payload");
    // Warm up: callsite registration may allocate once.
    for _ in 0..2 {
        tracing::debug!(target: "aster::probe", p = %payload, "below {}", payload);
        tracing::info!(target: "other::probe", p = %payload, "below {}", payload);
        tracing::trace!(target: "aster::probe", "below");
    }
    let n = allocations(|| {
        for i in 0..10_000u32 {
            tracing::debug!(target: "aster::probe", p = %payload, i, "below {}", payload);
            tracing::info!(target: "other::probe", p = %payload, i, "below {}", payload);
            tracing::trace!(target: "aster::probe", i, "below");
        }
    });
    assert_eq!(n, 0, "disabled callsites allocated {n} times");
    assert!(take(&mut d, 16).is_empty());

    tracing::warn!(target: "aster::probe", p = %payload, "enabled {}", 1);
    let got = collect(&mut d, 1);
    assert_eq!(messages(&got), ["enabled 1"]);
    assert_eq!(got[0].field("p"), Some("synthetic-payload"));
}

/// FIFO order across many records, levels, targets, fields and thread names.
fn case_fifo_and_fields() {
    let _h = aster_otel::init(bridge_cfg("warn,aster=debug")).unwrap();
    let mut d = bridge::drain().unwrap();
    std::thread::Builder::new()
        .name("emitter-1".into())
        .spawn(|| {
            for i in 0..1000u32 {
                tracing::info!(target: "aster::probe::fifo", seq = i, ok = true, "rec-{i}");
            }
            tracing::debug!(target: "aster::probe", ratio = 0.5, name = "example", "debug-last");
        })
        .unwrap()
        .join()
        .unwrap();
    let got = collect(&mut d, 1001);
    assert_eq!(got.len(), 1001);
    for (i, r) in got[..1000].iter().enumerate() {
        assert_eq!(r.message, format!("rec-{i}"));
        assert_eq!(r.field("seq"), Some(i.to_string().as_str()));
        assert_eq!(r.field("ok"), Some("true"));
        assert_eq!(r.level, LogLevel::Info);
        assert_eq!(r.target, "aster::probe::fifo");
        assert_eq!(r.logger_name(), "aster.probe.fifo");
        assert_eq!(r.field("thread.name"), Some("emitter-1"));
        assert_eq!(r.flags, 0);
        assert!(r.timestamp_unix_ns > 0);
    }
    let last = &got[1000];
    assert_eq!(last.level, LogLevel::Debug);
    assert_eq!(last.field("ratio"), Some("0.5"));
    assert_eq!(last.field("name"), Some("example"));
}

/// A full queue drops the newest records, counts them, and the drain reports
/// them once as a synthetic WARN; the `aster.log.records_dropped` counter
/// follows (with an in-memory meter).
fn case_drops() {
    let mut cfg = bridge_cfg("warn,aster=info");
    cfg.log_queue = Some(16);
    #[cfg(feature = "testing")]
    let t = aster_otel::testing::TestExporters::new();
    #[cfg(feature = "testing")]
    {
        cfg.test_exporters = Some(t.clone());
        cfg.metric_export_interval = Some(Duration::from_secs(3600));
    }
    let _h = aster_otel::init(cfg).unwrap();
    let mut d = bridge::drain().unwrap();
    // Init's own notes, so the queue starts empty.
    let _ = d.try_drain(16);
    for i in 0..100u32 {
        tracing::info!(target: "aster::probe", "rec-{i}");
    }
    assert_eq!(bridge::dropped_total(), 84);
    let got = take(&mut d, 1000);
    assert_eq!(got.len(), 17);
    assert!(got[0].is_synthetic());
    assert_eq!(got[0].level, LogLevel::Warn);
    assert_eq!(got[0].target, bridge::DROP_REPORT_TARGET);
    assert_eq!(got[0].field("dropped"), Some("84"));
    let expect: Vec<String> = (0..16).map(|i| format!("rec-{i}")).collect();
    assert_eq!(messages(&got[1..]), expect);
    assert_eq!(bridge::audit_dropped_total(), 0);
    assert!(!bridge::audit_degraded());

    #[cfg(feature = "testing")]
    {
        aster_otel::flush(Duration::from_secs(10)).unwrap();
        let p = t.metric_points("aster.log.records_dropped");
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].value, 84.0);
        let p = t.metric_points("aster.log.audit_dropped");
        assert_eq!(p[0].value, 0.0);
    }
}

/// Audit records have their own channel: a flood of general records that
/// overflows the general queue does not displace them, and the drain hands
/// them out first. Overflowing the audit channel itself is counted and
/// degrades the audit signal.
fn case_audit_reserved() {
    let mut cfg = bridge_cfg("warn,aster=info");
    cfg.log_queue = Some(8);
    cfg.log_audit_queue = Some(4);
    let _h = aster_otel::init(cfg).unwrap();
    let mut d = bridge::drain().unwrap();
    let flood: Vec<_> = (0..4)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..2000u32 {
                    tracing::warn!(target: "aster::probe", t, i, "flood");
                }
            })
        })
        .collect();
    for i in 0..4u32 {
        // Below the general filter's level for `aster`, admitted for audit.
        tracing::info!(target: "aster::audit", decision = "admit", seq = i, "audit-{i}");
        std::thread::yield_now();
    }
    for f in flood {
        f.join().unwrap();
    }
    assert!(bridge::dropped_total() > 0, "the flood should overflow");
    assert_eq!(bridge::audit_dropped_total(), 0);
    let got = take(&mut d, 10_000);
    let audits: Vec<&LogRecord> = got.iter().filter(|r| r.is_audit()).collect();
    assert_eq!(
        messages(&audits.iter().map(|r| (*r).clone()).collect::<Vec<_>>()),
        ["audit-0", "audit-1", "audit-2", "audit-3"]
    );
    // Preferred: the audit records lead (after the drop report).
    let first_general = got
        .iter()
        .position(|r| !r.is_audit() && !r.is_synthetic())
        .unwrap();
    assert!(got[..first_general].iter().filter(|r| r.is_audit()).count() == 4);
    assert!(!bridge::audit_degraded());

    for i in 0..10u32 {
        tracing::info!(target: "aster::audit::gate0", seq = i, "over-{i}");
    }
    assert_eq!(bridge::audit_dropped_total(), 6);
    assert!(bridge::audit_degraded());
    let got = take(&mut d, 100);
    assert_eq!(
        messages(&got.into_iter().filter(|r| r.is_audit()).collect::<Vec<_>>()),
        ["over-0", "over-1", "over-2", "over-3"]
    );
}

/// The Q8 opt-in: with `log_audit_block`, an audit emitter waits for room
/// instead of dropping.
fn case_audit_block() {
    let mut cfg = bridge_cfg("warn,aster=info");
    cfg.log_audit_queue = Some(1);
    cfg.log_audit_block = Some(Duration::from_millis(1000));
    let _h = aster_otel::init(cfg).unwrap();
    let mut d = bridge::drain().unwrap();
    tracing::info!(target: "aster::audit", "first");
    let consumer = std::thread::spawn(move || {
        bridge::mark_drain_thread();
        std::thread::sleep(Duration::from_millis(100));
        let got = collect(&mut d, 2);
        messages(&got)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    let t0 = Instant::now();
    tracing::info!(target: "aster::audit", "second");
    assert!(
        t0.elapsed() >= Duration::from_millis(50),
        "should have waited"
    );
    assert_eq!(consumer.join().unwrap(), ["first", "second"]);
    assert_eq!(bridge::audit_dropped_total(), 0);
}

/// Nothing a drain thread emits reaches the queue, nor do exporter-internal
/// targets or events under an OTel telemetry-suppressed context.
fn case_no_feedback() {
    let _h = aster_otel::init(bridge_cfg("warn,aster=info")).unwrap();
    let mut d = bridge::drain().unwrap();
    let consumer = std::thread::spawn(move || {
        bridge::mark_drain_thread();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut batch = Vec::new();
            if d.poll_into(&mut batch, 64, Duration::from_millis(50)) == Poll::Closed {
                break;
            }
            for r in batch {
                // A native log handler that calls back into Rust.
                tracing::warn!(target: "aster::probe", "echo of {}", r.message);
                seen.push(r.message);
            }
            if seen.len() >= 50 {
                // Give a feedback loop time to show itself.
                let mut more = Vec::new();
                let _ = d.poll_into(&mut more, 64, Duration::from_millis(200));
                seen.extend(more.into_iter().map(|r| r.message));
                break;
            }
        }
        seen
    });
    for i in 0..50u32 {
        tracing::warn!(target: "aster::probe", "src-{i}");
        tracing::warn!(target: "reqwest::connect", "exporter-internal");
        tracing::warn!(target: "hyper_util::client", "exporter-internal");
        tracing::warn!(target: "opentelemetry_sdk::export", "exporter-internal");
        let _quiet = aster_otel::opentelemetry::Context::enter_telemetry_suppressed_scope();
        tracing::warn!(target: "aster::probe", "otel-suppressed");
    }
    {
        let _quiet = bridge::suppress();
        tracing::warn!(target: "aster::probe", "scoped-suppressed");
    }
    let seen = consumer.join().unwrap();
    let expect: Vec<String> = (0..50).map(|i| format!("src-{i}")).collect();
    assert_eq!(seen, expect);
}

/// Filter reload, one drain at a time, close on shutdown, a new queue after
/// re-init, joining a pipeline that has no bridge.
fn case_lifecycle() {
    assert!(matches!(bridge::drain(), Err(Error::LogBridgeNotOpen)));
    // A pipeline without the bridge (an ops listener's, say)...
    let plain = aster_otel::init(ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        log_filter: Some("warn".into()),
        ..Default::default()
    })
    .unwrap();
    assert!(!bridge::is_open());
    // ...is joined, not refused, by an init that asks for it.
    let h = aster_otel::init(bridge_cfg("warn")).unwrap();
    assert!(bridge::is_open());
    let mut d = bridge::drain().unwrap();
    assert!(matches!(bridge::drain(), Err(Error::LogDrainBusy)));

    tracing::info!(target: "aster::probe", "hidden");
    bridge::set_filter("warn,aster::probe=debug").unwrap();
    assert_eq!(bridge::filter().as_deref(), Some("warn,aster::probe=debug"));
    tracing::debug!(target: "aster::probe", "shown");
    assert!(matches!(
        bridge::set_filter("aster=notalevel"),
        Err(Error::Config(_))
    ));
    assert_eq!(messages(&take(&mut d, 16)), ["shown"]);

    drop(d);
    let mut d = bridge::drain().unwrap();
    tracing::warn!(target: "aster::probe", "queued-at-shutdown");
    drop((plain, h));
    assert!(!bridge::is_open());
    let mut out = Vec::new();
    assert_eq!(
        d.poll_into(&mut out, 16, Duration::from_secs(5)),
        Poll::Records(1)
    );
    assert_eq!(
        d.poll_into(&mut out, 16, Duration::from_secs(5)),
        Poll::Closed
    );
    tracing::warn!(target: "aster::probe", "after-shutdown");
    assert!(d.poll(16, Duration::ZERO).is_none());

    let _h = aster_otel::init(bridge_cfg("warn")).unwrap();
    let mut d2 = bridge::drain().unwrap();
    tracing::warn!(target: "aster::probe", "second-pipeline");
    assert_eq!(messages(&collect(&mut d2, 1)), ["second-pipeline"]);
}

/// Inside an `rpc` span (both of O2's shapes) a record carries the call id and
/// the call's service, method and peer; without a tracer there is no trace id.
fn case_rpc_span_context() {
    let _h = aster_otel::init(bridge_cfg("warn,aster=debug")).unwrap();
    let mut d = bridge::drain().unwrap();
    tracing::info_span!(
        target: "aster::rpc::dispatch",
        "rpc",
        rpc.system = "aster",
        rpc.service = "svc.Example",
        rpc.method = "Get",
        aster.call_id = 42u64,
        aster.peer = "peer-a",
    )
    .in_scope(|| {
        tracing::info_span!(target: "aster::probe", "inner").in_scope(|| {
            tracing::info!(target: "aster::probe", "in-call");
        });
    });
    tracing::debug_span!(
        target: "aster::rpc::dispatch",
        "rpc",
        service = "svc.Other",
        method = "Put",
        call_id = 7u64,
        peer = "peer-b",
    )
    .in_scope(|| tracing::info!(target: "aster::probe", "in-debug-call"));
    tracing::info!(target: "aster::probe", "outside");
    let got = collect(&mut d, 3);
    assert_eq!(messages(&got), ["in-call", "in-debug-call", "outside"]);
    let r = &got[0];
    assert!(r.in_call() && !r.has_trace());
    assert_eq!(r.call_id, 42);
    assert_eq!(r.field("rpc.service"), Some("svc.Example"));
    assert_eq!(r.field("rpc.method"), Some("Get"));
    assert_eq!(r.field("aster.peer"), Some("peer-a"));
    let r = &got[1];
    assert_eq!(r.call_id, 7);
    assert_eq!(r.field("rpc.service"), Some("svc.Other"));
    assert_eq!(r.field("aster.peer"), Some("peer-b"));
    assert!(!got[2].in_call() && got[2].call_id == 0);
}

/// With a tracer provider, a record inside an exported span carries that
/// span's trace id and span id.
#[cfg(feature = "testing")]
fn case_trace_ids() {
    let t = aster_otel::testing::TestExporters::new();
    let mut cfg = bridge_cfg("warn,aster=info");
    cfg.traces_exporter = None;
    cfg.test_exporters = Some(t.clone());
    let _h = aster_otel::init(cfg).unwrap();
    let mut d = bridge::drain().unwrap();
    tracing::info_span!(
        target: "aster::rpc::dispatch",
        "rpc",
        otel.name = "svc.Example/Get",
        aster.call_id = 9u64,
    )
    .in_scope(|| tracing::info!(target: "aster::probe", "traced"));
    tracing::info!(target: "aster::probe", "untraced");
    let got = collect(&mut d, 2);
    aster_otel::flush(Duration::from_secs(10)).unwrap();
    let span = t
        .spans
        .finished_spans()
        .into_iter()
        .find(|s| s.name == "svc.Example/Get")
        .expect("exported rpc span");
    let r = &got[0];
    assert!(r.has_trace() && r.in_call());
    assert_eq!(r.trace_id, span.span_context.trace_id().to_bytes());
    assert_eq!(r.span_id, span.span_context.span_id().to_bytes());
    assert!(!got[1].has_trace());
    assert_eq!(got[1].trace_id, [0; 16]);
}

/// Path A beside the bridge (§3.4): the OTLP logs layer exports the same Rust
/// event with the span's trace context, and an application record handed
/// over by a binding (path B) is exported with the context it carries.
#[cfg(all(feature = "testing", feature = "otlp-logs"))]
fn case_otlp_logs_beside() {
    let t = aster_otel::testing::TestExporters::new();
    let mut cfg = bridge_cfg("warn,aster=info");
    cfg.traces_exporter = None;
    cfg.test_exporters = Some(t.clone());
    let _h = aster_otel::init(cfg).unwrap();
    let mut d = bridge::drain().unwrap();
    tracing::info_span!(target: "aster::rpc::dispatch", "rpc", otel.name = "svc.Example/Get")
        .in_scope(|| tracing::info!(target: "aster::probe", "both-paths"));
    let rec = collect(&mut d, 1).remove(0);
    assert!(rec.has_trace());
    bridge::emit_app_log(&bridge::AppLogRecord {
        level: Some(LogLevel::Warn),
        logger: "app.orders",
        message: "app-record",
        fields: &[("order.id", "example-1")],
        context: Some(([7; 16], [3; 8], 1)),
        ..Default::default()
    })
    .unwrap();
    aster_otel::flush(Duration::from_secs(10)).unwrap();
    let logs = t.logs.get_emitted_logs().unwrap();
    let a = logs
        .iter()
        .find(|l| format!("{:?}", l.record.body()).contains("both-paths"))
        .expect("path A record");
    let tc = a.record.trace_context().expect("trace context");
    assert_eq!(tc.trace_id.to_bytes(), rec.trace_id);
    assert_eq!(tc.span_id.to_bytes(), rec.span_id);
    let b = logs
        .iter()
        .find(|l| format!("{:?}", l.record.body()).contains("app-record"))
        .expect("path B record");
    let tc = b.record.trace_context().expect("trace context");
    assert_eq!(tc.trace_id.to_bytes(), [7; 16]);
    assert_eq!(
        b.record.target().map(|t| t.to_string()).as_deref(),
        Some("app.orders")
    );
    // The bridge never sees path B.
    assert!(take(&mut d, 16).is_empty());
}

// ── the tests ───────────────────────────────────────────────────────────────

#[test]
fn disabled_callsite_costs_no_allocation() {
    run("filter_cost", &[]);
}

#[test]
fn records_arrive_in_order_with_fields() {
    run("fifo_and_fields", &[]);
}

#[test]
fn full_queue_drops_and_counts() {
    run("drops", &[]);
}

#[test]
fn audit_reservation_holds_under_pressure() {
    run("audit_reserved", &[]);
}

#[test]
fn audit_block_opt_in_waits_for_room() {
    run("audit_block", &[]);
}

#[test]
fn no_feedback_from_the_drain_thread() {
    run("no_feedback", &[]);
}

#[test]
fn filter_reload_single_drain_close_and_reinit() {
    run("lifecycle", &[]);
}

#[test]
fn rpc_span_fields_on_records() {
    run("rpc_span_context", &[]);
}

#[cfg(feature = "testing")]
#[test]
fn trace_ids_inside_an_exported_span() {
    run("trace_ids", &[]);
}

#[cfg(all(feature = "testing", feature = "otlp-logs"))]
#[test]
fn otlp_logs_beside_the_bridge() {
    run("otlp_logs_beside", &[]);
}
