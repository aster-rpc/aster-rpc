//! ABI 4.6 observability through the C functions (design
//! `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §7.1-§7.4):
//! init / shutdown / re-init refcounting, the log poll (order, drops,
//! refusals, close), binding metrics, binding spans and recorded spans with
//! the `traceparent` codec, client metrics and server trace context on the
//! dispatch path, and NULL / unknown-handle safety.
//!
//! The pipeline is process-global, so the cases take one lock and each ends
//! with no pipeline running.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster::rpc::{DynamicService, MethodDesc, MethodPattern, TypeRef};
use aster_otel::testing::TestExporters;
use aster_otel::ObservabilityConfig;
use aster_transport_ffi::call::aster_call_buffer_release;
use aster_transport_ffi::node::*;
use aster_transport_ffi::observability::*;
use aster_transport_ffi::rpc::*;
use aster_transport_ffi::server::*;
use aster_transport_ffi::upcall::*;
use aster_transport_ffi::*;

mod common;

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const NOT_FOUND: i32 = aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
const EXISTS: i32 = aster_status_t::ASTER_STATUS_ALREADY_EXISTS as i32;
const CLOSED: i32 = aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
const RPC_ALPN: &[u8] = b"aster/1";
const REC: u32 = std::mem::size_of::<aster_log_record_t>() as u32;

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn bytes(b: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: if b.is_empty() {
            ptr::null()
        } else {
            b.as_ptr()
        },
        len: b.len(),
    }
}

fn kv(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = (pairs.len() as u32).to_le_bytes().to_vec();
    for (k, v) in pairs {
        out.extend((k.len() as u32).to_le_bytes());
        out.extend(k.as_bytes());
        out.extend((v.len() as u32).to_le_bytes());
        out.extend(v.as_bytes());
    }
    out
}

fn decode_kv(b: &[u8]) -> Vec<(String, String)> {
    if b.is_empty() {
        return Vec::new();
    }
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    let mut o = 4;
    let mut out = Vec::new();
    for _ in 0..u32_at(0) {
        let kl = u32_at(o);
        let k = String::from_utf8(b[o + 4..o + 4 + kl].to_vec()).unwrap();
        o += 4 + kl;
        let vl = u32_at(o);
        let v = String::from_utf8(b[o + 4..o + 4 + vl].to_vec()).unwrap();
        o += 4 + vl;
        out.push((k, v));
    }
    out
}

fn last_error() -> String {
    let mut buf = vec![0u8; 1024];
    // SAFETY: a writable buffer of its length.
    let n = unsafe { aster_last_error_message(buf.as_mut_ptr(), buf.len()) };
    buf.truncate(n.min(buf.len()));
    String::from_utf8_lossy(&buf).into_owned()
}

fn config(
    flags: u32,
    service: &'static [u8],
    log_filter: &'static [u8],
) -> aster_observability_config_t {
    aster_observability_config_t {
        struct_size: std::mem::size_of::<aster_observability_config_t>() as u32,
        flags,
        service_name: bytes(service),
        otlp_endpoint: bytes(b""),
        otlp_headers: bytes(b""),
        resource_attrs: bytes(b""),
        log_filter: bytes(log_filter),
        extra_toml: bytes(b""),
        trace_sample_ratio: -1.0,
        protocol: 0,
        log_queue_capacity: 0,
        binding: aster_binding_kind_t::ASTER_BINDING_GO as u32,
        _reserved: 0,
    }
}

fn test_cfg(t: &TestExporters) -> ObservabilityConfig {
    let mut cfg = ObservabilityConfig {
        from_env: false,
        test_exporters: Some(t.clone()),
        metric_export_interval: Some(Duration::from_secs(3600)),
        ..Default::default()
    };
    cfg.span_batch.scheduled_delay = Some(Duration::from_secs(3600));
    cfg
}

/// Run `f` on a fresh thread (so the calling thread's drain mark, set by
/// `aster_log_poll`, does not apply).
fn on_thread<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| s.spawn(f).join().unwrap())
}

/// One polled record, copied out of the batch arena.
#[derive(Debug, Clone)]
struct Rec {
    level: u8,
    flags: u8,
    target: String,
    message: String,
    fields: Vec<(String, String)>,
}

unsafe fn copy(p: *const u8, n: u32) -> Vec<u8> {
    if p.is_null() || n == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(p, n as usize).to_vec()
    }
}

/// Poll once (on this thread) and copy the batch out.
fn poll(max: u32, timeout_ms: u32) -> Vec<Rec> {
    let mut out: Vec<aster_log_record_t> = Vec::with_capacity(max as usize);
    // SAFETY: room for `max` records.
    let n = unsafe { aster_log_poll(out.as_mut_ptr(), REC, max, timeout_ms) };
    // SAFETY: `n` records were written.
    unsafe { out.set_len(n as usize) };
    out.iter()
        .map(|r| unsafe {
            Rec {
                level: r.level,
                flags: r.flags,
                target: String::from_utf8(copy(r.target_ptr, r.target_len)).unwrap(),
                message: String::from_utf8(copy(r.message_ptr, r.message_len)).unwrap(),
                fields: decode_kv(&copy(r.fields_ptr, r.fields_len)),
            }
        })
        .collect()
}

/// Poll until `n` records with target `target` arrived (or 10 s).
fn collect(target: &str, n: usize) -> Vec<Rec> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < n && Instant::now() < deadline {
        got.extend(
            poll(64, 100)
                .into_iter()
                .filter(|r| r.target == target || r.flags & ASTER_LOG_FLAG_SYNTHETIC != 0),
        );
    }
    got
}

// ── 7.1 pipeline ────────────────────────────────────────────────────────────

#[test]
fn init_shutdown_and_reinit_refcount() {
    let _g = serial();
    let cfg = config(0, b"obs-refcount", b"");
    unsafe {
        assert_eq!(aster_observability_init(&cfg), OK);
        let generation = aster_otel::generation();
        assert_eq!(aster_observability_init(&cfg), OK, "same config joins");
        assert_eq!(
            aster_otel::generation(),
            generation,
            "joining builds nothing"
        );
        let other = config(0, b"obs-other", b"");
        assert_eq!(aster_observability_init(&other), EXISTS);
        assert!(last_error().contains("service_name"), "{}", last_error());
        let mut bad = cfg;
        bad.struct_size -= 8;
        assert_eq!(aster_observability_init(&bad), INVALID);
    }
    assert_eq!(aster_observability_flush(2_000), OK);
    assert_eq!(aster_observability_shutdown(5_000), OK);
    assert!(aster_otel::state().is_some(), "one reference left");
    assert_eq!(aster_observability_shutdown(5_000), OK);
    assert!(aster_otel::state().is_none(), "last reference shut it down");
    assert_eq!(aster_observability_shutdown(5_000), CLOSED);
    assert_eq!(
        aster_observability_flush(100),
        OK,
        "no pipeline: nothing to flush"
    );

    // Re-init, from a tokio runtime thread (the call moves off it).
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        assert_eq!(unsafe { aster_observability_init(&cfg) }, OK);
        assert!(aster_otel::state().is_some());
        assert_eq!(aster_observability_shutdown(5_000), OK);
    });
    assert!(aster_otel::state().is_none());
}

// ── 7.2 logs ────────────────────────────────────────────────────────────────

#[test]
fn log_poll_order_drops_audit_and_refusals() {
    let _g = serial();
    let mut cfg = config(
        ASTER_OBSERVABILITY_LOG_BRIDGE,
        b"obs-logs",
        b"warn,obs_test=info",
    );
    cfg.log_queue_capacity = 4;
    assert_eq!(unsafe { aster_observability_init(&cfg) }, OK);

    // Stride and argument checks.
    let mut one: [std::mem::MaybeUninit<aster_log_record_t>; 1] = [std::mem::MaybeUninit::uninit()];
    let out = one.as_mut_ptr().cast::<aster_log_record_t>();
    assert_eq!(unsafe { aster_log_poll(out, REC - 8, 1, 0) }, 0);
    assert!(last_error().starts_with("ASTER_STATUS_INVALID_ARGUMENT"));
    assert_eq!(unsafe { aster_log_poll(ptr::null_mut(), REC, 1, 0) }, 0);
    assert_eq!(unsafe { aster_log_poll(out, REC, 0, 0) }, 0);

    // FIFO with fields.
    on_thread(|| {
        for n in 0..3 {
            tracing::info!(target: "obs_test", n, "record {n}");
        }
        tracing::debug!(target: "obs_test", "filtered out");
    });
    let got = collect("obs_test", 3);
    let messages: Vec<&str> = got.iter().map(|r| r.message.as_str()).collect();
    assert_eq!(messages, ["record 0", "record 1", "record 2"]);
    assert_eq!(got[0].level, aster_log_level_t::ASTER_LOG_INFO as u8);
    assert!(
        got[1].fields.contains(&("n".into(), "1".into())),
        "{:?}",
        got[1].fields
    );

    // Drops: the queue holds 4; nobody drains while 10 arrive.
    let mut before = 0u64;
    assert_eq!(unsafe { aster_log_dropped(&mut before) }, OK);
    on_thread(|| {
        for n in 0..10 {
            tracing::warn!(target: "obs_test", n, "burst {n}");
        }
    });
    let mut after = 0u64;
    assert_eq!(unsafe { aster_log_dropped(&mut after) }, OK);
    assert_eq!(after - before, 6);
    let batch = poll(64, 1_000);
    assert!(batch[0].flags & ASTER_LOG_FLAG_SYNTHETIC != 0, "{batch:?}");
    assert_eq!(batch[0].target, "aster::log");
    assert!(batch[0].fields.contains(&("dropped".into(), "6".into())));
    let burst: Vec<&str> = batch[1..]
        .iter()
        .filter(|r| r.target == "obs_test")
        .map(|r| r.message.as_str())
        .collect();
    assert_eq!(burst, ["burst 0", "burst 1", "burst 2", "burst 3"]);

    // Audit records come first.
    on_thread(|| {
        tracing::warn!(target: "obs_test", "general");
        tracing::info!(target: "aster::audit", decision = "deny", "audit");
    });
    // Both are queued before the poll, so one batch holds them.
    let batch: Vec<Rec> = poll(8, 1_000)
        .into_iter()
        .filter(|r| r.target == "obs_test" || r.target == "aster::audit")
        .collect();
    let order: Vec<&str> = batch.iter().map(|r| r.message.as_str()).collect();
    assert_eq!(order[..2], ["audit", "general"], "{batch:?}");
    assert!(batch[0].flags & ASTER_LOG_FLAG_AUDIT != 0);

    // This thread polled, so it is a drain thread: its own events never
    // reach the queue (no feedback loop).
    tracing::warn!(target: "obs_test", "from the drain thread");
    assert!(poll(8, 200).iter().all(|r| r.target != "obs_test"));

    // One poller per process: a second concurrent poll is refused.
    let waiting = Arc::new(AtomicBool::new(false));
    let w2 = waiting.clone();
    let poller = std::thread::spawn(move || {
        w2.store(true, Ordering::SeqCst);
        poll(8, 5_000)
    });
    while !waiting.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(poll(8, 0).is_empty());
    assert!(
        last_error().starts_with("ASTER_STATUS_ALREADY_EXISTS"),
        "{}",
        last_error()
    );
    on_thread(|| tracing::warn!(target: "obs_test", "wake"));
    let woke = poller.join().unwrap();
    assert_eq!(woke.len(), 1);
    assert_eq!(woke[0].message, "wake");

    // Refused on a tokio runtime thread (it blocks), as is aster_upcall_poll.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        assert_eq!(unsafe { aster_log_poll(out, REC, 1, 0) }, 0);
        assert!(last_error().starts_with("ASTER_STATUS_UNSUPPORTED"));
        let mut ev: std::mem::MaybeUninit<aster_upcall_event_t> = std::mem::MaybeUninit::uninit();
        assert_eq!(unsafe { aster_upcall_poll(1, 1, ev.as_mut_ptr(), 1, 0) }, 0);
        assert!(last_error().starts_with("ASTER_STATUS_UNSUPPORTED"));
    });

    // Filter reload.
    assert_eq!(unsafe { aster_log_set_filter(b"[[".as_ptr(), 2) }, INVALID);
    let f = b"warn,obs_test=debug";
    assert_eq!(
        unsafe { aster_log_set_filter(f.as_ptr(), f.len() as u32) },
        OK
    );
    on_thread(|| tracing::debug!(target: "obs_test", "now visible"));
    assert_eq!(collect("obs_test", 1)[0].message, "now visible");

    // Close wakes a waiting poller; later polls return 0.
    let start = Instant::now();
    let poller = std::thread::spawn(|| poll(8, 10_000));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(aster_log_close(), OK);
    assert!(poller.join().unwrap().is_empty());
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(poll(8, 0).is_empty());
    assert!(last_error().starts_with("ASTER_STATUS_NOT_FOUND"));
    assert_eq!(
        unsafe { aster_log_set_filter(f.as_ptr(), f.len() as u32) },
        NOT_FOUND
    );

    assert_eq!(aster_observability_shutdown(5_000), OK);
    assert!(aster_otel::state().is_none());
}

// ── 7.3 metrics ─────────────────────────────────────────────────────────────

fn desc(kind: aster_instrument_kind_t, name: &'static [u8]) -> aster_instrument_desc_t {
    aster_instrument_desc_t {
        struct_size: std::mem::size_of::<aster_instrument_desc_t>() as u32,
        kind: kind as u32,
        name: bytes(name),
        description: bytes(b"synthetic test instrument"),
        unit: bytes(b""),
        auto_attrs: bytes(b""),
        cardinality_limit: 0,
        _p: 0,
        boundaries: ptr::null(),
        boundaries_len: 0,
        _p1: 0,
    }
}

unsafe fn instrument(kind: aster_instrument_kind_t, name: &'static [u8]) -> aster_instrument_t {
    let mut h = 0;
    assert_eq!(aster_instrument_new(&desc(kind, name), &mut h), OK);
    h
}

#[test]
fn metrics_record_through_handles_and_scrape() {
    let _g = serial();
    unsafe {
        // Declared before any pipeline: records once one exists.
        let counter = instrument(
            aster_instrument_kind_t::ASTER_INSTRUMENT_COUNTER_U64,
            b"app.jobs",
        );
        let ex = TestExporters::new();
        let handle = aster_otel::init_with_prometheus(test_cfg(&ex)).unwrap();
        let updown = instrument(
            aster_instrument_kind_t::ASTER_INSTRUMENT_UP_DOWN_I64,
            b"app.queue_depth",
        );
        let hist = instrument(
            aster_instrument_kind_t::ASTER_INSTRUMENT_HISTOGRAM_F64,
            b"app.latency",
        );
        let gauge = instrument(
            aster_instrument_kind_t::ASTER_INSTRUMENT_GAUGE_F64,
            b"app.temperature",
        );
        // Seconds histogram (default buckets) and one with explicit boundaries.
        let mut secs_desc = desc(
            aster_instrument_kind_t::ASTER_INSTRUMENT_HISTOGRAM_F64,
            b"app.wait",
        );
        secs_desc.unit = bytes(b"s");
        let mut secs = 0;
        assert_eq!(aster_instrument_new(&secs_desc, &mut secs), OK);
        let bounds = [1.0f64, 5.0];
        let mut custom_desc = desc(
            aster_instrument_kind_t::ASTER_INSTRUMENT_HISTOGRAM_F64,
            b"app.sizes",
        );
        custom_desc.boundaries = bounds.as_ptr();
        custom_desc.boundaries_len = 2;
        let mut custom = 0;
        assert_eq!(aster_instrument_new(&custom_desc, &mut custom), OK);
        assert_eq!(aster_metric_record_f64(secs, 0.3, 0, ptr::null(), 0, 0), OK);
        assert_eq!(
            aster_metric_record_f64(custom, 3.0, 0, ptr::null(), 0, 0),
            OK
        );
        // Bad boundaries, and boundaries on a counter, are refused.
        let bad = [2.0f64, 1.0];
        let mut bad_desc = custom_desc;
        bad_desc.name = bytes(b"app.bad");
        bad_desc.boundaries = bad.as_ptr();
        let mut h = 0;
        assert_eq!(aster_instrument_new(&bad_desc, &mut h), INVALID);
        let mut ctr = desc(
            aster_instrument_kind_t::ASTER_INSTRUMENT_COUNTER_U64,
            b"app.ctr_bounds",
        );
        ctr.boundaries = bounds.as_ptr();
        ctr.boundaries_len = 2;
        assert_eq!(aster_instrument_new(&ctr, &mut h), INVALID);

        let attrs = kv(&[("tenant.id", "t-1")]);
        let mut set = 0;
        assert_eq!(
            aster_attrset_new(attrs.as_ptr(), attrs.len() as u32, &mut set),
            OK
        );
        let extra = kv(&[("region", "eu")]);
        assert_eq!(aster_metric_add_u64(counter, 2, set, ptr::null(), 0, 0), OK);
        assert_eq!(aster_metric_add_u64(counter, 3, set, ptr::null(), 0, 0), OK);
        assert_eq!(
            aster_metric_add_u64(counter, 1, set, extra.as_ptr(), extra.len() as u32, 0),
            OK
        );
        assert_eq!(aster_metric_add_i64(updown, 5, 0, ptr::null(), 0, 0), OK);
        assert_eq!(aster_metric_add_i64(updown, -2, 0, ptr::null(), 0, 0), OK);
        assert_eq!(
            aster_metric_record_f64(hist, 0.25, 0, ptr::null(), 0, 0),
            OK
        );
        assert_eq!(
            aster_metric_record_f64(hist, 0.75, 0, ptr::null(), 0, 0),
            OK
        );
        assert_eq!(
            aster_metric_record_f64(gauge, 21.5, 0, ptr::null(), 0, 0),
            OK
        );

        // Kind and handle checks.
        assert_eq!(
            aster_metric_record_f64(counter, 1.0, 0, ptr::null(), 0, 0),
            INVALID
        );
        assert_eq!(
            aster_metric_add_u64(counter, 1, 999_999, ptr::null(), 0, 0),
            NOT_FOUND
        );
        let garbage = [9u8, 0, 0, 0];
        assert_eq!(
            aster_metric_add_u64(counter, 1, 0, garbage.as_ptr(), 4, 0),
            INVALID
        );
        let mut h = 0;
        assert_eq!(
            aster_instrument_new(
                &desc(
                    aster_instrument_kind_t::ASTER_INSTRUMENT_GAUGE_F64,
                    b"app.jobs"
                ),
                &mut h
            ),
            INVALID,
            "same name, other kind"
        );
        assert_eq!(
            aster_instrument_new(
                &desc(
                    aster_instrument_kind_t::ASTER_INSTRUMENT_GAUGE_F64,
                    b"9bad name"
                ),
                &mut h
            ),
            INVALID
        );
        let mut d = desc(
            aster_instrument_kind_t::ASTER_INSTRUMENT_COUNTER_U64,
            b"app.x",
        );
        d.kind = 42;
        assert_eq!(aster_instrument_new(&d, &mut h), INVALID);

        assert_eq!(aster_observability_flush(5_000), OK);
        let jobs = ex.metric_points("app.jobs");
        let with = |region: Option<&str>| {
            jobs.iter()
                .find(|p| p.attr("tenant.id") == Some("t-1") && p.attr("region") == region)
                .map(|p| p.value)
        };
        assert_eq!(with(None), Some(5.0), "{jobs:?}");
        assert_eq!(with(Some("eu")), Some(1.0), "{jobs:?}");
        assert_eq!(ex.metric_points("app.queue_depth")[0].value, 3.0);
        let lat = &ex.metric_points("app.latency")[0];
        assert_eq!((lat.count, lat.value), (2, 1.0));
        assert_eq!(ex.metric_points("app.temperature")[0].value, 21.5);

        // The same numbers through the Prometheus pull reader.
        let mut text = bytes(b"");
        assert_eq!(aster_metrics_prometheus(&mut text), OK);
        let body = String::from_utf8(copy(text.ptr, text.len as u32)).unwrap();
        assert_eq!(aster_string_release(text.ptr, text.len), OK);
        assert!(body.contains("app_jobs_total{"), "{body}");
        assert!(body.contains("otel_scope_name=\"aster.app\""), "{body}");
        assert!(
            body.contains("app_wait_seconds_bucket{") && body.contains("le=\"0.005\""),
            "seconds histogram uses the RPC buckets: {body}"
        );
        assert!(body.contains("le=\"7.5\""), "{body}");
        let sizes: Vec<&str> = body
            .lines()
            .filter(|l| l.starts_with("app_sizes_bucket"))
            .collect();
        assert_eq!(sizes.len(), 3, "explicit boundaries 1, 5, +Inf: {body}");
        assert!(
            sizes[0].contains("le=\"1\"") && sizes[1].contains("le=\"5\""),
            "{sizes:?}"
        );
        assert_eq!(aster_metrics_prometheus(ptr::null_mut()), INVALID);

        assert_eq!(aster_attrset_free(set), OK);
        assert_eq!(aster_attrset_free(set), NOT_FOUND);
        assert_eq!(aster_instrument_free(gauge), OK);
        assert_eq!(aster_instrument_free(gauge), NOT_FOUND);

        // Shutdown frees every handle (§7.1).
        drop(handle);
        assert!(aster_otel::state().is_none());
        assert_eq!(
            aster_metric_add_u64(counter, 1, 0, ptr::null(), 0, 0),
            NOT_FOUND
        );
        assert_eq!(aster_instrument_free(hist), NOT_FOUND);
        let mut text = bytes(b"");
        assert_eq!(aster_metrics_prometheus(&mut text), NOT_FOUND);
    }
}

// ── 7.4 spans and trace context ─────────────────────────────────────────────

fn ctx_of(s: aster_span_t) -> aster_span_context_t {
    let mut c = aster_span_context_t::default();
    assert_eq!(unsafe { aster_span_context(s, &mut c) }, OK);
    c
}

fn traceparent(c: &aster_span_context_t) -> Vec<u8> {
    let mut buf = [0u8; 64];
    let mut n = 0;
    assert_eq!(
        unsafe { aster_trace_context_format(c, buf.as_mut_ptr(), buf.len() as u32, &mut n) },
        OK
    );
    buf[..n as usize].to_vec()
}

fn unix_nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
}

#[test]
fn spans_record_and_traceparent_round_trip() {
    let _g = serial();
    let ex = TestExporters::new();
    let handle = aster_otel::init(test_cfg(&ex)).unwrap();
    unsafe {
        let attrs = kv(&[("job", "nightly")]);
        let mut root = 0;
        assert_eq!(
            aster_span_start(
                bytes(b"root-op"),
                2,
                ptr::null(),
                attrs.as_ptr(),
                attrs.len() as u32,
                &mut root
            ),
            OK
        );
        let rc = ctx_of(root);
        assert!(rc.trace_id != [0; 16] && rc.trace_flags & 1 == 1 && rc.is_remote == 0);

        // traceparent round trip.
        let tp = traceparent(&rc);
        assert_eq!(tp.len(), 55);
        let mut parsed = aster_span_context_t::default();
        assert_eq!(
            aster_trace_context_parse(tp.as_ptr(), tp.len() as u32, &mut parsed),
            OK
        );
        assert_eq!(
            (parsed.trace_id, parsed.span_id, parsed.trace_flags),
            (rc.trace_id, rc.span_id, rc.trace_flags)
        );
        assert_eq!(parsed.is_remote, 1);

        // A child of the parsed (remote) context.
        let mut child = 0;
        assert_eq!(
            aster_span_start(bytes(b"child-op"), 0, &parsed, ptr::null(), 0, &mut child),
            OK
        );
        let cc = ctx_of(child);
        assert_eq!(cc.trace_id, rc.trace_id);
        let ev = kv(&[("attempt", "2")]);
        assert_eq!(
            aster_span_add_event(child, bytes(b"retry"), ev.as_ptr(), ev.len() as u32),
            OK
        );
        assert_eq!(
            aster_span_set_attributes(child, ev.as_ptr(), ev.len() as u32),
            OK
        );
        assert_eq!(
            aster_span_record_error(child, bytes(b"IOError"), bytes(b"disk full")),
            OK
        );
        assert_eq!(aster_span_end(child), OK);
        assert_eq!(aster_span_end(child), NOT_FOUND);
        assert_eq!(aster_span_set_status(root, 1, bytes(b"")), OK);

        // A finished span recorded with the binding's own ids and times.
        let span_id = [0x5a; 8];
        let start = SystemTime::now() - Duration::from_millis(30);
        let end = start + Duration::from_millis(20);
        let mut events = 1u32.to_le_bytes().to_vec();
        events.extend(5u32.to_le_bytes());
        events.extend(b"phase");
        events.extend(unix_nanos(start + Duration::from_millis(5)).to_le_bytes());
        let ekv = kv(&[("step", "1")]);
        events.extend((ekv.len() as u32).to_le_bytes());
        events.extend(&ekv);
        let rec_attrs = kv(&[("activity.source", "Aster")]);
        let rec = aster_span_record_t {
            context: aster_span_context_t {
                span_id,
                trace_flags: 1,
                ..rc
            },
            parent: cc,
            start_unix_nanos: unix_nanos(start),
            end_unix_nanos: unix_nanos(end),
            kind: aster_span_kind_t::ASTER_SPAN_CLIENT as u32,
            status_code: 2,
            name: bytes(b"recorded-op"),
            status_message: bytes(b"upstream 503"),
            kv: rec_attrs.as_ptr(),
            kv_len: rec_attrs.len() as u32,
            _p0: 0,
            events: events.as_ptr(),
            events_len: events.len() as u32,
            _p1: 0,
        };
        assert_eq!(aster_span_record(&rec), OK);
        // Not sampled: accepted, not exported.
        let unsampled = aster_span_record_t {
            context: aster_span_context_t {
                span_id: [0x6b; 8],
                trace_flags: 0,
                ..rc
            },
            name: bytes(b"unsampled-op"),
            ..rec
        };
        assert_eq!(aster_span_record(&unsampled), OK);
        // A parent from another trace, an end before the start, bad events.
        let other_trace = aster_span_record_t {
            parent: aster_span_context_t {
                trace_id: [7; 16],
                ..cc
            },
            ..rec
        };
        assert_eq!(aster_span_record(&other_trace), INVALID);
        let backwards = aster_span_record_t {
            end_unix_nanos: rec.start_unix_nanos - 1,
            ..rec
        };
        assert_eq!(aster_span_record(&backwards), INVALID);
        let bad_events = aster_span_record_t {
            events_len: rec.events_len - 1,
            ..rec
        };
        assert_eq!(aster_span_record(&bad_events), INVALID);

        // A span left open is ended by the shutdown below.
        let mut leaked = 0;
        assert_eq!(
            aster_span_start(
                bytes(b"leaked-op"),
                0,
                ptr::null(),
                ptr::null(),
                0,
                &mut leaked
            ),
            OK
        );
        assert_eq!(aster_span_end(root), OK);
        drop(handle);
        assert_eq!(
            aster_span_end(leaked),
            NOT_FOUND,
            "shutdown freed the handle"
        );
        assert_eq!(aster_otel::spans::live_spans(), 0);

        let spans = ex.spans.finished_spans();
        let find = |n: &str| {
            spans
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("{n} in {spans:?}"))
        };
        let root_s = find("root-op");
        assert_eq!(root_s.span_context.span_id().to_bytes(), rc.span_id);
        assert!(root_s.attributes.iter().any(|kv| kv.key.as_str() == "job"));
        let child_s = find("child-op");
        assert_eq!(child_s.parent_span_id.to_bytes(), rc.span_id);
        assert!(child_s.parent_span_is_remote);
        assert!(child_s.events.events.iter().any(|e| e.name == "retry"));
        assert!(child_s.events.events.iter().any(|e| e.name == "exception"));
        assert!(matches!(
            child_s.status,
            aster_otel::opentelemetry::trace::Status::Error { .. }
        ));
        let rec_s = find("recorded-op");
        assert_eq!(rec_s.span_context.trace_id().to_bytes(), rc.trace_id);
        assert_eq!(rec_s.span_context.span_id().to_bytes(), span_id);
        assert_eq!(rec_s.parent_span_id.to_bytes(), cc.span_id);
        assert_eq!(
            rec_s.start_time,
            UNIX_EPOCH + Duration::from_nanos(unix_nanos(start))
        );
        assert_eq!(
            rec_s.end_time,
            UNIX_EPOCH + Duration::from_nanos(unix_nanos(end))
        );
        assert_eq!(
            rec_s.span_kind,
            aster_otel::opentelemetry::trace::SpanKind::Client
        );
        assert_eq!(rec_s.events.events[0].name, "phase");
        assert!(matches!(
            rec_s.status,
            aster_otel::opentelemetry::trace::Status::Error { .. }
        ));
        assert!(spans.iter().all(|s| s.name != "unsampled-op"));
        assert!(
            spans.iter().any(|s| s.name == "leaked-op"),
            "ended at shutdown"
        );
    }
}

// ── the dispatch path: client metrics, server trace context ────────────────

fn hash(b: u8) -> TypeRef {
    TypeRef::new(format!("{b:02x}").repeat(32))
}

fn echo_json() -> String {
    let mut svc = DynamicService::new("obs.Echo", 1);
    svc.methods = vec![MethodDesc::new(
        "echo",
        MethodPattern::Unary,
        hash(0xa1),
        hash(0xb2),
    )];
    svc.to_json().unwrap()
}

unsafe fn start_node(rt: aster_runtime_t) -> aster_node_t {
    let alpn = [bytes(RPC_ALPN)];
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), 0);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.alpns = aster_bytes_list_t {
        items: alpn.as_ptr(),
        len: 1,
    };
    let mut op = 0;
    assert_eq!(aster_node_start(rt, &cfg, 0, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_NODE_CREATED)
        .handle
}

unsafe fn connect(
    rt: aster_runtime_t,
    client: aster_node_t,
    server: aster_node_t,
) -> aster_connection_t {
    let end = Instant::now() + Duration::from_secs(10);
    let (id, direct) = loop {
        let mut buf = vec![0u8; 4096];
        let mut out: aster_node_addr_t = std::mem::zeroed();
        assert_eq!(
            aster_node_addr_info(rt, server, buf.as_mut_ptr(), buf.len(), &mut out),
            OK
        );
        let s = |b: &aster_bytes_t| String::from_utf8(copy(b.ptr, b.len as u32)).unwrap();
        let direct: Vec<String> = if out.direct_addresses.len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(out.direct_addresses.items, out.direct_addresses.len)
                .iter()
                .map(s)
                .collect()
        };
        if !direct.is_empty() || Instant::now() > end {
            break (s(&out.endpoint_id), direct);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let direct_b: Vec<aster_bytes_t> = direct.iter().map(|d| bytes(d.as_bytes())).collect();
    let addr = aster_node_addr_t {
        endpoint_id: bytes(id.as_bytes()),
        relay_url: bytes(b""),
        direct_addresses: aster_bytes_list_t {
            items: direct_b.as_ptr(),
            len: direct_b.len(),
        },
    };
    let cfg = aster_connect_config_t {
        struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
        flags: 0,
        node_id: bytes(id.as_bytes()),
        alpn: bytes(RPC_ALPN),
        addr: &addr,
    };
    let mut op = 0;
    assert_eq!(aster_connect(rt, client, &cfg, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CONNECTED).handle
}

#[test]
fn dispatch_carries_trace_context_and_rpc_unary_records_client_metrics() {
    let _g = serial();
    let ex = TestExporters::new();
    let handle = aster_otel::init(test_cfg(&ex)).unwrap();
    unsafe {
        let mut rt = 0;
        assert_eq!(
            aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
            OK
        );
        let server_node = start_node(rt);
        let mut b = 0;
        assert_eq!(aster_server_builder_new(rt, server_node, &mut b), OK);
        let json = echo_json();
        let mut idx = 0;
        assert_eq!(
            aster_server_add_service_json(b, json.as_ptr(), json.len() as u32, &mut idx),
            OK
        );
        let mut server = 0;
        assert_eq!(aster_server_start(rt, b, 64, &mut server), OK);

        // The "binding": answer one call, reporting its flags and context.
        let binding = std::thread::spawn(move || {
            let mut evs: Vec<aster_dispatch_event_t> = Vec::with_capacity(8);
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let n = aster_server_poll(rt, server, evs.as_mut_ptr(), 8, 100);
                evs.set_len(n as usize);
                for ev in evs.drain(..) {
                    if ev.kind != aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_CALL as u8 {
                        continue;
                    }
                    let mut cx = aster_span_context_t::default();
                    assert_eq!(
                        aster_dispatch_trace_context(rt, server, ev.call_id, &mut cx),
                        OK
                    );
                    // Inbound baggage / tracestate: off the call, not in metadata.
                    let kv_of = |f: unsafe extern "C" fn(
                        aster_runtime_t,
                        aster_server_t,
                        u64,
                        *mut *const u8,
                        *mut u32,
                        *mut aster_buffer_t,
                    ) -> i32| {
                        let (mut p, mut l, mut b) = (ptr::null(), 0u32, 0u64);
                        assert_eq!(f(rt, server, ev.call_id, &mut p, &mut l, &mut b), OK);
                        let v = decode_kv(&copy(p, l));
                        if b != 0 {
                            aster_dispatch_buffer_release(rt, server, b);
                        }
                        v
                    };
                    let inbound = kv_of(aster_dispatch_inbound_trace);
                    let metadata = kv_of(aster_dispatch_metadata);
                    let req = copy(ev.request_ptr, ev.request_len);
                    let mut required = 0;
                    assert_eq!(
                        aster_dispatch_respond(
                            rt,
                            server,
                            ev.call_id,
                            req.as_ptr(),
                            req.len() as u32,
                            ptr::null(),
                            0,
                            &mut required
                        ),
                        OK
                    );
                    aster_dispatch_buffer_release(rt, server, ev.buffer);
                    assert_eq!(
                        aster_dispatch_trace_context(rt, server, ev.call_id, &mut cx),
                        NOT_FOUND,
                        "call over"
                    );
                    let (mut p, mut l, mut b) = (ptr::null(), 0u32, 0u64);
                    assert_eq!(
                        aster_dispatch_inbound_trace(
                            rt, server, ev.call_id, &mut p, &mut l, &mut b
                        ),
                        NOT_FOUND,
                        "call over"
                    );
                    return (ev.flags, cx, inbound, metadata);
                }
            }
            panic!("no call arrived");
        });

        let client_node = start_node(rt);
        let conn = connect(rt, client_node, server_node);
        // The caller's span, passed as `traceparent` (§5.4).
        let mut caller = 0;
        assert_eq!(
            aster_span_start(
                bytes(b"caller-op"),
                2,
                ptr::null(),
                ptr::null(),
                0,
                &mut caller
            ),
            OK
        );
        let caller_cx = ctx_of(caller);
        let tp = traceparent(&caller_cx);
        let meta = kv(&[
            ("traceparent", std::str::from_utf8(&tp).unwrap()),
            ("baggage", "tenant=acme,flag=1"),
            ("tracestate", "vendor=a"),
            ("x-user", "1"),
        ]);
        let opts = aster_call_options_t {
            struct_size: std::mem::size_of::<aster_call_options_t>() as u32,
            version: 1,
            service: bytes(b"obs.Echo"),
            method: bytes(b"echo"),
            metadata: bytes(&meta),
            session_id: 0,
            deadline_ms: 5_000,
            serialization_mode: 0,
            wire_call_id: 0,
        };
        let (mut rp, mut rl, mut rb) = (ptr::null(), 0u32, 0u64);
        let (mut code, mut sp, mut sl, mut sb) = (0i32, ptr::null(), 0u32, 0u64);
        let req = b"ping";
        assert_eq!(
            aster_rpc_unary(
                rt,
                conn,
                &opts,
                req.as_ptr(),
                4,
                &mut rp,
                &mut rl,
                &mut rb,
                &mut code,
                &mut sp,
                &mut sl,
                &mut sb
            ),
            OK
        );
        assert_eq!(code, 0);
        assert_eq!(copy(rp, rl), req);
        aster_call_buffer_release(rt, rb);
        aster_call_buffer_release(rt, sb);

        let (flags, server_cx, inbound, metadata) = binding.join().unwrap();
        assert!(
            flags & ASTER_DISPATCH_FLAG_HAS_TRACE != 0,
            "flags {flags:#x}"
        );
        assert!(
            flags & ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE != 0,
            "flags {flags:#x}"
        );
        assert_eq!(
            inbound,
            [
                ("baggage".to_string(), "tenant=acme,flag=1".to_string()),
                ("tracestate".to_string(), "vendor=a".to_string())
            ]
        );
        assert_eq!(
            metadata,
            [("x-user".to_string(), "1".to_string())],
            "reserved keys are not metadata"
        );
        assert_eq!(
            server_cx.trace_id, caller_cx.trace_id,
            "one trace from caller to handler"
        );
        assert_ne!(server_cx.span_id, caller_cx.span_id);
        assert_eq!(aster_span_end(caller), OK);

        let mut cx = aster_span_context_t::default();
        assert_eq!(
            aster_dispatch_trace_context(rt, 999_999, 1, &mut cx),
            NOT_FOUND
        );
        assert_eq!(
            aster_dispatch_trace_context(rt, server, 1, ptr::null_mut()),
            INVALID
        );
        assert_eq!(
            aster_dispatch_inbound_trace(
                rt,
                server,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut()
            ),
            INVALID
        );

        assert_eq!(aster_observability_flush(5_000), OK);
        let completed = ex.metric_points("aster.rpc.client.requests_completed");
        let p = completed
            .iter()
            .find(|p| p.attr("rpc.service") == Some("obs.Echo"))
            .unwrap_or_else(|| panic!("{completed:?}"));
        assert_eq!(
            (p.attr("rpc.method"), p.attr("aster.status"), p.value),
            (Some("echo"), Some("OK"), 1.0)
        );
        assert!(!ex.metric_points("aster.rpc.client.duration").is_empty());

        assert_eq!(aster_server_destroy(rt, server), OK);
        aster_runtime_close(rt);
    }
    drop(handle);
    let spans = ex.spans.finished_spans();
    let client_span = spans
        .iter()
        .find(|s| {
            s.span_kind == aster_otel::opentelemetry::trace::SpanKind::Client
                && s.name.contains("obs.Echo")
        })
        .unwrap_or_else(|| panic!("client span in {spans:?}"));
    let caller_span = spans.iter().find(|s| s.name == "caller-op").unwrap();
    assert_eq!(
        client_span.parent_span_id,
        caller_span.span_context.span_id(),
        "the client span parents on the caller's traceparent"
    );
}

// ── NULL and unknown handles ────────────────────────────────────────────────

#[test]
fn null_and_unknown_handles_are_refused() {
    let _g = serial();
    unsafe {
        let mut h = 0u64;
        let mut cx = aster_span_context_t::default();
        let mut text = bytes(b"");
        assert_eq!(aster_instrument_new(ptr::null(), &mut h), INVALID);
        let d = desc(
            aster_instrument_kind_t::ASTER_INSTRUMENT_COUNTER_U64,
            b"app.null",
        );
        assert_eq!(aster_instrument_new(&d, ptr::null_mut()), INVALID);
        assert_eq!(aster_instrument_free(0), NOT_FOUND);
        assert_eq!(aster_attrset_new(ptr::null(), 0, ptr::null_mut()), INVALID);
        assert_eq!(aster_attrset_free(0), NOT_FOUND);
        assert_eq!(aster_metric_add_u64(0, 1, 0, ptr::null(), 0, 0), NOT_FOUND);
        assert_eq!(
            aster_metric_add_i64(u64::MAX, 1, 0, ptr::null(), 0, 0),
            NOT_FOUND
        );
        assert_eq!(
            aster_metric_record_f64(7, 1.0, 0, ptr::null(), 0, 0),
            NOT_FOUND
        );
        assert_eq!(
            aster_span_start(bytes(b"x"), 0, ptr::null(), ptr::null(), 0, ptr::null_mut()),
            INVALID
        );
        assert_eq!(
            aster_span_start(bytes(&[0xff]), 0, ptr::null(), ptr::null(), 0, &mut h),
            INVALID
        );
        assert_eq!(aster_span_set_attributes(0, ptr::null(), 0), NOT_FOUND);
        assert_eq!(
            aster_span_add_event(0, bytes(b"e"), ptr::null(), 0),
            NOT_FOUND
        );
        assert_eq!(aster_span_set_status(0, 1, bytes(b"")), NOT_FOUND);
        assert_eq!(
            aster_span_record_error(0, bytes(b""), bytes(b"")),
            NOT_FOUND
        );
        assert_eq!(aster_span_context(0, &mut cx), NOT_FOUND);
        assert_eq!(aster_span_context(0, ptr::null_mut()), INVALID);
        assert_eq!(aster_span_end(0), NOT_FOUND);
        assert_eq!(aster_span_record(ptr::null()), INVALID);
        let zero = aster_span_record_t {
            context: cx,
            parent: cx,
            start_unix_nanos: 0,
            end_unix_nanos: 0,
            kind: 0,
            status_code: 0,
            name: bytes(b""),
            status_message: bytes(b""),
            kv: ptr::null(),
            kv_len: 0,
            _p0: 0,
            events: ptr::null(),
            events_len: 0,
            _p1: 0,
        };
        assert_eq!(aster_span_record(&zero), INVALID, "all-zero ids");
        assert_eq!(aster_trace_context_parse(ptr::null(), 0, &mut cx), INVALID);
        assert_eq!(
            aster_trace_context_parse(b"x".as_ptr(), 1, ptr::null_mut()),
            INVALID
        );
        let mut n = 0;
        assert_eq!(
            aster_trace_context_format(&cx, ptr::null_mut(), 0, &mut n),
            INVALID
        );
        assert_eq!(aster_log_emit(ptr::null()), INVALID);
        let mut emit: aster_log_emit_t = std::mem::zeroed();
        assert_eq!(aster_log_emit(&emit), INVALID, "struct_size 0");
        emit.struct_size = std::mem::size_of::<aster_log_emit_t>() as u32;
        emit.level = 9;
        assert_eq!(aster_log_emit(&emit), INVALID);
        emit.level = 0;
        assert_eq!(aster_log_emit(&emit), OK, "no pipeline: nothing exported");
        assert_eq!(aster_log_dropped(ptr::null_mut()), INVALID);
        assert_eq!(aster_log_close(), OK);
        assert_eq!(aster_log_set_filter(b"warn".as_ptr(), 4), NOT_FOUND);
        assert_eq!(aster_metrics_prometheus(&mut text), NOT_FOUND);
        assert_eq!(aster_observability_shutdown(100), CLOSED);
        let mut cfg = config(0, b"obs-null", b"");
        cfg.service_name = bytes(&[0xff, 0xfe]);
        assert_eq!(
            aster_observability_init(&cfg),
            INVALID,
            "service_name not UTF-8"
        );
        let mut cfg = config(0, b"obs-null", b"");
        let bad_kv = [3u8, 0, 0, 0];
        cfg.resource_attrs = bytes(&bad_kv);
        assert_eq!(aster_observability_init(&cfg), INVALID);
        let mut cfg = config(0, b"obs-null", b"");
        cfg.extra_toml = bytes(b"no_such_key = 1");
        assert_eq!(aster_observability_init(&cfg), INVALID);
        assert!(aster_otel::state().is_none());
    }
}
