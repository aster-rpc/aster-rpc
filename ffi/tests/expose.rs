//! ABI 4.2 (F5 expose) through the C functions. A "foreign" thread polls
//! `aster_upcall_poll`, decodes `C0` / `C4` / `C7` and encodes `C5` / `C6` /
//! `C8` / `CF` **by hand** from the layout in
//! `docs/_internal/bindings/upcall-design.md` §3 (the hand codec is checked
//! against `conformance/vectors/upcalls.json` first), and moves HTTP bodies
//! with `aster_http_body_*`. The consumer is a second node reached through
//! `aster_expose_relay_request` / `aster_expose_request_route`, over real QUIC.

use std::net::TcpListener;
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aster_transport_ffi::expose::*;
use aster_transport_ffi::node::{aster_config_init, aster_config_t, aster_node_start};
use aster_transport_ffi::upcall::*;
use aster_transport_ffi::*;

mod common;

const OK: i32 = 0;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const NOT_FOUND: i32 = aster_status_t::ASTER_STATUS_NOT_FOUND as i32;

const K_CANCEL: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_CANCEL as u16;
const K_AUTHORIZE: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_EXPOSE_AUTHORIZE as u16;
const K_HTTP: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_EXPOSE_HTTP as u16;
const K_ROUTES: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_EXPOSE_ROUTE_POLICY as u16;

const S_OK: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_OK as u32;
const S_ERROR: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_ERROR as u32;
const S_UNIMPLEMENTED: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_UNIMPLEMENTED as u32;

const FAIL_OPEN: u32 = aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_OPEN as u32;
const FAIL_CLOSED: u32 = aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED as u32;

const B_OK: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_OK as i32;
const B_UNKNOWN_STREAM: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_UNKNOWN_STREAM as i32;
const B_CANCELLED: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_CANCELLED as i32;
const B_TOO_LARGE: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_TOO_LARGE as i32;
const B_CHUNK_TOO_LARGE: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_CHUNK_TOO_LARGE as i32;
const B_FINISHED: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_FINISHED as i32;
const B_TIMEOUT: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_TIMEOUT as i32;
const B_INVALID_TRAILERS: i32 = aster_http_body_status_t::ASTER_HTTP_BODY_INVALID_TRAILERS as i32;
const B_BLOCKING_ON_RUNTIME: i32 =
    aster_http_body_status_t::ASTER_HTTP_BODY_BLOCKING_ON_RUNTIME as i32;

const ALPN: &[u8] = b"aster-expose/ffi-test";
const SVC: &str = "app";
const WAIT: Duration = Duration::from_secs(20);

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

// ============================================================================
// Hand-written UCv1 (design §3): the part of the codec an expose binding needs
// ============================================================================

struct Wr(Vec<u8>);

impl Wr {
    fn new(tag: u8) -> Self {
        Wr(vec![tag, 0x01])
    }
    fn varint(&mut self, mut v: u64) -> &mut Self {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(b);
                return self;
            }
            self.0.push(b | 0x80);
        }
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    /// `varint((utf8_len << 2) | 2) utf8`
    fn str(&mut self, s: &str) -> &mut Self {
        self.varint(((s.len() as u64) << 2) | 2);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    /// `varint(len) raw`
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.varint(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }
    /// `varint(count) 0x0C`, then the items.
    fn list(&mut self, n: usize) -> &mut Self {
        self.varint(n as u64);
        self.0.push(0x0c);
        self
    }
    fn headers(&mut self, h: &[(String, Vec<u8>)]) -> &mut Self {
        self.list(h.len());
        for (n, v) in h {
            self.str(n).bytes(v);
        }
        self
    }
    fn body(&mut self, b: &Option<Vec<u8>>) -> &mut Self {
        match b {
            Some(b) => self.u8(0).bytes(b),
            None => self.u8(1),
        }
    }
}

struct Rd<'a>(&'a [u8], usize);

impl<'a> Rd<'a> {
    fn new(buf: &'a [u8], tag: u8) -> Self {
        assert_eq!(&buf[..2], &[tag, 0x01], "tag/version");
        Rd(buf, 2)
    }
    fn u8(&mut self) -> u8 {
        self.1 += 1;
        self.0[self.1 - 1]
    }
    fn varint(&mut self) -> u64 {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = self.u8();
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return v;
            }
            shift += 7;
        }
    }
    fn str(&mut self) -> String {
        let h = self.varint();
        assert_eq!(h & 3, 2, "string header");
        let n = (h >> 2) as usize;
        let s = std::str::from_utf8(&self.0[self.1..self.1 + n]).unwrap();
        self.1 += n;
        s.to_owned()
    }
    fn bytes(&mut self) -> Vec<u8> {
        let n = self.varint() as usize;
        self.1 += n;
        self.0[self.1 - n..self.1].to_vec()
    }
    fn list(&mut self) -> usize {
        let n = self.varint() as usize;
        assert_eq!(self.u8(), 0x0c, "list marker");
        n
    }
    fn headers(&mut self) -> Vec<(String, Vec<u8>)> {
        (0..self.list())
            .map(|_| (self.str(), self.bytes()))
            .collect()
    }
    fn body(&mut self) -> Option<Vec<u8>> {
        match self.u8() {
            0 => Some(self.bytes()),
            1 => None,
            m => panic!("body mode {m}"),
        }
    }
    fn end(&self) {
        assert_eq!(self.1, self.0.len(), "trailing bytes");
    }
}

type Headers = Vec<(String, Vec<u8>)>;

fn hdrs(h: &[(&str, &str)]) -> Headers {
    h.iter()
        .map(|(n, v)| (n.to_string(), v.as_bytes().to_vec()))
        .collect()
}

/// `C4` ExposeHttpRequest. `body` None = streamed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Req {
    service_id: String,
    peer_id: Option<String>,
    stream_id: u64,
    version: u8,
    method: String,
    uri: String,
    headers: Headers,
    body: Option<Vec<u8>>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }
}

fn enc_c4(r: &Req) -> Vec<u8> {
    let mut w = Wr::new(0xC4);
    w.str(&r.service_id);
    match &r.peer_id {
        Some(p) => w.u8(0).str(p),
        None => w.u8(0xFD),
    };
    w.varint(r.stream_id)
        .u8(r.version)
        .str(&r.method)
        .str(&r.uri)
        .headers(&r.headers)
        .body(&r.body);
    w.0
}

fn dec_c4(buf: &[u8]) -> Req {
    let mut r = Rd::new(buf, 0xC4);
    let service_id = r.str();
    let peer_id = match r.u8() {
        0xFD => None,
        0 => Some(r.str()),
        m => panic!("opt marker {m}"),
    };
    let req = Req {
        service_id,
        peer_id,
        stream_id: r.varint(),
        version: r.u8(),
        method: r.str(),
        uri: r.str(),
        headers: r.headers(),
        body: r.body(),
    };
    r.end();
    req
}

/// `C5` ExposeHttpResponse.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Resp {
    status: u16,
    headers: Headers,
    body: Option<Vec<u8>>,
}

fn enc_c5(r: &Resp) -> Vec<u8> {
    let mut w = Wr::new(0xC5);
    w.varint(r.status as u64).headers(&r.headers).body(&r.body);
    w.0
}

fn dec_c5(buf: &[u8]) -> Resp {
    let mut r = Rd::new(buf, 0xC5);
    let resp = Resp {
        status: r.varint() as u16,
        headers: r.headers(),
        body: r.body(),
    };
    r.end();
    resp
}

/// `C6` ExposeHttpTrailers.
fn enc_c6(h: &Headers) -> Vec<u8> {
    let mut w = Wr::new(0xC6);
    w.headers(h);
    w.0
}

/// `C7` ExposeRoutePolicyRequest: (peer, metadata, [(host, port, proto, service)]).
type Specs = Vec<(String, u16, u8, String)>;

fn enc_c7(peer: &str, metadata: &[u8], specs: &Specs) -> Vec<u8> {
    let mut w = Wr::new(0xC7);
    w.str(peer).bytes(metadata).list(specs.len());
    for (host, port, proto, svc) in specs {
        w.str(host).varint(*port as u64).u8(*proto).str(svc);
    }
    w.0
}

fn dec_c7(buf: &[u8]) -> (String, Vec<u8>, Specs) {
    let mut r = Rd::new(buf, 0xC7);
    let peer = r.str();
    let metadata = r.bytes();
    let specs = (0..r.list())
        .map(|_| (r.str(), r.varint() as u16, r.u8(), r.str()))
        .collect();
    r.end();
    (peer, metadata, specs)
}

/// `C8` ExposeRouteGrant.
fn enc_c8(granted: &[u32]) -> Vec<u8> {
    let mut w = Wr::new(0xC8);
    w.list(granted.len());
    for g in granted {
        w.varint(*g as u64);
    }
    w.0
}

fn dec_c8(buf: &[u8]) -> Vec<u32> {
    let mut r = Rd::new(buf, 0xC8);
    let v = (0..r.list()).map(|_| r.varint() as u32).collect();
    r.end();
    v
}

/// `C0` ExposeAuthorizeRequest: (service, peer, metadata).
fn dec_c0(buf: &[u8]) -> (String, String, Vec<u8>) {
    let mut r = Rd::new(buf, 0xC0);
    let v = (r.str(), r.str(), r.bytes());
    r.end();
    v
}

/// `CF` Decision.
fn enc_cf(allowed: bool) -> Vec<u8> {
    vec![0xCF, 0x01, u8::from(allowed)]
}

#[test]
fn hand_codec_matches_the_vectors() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conformance/vectors/upcalls.json"
    );
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut seen = 0;
    for case in doc["valid"].as_array().unwrap() {
        let bytes = hex::decode(case["hex"].as_str().unwrap()).unwrap();
        let again = match bytes[0] {
            0xC0 => {
                let (s, p, m) = dec_c0(&bytes);
                let mut w = Wr::new(0xC0);
                w.str(&s).str(&p).bytes(&m);
                w.0
            }
            0xC4 => enc_c4(&dec_c4(&bytes)),
            0xC5 => enc_c5(&dec_c5(&bytes)),
            0xC6 => {
                let mut r = Rd::new(&bytes, 0xC6);
                let h = r.headers();
                r.end();
                enc_c6(&h)
            }
            0xC7 => {
                let (p, m, s) = dec_c7(&bytes);
                enc_c7(&p, &m, &s)
            }
            0xC8 => enc_c8(&dec_c8(&bytes)),
            0xCF => {
                assert_eq!(bytes.len(), 3);
                enc_cf(bytes[2] == 1)
            }
            _ => continue,
        };
        assert_eq!(again, bytes, "{}", case["name"]);
        seen += 1;
    }
    assert!(seen >= 10, "only {seen} expose vectors exercised");
}

// ============================================================================
// Harness
// ============================================================================

unsafe fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    let cfg = aster_runtime_config_t {
        struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
        worker_threads: 2,
        event_queue_capacity: 1024,
        reserved: 0,
    };
    assert_eq!(
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &cfg, &mut rt),
        OK
    );
    rt
}

unsafe fn start_node(rt: aster_runtime_t, alpns: &[&[u8]]) -> aster_node_t {
    let alpn_b: Vec<aster_bytes_t> = alpns.iter().map(|a| bytes(a)).collect();
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
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

unsafe fn node_id(rt: aster_runtime_t, node: aster_node_t) -> String {
    let mut buf = [0u8; 128];
    let mut len = 0usize;
    assert_eq!(
        aster_node_id(rt, node, buf.as_mut_ptr(), buf.len(), &mut len),
        OK
    );
    String::from_utf8(buf[..len].to_vec()).unwrap()
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
            String::from_utf8(slice::from_raw_parts(b.ptr, b.len).to_vec()).unwrap()
        };
        let direct: Vec<String> = if out.direct_addresses.len == 0 {
            Vec::new()
        } else {
            slice::from_raw_parts(out.direct_addresses.items, out.direct_addresses.len)
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

unsafe fn connect_start(
    rt: aster_runtime_t,
    client: aster_node_t,
    server: aster_node_t,
) -> aster_operation_t {
    let (id, direct) = addr_of(rt, server);
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
        alpn: bytes(ALPN),
        addr: &addr,
    };
    let mut op = 0;
    assert_eq!(aster_connect(rt, client, &cfg, 0, &mut op), OK);
    op
}

unsafe fn connect(
    rt: aster_runtime_t,
    client: aster_node_t,
    server: aster_node_t,
) -> aster_connection_t {
    let op = connect_start(rt, client, server);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CONNECTED).handle
}

/// Connect while the server accepts (`aster_accept`): both completions are
/// collected from one poll loop, since waiting for one discards the other.
unsafe fn connect_and_accept(
    rt: aster_runtime_t,
    client: aster_node_t,
    server: aster_node_t,
) -> (aster_connection_t, aster_connection_t) {
    let mut accept_op = 0;
    assert_eq!(aster_accept(rt, server, 0, &mut accept_op), OK);
    let connect_op = connect_start(rt, client, server);
    let (mut conn, mut accepted) = (0, 0);
    let end = Instant::now() + WAIT;
    while conn == 0 || accepted == 0 {
        assert!(Instant::now() < end, "connect/accept did not complete");
        let mut events: [aster_event_t; 8] = std::mem::zeroed();
        let n = aster_poll_events(rt, events.as_mut_ptr(), events.len(), 100);
        for ev in events.iter().take(n) {
            let ok = ev.status == 0;
            if ev.operation == connect_op {
                assert!(ok && ev.kind == aster_event_kind_t::ASTER_EVENT_CONNECTED as u32);
                conn = ev.handle;
            } else if ev.operation == accept_op {
                assert!(
                    ok && ev.kind == aster_event_kind_t::ASTER_EVENT_CONNECTION_ACCEPTED as u32
                );
                accepted = ev.handle;
            }
            if ev.buffer != 0 {
                aster_buffer_release(rt, ev.buffer);
            }
        }
    }
    (conn, accepted)
}

/// A finished operation: the event plus a copy of its payload (buffer freed).
struct Ev {
    ev: aster_event_t,
    data: Vec<u8>,
}

impl Ev {
    fn is_ok(&self, kind: aster_event_kind_t) -> bool {
        self.ev.kind == kind as u32 && self.ev.status == 0
    }
    fn is_error(&self) -> bool {
        self.ev.kind == aster_event_kind_t::ASTER_EVENT_ERROR as u32
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
    /// The `C5` of a `BYTES_RESULT`.
    fn resp(&self) -> Resp {
        assert!(
            self.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT),
            "expected BYTES_RESULT, got kind {} status {}: {}",
            self.ev.kind,
            self.ev.status,
            self.text()
        );
        dec_c5(&self.data)
    }
}

unsafe fn finish(rt: aster_runtime_t, op: aster_operation_t) -> Ev {
    let ev = common::wait_for_operation(rt, op);
    let data = if ev.data_len == 0 || ev.data_ptr.is_null() {
        Vec::new()
    } else {
        slice::from_raw_parts(ev.data_ptr, ev.data_len).to_vec()
    };
    if ev.buffer != 0 {
        aster_buffer_release(rt, ev.buffer);
    }
    Ev { ev, data }
}

/// What a foreign HTTP handler gets to work with (all plain integers: `Copy`).
#[derive(Clone, Copy)]
struct Ctx {
    rt: aster_runtime_t,
    upcalls: aster_upcalls_t,
    bodies: aster_http_bodies_t,
}

impl Ctx {
    fn respond(&self, id: u64, status: u32, payload: &[u8]) -> i32 {
        unsafe {
            aster_upcall_respond(
                self.rt,
                self.upcalls,
                id,
                status,
                if payload.is_empty() {
                    ptr::null()
                } else {
                    payload.as_ptr()
                },
                payload.len() as u32,
            )
        }
    }

    fn respond_http(&self, id: u64, status: u16, headers: &[(&str, &str)], body: Option<&[u8]>) {
        let c5 = enc_c5(&Resp {
            status,
            headers: hdrs(headers),
            body: body.map(<[u8]>::to_vec),
        });
        assert_eq!(self.respond(id, S_OK, &c5), OK);
    }

    /// One blocking read: `Ok(Some(n))` bytes in `buf`, `Ok(None)` = end.
    fn read(&self, stream: u64, buf: &mut [u8], timeout_ms: u32) -> Result<Option<usize>, i32> {
        let (mut n, mut eof) = (0usize, 0u32);
        let rc = unsafe {
            aster_http_body_read(
                self.rt,
                self.bodies,
                stream,
                buf.as_mut_ptr(),
                buf.len(),
                timeout_ms,
                &mut n,
                &mut eof,
            )
        };
        match (rc, eof) {
            (B_OK, 1) => {
                assert_eq!(n, 0);
                Ok(None)
            }
            (B_OK, _) => Ok(Some(n)),
            (rc, _) => Err(rc),
        }
    }

    /// Drain a streamed request body in `step`-byte reads.
    fn read_all(&self, stream: u64, step: usize) -> Result<Vec<u8>, i32> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; step];
        while let Some(n) = self.read(stream, &mut buf, 20_000)? {
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    fn write(&self, stream: u64, chunk: &[u8], timeout_ms: u32) -> i32 {
        unsafe {
            aster_http_body_write(
                self.rt,
                self.bodies,
                stream,
                chunk.as_ptr(),
                chunk.len(),
                timeout_ms,
            )
        }
    }

    fn try_write(&self, stream: u64, chunk: &[u8]) -> (i32, u32) {
        let mut queued = 7;
        let rc = unsafe {
            aster_http_body_try_write(
                self.rt,
                self.bodies,
                stream,
                chunk.as_ptr(),
                chunk.len(),
                &mut queued,
            )
        };
        (rc, queued)
    }

    fn finish(&self, stream: u64, trailers: Option<&[u8]>) -> i32 {
        let (p, n) = trailers.map_or((ptr::null(), 0), |t| (t.as_ptr(), t.len()));
        unsafe { aster_http_body_finish(self.rt, self.bodies, stream, p, n) }
    }

    fn abort(&self, stream: u64, msg: &str) -> i32 {
        unsafe { aster_http_body_abort(self.rt, self.bodies, stream, msg.as_ptr(), msg.len()) }
    }

    fn open_count(&self) -> u64 {
        let mut n = 99;
        assert_eq!(
            unsafe { aster_http_bodies_open_count(self.rt, self.bodies, &mut n) },
            OK
        );
        n
    }
}

type HttpFn = Arc<dyn Fn(Ctx, u64, Req) + Send + Sync>;
/// Answers a `C7`: the `C8` bytes to send, or `None` for no answer.
type RouteFn = Arc<dyn Fn(&(String, Vec<u8>, Specs)) -> Option<Vec<u8>> + Send + Sync>;

#[derive(Default)]
struct Seen {
    /// kind 48: (service, peer, metadata).
    authorize: Mutex<Vec<(String, String, Vec<u8>)>>,
    /// kind 49 count.
    http: AtomicUsize,
    /// kind 50 requests.
    routes: Mutex<Vec<(String, Vec<u8>, Specs)>>,
    /// `CANCEL` events: (invocation id, reason).
    cancels: Mutex<Vec<(u64, u8)>>,
}

/// The "binding": one thread draining the queue. Authorize and route-policy
/// upcalls are answered inline; each HTTP upcall runs on its own thread.
struct Foreign {
    stop: Arc<AtomicBool>,
    allow: Arc<AtomicBool>,
    seen: Arc<Seen>,
    handlers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    thread: Option<JoinHandle<()>>,
}

impl Foreign {
    fn start(ctx: Ctx, http: HttpFn, routes: RouteFn) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let allow = Arc::new(AtomicBool::new(true));
        let seen = Arc::new(Seen::default());
        let handlers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let (s, a, sn, hs) = (stop.clone(), allow.clone(), seen.clone(), handlers.clone());
        let thread = std::thread::spawn(move || {
            let mut events: [aster_upcall_event_t; 8] = unsafe { std::mem::zeroed() };
            while !s.load(Ordering::Acquire) {
                let n =
                    unsafe { aster_upcall_poll(ctx.rt, ctx.upcalls, events.as_mut_ptr(), 8, 50) };
                for ev in events.iter().take(n as usize) {
                    let payload = if ev.payload_len == 0 {
                        Vec::new()
                    } else {
                        unsafe { slice::from_raw_parts(ev.payload_ptr, ev.payload_len as usize) }
                            .to_vec()
                    };
                    assert_eq!(
                        unsafe { aster_upcall_buffer_release(ctx.rt, ctx.upcalls, ev.buffer) },
                        OK
                    );
                    match ev.kind {
                        K_CANCEL => sn
                            .cancels
                            .lock()
                            .unwrap()
                            .push((ev.invocation_id, ev.cancel_reason)),
                        K_AUTHORIZE => {
                            sn.authorize.lock().unwrap().push(dec_c0(&payload));
                            let c = enc_cf(a.load(Ordering::Acquire));
                            assert_eq!(ctx.respond(ev.invocation_id, S_OK, &c), OK);
                        }
                        K_HTTP => {
                            sn.http.fetch_add(1, Ordering::AcqRel);
                            let (req, id, http) =
                                (dec_c4(&payload), ev.invocation_id, http.clone());
                            hs.lock()
                                .unwrap()
                                .push(std::thread::spawn(move || http(ctx, id, req)));
                        }
                        K_ROUTES => {
                            let req = dec_c7(&payload);
                            let answer = routes(&req);
                            sn.routes.lock().unwrap().push(req);
                            if let Some(c8) = answer {
                                ctx.respond(ev.invocation_id, S_OK, &c8);
                            }
                        }
                        k => panic!("unexpected upcall kind {k}"),
                    }
                }
            }
        });
        Foreign {
            stop,
            allow,
            seen,
            handlers,
            thread: Some(thread),
        }
    }
}

impl Drop for Foreign {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let hs: Vec<_> = self.handlers.lock().unwrap().drain(..).collect();
        for h in hs {
            let _ = h.join();
        }
    }
}

/// Two nodes (`server` exposes, `client` consumes over `conn`), one expose
/// registry with the foreign handler installed under `SVC`.
struct Rig {
    rt: aster_runtime_t,
    server: aster_node_t,
    client: aster_node_t,
    conn: aster_connection_t,
    expose: aster_expose_t,
    upcalls: aster_upcalls_t,
    bodies: aster_http_bodies_t,
    foreign: Option<Foreign>,
    client_id: String,
}

fn no_http() -> HttpFn {
    Arc::new(|_, id, req| panic!("unexpected HTTP upcall {id}: {req:?}"))
}

fn no_routes() -> RouteFn {
    Arc::new(|_| None)
}

impl Rig {
    /// Nodes + registry + connection; nothing registered yet, no foreign
    /// thread yet. `serve`: own the accept loop with `aster_expose_serve`.
    fn bare(serve: bool) -> Rig {
        unsafe {
            let rt = runtime();
            let server = start_node(rt, &[ALPN]);
            let client = start_node(rt, &[ALPN]);
            let mut upcalls = 0;
            assert_eq!(aster_upcalls_new(rt, 0, &mut upcalls), OK);
            let mut bodies = 0;
            assert_eq!(aster_http_bodies_new(rt, &mut bodies), OK);
            let mut expose = 0;
            assert_eq!(aster_expose_new(rt, server, &mut expose), OK);
            assert_ne!(expose, 0);
            let conn;
            if serve {
                let alpns = [bytes(ALPN)];
                let list = aster_bytes_list_t {
                    items: alpns.as_ptr(),
                    len: 1,
                };
                let mut op = 0;
                assert_eq!(aster_expose_serve(rt, server, expose, list, 0, &mut op), OK);
                conn = connect(rt, client, server);
            } else {
                let (c, accepted) = connect_and_accept(rt, client, server);
                conn = c;
                assert_eq!(aster_expose_handle_connection(rt, expose, accepted), OK);
            }
            let client_id = node_id(rt, client);
            Rig {
                rt,
                server,
                client,
                conn,
                expose,
                upcalls,
                bodies,
                foreign: None,
                client_id,
            }
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            rt: self.rt,
            upcalls: self.upcalls,
            bodies: self.bodies,
        }
    }

    fn start_foreign(&mut self, http: HttpFn, routes: RouteFn) {
        self.foreign = Some(Foreign::start(self.ctx(), http, routes));
    }

    /// HTTP service `SVC` with `opts`, answered by `http`.
    fn http(
        opts: Option<aster_http_foreign_options_t>,
        http: impl Fn(Ctx, u64, Req) + Send + Sync + 'static,
    ) -> Rig {
        let mut rig = Rig::bare(true);
        rig.register(SVC, opts);
        rig.start_foreign(Arc::new(http), no_routes());
        rig
    }

    fn register(&self, service: &str, opts: Option<aster_http_foreign_options_t>) {
        unsafe {
            assert_eq!(
                aster_expose_http_foreign(
                    self.rt,
                    self.expose,
                    self.upcalls,
                    self.bodies,
                    bytes(service.as_bytes()),
                    opts.as_ref().map_or(ptr::null(), |o| o as *const _),
                ),
                OK
            );
        }
    }

    fn seen(&self) -> &Seen {
        &self.foreign.as_ref().expect("foreign started").seen
    }

    fn set_allow(&self, allow: bool) {
        self.foreign
            .as_ref()
            .unwrap()
            .allow
            .store(allow, Ordering::Release);
    }

    /// Start a relay without waiting for it.
    fn relay_start(&self, c4: &[u8]) -> aster_operation_t {
        let mut op = 0;
        assert_eq!(
            unsafe { aster_expose_relay_request(self.rt, self.conn, bytes(c4), 3, &mut op) },
            OK
        );
        op
    }

    fn relay(&self, c4: &[u8]) -> Ev {
        let op = self.relay_start(c4);
        unsafe { finish(self.rt, op) }
    }

    /// Wait (bounded) until `cond` holds.
    fn wait_for(&self, what: &str, cond: impl Fn(&Rig) -> bool) {
        let end = Instant::now() + WAIT;
        while !cond(self) {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.foreign.take();
        unsafe {
            let mut op = 0;
            let _ = aster_node_close(self.rt, self.client, 0, &mut op);
            let _ = aster_node_close(self.rt, self.server, 0, &mut op);
            let _ = aster_runtime_close(self.rt);
        }
    }
}

fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Req {
    let mut h = hdrs(headers);
    if let Some(b) = body {
        h.push(("content-length".into(), b.len().to_string().into_bytes()));
    }
    Req {
        service_id: SVC.into(),
        peer_id: None,
        stream_id: 0,
        version: 2,
        method: method.into(),
        uri: uri.into(),
        headers: h,
        body: Some(body.unwrap_or_default().to_vec()),
    }
}

fn opts(f: impl FnOnce(&mut aster_http_foreign_options_t)) -> Option<aster_http_foreign_options_t> {
    let mut o: aster_http_foreign_options_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { aster_http_foreign_options_init(&mut o) }, OK);
    f(&mut o);
    Some(o)
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

// ============================================================================
// Inline and streamed exchanges
// ============================================================================

#[test]
fn inline_request_and_response() {
    let rig = Rig::http(None, |ctx, id, req| {
        if req.method == "POST" {
            let body = req.body.expect("inline request body");
            let mut echo = body.clone();
            echo.extend_from_slice(b"!");
            ctx.respond_http(
                id,
                201,
                &[("content-type", "text/plain"), ("x-seen-uri", &req.uri)],
                Some(&echo),
            );
        } else {
            // No declared length: the body is streamed and empty.
            assert!(req.body.is_none());
            let mut buf = [0u8; 64];
            assert_eq!(ctx.read(req.stream_id, &mut buf, 5_000), Ok(None));
            ctx.respond_http(id, 200, &[], Some(b"ok"));
        }
    });

    let ev = rig.relay(&enc_c4(&request(
        "POST",
        "/echo?x=1",
        &[("host", "app.local"), ("x-bin", "v")],
        Some(b"hello"),
    )));
    let resp = ev.resp();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body.as_deref(), Some(&b"hello!"[..]));
    assert!(resp
        .headers
        .contains(&("content-type".into(), b"text/plain".to_vec())));
    assert!(resp
        .headers
        .contains(&("x-seen-uri".into(), b"/echo?x=1".to_vec())));

    let resp = rig.relay(&enc_c4(&request("GET", "/", &[], None))).resp();
    assert_eq!((resp.status, resp.body.as_deref()), (200, Some(&b"ok"[..])));

    // The kind-48 admission ran once for the connection and carried the
    // authenticated peer.
    let authz = rig.seen().authorize.lock().unwrap().clone();
    assert!(!authz.is_empty());
    assert!(authz
        .iter()
        .all(|(s, p, _)| s == SVC && *p == rig.client_id));
    rig.wait_for("exchanges to close", |r| r.ctx().open_count() == 0);
}

#[test]
fn inline_request_fields_reach_the_handler() {
    let seen: Arc<Mutex<Option<Req>>> = Arc::default();
    let s = seen.clone();
    let rig = Rig::http(None, move |ctx, id, req| {
        *s.lock().unwrap() = Some(req);
        ctx.respond_http(id, 204, &[], Some(b""));
    });
    let resp = rig
        .relay(&enc_c4(&request(
            "PUT",
            "/a/b?q=%20",
            &[("host", "app.local"), ("accept", "*/*")],
            Some(b"xyz"),
        )))
        .resp();
    assert_eq!(resp.status, 204);
    let req = seen.lock().unwrap().clone().unwrap();
    assert_eq!(req.service_id, SVC);
    assert_eq!(req.peer_id.as_deref(), Some(rig.client_id.as_str()));
    assert!(req.stream_id >= 1);
    assert_eq!(
        (req.method.as_str(), req.uri.as_str()),
        ("PUT", "/a/b?q=%20")
    );
    assert_eq!(req.body.as_deref(), Some(&b"xyz"[..]));
    assert_eq!(req.header("host"), Some(&b"app.local"[..]));
    assert_eq!(req.header("accept"), Some(&b"*/*"[..]));
}

#[test]
fn streamed_request_and_response_with_backpressure() {
    let upload = pattern(200 * 1024);
    let want = upload.clone();
    // A buffer of 2 chunks makes `try_write` report "full" before the head
    // goes out.
    let rig = Rig::http(opts(|o| o.response_buffer = 2), move |ctx, id, req| {
        assert!(req.body.is_none(), "200 KiB must stream");
        let got = ctx.read_all(req.stream_id, 4096).expect("read body");
        assert_eq!(got, want);
        let chunks: Vec<&[u8]> = got.chunks(16 * 1024).collect();
        // Writes before the respond queue up to the buffer.
        assert_eq!(ctx.try_write(req.stream_id, chunks[0]), (B_OK, 1));
        assert_eq!(ctx.try_write(req.stream_id, chunks[1]), (B_OK, 1));
        assert_eq!(ctx.try_write(req.stream_id, chunks[2]), (B_OK, 0), "full");
        ctx.respond_http(
            id,
            200,
            &[("content-type", "application/octet-stream")],
            None,
        );
        for c in &chunks[2..] {
            assert_eq!(ctx.write(req.stream_id, c, 10_000), B_OK);
        }
        // Trailers must be a valid C6; a bad one leaves the body open.
        assert_eq!(ctx.finish(req.stream_id, Some(b"junk")), B_INVALID_TRAILERS);
        let c6 = enc_c6(&hdrs(&[("x-checksum", "abc")]));
        assert_eq!(ctx.finish(req.stream_id, Some(&c6)), B_OK);
        // Done: the exchange is finished (or already closed by the relay).
        let rc = ctx.write(req.stream_id, b"late", 1_000);
        assert!(rc == B_FINISHED || rc == B_UNKNOWN_STREAM, "{rc}");
    });

    let req = request("POST", "/upload", &[("host", "app.local")], Some(&upload));
    let resp = rig.relay(&enc_c4(&req)).resp();
    assert_eq!(resp.status, 200);
    // The buffered consumer collects a streamed body into one inline body.
    assert_eq!(resp.body.as_deref(), Some(upload.as_slice()));
    rig.wait_for("exchanges to close", |r| r.ctx().open_count() == 0);
}

#[test]
fn write_before_respond_without_waiting_uses_blocking_write() {
    // Writes made before the respond that fit the buffer are queued; a
    // blocking write beyond it times out (nothing queued) until the head goes.
    let rig = Rig::http(opts(|o| o.response_buffer = 1), |ctx, id, req| {
        assert_eq!(ctx.write(req.stream_id, b"one", 2_000), B_OK);
        assert_eq!(ctx.write(req.stream_id, b"two", 100), B_TIMEOUT);
        ctx.respond_http(id, 200, &[], None);
        assert_eq!(ctx.write(req.stream_id, b"two", 10_000), B_OK);
        assert_eq!(ctx.finish(req.stream_id, None), B_OK);
    });
    let resp = rig.relay(&enc_c4(&request("GET", "/", &[], None))).resp();
    assert_eq!(resp.body.as_deref(), Some(&b"onetwo"[..]));
}

// ============================================================================
// Authorization
// ============================================================================

#[test]
fn authorize_deny_refuses_the_request_before_any_http_upcall() {
    let rig = Rig::http(None, |ctx, id, _| {
        ctx.respond_http(id, 200, &[], Some(b"in"))
    });
    rig.set_allow(false);
    let ev = rig.relay(&enc_c4(&request("GET", "/", &[], Some(b""))));
    assert!(ev.is_error(), "a denied peer must not get a response");
    assert_eq!(rig.seen().http.load(Ordering::Acquire), 0);
    let authz = rig.seen().authorize.lock().unwrap().clone();
    assert!(authz
        .iter()
        .any(|(s, p, _)| s == SVC && *p == rig.client_id));

    // The decision is the binding's: flip it and a fresh stream is admitted.
    rig.set_allow(true);
    let resp = rig
        .relay(&enc_c4(&request("GET", "/", &[], Some(b""))))
        .resp();
    assert_eq!(resp.body.as_deref(), Some(&b"in"[..]));
}

#[test]
fn authorize_without_an_answer_follows_the_failure_mode() {
    // Nobody polls the upcall queue.
    let rig = Rig::bare(true);
    rig.register(SVC, None);
    let set = |kind: u16, ms: u32, mode: u32| unsafe {
        assert_eq!(
            aster_upcall_set_policy(rig.rt, rig.upcalls, kind as u32, ms, mode),
            OK
        );
    };
    let c4 = enc_c4(&request("GET", "/", &[], Some(b"")));

    // Fail-closed: the unanswered admission refuses.
    set(K_AUTHORIZE, 200, FAIL_CLOSED);
    assert!(rig.relay(&c4).is_error());

    // Fail-open: admitted, so the request reaches the (equally silent)
    // handler upcall, which times out into a 504.
    set(K_AUTHORIZE, 200, FAIL_OPEN);
    set(K_HTTP, 200, FAIL_CLOSED);
    assert_eq!(rig.relay(&c4).resp().status, 504);
}

// ============================================================================
// Route policy
// ============================================================================

fn route_specs() -> Specs {
    vec![
        ("a.example".into(), 443, 0, "web".into()),
        ("".into(), 8080, 1, "db".into()),
        ("b.example".into(), 0, 0, "api".into()),
    ]
}

unsafe fn request_routes(rig: &Rig, specs: &Specs, metadata: &[u8]) -> Ev {
    let hosts: Vec<_> = specs.iter().map(|s| s.0.clone()).collect();
    let svcs: Vec<_> = specs.iter().map(|s| s.3.clone()).collect();
    let raw: Vec<aster_route_spec_t> = specs
        .iter()
        .enumerate()
        .map(|(i, s)| aster_route_spec_t {
            host: bytes(hosts[i].as_bytes()),
            service_id: bytes(svcs[i].as_bytes()),
            port: s.1 as u32,
            protocol: s.2 as u32,
        })
        .collect();
    let mut op = 0;
    assert_eq!(
        aster_expose_request_route(
            rig.rt,
            rig.conn,
            raw.as_ptr(),
            raw.len(),
            bytes(metadata),
            4,
            &mut op
        ),
        OK
    );
    finish(rig.rt, op)
}

#[test]
fn route_policy_grant_and_deny() {
    let mode: Arc<Mutex<&'static str>> = Arc::new(Mutex::new("grant"));
    let m = mode.clone();
    let routes: RouteFn = Arc::new(move |_req| match *m.lock().unwrap() {
        "grant" => Some(enc_c8(&[0, 2])),
        "deny" => Some(enc_c8(&[])),
        "out-of-range" => Some(enc_c8(&[1, 9])),
        _ => None,
    });
    let mut rig = Rig::bare(true);
    unsafe {
        assert_eq!(
            aster_expose_route_policy_foreign(rig.rt, rig.expose, rig.upcalls),
            OK
        );
        rig.start_foreign(no_http(), routes);
        let specs = route_specs();

        // Grant: the subset the binding named, as indexes into the request.
        let ev = request_routes(&rig, &specs, b"tok");
        assert!(
            ev.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT),
            "{}",
            ev.text()
        );
        assert_eq!(dec_c8(&ev.data), vec![0, 2]);
        assert_eq!(ev.ev.user_data, 4);
        let reg = ev.ev.handle;
        assert_ne!(reg, 0);
        let seen = rig.seen().routes.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![(rig.client_id.clone(), b"tok".to_vec(), specs.clone())]
        );
        assert_eq!(aster_route_registration_free(rig.rt, reg), OK);
        assert_eq!(aster_route_registration_free(rig.rt, reg), NOT_FOUND);

        // Deny: nothing granted.
        *mode.lock().unwrap() = "deny";
        let ev = request_routes(&rig, &specs, b"");
        assert!(
            ev.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT),
            "{}",
            ev.text()
        );
        assert_eq!(dec_c8(&ev.data), Vec::<u32>::new());
        assert_eq!(aster_route_registration_free(rig.rt, ev.ev.handle), OK);

        // A malformed answer (index out of range) is "no usable answer":
        // fail-closed (the default) grants nothing.
        *mode.lock().unwrap() = "out-of-range";
        let ev = request_routes(&rig, &specs, b"");
        assert!(
            ev.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT),
            "{}",
            ev.text()
        );
        assert_eq!(dec_c8(&ev.data), Vec::<u32>::new());
        assert_eq!(aster_route_registration_free(rig.rt, ev.ev.handle), OK);

        // No answer at all: the kind's failure mode decides. Fail-open
        // grants every requested route.
        assert_eq!(
            aster_upcall_set_policy(rig.rt, rig.upcalls, K_ROUTES as u32, 300, FAIL_OPEN),
            OK
        );
        *mode.lock().unwrap() = "silent";
        let ev = request_routes(&rig, &specs, b"");
        assert!(
            ev.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT),
            "{}",
            ev.text()
        );
        assert_eq!(dec_c8(&ev.data), vec![0, 1, 2]);
        assert_eq!(aster_route_registration_free(rig.rt, ev.ev.handle), OK);
        assert_eq!(
            aster_upcall_set_policy(rig.rt, rig.upcalls, K_ROUTES as u32, 300, FAIL_CLOSED),
            OK
        );
        let ev = request_routes(&rig, &specs, b"");
        assert!(
            ev.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT),
            "{}",
            ev.text()
        );
        assert_eq!(dec_c8(&ev.data), Vec::<u32>::new());
        assert_eq!(aster_route_registration_free(rig.rt, ev.ev.handle), OK);
    }
}

#[test]
fn request_route_validates_its_arguments() {
    let rig = Rig::bare(true);
    unsafe {
        let mut op = 0;
        let bad_proto = aster_route_spec_t {
            host: bytes(b""),
            service_id: bytes(b"x"),
            port: 1,
            protocol: 7,
        };
        let bad_port = aster_route_spec_t {
            protocol: 0,
            port: 70_000,
            ..bad_proto
        };
        for s in [bad_proto, bad_port] {
            assert_eq!(
                aster_expose_request_route(rig.rt, rig.conn, &s, 1, bytes(b""), 0, &mut op),
                INVALID
            );
        }
        assert_eq!(
            aster_expose_request_route(rig.rt, rig.conn, ptr::null(), 1, bytes(b""), 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_expose_request_route(rig.rt, 424_242, ptr::null(), 0, bytes(b""), 0, &mut op),
            NOT_FOUND
        );
        assert_eq!(
            aster_expose_request_route(
                rig.rt,
                rig.conn,
                ptr::null(),
                0,
                bytes(b""),
                0,
                ptr::null_mut()
            ),
            INVALID
        );
    }
}

// ============================================================================
// Failure statuses
// ============================================================================

#[test]
fn handler_failures_become_5xx_statuses() {
    let late: Arc<Mutex<Vec<i32>>> = Arc::default();
    let l = late.clone();
    let rig = Rig::http(None, move |ctx, id, req| match req.uri.as_str() {
        "/error" => assert_eq!(ctx.respond(id, S_ERROR, b"secret detail"), OK),
        "/unimplemented" => assert_eq!(ctx.respond(id, S_UNIMPLEMENTED, b""), OK),
        "/garbage" => assert_eq!(ctx.respond(id, S_OK, b"\xC5\x01not a message"), OK),
        "/bad-status" => {
            // A well-formed C5 whose status is outside 100..=999.
            let mut w = Wr::new(0xC5);
            w.varint(99).headers(&[]).body(&Some(Vec::new()));
            assert_eq!(ctx.respond(id, S_OK, &w.0), OK);
        }
        "/slow" => {
            std::thread::sleep(Duration::from_millis(900));
            l.lock().unwrap().push(ctx.respond(
                id,
                S_OK,
                &enc_c5(&Resp {
                    status: 200,
                    headers: vec![],
                    body: Some(vec![]),
                }),
            ));
        }
        other => panic!("unexpected {other}"),
    });
    unsafe {
        assert_eq!(
            aster_upcall_set_policy(rig.rt, rig.upcalls, K_HTTP as u32, 400, FAIL_CLOSED),
            OK
        );
    }

    for (uri, status) in [
        ("/error", 500),
        ("/unimplemented", 501),
        ("/garbage", 502),
        ("/bad-status", 502),
        ("/slow", 504),
    ] {
        let resp = rig
            .relay(&enc_c4(&request("GET", uri, &[], Some(b""))))
            .resp();
        assert_eq!(resp.status, status, "{uri}");
        // Body-less, and the foreign message never reaches the peer.
        assert_eq!(resp.body.as_deref().unwrap_or_default(), b"", "{uri}");
        assert!(resp
            .headers
            .iter()
            .all(|(_, v)| !v.windows(6).any(|w| w == b"secret")));
    }

    // The slow handler was told to stop (timeout cancel) and its late
    // respond is NOT_FOUND.
    rig.wait_for("late respond", |_| !late.lock().unwrap().is_empty());
    assert_eq!(*late.lock().unwrap(), vec![NOT_FOUND]);
    let cancels = rig.seen().cancels.lock().unwrap().clone();
    assert!(
        cancels
            .iter()
            .any(|(_, r)| *r == aster_upcall_cancel_reason_t::ASTER_UPCALL_CANCEL_TIMEOUT as u8),
        "{cancels:?}"
    );
    rig.wait_for("exchanges to close", |r| r.ctx().open_count() == 0);
}

// ============================================================================
// Limits
// ============================================================================

#[test]
fn request_body_limits() {
    let rig = Rig::http(opts(|o| o.max_request_body = 2000), |ctx, id, req| {
        // Only reached for a body without a declared length.
        assert!(req.body.is_none());
        match ctx.read_all(req.stream_id, 512) {
            Err(rc) if rc == B_TOO_LARGE => ctx.respond_http(id, 413, &[], Some(b"")),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    });

    // A declared length over the cap is a 413 with no upcall.
    let big = pattern(5000);
    let resp = rig
        .relay(&enc_c4(&request("POST", "/", &[], Some(&big))))
        .resp();
    assert_eq!(resp.status, 413);
    assert_eq!(rig.seen().http.load(Ordering::Acquire), 0);

    // An undeclared body that grows past the cap fails the read.
    let mut r = request("POST", "/", &[], None);
    r.body = Some(pattern(5000));
    let resp = rig.relay(&enc_c4(&r)).resp();
    assert_eq!(resp.status, 413);
    assert_eq!(rig.seen().http.load(Ordering::Acquire), 1);
}

#[test]
fn chunk_and_response_size_limits() {
    let codes: Arc<Mutex<Vec<i32>>> = Arc::default();
    let c = codes.clone();
    let rig = Rig::http(
        opts(|o| {
            o.max_chunk = 1024;
            o.max_response_body = 3000;
        }),
        move |ctx, id, req| {
            let s = req.stream_id;
            ctx.respond_http(id, 200, &[], None);
            let mut out = c.lock().unwrap();
            // Over max_chunk: refused, nothing queued, the body stays open.
            out.push(ctx.write(s, &[7u8; 2048], 2_000));
            out.push(ctx.try_write(s, &[7u8; 2048]).0);
            // Three 1000-byte chunks fit the 3000-byte cap; the fourth
            // crosses it and aborts the response.
            for _ in 0..4 {
                out.push(ctx.write(s, &[1u8; 1000], 5_000));
            }
            // The crossing write already ended the body.
            out.push(ctx.abort(s, "too big"));
            out.push(ctx.finish(s, None));
        },
    );
    let ev = rig.relay(&enc_c4(&request("GET", "/", &[], Some(b""))));
    // The consumer outcome (aborted response) is checked in
    // `aborted_streamed_response_fails_the_consumer`; here it only has to
    // complete.
    let _ = ev;
    rig.wait_for("handler", |_| codes.lock().unwrap().len() == 8);
    assert_eq!(
        codes.lock().unwrap()[..6],
        vec![
            B_CHUNK_TOO_LARGE,
            B_CHUNK_TOO_LARGE,
            B_OK,
            B_OK,
            B_OK,
            B_TOO_LARGE,
        ]
    );
    // The crossing write already ended the body (or the relay already closed
    // the exchange).
    for rc in &codes.lock().unwrap()[6..] {
        assert!(*rc == B_FINISHED || *rc == B_UNKNOWN_STREAM, "{rc}");
    }
}

#[test]
fn inline_response_over_the_cap_is_a_502() {
    let rig = Rig::http(opts(|o| o.max_response_body = 10), |ctx, id, _| {
        ctx.respond_http(id, 200, &[], Some(&[b'x'; 64]));
    });
    let resp = rig
        .relay(&enc_c4(&request("GET", "/", &[], Some(b""))))
        .resp();
    assert_eq!(resp.status, 502);
}

/// The documented behaviour of `abort`: the relay resets the stream and the
/// consumer sees a **failed** response. Until the expose abort fix lands the
/// consumer gets a clean, truncated finish instead.
#[test]
fn aborted_streamed_response_fails_the_consumer() {
    let rig = Rig::http(None, |ctx, id, req| {
        ctx.respond_http(id, 200, &[], None);
        assert_eq!(ctx.write(req.stream_id, &[9u8; 4096], 5_000), B_OK);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(ctx.abort(req.stream_id, "gave up"), B_OK);
    });
    let ev = rig.relay(&enc_c4(&request("GET", "/", &[], Some(b""))));
    assert!(ev.is_error(), "an aborted body must fail, not truncate");
}

#[test]
fn response_over_the_cap_fails_the_consumer() {
    let rig = Rig::http(opts(|o| o.max_response_body = 1500), |ctx, id, req| {
        ctx.respond_http(id, 200, &[], None);
        assert_eq!(ctx.write(req.stream_id, &[1u8; 1000], 5_000), B_OK);
        assert_eq!(ctx.write(req.stream_id, &[1u8; 1000], 5_000), B_TOO_LARGE);
    });
    let ev = rig.relay(&enc_c4(&request("GET", "/", &[], Some(b""))));
    assert!(
        ev.is_error(),
        "an over-cap streamed body must fail, not truncate"
    );
}

// ============================================================================
// Cancellation
// ============================================================================

#[test]
fn consumer_cancel_before_the_head_cancels_the_upcall() {
    let handler_id: Arc<Mutex<Option<(u64, u64)>>> = Arc::default();
    let h = handler_id.clone();
    let late: Arc<Mutex<Vec<i32>>> = Arc::default();
    let l = late.clone();
    let rig = Rig::http(None, move |ctx, id, req| {
        *h.lock().unwrap() = Some((id, req.stream_id));
        // Answer only after the consumer has gone.
        std::thread::sleep(Duration::from_millis(1500));
        l.lock().unwrap().push(ctx.respond(
            id,
            S_OK,
            &enc_c5(&Resp {
                status: 200,
                headers: vec![],
                body: Some(vec![]),
            }),
        ));
        let rc = ctx.write(req.stream_id, b"x", 500);
        l.lock().unwrap().push(rc);
    });

    let op = rig.relay_start(&enc_c4(&request("GET", "/", &[], Some(b""))));
    rig.wait_for("the HTTP upcall", |_| handler_id.lock().unwrap().is_some());
    let (id, _) = handler_id.lock().unwrap().unwrap();
    // The consumer goes away before any response head.
    unsafe {
        assert_eq!(
            aster_connection_close(rig.rt, rig.conn, 0, bytes(b"cancel")),
            OK
        );
    }
    let ev = unsafe { finish(rig.rt, op) };
    assert!(ev.is_error(), "the abandoned relay must not complete");

    // Rust withdrew the upcall: Cancel{abandoned}, then a late respond is
    // UnknownInvocation and body calls on the closed exchange fail.
    rig.wait_for("Cancel", |r| {
        r.seen()
            .cancels
            .lock()
            .unwrap()
            .iter()
            .any(|(i, _)| *i == id)
    });
    let reason = rig
        .seen()
        .cancels
        .lock()
        .unwrap()
        .iter()
        .find(|(i, _)| *i == id)
        .unwrap()
        .1;
    assert_eq!(
        reason,
        aster_upcall_cancel_reason_t::ASTER_UPCALL_CANCEL_ABANDONED as u8
    );
    rig.wait_for("late answers", |_| late.lock().unwrap().len() == 2);
    let late = late.lock().unwrap().clone();
    assert_eq!(late[0], NOT_FOUND, "late respond");
    assert!(
        late[1] == B_CANCELLED || late[1] == B_UNKNOWN_STREAM,
        "{late:?}"
    );
    rig.wait_for("exchange to close", |r| r.ctx().open_count() == 0);
}

#[test]
fn consumer_cancel_mid_stream_makes_writes_cancelled() {
    let outcome: Arc<Mutex<Option<i32>>> = Arc::default();
    let started = Arc::new(AtomicBool::new(false));
    let (o, st) = (outcome.clone(), started.clone());
    let rig = Rig::http(None, move |ctx, id, req| {
        ctx.respond_http(id, 200, &[], None);
        // Flood the exchange: the bounded buffer fills, so a write is parked
        // when the consumer disappears and wakes with Cancelled.
        let chunk = vec![5u8; 256 * 1024];
        let end = Instant::now() + WAIT;
        loop {
            let rc = ctx.write(req.stream_id, &chunk, 5_000);
            st.store(true, Ordering::Release);
            if rc != B_OK && rc != B_TIMEOUT {
                *o.lock().unwrap() = Some(rc);
                return;
            }
            if Instant::now() > end {
                *o.lock().unwrap() = Some(rc);
                return;
            }
        }
    });
    // The stream never finishes, so the buffered relay stays pending; the
    // consumer then gives up by closing the connection.
    let op = rig.relay_start(&enc_c4(&request("GET", "/", &[], Some(b""))));
    rig.wait_for("streaming to start", |_| started.load(Ordering::Acquire));
    unsafe {
        assert_eq!(
            aster_connection_close(rig.rt, rig.conn, 0, bytes(b"bye")),
            OK
        );
    }
    let ev = unsafe { finish(rig.rt, op) };
    assert!(ev.is_error());
    rig.wait_for("handler to notice", |_| outcome.lock().unwrap().is_some());
    assert_eq!(*outcome.lock().unwrap(), Some(B_CANCELLED));
    rig.wait_for("exchange to close", |r| r.ctx().open_count() == 0);
}

// ============================================================================
// Socket targets
// ============================================================================

#[test]
fn socket_target_with_and_without_a_foreign_authorizer() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let accepted = || listener.accept().ok();

    let mut rig = Rig::bare(true);
    rig.start_foreign(no_http(), no_routes());
    unsafe {
        // Argument validation.
        let id = bytes(b"sock");
        assert_eq!(
            aster_expose_socket(rig.rt, rig.expose, id, bytes(b"not an addr"), 0),
            INVALID
        );
        assert_eq!(
            aster_expose_socket(rig.rt, rig.expose, bytes(b""), bytes(addr.as_bytes()), 0),
            INVALID
        );
        assert_eq!(
            aster_expose_socket(rig.rt, 424_242, id, bytes(addr.as_bytes()), 0),
            NOT_FOUND
        );
        assert_eq!(
            aster_expose_socket(rig.rt, rig.expose, id, bytes(addr.as_bytes()), 424_242),
            NOT_FOUND
        );

        // Guarded by the foreign authorizer: denied, so nothing is spliced.
        assert_eq!(
            aster_expose_socket(rig.rt, rig.expose, id, bytes(addr.as_bytes()), rig.upcalls),
            OK
        );
        rig.set_allow(false);
        let mut r = request("GET", "/", &[], Some(b""));
        r.service_id = "sock".into();
        let ev = rig.relay(&enc_c4(&r));
        assert!(ev.is_error(), "denied peer must not reach the socket");
        assert!(rig
            .seen()
            .authorize
            .lock()
            .unwrap()
            .iter()
            .any(|(s, p, _)| s == "sock" && *p == rig.client_id));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            accepted().is_none(),
            "no connection to the target while denied"
        );

        // Allowed: the stream is spliced to the target, which sees a TCP
        // connection (the consumer's framing is not HTTP/1.1, so the relay
        // call itself only ends once the target hangs up).
        rig.set_allow(true);
        let op = rig.relay_start(&enc_c4(&r));
        let end = Instant::now() + WAIT;
        let target_side = loop {
            if let Some((s, _)) = accepted() {
                break s;
            }
            assert!(Instant::now() < end, "target never saw a connection");
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(target_side);
        let _ = finish(rig.rt, op);

        // Without an authorizer every peer is admitted and no kind-48
        // upcall is made for the service.
        let open = bytes(b"open");
        assert_eq!(
            aster_expose_socket(rig.rt, rig.expose, open, bytes(addr.as_bytes()), 0),
            OK
        );
        let before = rig.seen().authorize.lock().unwrap().len();
        let mut r2 = request("GET", "/", &[], Some(b""));
        r2.service_id = "open".into();
        let op = rig.relay_start(&enc_c4(&r2));
        let end = Instant::now() + WAIT;
        let target_side = loop {
            if let Some((s, _)) = accepted() {
                break s;
            }
            assert!(
                Instant::now() < end,
                "open service never reached the target"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(target_side);
        let _ = finish(rig.rt, op);
        assert_eq!(rig.seen().authorize.lock().unwrap().len(), before);
    }
}

// ============================================================================
// Lifecycle, accept paths, validation
// ============================================================================

#[test]
fn handle_connection_feeds_accepted_connections() {
    let mut rig = Rig::bare(false);
    rig.register(SVC, None);
    rig.start_foreign(
        Arc::new(|ctx, id, _| ctx.respond_http(id, 200, &[], Some(b"fed"))),
        no_routes(),
    );
    let resp = rig
        .relay(&enc_c4(&request("GET", "/", &[], Some(b""))))
        .resp();
    assert_eq!(resp.body.as_deref(), Some(&b"fed"[..]));
    unsafe {
        assert_eq!(
            aster_expose_handle_connection(rig.rt, rig.expose, 424_242),
            NOT_FOUND
        );
        assert_eq!(
            aster_expose_handle_connection(rig.rt, 424_242, rig.conn),
            NOT_FOUND
        );
    }
}

#[test]
fn unexpose_stops_the_service() {
    let rig = Rig::http(None, |ctx, id, _| {
        ctx.respond_http(id, 200, &[], Some(b"up"))
    });
    let c4 = enc_c4(&request("GET", "/", &[], Some(b"")));
    assert_eq!(rig.relay(&c4).resp().body.as_deref(), Some(&b"up"[..]));
    unsafe {
        assert_eq!(
            aster_expose_unexpose(rig.rt, rig.expose, bytes(SVC.as_bytes())),
            OK
        );
        // Idempotent.
        assert_eq!(
            aster_expose_unexpose(rig.rt, rig.expose, bytes(SVC.as_bytes())),
            OK
        );
    }
    assert!(rig.relay(&c4).is_error(), "unexposed service must refuse");
    // Registering it again brings it back.
    rig.register(SVC, None);
    assert_eq!(rig.relay(&c4).resp().body.as_deref(), Some(&b"up"[..]));
}

#[test]
fn foreign_http_option_and_handle_validation() {
    let rig = Rig::bare(true);
    unsafe {
        let id = bytes(b"svc");
        let mut o: aster_http_foreign_options_t = std::mem::zeroed();
        assert_eq!(aster_http_foreign_options_init(&mut o), OK);
        assert_eq!(
            o.struct_size as usize,
            std::mem::size_of::<aster_http_foreign_options_t>()
        );
        assert_eq!(aster_http_foreign_options_init(ptr::null_mut()), INVALID);
        let go = |o: *const aster_http_foreign_options_t| {
            aster_expose_http_foreign(rig.rt, rig.expose, rig.upcalls, rig.bodies, id, o)
        };
        assert_eq!(go(&o), OK);
        assert_eq!(go(ptr::null()), OK, "NULL = defaults");
        let mut bad = o;
        bad.struct_size = 8;
        assert_eq!(go(&bad), INVALID);
        let mut bad = o;
        bad.flags = 1;
        assert_eq!(go(&bad), INVALID);
        let mut bad = o;
        bad.reserved = 1;
        assert_eq!(go(&bad), INVALID);

        let call = |e, u, b, id| aster_expose_http_foreign(rig.rt, e, u, b, id, ptr::null());
        assert_eq!(
            call(rig.expose, rig.upcalls, rig.bodies, bytes(b"")),
            INVALID
        );
        assert_eq!(call(424_242, rig.upcalls, rig.bodies, id), NOT_FOUND);
        assert_eq!(call(rig.expose, 424_242, rig.bodies, id), NOT_FOUND);
        assert_eq!(call(rig.expose, rig.upcalls, 424_242, id), NOT_FOUND);
        assert_eq!(
            aster_expose_route_policy_foreign(rig.rt, rig.expose, 424_242),
            NOT_FOUND
        );
        assert_eq!(
            aster_expose_route_policy_foreign(rig.rt, 424_242, rig.upcalls),
            NOT_FOUND
        );

        // Freeing the registry or the table never disturbs the other.
        let mut b2 = 0;
        assert_eq!(aster_http_bodies_new(rig.rt, &mut b2), OK);
        assert_eq!(aster_http_bodies_free(rig.rt, b2), OK);
        assert_eq!(aster_http_bodies_free(rig.rt, b2), NOT_FOUND);
        let mut e2 = 0;
        assert_eq!(aster_expose_new(rig.rt, rig.server, &mut e2), OK);
        assert_eq!(aster_expose_free(rig.rt, e2), OK);
        assert_eq!(aster_expose_free(rig.rt, e2), NOT_FOUND);
        assert_eq!(
            aster_expose_new(rig.rt, rig.server, ptr::null_mut()),
            INVALID
        );
    }
}

#[test]
fn relay_request_validates_its_arguments() {
    let rig = Rig::bare(true);
    unsafe {
        let mut op = 0;
        let good = enc_c4(&request("GET", "/", &[], Some(b"")));
        assert_eq!(
            aster_expose_relay_request(rig.rt, 424_242, bytes(&good), 0, &mut op),
            NOT_FOUND
        );
        assert_eq!(
            aster_expose_relay_request(rig.rt, rig.conn, bytes(&good), 0, ptr::null_mut()),
            INVALID
        );
        assert_eq!(
            aster_expose_relay_request(rig.rt, rig.conn, bytes(b"\xC4\x01garbage"), 0, &mut op),
            INVALID
        );
        // A streamed request body cannot be relayed from a buffer.
        let mut streamed = request("POST", "/", &[], None);
        streamed.body = None;
        assert_eq!(
            aster_expose_relay_request(rig.rt, rig.conn, bytes(&enc_c4(&streamed)), 0, &mut op),
            INVALID
        );
        let mut nosvc = request("GET", "/", &[], Some(b""));
        nosvc.service_id = String::new();
        assert_eq!(
            aster_expose_relay_request(rig.rt, rig.conn, bytes(&enc_c4(&nosvc)), 0, &mut op),
            INVALID
        );
    }
}

#[test]
fn body_calls_validate_and_refuse_runtime_threads() {
    let rig = Rig::bare(true);
    let c = rig.ctx();
    let mut buf = [0u8; 8];
    assert_eq!(c.read(99, &mut buf, 0), Err(B_UNKNOWN_STREAM));
    assert_eq!(c.write(99, b"x", 0), B_UNKNOWN_STREAM);
    assert_eq!(c.try_write(99, b"x").0, B_UNKNOWN_STREAM);
    assert_eq!(c.finish(99, None), B_UNKNOWN_STREAM);
    assert_eq!(c.abort(99, "m"), B_UNKNOWN_STREAM);
    unsafe {
        let (mut n, mut eof) = (0usize, 0u32);
        let invalid = aster_http_body_status_t::ASTER_HTTP_BODY_INVALID_ARGUMENT as i32;
        let unknown = aster_http_body_status_t::ASTER_HTTP_BODY_UNKNOWN_HANDLE as i32;
        assert_eq!(
            aster_http_body_read(
                rig.rt,
                rig.bodies,
                1,
                ptr::null_mut(),
                8,
                0,
                &mut n,
                &mut eof
            ),
            invalid
        );
        assert_eq!(
            aster_http_body_read(
                rig.rt,
                rig.bodies,
                1,
                buf.as_mut_ptr(),
                0,
                0,
                &mut n,
                &mut eof
            ),
            invalid
        );
        assert_eq!(
            aster_http_body_read(
                rig.rt,
                rig.bodies,
                1,
                buf.as_mut_ptr(),
                8,
                0,
                ptr::null_mut(),
                &mut eof
            ),
            invalid
        );
        assert_eq!(
            aster_http_body_write(rig.rt, rig.bodies, 1, ptr::null(), 3, 0),
            invalid
        );
        assert_eq!(
            aster_http_body_try_write(rig.rt, rig.bodies, 1, b"x".as_ptr(), 1, ptr::null_mut()),
            invalid
        );
        assert_eq!(
            aster_http_body_finish(rig.rt, rig.bodies, 1, ptr::null(), 4),
            invalid
        );
        assert_eq!(
            aster_http_body_abort(rig.rt, rig.bodies, 1, ptr::null(), 4),
            invalid
        );
        assert_eq!(
            aster_http_body_write(rig.rt, 424_242, 1, b"x".as_ptr(), 1, 0),
            unknown
        );
        assert_eq!(
            aster_http_body_write(424_242, rig.bodies, 1, b"x".as_ptr(), 1, 0),
            unknown
        );
        assert_eq!(
            aster_http_bodies_open_count(rig.rt, rig.bodies, ptr::null_mut()),
            INVALID
        );
        assert_eq!(
            aster_http_bodies_open_count(rig.rt, 424_242, &mut 0u64),
            NOT_FOUND
        );
        assert_eq!(aster_http_bodies_new(rig.rt, ptr::null_mut()), INVALID);
    }

    // A blocking call from a tokio runtime thread is refused, not deadlocked.
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    tokio_rt.block_on(async {
        let mut buf = [0u8; 8];
        assert_eq!(c.read(99, &mut buf, 10), Err(B_BLOCKING_ON_RUNTIME));
        assert_eq!(c.write(99, b"x", 10), B_BLOCKING_ON_RUNTIME);
        // The non-blocking calls are fine anywhere.
        assert_eq!(c.try_write(99, b"x").0, B_UNKNOWN_STREAM);
        assert_eq!(c.finish(99, None), B_UNKNOWN_STREAM);
    });
}
