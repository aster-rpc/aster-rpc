//! ABI 3.5 (F4): dynamic dispatch through the C functions only.
//!
//! Server: `aster_server_*` + `aster_dispatch_*` on an `aster_node_start`
//! node; a "binding" poll thread drains `aster_server_poll` and runs each
//! call on its own handler thread, answering by `call_id` (what the Java
//! binding does with virtual threads). Client: `aster_rpc_unary` /
//! `aster_rpc_open` / `aster_call_*_payload` on an `aster_connect`ed
//! connection of a second node. Mirrors `aster/tests/dynamic_dispatch.rs`
//! (all four patterns, sessions, cancel, deadline, `QUEUE_FULL`/reserve,
//! connection close, Gate 3, frame limits) and ends every test with an
//! empty call table.

use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aster::rpc::{
    require_role, DynamicService, MethodDesc, MethodPattern, RpcStatus, ScopeKind, StatusCode,
    TypeRef,
};
use aster_transport_ffi::call::{aster_call_buffer_release, aster_call_release, aster_call_t};
use aster_transport_ffi::node::*;
use aster_transport_ffi::rpc::*;
use aster_transport_ffi::server::*;
use aster_transport_ffi::*;

mod common;

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const NOT_FOUND: i32 = aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const QUEUE_FULL: i32 = aster_status_t::ASTER_STATUS_QUEUE_FULL as i32;
const TIMEOUT: i32 = aster_status_t::ASTER_STATUS_TIMEOUT as i32;
const CLOSED: i32 = aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
const RPC_ALPN: &[u8] = b"aster/1";
const ADMISSION_ALPN: &[u8] = b"aster.consumer_admission";

// ── the services (as in aster/tests/dynamic_dispatch.rs) ────────────────────

const ECHO: u32 = 0;
const COUNTER: u32 = 1;
const ECHO_UNARY: u32 = 0;
const COUNT: u32 = 1;
const SUM: u32 = 2;
const CHAT: u32 = 3;
const FLOOD: u32 = 4;
const HANG: u32 = 5;
const SLOW: u32 = 6;
const DROP: u32 = 7;
const ADMIN: u32 = 8;
const SMALL: u32 = 9;
const META: u32 = 10;
const PARK: u32 = 11;
const INCR: u32 = 0;
const FLOOD_AFTER_FULL: usize = 8;
const FLOOD_FRAME: usize = 64 * 1024;

fn hash(b: u8) -> TypeRef {
    TypeRef::new(format!("{b:02x}").repeat(32))
}

fn method(name: &str, pattern: MethodPattern) -> MethodDesc {
    MethodDesc::new(name, pattern, hash(0xa1), hash(0xb2))
}

fn echo_json() -> String {
    let mut svc = DynamicService::new("test.Echo", 1);
    let mut slow = method("slow", MethodPattern::Unary);
    slow.default_timeout = Some(0.3);
    let mut admin = method("admin", MethodPattern::Unary);
    admin.requires = Some(require_role("admin"));
    let mut small = method("small", MethodPattern::Unary);
    small.limits.max_frame_size = Some(1024);
    svc.methods = vec![
        method("echo", MethodPattern::Unary),
        method("count", MethodPattern::ServerStream),
        method("sum", MethodPattern::ClientStream),
        method("chat", MethodPattern::BidiStream),
        method("flood", MethodPattern::ServerStream),
        method("hang", MethodPattern::ServerStream),
        slow,
        method("drop", MethodPattern::Unary),
        admin,
        small,
        method("meta", MethodPattern::Unary),
        method("park", MethodPattern::Unary),
    ];
    svc.to_json().unwrap()
}

fn counter_json() -> String {
    let mut svc = DynamicService::new("test.Counter", 1);
    svc.scope = ScopeKind::Session;
    svc.methods = vec![
        method("incr", MethodPattern::Unary),
        method("count", MethodPattern::ServerStream),
    ];
    svc.to_json().unwrap()
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
    let n = u32_at(0);
    let mut o = 4;
    let mut out = Vec::new();
    for _ in 0..n {
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

fn status_aev1(code: StatusCode, msg: &str) -> Vec<u8> {
    aster::rpc::codec::encode_rpc_status(&RpcStatus::error(code, msg)).unwrap()
}

/// Incompressible bytes.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x: u64 = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

// ── the fake binding (C functions only) ─────────────────────────────────────

/// A copied-out dispatch event (lease released).
#[derive(Clone, Debug)]
struct Ev {
    kind: u8,
    reason: u8,
    pattern: u8,
    flags: u32,
    call_id: u64,
    service_idx: u32,
    method_idx: u32,
    session_key: u64,
    session_id: u32,
    connection_id: u64,
    connection_handle: u64,
    deadline_unix_ms: i64,
    request: Vec<u8>,
    peer: String,
}

const K_CALL: u8 = aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_CALL as u8;
const K_CANCELLED: u8 = aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_CANCELLED as u8;
const K_WRITABLE: u8 = aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_RESPONSE_WRITABLE as u8;
const K_RCLOSED: u8 = aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_RESPONSE_CLOSED as u8;
const K_SESSION_CLOSED: u8 = aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_SESSION_CLOSED as u8;
const K_CONN_CLOSED: u8 = aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_CONNECTION_CLOSED as u8;
const K_NON_RPC: u8 = aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_NON_RPC as u8;
const R_PEER: u8 = aster_cancel_reason_t::ASTER_CANCEL_REASON_PEER as u8;
const R_DEADLINE: u8 = aster_cancel_reason_t::ASTER_CANCEL_REASON_DEADLINE as u8;

#[derive(Default)]
struct Observed {
    cancelled: Vec<(u64, u8, u32)>,
    sessions_opened: Vec<u64>,
    sessions_closed: Vec<u64>,
    connections_closed: Vec<(u64, String, u64)>,
    non_rpc: Vec<(Vec<u8>, u64)>,
    calls: Vec<(u32, u32, u32)>,
    notes: Vec<String>,
}

#[derive(Clone, Copy)]
struct Srv {
    rt: aster_runtime_t,
    server: aster_server_t,
}

struct Binding {
    srv: Srv,
    observed: Arc<Mutex<Observed>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Binding {
    fn start(srv: Srv) -> Self {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let observed = observed.clone();
            let stop = stop.clone();
            std::thread::spawn(move || poll_loop(srv, observed, stop))
        };
        Binding {
            srv,
            observed,
            stop,
            thread: Some(thread),
        }
    }

    fn observed(&self) -> std::sync::MutexGuard<'_, Observed> {
        self.observed.lock().unwrap()
    }

    fn until(&self, what: &str, f: impl Fn(&Observed) -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if f(&self.observed()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    fn assert_table_drains(&self) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if aster_server_live_calls(self.srv.server) == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "{} call(s) still in the call table",
            aster_server_live_calls(self.srv.server)
        );
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe fn copy(p: *const u8, n: u32) -> Vec<u8> {
    if p.is_null() || n == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(p, n as usize).to_vec()
    }
}

fn poll_loop(srv: Srv, observed: Arc<Mutex<Observed>>, stop: Arc<AtomicBool>) {
    let mut routes: HashMap<u64, mpsc::Sender<Ev>> = HashMap::new();
    let mut method_of: HashMap<u64, u32> = HashMap::new();
    let mut buf: Vec<aster_dispatch_event_t> = Vec::with_capacity(64);
    while !stop.load(Ordering::SeqCst) {
        let n = unsafe { aster_server_poll(srv.rt, srv.server, buf.as_mut_ptr(), 64, 20) };
        unsafe { buf.set_len(n as usize) };
        for raw in buf.drain(..) {
            let ev = unsafe {
                Ev {
                    kind: raw.kind,
                    reason: raw.reason,
                    pattern: raw.pattern,
                    flags: raw.flags,
                    call_id: raw.call_id,
                    service_idx: raw.service_idx,
                    method_idx: raw.method_idx,
                    session_key: raw.session_key,
                    session_id: raw.session_id,
                    connection_id: raw.connection_id,
                    connection_handle: raw.connection_handle,
                    deadline_unix_ms: raw.deadline_unix_ms,
                    request: copy(raw.request_ptr, raw.request_len),
                    peer: String::from_utf8(copy(raw.peer_ptr, raw.peer_len)).unwrap(),
                }
            };
            assert_eq!(
                aster_dispatch_buffer_release(srv.rt, srv.server, raw.buffer),
                OK
            );
            match ev.kind {
                K_CALL => {
                    {
                        let mut o = observed.lock().unwrap();
                        o.calls.push((ev.service_idx, ev.method_idx, ev.flags));
                        if ev.flags & ASTER_DISPATCH_FLAG_NEW_SESSION != 0 {
                            o.sessions_opened.push(ev.session_key);
                        }
                    }
                    let (tx, rx) = mpsc::channel();
                    routes.insert(ev.call_id, tx);
                    method_of.insert(ev.call_id, ev.method_idx);
                    let observed = observed.clone();
                    std::thread::spawn(move || handle(srv, ev, rx, observed));
                }
                K_CANCELLED | K_WRITABLE | K_RCLOSED => {
                    if ev.kind == K_CANCELLED {
                        observed.lock().unwrap().cancelled.push((
                            ev.call_id,
                            ev.reason,
                            method_of.get(&ev.call_id).copied().unwrap_or(u32::MAX),
                        ));
                    }
                    if let Some(tx) = routes.get(&ev.call_id) {
                        let _ = tx.send(ev);
                    }
                }
                K_SESSION_CLOSED => observed
                    .lock()
                    .unwrap()
                    .sessions_closed
                    .push(ev.session_key),
                K_CONN_CLOSED => observed.lock().unwrap().connections_closed.push((
                    ev.connection_id,
                    ev.peer.clone(),
                    ev.connection_handle,
                )),
                K_NON_RPC => {
                    observed
                        .lock()
                        .unwrap()
                        .non_rpc
                        .push((ev.request.clone(), ev.connection_handle));
                    // Owned by the binding.
                    unsafe { aster_connection_free(srv.rt, ev.connection_handle) };
                }
                other => panic!("unknown event kind {other}"),
            }
        }
    }
}

fn note(observed: &Arc<Mutex<Observed>>, s: impl Into<String>) {
    observed.lock().unwrap().notes.push(s.into());
}

fn send(srv: Srv, id: u64, payload: &[u8], required: &mut u64) -> i32 {
    unsafe {
        aster_dispatch_send(
            srv.rt,
            srv.server,
            id,
            payload.as_ptr(),
            payload.len() as u32,
            required,
        )
    }
}

fn respond(srv: Srv, id: u64, payload: &[u8], status: Option<&[u8]>) -> i32 {
    let mut required = 0;
    let (sp, sl) = status.map_or((ptr::null(), 0), |s| (s.as_ptr(), s.len() as u32));
    unsafe {
        aster_dispatch_respond(
            srv.rt,
            srv.server,
            id,
            payload.as_ptr(),
            payload.len() as u32,
            sp,
            sl,
            &mut required,
        )
    }
}

fn finish(srv: Srv, id: u64) -> i32 {
    let mut required = 0;
    loop {
        let rc =
            unsafe { aster_dispatch_finish(srv.rt, srv.server, id, ptr::null(), 0, &mut required) };
        if rc != QUEUE_FULL {
            return rc;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Send `payload`, parking on a reservation whenever the queue is full.
fn send_parked(srv: Srv, id: u64, payload: &[u8], events: &mpsc::Receiver<Ev>) -> i32 {
    loop {
        let mut required = 0;
        match send(srv, id, payload, &mut required) {
            QUEUE_FULL => {
                let rc = aster_dispatch_reserve(srv.rt, srv.server, id, required);
                if rc != OK {
                    return rc;
                }
                match events.recv_timeout(Duration::from_secs(20)) {
                    Ok(e) if e.kind == K_WRITABLE => continue,
                    _ => return CLOSED,
                }
            }
            rc => return rc,
        }
    }
}

/// `Some(payload)`, or `None` at the end of the request stream / call.
///
/// Polls with a short finite timeout, retrying on TIMEOUT, so every
/// streaming test also covers the finite-timeout path (which once panicked
/// by building its timer outside the runtime).
fn recv(srv: Srv, id: u64) -> Option<Vec<u8>> {
    let (mut p, mut l, mut b) = (ptr::null(), 0u32, 0u64);
    let rc = loop {
        let rc = unsafe { aster_dispatch_recv(srv.rt, srv.server, id, 20, &mut p, &mut l, &mut b) };
        if rc != aster_status_t::ASTER_STATUS_TIMEOUT as i32 {
            break rc;
        }
    };
    if rc != OK || b == 0 {
        return None;
    }
    let out = unsafe { copy(p, l) };
    assert_eq!(aster_dispatch_buffer_release(srv.rt, srv.server, b), OK);
    Some(out)
}

unsafe fn lease_kv(
    srv: Srv,
    id: u64,
    f: unsafe extern "C" fn(
        aster_runtime_t,
        aster_server_t,
        u64,
        *mut *const u8,
        *mut u32,
        *mut u64,
    ) -> i32,
) -> Vec<(String, String)> {
    let (mut p, mut l, mut b) = (ptr::null(), 0u32, 0u64);
    assert_eq!(f(srv.rt, srv.server, id, &mut p, &mut l, &mut b), OK);
    let out = decode_kv(&copy(p, l));
    assert_eq!(aster_dispatch_buffer_release(srv.rt, srv.server, b), OK);
    out
}

fn handle(srv: Srv, call: Ev, events: mpsc::Receiver<Ev>, observed: Arc<Mutex<Observed>>) {
    let id = call.call_id;
    match (call.service_idx, call.method_idx) {
        (ECHO, ECHO_UNARY) => {
            assert_ne!(call.flags & ASTER_DISPATCH_FLAG_FIRST_IS_END, 0);
            assert_ne!(call.flags & ASTER_DISPATCH_FLAG_IROH_AUTHENTICATED, 0);
            assert_eq!(call.flags & ASTER_DISPATCH_FLAG_HAS_METADATA, 0);
            assert_eq!(call.session_key, 0);
            assert_eq!(call.pattern, 0);
            assert_ne!(call.connection_handle, 0);
            if call.request == b"fail" {
                let st = status_aev1(StatusCode::NotFound, "no such thing");
                assert_eq!(respond(srv, id, &[], Some(&st)), OK);
            } else if call.request.starts_with(b"race") {
                // unary_cancel_token_races_with_completion: the client may
                // have cancelled already.
                let rc = respond(srv, id, &call.request, None);
                assert!(rc == OK || rc == NOT_FOUND, "race respond: {rc}");
            } else {
                assert_eq!(respond(srv, id, &call.request, None), OK);
            }
            assert_eq!(respond(srv, id, b"x", None), NOT_FOUND, "terminal");
        }
        (ECHO, META) => {
            assert_ne!(call.flags & ASTER_DISPATCH_FLAG_HAS_METADATA, 0);
            let md = unsafe { lease_kv(srv, id, aster_dispatch_metadata) };
            let mut out = Vec::new();
            for (k, v) in md {
                out.extend(format!("{k}={v};").into_bytes());
            }
            assert_eq!(respond(srv, id, &out, None), OK);
        }
        (ECHO, COUNT) | (COUNTER, 1) => {
            assert_eq!(call.pattern, 1);
            let n = call.request[0];
            for i in 0..n {
                assert_eq!(
                    send_parked(srv, id, &[i, call.session_key as u8], &events),
                    OK
                );
            }
            assert_eq!(finish(srv, id), OK);
        }
        (ECHO, SUM) => {
            assert_eq!(call.flags & ASTER_DISPATCH_FLAG_FIRST_IS_END, 0);
            let mut total: u32 = call.request.iter().map(|b| *b as u32).sum();
            while let Some(more) = recv(srv, id) {
                total += more.iter().map(|b| *b as u32).sum::<u32>();
            }
            assert_eq!(respond(srv, id, &total.to_le_bytes(), None), OK);
        }
        (ECHO, CHAT) => {
            let mut next = Some(call.request);
            while let Some(msg) = next {
                let mut out = b"re:".to_vec();
                out.extend(msg);
                assert_eq!(send_parked(srv, id, &out, &events), OK);
                next = recv(srv, id);
            }
            assert_eq!(finish(srv, id), OK);
        }
        (ECHO, FLOOD) => {
            let mut seed = 7u64;
            let mut frame = noise(FLOOD_FRAME, seed);
            let mut sent = 0usize;
            let required = loop {
                let mut required = 0;
                match send(srv, id, &frame, &mut required) {
                    OK => sent += 1,
                    QUEUE_FULL => break required,
                    rc => panic!("flood: {rc}"),
                }
                seed += 1;
                frame = noise(FLOOD_FRAME, seed);
                assert!(sent < 100_000, "the response queue never filled");
            };
            assert!(required as usize >= FLOOD_FRAME);
            note(&observed, format!("flood:full:{sent}"));
            assert_eq!(aster_dispatch_reserve(srv.rt, srv.server, id, required), OK);
            assert_eq!(
                aster_dispatch_reserve(srv.rt, srv.server, id, required),
                INVALID,
                "one reservation per call"
            );
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(e) if e.kind == K_WRITABLE => assert_eq!(e.call_id, id),
                other => panic!("expected RESPONSE_WRITABLE, got {other:?}"),
            }
            let mut req = 0;
            assert_eq!(send(srv, id, &frame, &mut req), OK, "held permit fits");
            sent += 1;
            for _ in 1..FLOOD_AFTER_FULL {
                seed += 1;
                frame = noise(FLOOD_FRAME, seed);
                assert_eq!(send_parked(srv, id, &frame, &events), OK);
                sent += 1;
            }
            assert_eq!(finish(srv, id), OK);
            note(&observed, format!("flood:sent:{sent}"));
        }
        (ECHO, HANG) => {
            let mut req = 0;
            assert_eq!(send(srv, id, b"first", &mut req), OK);
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(e) if e.kind == K_CANCELLED => assert_eq!(e.reason, R_PEER),
                other => panic!("expected CANCELLED, got {other:?}"),
            }
            assert_eq!(aster_dispatch_is_cancelled(srv.rt, srv.server, id), 1);
            assert_eq!(send(srv, id, b"late", &mut req), NOT_FOUND);
            note(&observed, "hang:cancelled");
        }
        (ECHO, SLOW) => {
            assert!(call.deadline_unix_ms > 0);
            assert_eq!(aster_dispatch_is_cancelled(srv.rt, srv.server, id), 0);
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(e) if e.kind == K_CANCELLED => assert_eq!(e.reason, R_DEADLINE),
                other => panic!("expected CANCELLED(deadline), got {other:?}"),
            }
            assert_eq!(respond(srv, id, b"late", None), NOT_FOUND);
            note(&observed, "slow:expired");
        }
        (ECHO, DROP) => {
            assert_eq!(aster_dispatch_abandon(srv.rt, srv.server, id), OK);
            assert_eq!(aster_dispatch_abandon(srv.rt, srv.server, id), NOT_FOUND);
        }
        (ECHO, ADMIN) => {
            assert_ne!(call.flags & ASTER_DISPATCH_FLAG_HAS_ATTRIBUTES, 0);
            let attrs = unsafe { lease_kv(srv, id, aster_dispatch_attributes) };
            let role = attrs
                .iter()
                .find(|(k, _)| k == aster::rpc::ROLE_ATTRIBUTE)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            assert_eq!(respond(srv, id, role.as_bytes(), None), OK);
        }
        (ECHO, SMALL) => assert_eq!(respond(srv, id, &call.request, None), OK),
        (ECHO, PARK) => {
            // A unary handler that never answers on its own: it waits for the
            // client's cancel (reason PEER) and records the request's tag.
            // (Large requests carry a short tag before ':'.)
            let head = call.request.split(|b| *b == b':').next().unwrap();
            let tag = String::from_utf8(head.to_vec()).unwrap();
            note(&observed, format!("park:start:{tag}"));
            match events.recv_timeout(Duration::from_secs(20)) {
                // "dl*" calls also carry a deadline that races the cancel.
                Ok(e) if e.kind == K_CANCELLED && tag.starts_with("dl") => {
                    assert!(e.reason == R_PEER || e.reason == R_DEADLINE, "{}", e.reason)
                }
                Ok(e) if e.kind == K_CANCELLED => assert_eq!(e.reason, R_PEER),
                other => panic!("park: expected CANCELLED, got {other:?}"),
            }
            assert_eq!(aster_dispatch_is_cancelled(srv.rt, srv.server, id), 1);
            assert_eq!(respond(srv, id, b"late", None), NOT_FOUND);
            note(&observed, format!("park:cancelled:{tag}"));
        }
        (COUNTER, INCR) => {
            static COUNTS: Mutex<Option<HashMap<u64, u8>>> = Mutex::new(None);
            let mut counts = COUNTS.lock().unwrap();
            let counts = counts.get_or_insert_with(HashMap::new);
            assert_ne!(call.session_key, 0);
            assert_ne!(call.session_id, 0, "the wire session id");
            if call.flags & ASTER_DISPATCH_FLAG_NEW_SESSION != 0 {
                assert!(counts.insert(call.session_key, 0).is_none());
            }
            let n = counts
                .get_mut(&call.session_key)
                .expect("NEW_SESSION first");
            *n += 1;
            assert_eq!(respond(srv, id, &[*n], None), OK);
        }
        other => panic!("unexpected call {other:?}"),
    }
}

// ── harness ─────────────────────────────────────────────────────────────────

unsafe fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    assert_eq!(
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
        0
    );
    rt
}

unsafe fn start_node(rt: aster_runtime_t, alpns: &[&[u8]]) -> aster_node_t {
    let alpn_b: Vec<aster_bytes_t> = alpns.iter().map(|a| bytes(a)).collect();
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), 0);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.alpns = aster_bytes_list_t {
        items: alpn_b.as_ptr(),
        len: alpn_b.len(),
    };
    let mut op = 0;
    assert_eq!(aster_node_start(rt, &cfg, 0, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_NODE_CREATED)
        .handle
}

/// (node id hex, direct addresses), once the node has a loopback address.
unsafe fn addr_of(rt: aster_runtime_t, node: aster_node_t) -> (String, Vec<String>) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let mut buf = vec![0u8; 4096];
        let mut out: aster_node_addr_t = std::mem::zeroed();
        assert_eq!(
            aster_node_addr_info(rt, node, buf.as_mut_ptr(), buf.len(), &mut out),
            OK
        );
        let s = |b: &aster_bytes_t| {
            String::from_utf8(std::slice::from_raw_parts(b.ptr, b.len).to_vec()).unwrap()
        };
        let direct: Vec<String> = if out.direct_addresses.len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(out.direct_addresses.items, out.direct_addresses.len)
                .iter()
                .map(s)
                .collect()
        };
        if !direct.is_empty() || Instant::now() > end {
            return (s(&out.endpoint_id), direct);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

unsafe fn connect(
    rt: aster_runtime_t,
    client: aster_node_t,
    server_node: aster_node_t,
    alpn: &[u8],
) -> aster_connection_t {
    let (id, direct) = addr_of(rt, server_node);
    let direct_b: Vec<aster_bytes_t> = direct.iter().map(|d| bytes(d.as_bytes())).collect();
    let addr = aster_node_addr_t {
        endpoint_id: bytes(id.as_bytes()),
        relay_url: bytes(&[]),
        direct_addresses: aster_bytes_list_t {
            items: direct_b.as_ptr(),
            len: direct_b.len(),
        },
    };
    let cfg = aster_connect_config_t {
        struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
        flags: 0,
        node_id: bytes(id.as_bytes()),
        alpn: bytes(alpn),
        addr: &addr,
    };
    let mut op = 0;
    assert_eq!(aster_connect(rt, client, &cfg, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CONNECTED).handle
}

struct Setup {
    rt: aster_runtime_t,
    server_node: aster_node_t,
    srv: Srv,
    binding: Binding,
}

unsafe fn start_server() -> Setup {
    let rt = runtime();
    let server_node = start_node(rt, &[RPC_ALPN, ADMISSION_ALPN]);
    let mut builder = 0;
    assert_eq!(aster_server_builder_new(rt, server_node, &mut builder), OK);
    for (json, want) in [(echo_json(), ECHO), (counter_json(), COUNTER)] {
        let mut idx = u32::MAX;
        assert_eq!(
            aster_server_add_service_json(builder, json.as_ptr(), json.len() as u32, &mut idx),
            OK
        );
        assert_eq!(idx, want);
    }
    let mut server = 0;
    assert_eq!(aster_server_start(rt, builder, 64, &mut server), OK);
    let srv = Srv { rt, server };
    Setup {
        rt,
        server_node,
        srv,
        binding: Binding::start(srv),
    }
}

/// A client node connected to the server on `aster/1`.
unsafe fn client(s: &Setup) -> (aster_node_t, aster_connection_t) {
    let node = start_node(s.rt, &[RPC_ALPN]);
    let conn = connect(s.rt, node, s.server_node, RPC_ALPN);
    (node, conn)
}

unsafe fn teardown(s: Setup) {
    let Setup {
        rt, srv, binding, ..
    } = s;
    drop(binding);
    assert_eq!(aster_server_destroy(rt, srv.server), OK);
    assert_eq!(aster_server_destroy(rt, srv.server), NOT_FOUND);
    aster_runtime_close(rt);
}

struct Opts {
    service: String,
    method: String,
    metadata: Vec<u8>,
    session_id: u32,
    deadline_ms: u32,
}

fn echo(method: &str) -> Opts {
    Opts {
        service: "test.Echo".into(),
        method: method.into(),
        metadata: Vec::new(),
        session_id: 0,
        deadline_ms: 0,
    }
}

impl Opts {
    fn raw(&self) -> aster_call_options_t {
        aster_call_options_t {
            struct_size: std::mem::size_of::<aster_call_options_t>() as u32,
            version: 1,
            service: bytes(self.service.as_bytes()),
            method: bytes(self.method.as_bytes()),
            metadata: bytes(&self.metadata),
            session_id: self.session_id,
            deadline_ms: self.deadline_ms,
            serialization_mode: 0,
            wire_call_id: 0,
        }
    }
}

/// Unary through `aster_rpc_unary`: `Ok(response)` or `Err((code, message))`.
unsafe fn unary(
    rt: aster_runtime_t,
    conn: aster_connection_t,
    o: &Opts,
    req: &[u8],
) -> Result<Vec<u8>, (i32, String)> {
    let raw = o.raw();
    let (mut rp, mut rl, mut rb) = (ptr::null(), 0u32, 0u64);
    let (mut code, mut sp, mut sl, mut sb) = (0i32, ptr::null(), 0u32, 0u64);
    let rc = aster_rpc_unary(
        rt,
        conn,
        &raw,
        req.as_ptr(),
        req.len() as u32,
        &mut rp,
        &mut rl,
        &mut rb,
        &mut code,
        &mut sp,
        &mut sl,
        &mut sb,
    );
    assert_eq!(rc, OK, "aster_rpc_unary");
    let resp = copy(rp, rl);
    let st = copy(sp, sl);
    aster_call_buffer_release(rt, rb);
    aster_call_buffer_release(rt, sb);
    if code == 0 {
        assert!(st.is_empty());
        Ok(resp)
    } else {
        let status = aster::rpc::codec::decode_rpc_status(&st).unwrap();
        assert_eq!(status.code, code);
        Err((code, status.message))
    }
}

/// Unary through `aster_rpc_unary_cancellable` with `token`: the call's
/// return code, then `Ok(response)` / `Err((code, message))` when it is `OK`.
unsafe fn unary_cancellable(
    rt: aster_runtime_t,
    conn: aster_connection_t,
    o: &Opts,
    token: aster_cancel_token_t,
    req: &[u8],
) -> (i32, Result<Vec<u8>, (i32, String)>) {
    let raw = o.raw();
    let (mut rp, mut rl, mut rb) = (ptr::null(), 0u32, 0u64);
    let (mut code, mut sp, mut sl, mut sb) = (0i32, ptr::null(), 0u32, 0u64);
    let rc = aster_rpc_unary_cancellable(
        rt,
        conn,
        &raw,
        token,
        req.as_ptr(),
        req.len() as u32,
        &mut rp,
        &mut rl,
        &mut rb,
        &mut code,
        &mut sp,
        &mut sl,
        &mut sb,
    );
    if rc != OK {
        return (rc, Err((-1, String::new())));
    }
    let resp = copy(rp, rl);
    let st = copy(sp, sl);
    aster_call_buffer_release(rt, rb);
    aster_call_buffer_release(rt, sb);
    if code == 0 {
        assert!(st.is_empty());
        (rc, Ok(resp))
    } else {
        let status = aster::rpc::codec::decode_rpc_status(&st).unwrap();
        assert_eq!(status.code, code);
        (rc, Err((code, status.message)))
    }
}

fn new_token(rt: aster_runtime_t) -> aster_cancel_token_t {
    let mut t = 0;
    assert_eq!(unsafe { aster_cancel_token_new(rt, &mut t) }, OK);
    assert_ne!(t, 0);
    t
}

unsafe fn open(
    rt: aster_runtime_t,
    conn: aster_connection_t,
    pattern: aster_method_pattern_t,
    o: &Opts,
) -> aster_call_t {
    let raw = o.raw();
    let mut call = 0;
    assert_eq!(aster_rpc_open(rt, conn, pattern as u8, &raw, &mut call), OK);
    call
}

unsafe fn send_payload(rt: aster_runtime_t, call: aster_call_t, p: &[u8], end: bool) -> i32 {
    aster_call_send_payload(rt, call, p.as_ptr(), p.len() as u32, end as u8)
}

#[derive(Debug, PartialEq)]
enum Item {
    Data(Vec<u8>),
    Trailer(i32, String),
    Timeout,
}

unsafe fn recv_item(rt: aster_runtime_t, call: aster_call_t, timeout_ms: u32) -> Item {
    let (mut p, mut l, mut b, mut t) = (ptr::null(), 0u32, 0u64, 0u8);
    let rc = aster_call_recv_payload(rt, call, timeout_ms, &mut p, &mut l, &mut b, &mut t);
    if rc == TIMEOUT {
        return Item::Timeout;
    }
    assert_eq!(rc, OK, "aster_call_recv_payload");
    let bytes = copy(p, l);
    aster_call_buffer_release(rt, b);
    if t == 1 {
        let st = aster::rpc::codec::decode_rpc_status(&bytes).unwrap();
        Item::Trailer(st.code, st.message)
    } else {
        Item::Data(bytes)
    }
}

/// Every data item until the trailer; the trailer's code.
unsafe fn collect(rt: aster_runtime_t, call: aster_call_t) -> (Vec<Vec<u8>>, i32) {
    let mut out = Vec::new();
    loop {
        match recv_item(rt, call, 30_000) {
            Item::Data(d) => out.push(d),
            Item::Trailer(code, _) => return (out, code),
            Item::Timeout => panic!("timed out reading a stream"),
        }
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[test]
fn all_four_patterns_through_the_c_abi() {
    unsafe {
        let s = start_server();
        let rt = s.rt;
        let (_client, conn) = client(&s);

        // Unary, plain and compressible-large (Rust inflates / deflates).
        assert_eq!(
            unary(rt, conn, &echo("echo"), b"hello"),
            Ok(b"hello".to_vec())
        );
        let big = vec![b'z'; 64 * 1024];
        assert_eq!(unary(rt, conn, &echo("echo"), &big), Ok(big.clone()));
        // An error status passes through, message intact.
        assert_eq!(
            unary(rt, conn, &echo("echo"), b"fail"),
            Err((StatusCode::NotFound.as_i32(), "no such thing".into()))
        );
        // Metadata reaches the handler (k/v lists both ways).
        let mut meta = echo("meta");
        meta.metadata = kv(&[("a", "1"), ("trace", "xyz")]);
        assert_eq!(unary(rt, conn, &meta, &[0]), Ok(b"a=1;trace=xyz;".to_vec()));

        // Server streaming: the request goes with the header on send.
        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_SERVER_STREAM,
            &echo("count"),
        );
        assert_eq!(
            aster_call_recv_payload(rt, call, 0, &mut ptr::null(), &mut 0, &mut 0, &mut 0),
            INVALID,
            "recv before the request"
        );
        assert_eq!(send_payload(rt, call, &[5], true), OK);
        assert_eq!(send_payload(rt, call, &[5], true), INVALID, "one request");
        let (items, code) = collect(rt, call);
        assert_eq!((items.len(), code), (5, 0));
        assert_eq!(items[4], vec![4, 0]);
        assert_eq!(
            aster_call_recv_payload(rt, call, 0, &mut ptr::null(), &mut 0, &mut 0, &mut 0),
            CLOSED,
            "after the trailer"
        );
        assert_eq!(aster_call_release(rt, call), OK);

        // Client streaming, incremental.
        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_CLIENT_STREAM,
            &echo("sum"),
        );
        assert_eq!(send_payload(rt, call, &[1, 2], false), OK);
        assert_eq!(send_payload(rt, call, &[3], false), OK);
        assert_eq!(send_payload(rt, call, &[4, 5], true), OK);
        let (items, code) = collect(rt, call);
        assert_eq!(code, 0);
        assert_eq!(u32::from_le_bytes(items[0].clone().try_into().unwrap()), 15);
        assert_eq!(aster_call_release(rt, call), OK);

        // Client streaming, batched: one call frames them all.
        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_CLIENT_STREAM,
            &echo("sum"),
        );
        let parts: [&[u8]; 3] = [&[1, 2], &[3], &[4, 5, 6]];
        let descs: Vec<aster_bytes_t> = parts.iter().map(|p| bytes(p)).collect();
        assert_eq!(
            aster_call_send_payloads(rt, call, descs.as_ptr(), descs.len() as u32, 1),
            OK
        );
        let (items, code) = collect(rt, call);
        assert_eq!(code, 0);
        assert_eq!(u32::from_le_bytes(items[0].clone().try_into().unwrap()), 21);
        assert_eq!(aster_call_release(rt, call), OK);

        // Bidi, interleaved; an empty end-of-stream send finishes.
        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_BIDI_STREAM,
            &echo("chat"),
        );
        for word in ["a", "bb", "ccc"] {
            assert_eq!(send_payload(rt, call, word.as_bytes(), false), OK);
            assert_eq!(
                recv_item(rt, call, 30_000),
                Item::Data(format!("re:{word}").into_bytes())
            );
        }
        assert_eq!(
            send_payload(rt, call, &[], false),
            INVALID,
            "empty, not end"
        );
        assert_eq!(send_payload(rt, call, &[], true), OK);
        assert_eq!(recv_item(rt, call, 30_000), Item::Trailer(0, String::new()));
        assert_eq!(aster_call_release(rt, call), OK);

        // Refusals decided in Rust never reach the binding.
        assert_eq!(
            unary(rt, conn, &echo("nope"), &[1]).unwrap_err().0,
            StatusCode::Unimplemented.as_i32()
        );
        let mut shared_sid = echo("echo");
        shared_sid.session_id = 7;
        assert_eq!(
            unary(rt, conn, &shared_sid, &[1]).unwrap_err().0,
            StatusCode::FailedPrecondition.as_i32()
        );
        // Baseline NodeInfo is served in Rust (it answers; the binding never
        // sees it).
        let node_info = Opts {
            service: "aster.ops.NodeInfo".into(),
            method: "describe".into(),
            metadata: Vec::new(),
            session_id: 0,
            deadline_ms: 0,
        };
        if let Err((code, msg)) = unary(rt, conn, &node_info, &[]) {
            assert!(
                code != StatusCode::Unimplemented.as_i32() && code != StatusCode::NotFound.as_i32(),
                "NodeInfo is registered: {code} {msg}"
            );
        }

        s.binding.assert_table_drains();
        assert_eq!(
            s.binding.observed().calls.len(),
            8,
            "refusals never reach it"
        );
        teardown(s);
    }
}

#[test]
fn sessions_follow_the_connection_rules_and_close_with_it() {
    unsafe {
        let s = start_server();
        let rt = s.rt;
        let (_ca, a) = client(&s);
        let (cb, b) = client(&s);
        let incr = |sid: u32| Opts {
            service: "test.Counter".into(),
            method: "incr".into(),
            metadata: Vec::new(),
            session_id: sid,
            deadline_ms: 0,
        };
        // Ids from the shared per-connection counter.
        let (mut a1, mut a2, mut b1) = (0u32, 0u32, 0u32);
        assert_eq!(aster_connection_next_session_id(rt, a, &mut a1), OK);
        assert_eq!(aster_connection_next_session_id(rt, a, &mut a2), OK);
        assert_eq!(aster_connection_next_session_id(rt, b, &mut b1), OK);
        assert!(a1 > 0 && a2 > a1);

        for expect in 1..=3u8 {
            assert_eq!(unary(rt, a, &incr(a1), &[0]), Ok(vec![expect]));
        }
        assert_eq!(unary(rt, a, &incr(a2), &[0]), Ok(vec![1]));
        assert_eq!(unary(rt, b, &incr(b1), &[0]), Ok(vec![1]));
        assert_eq!(unary(rt, a, &incr(a1), &[0]), Ok(vec![4]));
        // A streaming call on a session carries its key.
        let mut count = incr(a2);
        count.method = "count".into();
        let call = open(
            rt,
            a,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_SERVER_STREAM,
            &count,
        );
        assert_eq!(send_payload(rt, call, &[2], true), OK);
        assert_eq!(collect(rt, call).0.len(), 2);
        assert_eq!(aster_call_release(rt, call), OK);

        assert_eq!(
            unary(rt, a, &incr(0), &[0]).unwrap_err().0,
            StatusCode::FailedPrecondition.as_i32(),
            "SESSION without a session id"
        );
        let opened = s.binding.observed().sessions_opened.clone();
        assert_eq!(opened.len(), 3, "NEW_SESSION once per session");

        // Closing A reaps its two sessions, then reports the close.
        assert_eq!(aster_connection_close(rt, a, 0, bytes(b"bye")), OK);
        s.binding.until("A's sessions closed", |o| {
            o.sessions_closed.len() == 2 && !o.connections_closed.is_empty()
        });
        {
            let o = s.binding.observed();
            assert!(o.sessions_closed.iter().all(|k| opened.contains(k)));
            let (_, peer, handle) = &o.connections_closed[0];
            assert_eq!(peer.len(), 64, "peer id on CONNECTION_CLOSED");
            assert_ne!(*handle, 0);
            assert!(
                debug_connection(rt, *handle).is_none(),
                "the borrowed handle is freed on close"
            );
        }
        // A's handle is gone with it; B's session is untouched.
        let o = echo("echo").raw();
        let mut call = 0;
        assert_eq!(aster_rpc_open(rt, a, 2, &o, &mut call), NOT_FOUND);
        assert_eq!(unary(rt, b, &incr(b1), &[0]), Ok(vec![2]));

        // B's whole node goes away: its connection handle stays registered,
        // and a call on it reports UNAVAILABLE; the server reaps B.
        let mut op = 0;
        assert_eq!(aster_node_close(rt, cb, 0, &mut op), OK);
        common::wait_for_operation(rt, op);
        let (code, msg) = unary(rt, b, &incr(b1), &[0]).unwrap_err();
        assert_eq!(code, StatusCode::Unavailable.as_i32(), "{msg}");
        s.binding.until("B closed", |o| {
            o.sessions_closed.len() == 3 && o.connections_closed.len() == 2
        });

        s.binding.assert_table_drains();
        teardown(s);
    }
}

#[test]
fn client_cancel_reaches_the_handler_and_ends_the_stream() {
    unsafe {
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);

        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_SERVER_STREAM,
            &echo("hang"),
        );
        assert_eq!(send_payload(rt, call, &[1], true), OK);
        assert_eq!(recv_item(rt, call, 30_000), Item::Data(b"first".to_vec()));
        // A read times out and resumes; then a blocked read is cancelled
        // from another thread.
        assert_eq!(recv_item(rt, call, 50), Item::Timeout);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(aster_call_cancel(rt, call), OK);
        });
        assert_eq!(
            recv_item(rt, call, ASTER_TIMEOUT_INFINITE),
            Item::Trailer(StatusCode::Cancelled.as_i32(), "call cancelled".into())
        );
        canceller.join().unwrap();
        assert_eq!(send_payload(rt, call, &[1], true), CLOSED);
        assert_eq!(aster_call_release(rt, call), OK);
        assert_eq!(aster_call_release(rt, call), NOT_FOUND);

        s.binding.until("hang cancelled", |o| {
            o.notes.iter().any(|n| n == "hang:cancelled")
        });
        assert!(s
            .binding
            .observed()
            .cancelled
            .iter()
            .any(|(_, r, m)| *r == R_PEER && *m == HANG));
        s.binding.assert_table_drains();

        // Releasing an unfinished call cancels it too.
        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_SERVER_STREAM,
            &echo("hang"),
        );
        assert_eq!(send_payload(rt, call, &[1], true), OK);
        assert_eq!(recv_item(rt, call, 30_000), Item::Data(b"first".to_vec()));
        assert_eq!(aster_call_release(rt, call), OK);
        s.binding.until("second hang cancelled", |o| {
            o.notes.iter().filter(|n| *n == "hang:cancelled").count() == 2
        });

        // The connection survives.
        assert_eq!(
            unary(rt, conn, &echo("echo"), b"after"),
            Ok(b"after".to_vec())
        );
        s.binding.assert_table_drains();
        teardown(s);
    }
}

const CANCELLED_CODE: i32 = 1;
const DEADLINE_CODE: i32 = 4;
const UNSUPPORTED: i32 = aster_status_t::ASTER_STATUS_UNSUPPORTED as i32;

/// Cancel-token tests run one at a time and each ends with an empty token
/// table (the table is process-global).
static TOKEN_TESTS: Mutex<()> = Mutex::new(());

fn token_test() -> std::sync::MutexGuard<'static, ()> {
    let g = TOKEN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        rpc::live_cancel_tokens_for_tests(),
        0,
        "tokens left by an earlier test"
    );
    g
}

fn assert_tokens_drained() {
    assert_eq!(
        rpc::live_cancel_tokens_for_tests(),
        0,
        "a cancel token leaked"
    );
}

fn parked(o: &Observed, what: &str, tag: &str) -> bool {
    o.notes.iter().any(|n| *n == format!("park:{what}:{tag}"))
}

/// ABI 4.5: `aster_cancel_token_cancel` aborts an in-flight pooled unary
/// call like dropping the Rust future: the blocked call returns CANCELLED,
/// the handler sees a PEER cancel, and the connection's pool keeps working.
#[test]
fn unary_cancel_token_aborts_the_pooled_call() {
    unsafe {
        let _serial = token_test();
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);

        // A call that completes: the token is consumed, a late cancel is a
        // no-op (NOT_FOUND), and so is releasing it.
        let t = new_token(rt);
        assert_eq!(
            unary_cancellable(rt, conn, &echo("echo"), t, b"done"),
            (OK, Ok(b"done".to_vec()))
        );
        assert_eq!(aster_cancel_token_cancel(rt, t), NOT_FOUND);
        assert_eq!(aster_cancel_token_release(rt, t), NOT_FOUND);
        // Failures keep their status on the cancellable path.
        let t = new_token(rt);
        assert_eq!(
            unary_cancellable(rt, conn, &echo("echo"), t, b"fail"),
            (
                OK,
                Err((StatusCode::NotFound.as_i32(), "no such thing".into()))
            )
        );
        // Token 0 is the plain fast path.
        assert_eq!(
            unary_cancellable(rt, conn, &echo("echo"), 0, b"zero"),
            (OK, Ok(b"zero".to_vec()))
        );

        // Cancelled before the call starts: CANCELLED without sending.
        let t = new_token(rt);
        assert_eq!(aster_cancel_token_cancel(rt, t), OK);
        assert_eq!(aster_cancel_token_cancel(rt, t), OK, "idempotent");
        assert_eq!(
            unary_cancellable(rt, conn, &echo("park"), t, b"early"),
            (OK, Err((CANCELLED_CODE, "call cancelled".into())))
        );
        assert_eq!(aster_cancel_token_release(rt, t), NOT_FOUND, "consumed");

        // Cancelled in flight, from another thread, while the handler runs.
        let t = new_token(rt);
        let observed = s.binding.observed.clone();
        let canceller = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(20);
            while !parked(&observed.lock().unwrap(), "start", "a") {
                assert!(Instant::now() < end, "park never started");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(aster_cancel_token_cancel(rt, t), OK);
        });
        let started = Instant::now();
        assert_eq!(
            unary_cancellable(rt, conn, &echo("park"), t, b"a"),
            (OK, Err((CANCELLED_CODE, "call cancelled".into())))
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        canceller.join().unwrap();
        s.binding
            .until("park a cancelled", |o| parked(o, "cancelled", "a"));
        assert!(!parked(&s.binding.observed(), "start", "early"));
        assert_eq!(aster_cancel_token_cancel(rt, t), NOT_FOUND);

        // Fill the SHARED pool (8 streams) and queue two more callers behind
        // it, then cancel all ten: parked calls, and calls still waiting for
        // a pooled stream. Every discarded stream gives its slot back.
        const N: usize = 10;
        let tokens: Vec<_> = (0..N).map(|_| new_token(rt)).collect();
        let calls: Vec<_> = tokens
            .iter()
            .enumerate()
            .map(|(i, &t)| {
                std::thread::spawn(move || {
                    unary_cancellable(rt, conn, &echo("park"), t, format!("p{i}").as_bytes())
                })
            })
            .collect();
        s.binding.until("eight parked", |o| {
            (0..N)
                .filter(|i| parked(o, "start", &format!("p{i}")))
                .count()
                >= 8
        });
        for &t in &tokens {
            assert_eq!(aster_cancel_token_cancel(rt, t), OK);
        }
        for c in calls {
            assert_eq!(
                c.join().unwrap(),
                (OK, Err((CANCELLED_CODE, "call cancelled".into())))
            );
        }
        s.binding.until("parked handlers cancelled", |o| {
            (0..N)
                .filter(|i| parked(o, "start", &format!("p{i}")))
                .all(|i| parked(o, "cancelled", &format!("p{i}")))
        });
        // The pool is whole again: sixteen calls at once all complete.
        let calls: Vec<_> = (0..16)
            .map(|i| {
                std::thread::spawn(move || {
                    let t = new_token(rt);
                    let msg = format!("after{i}");
                    assert_eq!(
                        unary_cancellable(rt, conn, &echo("echo"), t, msg.as_bytes()),
                        (OK, Ok(msg.into_bytes()))
                    );
                })
            })
            .collect();
        for c in calls {
            c.join().unwrap();
        }
        assert_eq!(
            unary(rt, conn, &echo("echo"), b"plain"),
            Ok(b"plain".to_vec())
        );
        s.binding.assert_table_drains();
        assert_tokens_drained();
        teardown(s);
    }
}

/// Cancels racing completion: every call ends with exactly one outcome
/// (its response or CANCELLED), none hangs, and the connection survives.
#[test]
fn unary_cancel_token_races_with_completion() {
    unsafe {
        let _serial = token_test();
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);
        let (mut ok, mut cancelled) = (0, 0);
        for i in 0..200u64 {
            let t = new_token(rt);
            let delay = Duration::from_micros((i * 37) % 400);
            let canceller = std::thread::spawn(move || {
                std::thread::sleep(delay);
                aster_cancel_token_cancel(rt, t)
            });
            let msg = format!("race{i}");
            match unary_cancellable(rt, conn, &echo("echo"), t, msg.as_bytes()) {
                (OK, Ok(r)) => {
                    assert_eq!(r, msg.as_bytes());
                    ok += 1;
                }
                (OK, Err((CANCELLED_CODE, m))) => {
                    assert_eq!(m, "call cancelled");
                    cancelled += 1;
                }
                other => panic!("call {i}: {other:?}"),
            }
            let rc = canceller.join().unwrap();
            assert!(rc == OK || rc == NOT_FOUND, "cancel: {rc}");
            assert_eq!(aster_cancel_token_release(rt, t), NOT_FOUND);
        }
        assert_eq!(ok + cancelled, 200);
        assert_eq!(
            unary(rt, conn, &echo("echo"), b"after"),
            Ok(b"after".to_vec())
        );
        s.binding.assert_table_drains();
        assert_tokens_drained();
        teardown(s);
    }
}

#[test]
fn unary_cancel_token_argument_rules() {
    unsafe {
        let _serial = token_test();
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);
        assert_eq!(aster_cancel_token_new(rt, ptr::null_mut()), INVALID);
        // Unknown token: refused, nothing sent.
        assert_eq!(
            unary_cancellable(rt, conn, &echo("echo"), u64::MAX - 7, b"x").0,
            INVALID
        );
        // A released token is gone.
        let t = new_token(rt);
        assert_eq!(aster_cancel_token_release(rt, t), OK);
        assert_eq!(aster_cancel_token_release(rt, t), NOT_FOUND);
        assert_eq!(aster_cancel_token_cancel(rt, t), NOT_FOUND);
        assert_eq!(
            unary_cancellable(rt, conn, &echo("echo"), t, b"x").0,
            INVALID
        );
        // Argument errors still consume the token.
        let t = new_token(rt);
        assert_eq!(
            unary_cancellable(rt, 0xdead, &echo("echo"), t, b"x").0,
            NOT_FOUND
        );
        assert_eq!(aster_cancel_token_cancel(rt, t), NOT_FOUND);
        // One token, one call: a second call with a token in use is refused
        // and leaves the first call cancellable.
        let t = new_token(rt);
        let first =
            std::thread::spawn(move || unary_cancellable(rt, conn, &echo("park"), t, b"owner"));
        s.binding
            .until("owner parked", |o| parked(o, "start", "owner"));
        assert_eq!(
            unary_cancellable(rt, conn, &echo("echo"), t, b"x").0,
            INVALID
        );
        // Nor can it be released under the call: still cancellable after.
        assert_eq!(aster_cancel_token_release(rt, t), INVALID);
        assert_eq!(aster_cancel_token_cancel(rt, t), OK);
        assert_eq!(
            first.join().unwrap(),
            (OK, Err((CANCELLED_CODE, "call cancelled".into())))
        );
        s.binding
            .until("owner cancelled", |o| parked(o, "cancelled", "owner"));
        s.binding.assert_table_drains();
        assert_tokens_drained();
        teardown(s);
    }
}

/// Every `park` handler whose tag starts with `prefix` that started has seen
/// its cancel.
fn parked_all_cancelled(o: &Observed, prefix: &str) -> bool {
    let start = format!("park:start:{prefix}");
    o.notes
        .iter()
        .filter_map(|n| n.strip_prefix("park:start:"))
        .filter(|tag| format!("park:start:{tag}").starts_with(&start))
        .all(|tag| parked(o, "cancelled", tag))
}

/// A blocking unary call from a tokio runtime thread is refused
/// (`UNSUPPORTED`) instead of panicking across `extern "C"`.
#[test]
fn unary_from_a_runtime_thread_is_refused() {
    unsafe {
        let _serial = token_test();
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let t = new_token(rt);
        let (plain, cancellable) = tokio_rt.block_on(async {
            let raw = echo("echo").raw();
            let (mut rp, mut rl, mut rb) = (ptr::null(), 0u32, 0u64);
            let (mut code, mut sp, mut sl, mut sb) = (0i32, ptr::null(), 0u32, 0u64);
            let plain = aster_rpc_unary(
                rt,
                conn,
                &raw,
                b"x".as_ptr(),
                1,
                &mut rp,
                &mut rl,
                &mut rb,
                &mut code,
                &mut sp,
                &mut sl,
                &mut sb,
            );
            (plain, unary_cancellable(rt, conn, &echo("echo"), t, b"x").0)
        });
        assert_eq!(plain, UNSUPPORTED);
        assert_eq!(cancellable, UNSUPPORTED);
        assert_eq!(aster_cancel_token_cancel(rt, t), NOT_FOUND, "consumed");
        // Off the runtime thread the same calls work.
        assert_eq!(unary(rt, conn, &echo("echo"), b"ok"), Ok(b"ok".to_vec()));
        s.binding.assert_table_drains();
        assert_tokens_drained();
        teardown(s);
    }
}

/// A cancel racing the call's own (client-side) deadline: one outcome,
/// CANCELLED or DEADLINE_EXCEEDED, and the handler sees the call end.
#[test]
fn unary_cancel_token_races_the_deadline() {
    unsafe {
        let _serial = token_test();
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);
        let mut o = echo("park");
        o.deadline_ms = 150;
        let (mut cancelled, mut expired) = (0, 0);
        for i in 0..24u64 {
            let t = new_token(rt);
            let delay = Duration::from_millis(130 + (i * 3) % 40);
            let canceller = std::thread::spawn(move || {
                std::thread::sleep(delay);
                aster_cancel_token_cancel(rt, t)
            });
            match unary_cancellable(rt, conn, &o, t, format!("dl{i}").as_bytes()) {
                (OK, Err((CANCELLED_CODE, _))) => cancelled += 1,
                (OK, Err((DEADLINE_CODE, _))) => expired += 1,
                other => panic!("call {i}: {other:?}"),
            }
            let rc = canceller.join().unwrap();
            assert!(rc == OK || rc == NOT_FOUND, "cancel: {rc}");
        }
        assert_eq!(cancelled + expired, 24);
        s.binding
            .until("dl handlers ended", |o| parked_all_cancelled(o, "dl"));
        assert_eq!(
            unary(rt, conn, &echo("echo"), b"after"),
            Ok(b"after".to_vec())
        );
        s.binding.assert_table_drains();
        assert_tokens_drained();
        teardown(s);
    }
}

/// Cancels landing while the pool is still opening its first streams (a
/// fresh connection: the acquire opens a new stream for each call), at
/// staggered moments from "before the call started" to "parked".
#[test]
fn unary_cancel_token_while_the_pool_opens_streams() {
    unsafe {
        let _serial = token_test();
        let s = start_server();
        let rt = s.rt;
        for k in 0..6u64 {
            let (_c, conn) = client(&s);
            let tokens: Vec<_> = (0..4).map(|_| new_token(rt)).collect();
            let calls: Vec<_> = tokens
                .iter()
                .enumerate()
                .map(|(j, &t)| {
                    std::thread::spawn(move || {
                        unary_cancellable(
                            rt,
                            conn,
                            &echo("park"),
                            t,
                            format!("e{k}_{j}").as_bytes(),
                        )
                    })
                })
                .collect();
            for (j, &t) in tokens.iter().enumerate() {
                std::thread::sleep(Duration::from_micros(150 * (j as u64 + k)));
                assert_eq!(aster_cancel_token_cancel(rt, t), OK);
            }
            for c in calls {
                assert_eq!(
                    c.join().unwrap(),
                    (OK, Err((CANCELLED_CODE, "call cancelled".into())))
                );
            }
            // The pool recovers on that connection.
            assert_eq!(
                unary(rt, conn, &echo("echo"), b"fresh"),
                Ok(b"fresh".to_vec())
            );
        }
        s.binding
            .until("e handlers ended", |o| parked_all_cancelled(o, "e"));
        s.binding.assert_table_drains();
        assert_tokens_drained();
        teardown(s);
    }
}

/// Cancels landing while a large request is still being written (the
/// pooled stream is then torn mid-frame, so the cancel is a reset only).
#[test]
fn unary_cancel_token_mid_write_of_a_large_request() {
    unsafe {
        let _serial = token_test();
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);
        for i in 0..8u64 {
            let mut req = format!("big{i}:").into_bytes();
            req.resize(8 * 1024 * 1024, b'x');
            let t = new_token(rt);
            let delay = Duration::from_micros(400 * i);
            let canceller = std::thread::spawn(move || {
                std::thread::sleep(delay);
                aster_cancel_token_cancel(rt, t)
            });
            assert_eq!(
                unary_cancellable(rt, conn, &echo("park"), t, &req),
                (OK, Err((CANCELLED_CODE, "call cancelled".into())))
            );
            assert_eq!(canceller.join().unwrap(), OK);
            // Each time the connection still serves calls.
            assert_eq!(
                unary(rt, conn, &echo("echo"), b"small"),
                Ok(b"small".to_vec())
            );
        }
        s.binding
            .until("big handlers ended", |o| parked_all_cancelled(o, "big"));
        s.binding.assert_table_drains();
        assert_tokens_drained();
        teardown(s);
    }
}

#[test]
fn deadlines_server_side_and_client_side() {
    unsafe {
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);

        // The method's default_timeout (0.3 s) expires the call in Rust.
        let (code, msg) = unary(rt, conn, &echo("slow"), &[1]).unwrap_err();
        assert_eq!(code, StatusCode::DeadlineExceeded.as_i32(), "{msg}");
        s.binding.until("slow expired", |o| {
            o.notes.iter().any(|n| n == "slow:expired")
        });
        assert!(s
            .binding
            .observed()
            .cancelled
            .iter()
            .any(|(_, r, m)| *r == R_DEADLINE && *m == SLOW));

        // A client deadline on a stream that never ends: DEADLINE_EXCEEDED,
        // and the server learns of it as a cancel.
        let mut hang = echo("hang");
        hang.deadline_ms = 300;
        let started = Instant::now();
        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_SERVER_STREAM,
            &hang,
        );
        assert_eq!(send_payload(rt, call, &[1], true), OK);
        let (items, code) = collect(rt, call);
        assert_eq!(items, vec![b"first".to_vec()]);
        assert_eq!(code, StatusCode::DeadlineExceeded.as_i32());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(aster_call_release(rt, call), OK);
        s.binding.until("hang ended", |o| {
            o.notes.iter().any(|n| n == "hang:cancelled")
        });

        s.binding.assert_table_drains();
        teardown(s);
    }
}

#[test]
fn queue_full_parks_on_a_reservation_until_writable() {
    unsafe {
        let s = start_server();
        let rt = s.rt;
        let (_c, conn) = client(&s);

        let call = open(
            rt,
            conn,
            aster_method_pattern_t::ASTER_METHOD_PATTERN_SERVER_STREAM,
            &echo("flood"),
        );
        assert_eq!(send_payload(rt, call, &[1], true), OK);
        s.binding.until("flood full", |o| {
            o.notes.iter().any(|n| n.starts_with("flood:full"))
        });
        let (items, code) = collect(rt, call);
        assert_eq!(code, 0);
        assert!(items.iter().all(|i| i.len() == FLOOD_FRAME));
        s.binding.until("flood done", |o| {
            o.notes.iter().any(|n| n.starts_with("flood:sent"))
        });
        let sent: usize = s
            .binding
            .observed()
            .notes
            .iter()
            .find_map(|n| n.strip_prefix("flood:sent:"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(items.len(), sent, "every frame arrived");
        assert_eq!(aster_call_release(rt, call), OK);
        s.binding.assert_table_drains();
        teardown(s);
    }
}

#[test]
fn abandon_gate3_and_frame_limits() {
    unsafe {
        let s = start_server();
        let rt = s.rt;
        let (client_node, conn) = client(&s);

        // Abandon answers INTERNAL.
        assert_eq!(
            unary(rt, conn, &echo("drop"), &[1]),
            Err((StatusCode::Internal.as_i32(), "handler dropped call".into()))
        );

        // Gate 3 in Rust; the attribute bridge admits.
        assert_eq!(
            unary(rt, conn, &echo("admin"), &[1]).unwrap_err().0,
            StatusCode::PermissionDenied.as_i32()
        );
        let (client_id, _) = addr_of(rt, client_node);
        let attrs = kv(&[(aster::rpc::ROLE_ATTRIBUTE, "admin")]);
        assert_eq!(
            aster_server_set_peer_attributes(
                s.srv.server,
                client_id.as_ptr(),
                client_id.len() as u32,
                attrs.as_ptr(),
                attrs.len() as u32
            ),
            OK
        );
        assert_eq!(unary(rt, conn, &echo("admin"), &[1]), Ok(b"admin".to_vec()));
        // Clearing them denies again.
        assert_eq!(
            aster_server_set_peer_attributes(
                s.srv.server,
                client_id.as_ptr(),
                client_id.len() as u32,
                ptr::null(),
                0
            ),
            OK
        );
        assert_eq!(
            unary(rt, conn, &echo("admin"), &[1]).unwrap_err().0,
            StatusCode::PermissionDenied.as_i32()
        );

        // Per-method frame limit (closes the "no frame-limit resolver" gap).
        assert_eq!(unary(rt, conn, &echo("small"), &[7; 512]), Ok(vec![7; 512]));
        assert!(unary(rt, conn, &echo("small"), &noise(4096, 3)).is_err());
        let small_calls = s
            .binding
            .observed()
            .calls
            .iter()
            .filter(|(sv, m, _)| *sv == ECHO && *m == SMALL)
            .count();
        assert_eq!(
            small_calls, 1,
            "the oversized call never reached the binding"
        );

        s.binding.assert_table_drains();
        teardown(s);
    }
}

#[test]
fn non_rpc_alpn_is_delivered_to_the_binding() {
    unsafe {
        let s = start_server();
        let rt = s.rt;
        let node = start_node(rt, &[RPC_ALPN]);
        let _conn = connect(rt, node, s.server_node, ADMISSION_ALPN);
        s.binding
            .until("non-rpc connection", |o| !o.non_rpc.is_empty());
        assert_eq!(s.binding.observed().non_rpc[0].0, ADMISSION_ALPN);
        teardown(s);
    }
}

#[test]
fn builder_and_argument_validation() {
    unsafe {
        let rt = runtime();
        let node = start_node(rt, &[RPC_ALPN]);
        let mut b = 0;
        assert_eq!(aster_server_builder_new(rt, 999_999, &mut b), NOT_FOUND);
        assert_eq!(aster_server_builder_new(rt, node, ptr::null_mut()), INVALID);
        assert_eq!(aster_server_builder_new(rt, node, &mut b), OK);

        let bad = br#"{"name":"x","version":1,"methods":[],"requirez":null}"#;
        assert_eq!(
            aster_server_add_service_json(b, bad.as_ptr(), bad.len() as u32, ptr::null_mut()),
            INVALID,
            "unknown keys are refused"
        );
        let json = echo_json();
        let mut idx = 9;
        assert_eq!(
            aster_server_add_service_json(b, json.as_ptr(), json.len() as u32, &mut idx),
            OK
        );
        assert_eq!(idx, 0);
        assert_eq!(
            aster_server_add_service_json(b, json.as_ptr(), json.len() as u32, &mut idx),
            INVALID,
            "duplicate (name, version)"
        );
        let opt = |k: aster_server_option_t, v| aster_server_set_option(b, k as u32, v);
        assert_eq!(
            opt(
                aster_server_option_t::ASTER_SERVER_OPTION_BUILTIN_NODE_INFO,
                2
            ),
            INVALID
        );
        assert_eq!(
            opt(
                aster_server_option_t::ASTER_SERVER_OPTION_BUILTIN_NODE_INFO,
                0
            ),
            OK
        );
        assert_eq!(
            opt(
                aster_server_option_t::ASTER_SERVER_OPTION_BUILTIN_TOPOLOGY,
                1
            ),
            OK
        );
        assert_eq!(
            opt(
                aster_server_option_t::ASTER_SERVER_OPTION_MAX_SESSIONS_PER_CONNECTION,
                0
            ),
            INVALID
        );
        assert_eq!(
            opt(
                aster_server_option_t::ASTER_SERVER_OPTION_HANDLER_TIMEOUT_MS,
                0
            ),
            OK
        );
        assert_eq!(aster_server_set_option(b, 99, 1), INVALID);

        // contract_id of the JSON form.
        let mut out: aster_bytes_t = std::mem::zeroed();
        assert_eq!(
            aster_dynamic_service_contract_id(json.as_ptr(), json.len() as u32, &mut out),
            OK
        );
        let id = String::from_utf8(std::slice::from_raw_parts(out.ptr, out.len).to_vec()).unwrap();
        assert_eq!(
            id,
            DynamicService::from_json(&json)
                .unwrap()
                .contract_id()
                .unwrap()
        );
        aster_string_release(out.ptr, out.len);

        let mut server = 0;
        assert_eq!(aster_server_start(rt, b, 0, &mut server), OK);
        assert_eq!(
            aster_server_start(rt, b, 0, &mut server),
            NOT_FOUND,
            "consumed"
        );
        assert_eq!(
            aster_server_set_option(b, 1, 1),
            NOT_FOUND,
            "the builder is gone"
        );
        // Operations on unknown calls.
        let mut req = 0;
        assert_eq!(
            aster_dispatch_respond(rt, server, 42, ptr::null(), 0, ptr::null(), 0, &mut req),
            NOT_FOUND
        );
        let garbage = [0xffu8; 3];
        assert_eq!(
            aster_dispatch_finish(rt, server, 42, garbage.as_ptr(), 3, &mut req),
            INVALID,
            "a malformed status is refused before the lookup"
        );
        assert_eq!(aster_dispatch_is_cancelled(rt, server, 42), 1);
        assert_eq!(aster_dispatch_buffer_release(rt, server, 0), OK);
        assert_eq!(aster_dispatch_buffer_release(rt, server, 12345), NOT_FOUND);
        let mut events: [aster_dispatch_event_t; 4] = std::mem::zeroed();
        assert_eq!(aster_server_poll(rt, server, events.as_mut_ptr(), 4, 0), 0);
        assert_eq!(aster_server_poll(rt, server, events.as_mut_ptr(), 4, 10), 0);

        // Client option validation.
        let (_c, conn) = {
            let client = start_node(rt, &[RPC_ALPN]);
            (client, connect(rt, client, node, RPC_ALPN))
        };
        let mut o = echo("echo").raw();
        o.struct_size = 8;
        let mut call = 0;
        assert_eq!(aster_rpc_open(rt, conn, 2, &o, &mut call), INVALID);
        let o = echo("echo").raw();
        assert_eq!(aster_rpc_open(rt, conn, 0, &o, &mut call), INVALID, "unary");
        assert_eq!(aster_rpc_open(rt, 777_777, 2, &o, &mut call), NOT_FOUND);
        let mut bad_md = echo("echo");
        bad_md.metadata = vec![1, 0, 0];
        let raw = bad_md.raw();
        assert_eq!(aster_rpc_open(rt, conn, 2, &raw, &mut call), INVALID);

        assert_eq!(aster_server_destroy(rt, server), OK);
        aster_runtime_close(rt);
    }
}
