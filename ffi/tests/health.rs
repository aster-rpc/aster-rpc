//! ABI 4.6 ops listener through the C functions (design
//! `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §6, §7.5): start,
//! attach to a dispatch server, readiness transitions on start / checks /
//! liveness / destroy, `/metrics`, stop, and NULL / unknown-handle safety.
//! Hit with a plain TCP client.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::ptr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aster::rpc::{DynamicService, MethodDesc, MethodPattern, TypeRef};
use aster_transport_ffi::health::*;
use aster_transport_ffi::node::*;
use aster_transport_ffi::observability::aster_metrics_prometheus;
use aster_transport_ffi::server::*;
use aster_transport_ffi::*;

mod common;

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const NOT_FOUND: i32 = aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
const UNSUPPORTED: i32 = aster_status_t::ASTER_STATUS_UNSUPPORTED as i32;
const CLOSED: i32 = aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;

/// The listeners share one process-wide pipeline.
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

unsafe fn runtime_and_node() -> (aster_runtime_t, aster_node_t) {
    let mut rt = 0;
    assert_eq!(
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
        OK
    );
    let alpn = [bytes(b"aster/1")];
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
    let node =
        common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_NODE_CREATED)
            .handle;
    (rt, node)
}

fn health_cfg(host: &'static [u8]) -> aster_health_config_t {
    aster_health_config_t {
        struct_size: std::mem::size_of::<aster_health_config_t>() as u32,
        port: 0,
        _p: 0,
        host: bytes(host),
    }
}

unsafe fn addr(h: aster_health_t) -> String {
    let mut out = bytes(b"");
    assert_eq!(aster_health_addr(h, &mut out), OK);
    let s = String::from_utf8(std::slice::from_raw_parts(out.ptr, out.len).to_vec()).unwrap();
    assert_eq!(aster_string_release(out.ptr, out.len), OK);
    s
}

/// `GET path` → (status, body).
fn get(addr: &str, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw[9..12].parse().unwrap();
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

fn echo_json() -> String {
    let h = |b: u8| TypeRef::new(format!("{b:02x}").repeat(32));
    let mut svc = DynamicService::new("health.Echo", 1);
    svc.methods = vec![MethodDesc::new(
        "echo",
        MethodPattern::Unary,
        h(0xa1),
        h(0xb2),
    )];
    svc.to_json().unwrap()
}

#[test]
fn attach_and_readiness_transitions() {
    let _g = serial();
    unsafe {
        let (rt, node) = runtime_and_node();
        // The node's config has health off: a NULL config is refused.
        let mut h = 0;
        assert_eq!(aster_health_start(rt, node, ptr::null(), &mut h), INVALID);
        let cfg = health_cfg(b"127.0.0.1");
        assert_eq!(aster_health_start(rt, node, &cfg, &mut h), OK);
        let a = addr(h);
        assert!(a.starts_with("127.0.0.1:"), "{a}");
        assert!(
            aster_otel::state().is_some(),
            "the listener started a pipeline"
        );

        let (st, body) = get(&a, "/healthz");
        assert_eq!(st, 200, "{body}");
        let (st, body) = get(&a, "/readyz");
        assert_eq!(st, 503, "not ready before a server starts: {body}");

        let mut b = 0;
        assert_eq!(aster_server_builder_new(rt, node, &mut b), OK);
        let json = echo_json();
        let mut idx = 0;
        assert_eq!(
            aster_server_add_service_json(b, json.as_ptr(), json.len() as u32, &mut idx),
            OK
        );
        assert_eq!(aster_server_attach_health(b, h), OK);
        assert_eq!(aster_server_attach_health(b, 999_999), NOT_FOUND);
        assert_eq!(aster_server_attach_health(999_999, h), NOT_FOUND);
        let mut srv = 0;
        assert_eq!(aster_server_start(rt, b, 32, &mut srv), OK);
        assert_eq!(aster_server_attach_health(b, h), NOT_FOUND, "consumed");

        let (st, body) = get(&a, "/readyz");
        assert_eq!(st, 200, "ready once serving: {body}");
        assert!(body.contains("\"services\":1"), "{body}");

        // A failing named check, then passing.
        assert_eq!(aster_health_check_set(h, bytes(b"db"), false), OK);
        let (st, body) = get(&a, "/readyz");
        assert_eq!(st, 503, "{body}");
        assert!(body.contains("\"db\":false"), "{body}");
        assert_eq!(aster_health_check_set(h, bytes(b"db"), true), OK);
        assert_eq!(get(&a, "/readyz").0, 200);

        // Liveness.
        assert_eq!(aster_health_set_healthy(h, false), OK);
        assert_eq!(get(&a, "/healthz").0, 503);
        assert_eq!(get(&a, "/readyz").0, 503);
        assert_eq!(aster_health_set_healthy(h, true), OK);
        assert_eq!(get(&a, "/readyz").0, 200);

        // The binding's own gating.
        assert_eq!(aster_health_set_ready(h, false), OK);
        assert_eq!(get(&a, "/readyz").0, 503);
        assert_eq!(aster_health_set_ready(h, true), OK);
        assert_eq!(get(&a, "/readyz").0, 200);

        // Metrics: HTTP and the in-process scrape agree on the format.
        let (st, body) = get(&a, "/metrics");
        assert_eq!(st, 200);
        assert!(body.contains("aster_health_ready"), "{body}");
        let mut text = bytes(b"");
        assert_eq!(aster_metrics_prometheus(&mut text), OK);
        let scraped =
            String::from_utf8(std::slice::from_raw_parts(text.ptr, text.len).to_vec()).unwrap();
        assert_eq!(aster_string_release(text.ptr, text.len), OK);
        assert!(scraped.contains("process_uptime_seconds"), "{scraped}");

        // Destroy: not ready first.
        assert_eq!(aster_server_destroy(rt, srv), OK);
        assert_eq!(get(&a, "/readyz").0, 503, "not ready after destroy");
        assert_eq!(get(&a, "/healthz").0, 200, "still alive");

        // Stop frees the port and the handle, and releases the pipeline.
        assert_eq!(aster_health_stop(rt, h), OK);
        assert!(TcpStream::connect(&a).is_err(), "port freed on return");
        assert_eq!(aster_health_stop(rt, h), NOT_FOUND);
        assert_eq!(aster_health_set_ready(h, true), NOT_FOUND);
        assert!(
            aster_otel::state().is_none(),
            "last pipeline reference released"
        );

        assert_eq!(aster_node_free(rt, node), OK);
        aster_runtime_close(rt);
    }
}

/// The C health listener shares readiness with the Rust-owned bridge. This
/// drives synthetic native tracing audit events because the public C ABI has
/// no audit-event emitter; it checks the C listener's observable contract.
#[test]
fn native_audit_drop_fails_and_recovers_c_health_readiness() {
    let _g = serial();
    // 5 s instead of the production 30 s (restored below); the serial lock
    // keeps the other tests in this binary from running meanwhile.
    aster_otel::bridge::set_audit_degraded_window_for_testing(Duration::from_secs(5));
    unsafe {
        let (rt, node) = runtime_and_node();
        let cfg = health_cfg(b"127.0.0.1");
        let mut h = 0;
        assert_eq!(aster_health_start(rt, node, &cfg, &mut h), OK);
        let a = addr(h);
        assert_eq!(aster_health_set_ready(h, true), OK);
        assert_eq!(get(&a, "/readyz").0, 200);

        // Opening the bridge is additive to the health listener's pipeline.
        let bridge = aster_otel::init(aster_otel::ObservabilityConfig {
            log_bridge: Some(true),
            ..Default::default()
        })
        .expect("open bridge on the listener's pipeline");
        let before = aster_otel::bridge::audit_dropped_total();
        // The default queue holds 1024 records. No consumer drains it here.
        for n in 0..1025 {
            tracing::info!(target: "aster::audit", n, "synthetic FFI health audit");
        }
        assert!(aster_otel::bridge::audit_dropped_total() > before);
        let (status, body) = get(&a, "/readyz");
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("\"audit\":false"), "{body}");

        let recovery_deadline = Instant::now() + Duration::from_secs(20);
        while aster_otel::bridge::audit_degraded() && Instant::now() < recovery_deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !aster_otel::bridge::audit_degraded(),
            "audit readiness did not recover within 20 seconds"
        );
        let (status, body) = get(&a, "/readyz");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"audit\":true"), "{body}");

        assert_eq!(aster_health_stop(rt, h), OK);
        drop(bridge);
        assert!(aster_otel::state().is_none());
        assert_eq!(aster_node_free(rt, node), OK);
        aster_runtime_close(rt);
    }
    aster_otel::bridge::set_audit_degraded_window_for_testing(
        aster_otel::bridge::AUDIT_DEGRADED_WINDOW,
    );
}

#[test]
fn null_unknown_and_runtime_thread_refusals() {
    let _g = serial();
    unsafe {
        let (rt, node) = runtime_and_node();
        let cfg = health_cfg(b"");
        let mut h = 0;
        assert_eq!(aster_health_start(rt, node, &cfg, ptr::null_mut()), INVALID);
        assert_eq!(aster_health_start(rt, 999_999, &cfg, &mut h), NOT_FOUND);
        assert_eq!(aster_health_start(999_999, node, &cfg, &mut h), NOT_FOUND);
        let mut bad = cfg;
        bad.struct_size = 4;
        assert_eq!(aster_health_start(rt, node, &bad, &mut h), INVALID);
        let mut bad_host = cfg;
        bad_host.host = bytes(&[0xff]);
        assert_eq!(aster_health_start(rt, node, &bad_host, &mut h), INVALID);
        assert_eq!(aster_health_set_ready(0, true), NOT_FOUND);
        assert_eq!(aster_health_set_healthy(0, true), NOT_FOUND);
        assert_eq!(aster_health_check_set(0, bytes(b"db"), true), NOT_FOUND);
        let mut out = bytes(b"");
        assert_eq!(aster_health_addr(0, &mut out), NOT_FOUND);
        assert_eq!(aster_health_addr(0, ptr::null_mut()), INVALID);
        assert_eq!(aster_health_stop(rt, 0), NOT_FOUND);

        // Start and stop block, so a tokio runtime thread is refused.
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        tokio_rt.block_on(async {
            assert_eq!(aster_health_start(rt, node, &cfg, &mut h), UNSUPPORTED);
            assert_eq!(aster_health_stop(rt, 1), UNSUPPORTED);
        });

        // An empty host binds 127.0.0.1; a bad check name is refused.
        assert_eq!(aster_health_start(rt, node, &cfg, &mut h), OK);
        assert!(addr(h).starts_with("127.0.0.1:"));
        assert_eq!(aster_health_check_set(h, bytes(b""), true), INVALID);
        assert_eq!(aster_health_check_set(h, bytes(&[0xc3]), true), INVALID);
        // A builder already started cannot take a listener.
        let mut b = 0;
        assert_eq!(aster_server_builder_new(rt, node, &mut b), OK);
        let mut srv = 0;
        assert_eq!(aster_server_start(rt, b, 8, &mut srv), OK);
        assert_eq!(aster_server_attach_health(b, h), NOT_FOUND);
        assert_eq!(aster_server_destroy(rt, srv), OK);
        assert_eq!(aster_health_stop(rt, h), OK);
        assert_eq!(aster_observability_shutdown_count(), CLOSED);
        assert_eq!(aster_node_free(rt, node), OK);
        aster_runtime_close(rt);
    }
}

/// The C ABI holds no init reference of its own here (the listener's was
/// released by stop).
fn aster_observability_shutdown_count() -> i32 {
    aster_transport_ffi::observability::aster_observability_shutdown(100)
}
