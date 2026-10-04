//! C ABI unary benchmark: the pooled fast path (`aster_rpc_unary`), the same
//! call made cancellable (`aster_rpc_unary_cancellable` + a cancel token, ABI
//! 4.5), and the dedicated-stream path the bindings used for cancellable unary
//! calls before 4.5 (`aster_rpc_open` server-stream + send + recv + release).
//!
//! Built as the `ffi_unary_bench` example of `aster_transport_ffi`:
//!
//! ```text
//! cargo run --release -p aster_transport_ffi --example ffi_unary_bench -- \
//!     [--modes unary,cancellable,open,open-poll] [--concurrency 1,16,100] \
//!     [--duration S] [--warmup S] [--runs N]
//! ```
//!
//! The server is a raw-bytes echo (`Bench.echo`, 90 B request, the size of the
//! InteropEcho benchmark request) on an in-process `AsterServer` with a tokio
//! runtime of its own; the client is the C ABI on a second node.
//!
//! Each worker is an OS thread making blocking calls back to back on ONE
//! connection (closed loop). Runs are interleaved across modes so drift on the
//! machine spreads over all modes. Prints one `RESULT {json}` line per
//! (run, mode, concurrency) and a median table at the end.
//!
//! Modes:
//! - `unary`: `aster_rpc_unary` (one crossing, pooled stream);
//! - `cancellable`: `aster_cancel_token_new` + `aster_rpc_unary_cancellable`
//!   (two crossings, pooled stream; never cancelled here);
//! - `open`: `aster_rpc_open` + `aster_call_send_payload` +
//!   `aster_call_recv_payload(INFINITE)` x2 + `aster_call_release`: what Go
//!   and .NET did for a cancellable context/token;
//! - `open-poll`: as `open`, reading in 200 ms slices (`Rpc.recvPolling`, what
//!   Java's `CallOptions.cancellable(true)` did);
//! - `rust-unary` / `rust-stream`: no C ABI -- `RpcConnection::unary_with` /
//!   `server_stream_with` + read to the end on a native client connection,
//!   one `block_on` per call. They split the C ABI gap into "stream per call"
//!   (rust-stream vs rust-unary) and "crossings" (open vs rust-stream).

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use aster::rpc::{
    AsterServer, Call, CallOptions, RpcConnection, RpcStatus, ServiceDispatch, StreamHeader,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_ffi::call::{aster_call_buffer_release, aster_call_release};
use aster_transport_ffi::node::*;
use aster_transport_ffi::rpc::*;
use aster_transport_ffi::server::ASTER_TIMEOUT_INFINITE;
use aster_transport_ffi::*;

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const TIMEOUT: i32 = aster_status_t::ASTER_STATUS_TIMEOUT as i32;

struct EchoService;

#[aster::rpc::async_trait]
impl ServiceDispatch for EchoService {
    fn name(&self) -> &str {
        "Bench"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["echo"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        let _ = call.respond(req, &RpcStatus::ok()).await;
    }
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

struct Args {
    modes: Vec<String>,
    concurrency: Vec<usize>,
    duration: f64,
    warmup: f64,
    runs: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        modes: vec!["unary".into(), "cancellable".into(), "open".into()],
        concurrency: vec![1, 16, 100],
        duration: 5.0,
        warmup: 1.0,
        runs: 3,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let v = argv.get(i + 1).cloned().unwrap_or_default();
        match argv[i].as_str() {
            "--modes" => a.modes = v.split(',').map(str::to_owned).collect(),
            "--concurrency" => a.concurrency = v.split(',').map(|s| s.parse().unwrap()).collect(),
            "--duration" => a.duration = v.parse().unwrap(),
            "--warmup" => a.warmup = v.parse().unwrap(),
            "--runs" => a.runs = v.parse().unwrap(),
            other => panic!("unknown arg {other}"),
        }
        i += 2;
    }
    a
}

#[derive(Clone, Copy)]
struct Client {
    rt: aster_runtime_t,
    conn: aster_connection_t,
}

struct Target {
    service: &'static str,
    method: &'static str,
    request: Vec<u8>,
    /// The `rust-*` modes: a native client on its own runtime, driven the
    /// way the C ABI drives its own (`Handle::block_on` per call).
    native: (tokio::runtime::Handle, RpcConnection),
    header: StreamHeader,
}

impl Target {
    fn options(&self) -> aster_call_options_t {
        aster_call_options_t {
            struct_size: std::mem::size_of::<aster_call_options_t>() as u32,
            version: 1,
            service: bytes(self.service.as_bytes()),
            method: bytes(self.method.as_bytes()),
            metadata: bytes(&[]),
            session_id: 0,
            deadline_ms: 0,
            serialization_mode: 0,
            wire_call_id: 0,
        }
    }
}

/// One call in `mode`; true on success.
unsafe fn call_once(c: Client, t: &Target, o: &aster_call_options_t, mode: &str) -> bool {
    match mode {
        "unary" | "cancellable" => {
            let (mut rp, mut rl, mut rb) = (ptr::null(), 0u32, 0u64);
            let (mut code, mut sp, mut sl, mut sb) = (0i32, ptr::null(), 0u32, 0u64);
            let rc = if mode == "unary" {
                aster_rpc_unary(
                    c.rt,
                    c.conn,
                    o,
                    t.request.as_ptr(),
                    t.request.len() as u32,
                    &mut rp,
                    &mut rl,
                    &mut rb,
                    &mut code,
                    &mut sp,
                    &mut sl,
                    &mut sb,
                )
            } else {
                let mut token = 0;
                assert_eq!(aster_cancel_token_new(c.rt, &mut token), OK);
                aster_rpc_unary_cancellable(
                    c.rt,
                    c.conn,
                    o,
                    token,
                    t.request.as_ptr(),
                    t.request.len() as u32,
                    &mut rp,
                    &mut rl,
                    &mut rb,
                    &mut code,
                    &mut sp,
                    &mut sl,
                    &mut sb,
                )
            };
            let ok = rc == OK && code == 0 && rl as usize > 0;
            if rb != 0 {
                aster_call_buffer_release(c.rt, rb);
            }
            if sb != 0 {
                aster_call_buffer_release(c.rt, sb);
            }
            ok
        }
        "open" | "open-poll" => {
            let slice = if mode == "open" {
                ASTER_TIMEOUT_INFINITE
            } else {
                200
            };
            let mut call = 0;
            if aster_rpc_open(c.rt, c.conn, 1, o, &mut call) != OK {
                return false;
            }
            let mut ok =
                aster_call_send_payload(c.rt, call, t.request.as_ptr(), t.request.len() as u32, 1)
                    == OK;
            let mut frames = 0;
            while ok {
                let (mut p, mut l, mut b, mut tr) = (ptr::null(), 0u32, 0u64, 0u8);
                let rc =
                    aster_call_recv_payload(c.rt, call, slice, &mut p, &mut l, &mut b, &mut tr);
                if rc == TIMEOUT {
                    continue;
                }
                if rc != OK {
                    ok = false;
                    break;
                }
                if tr == 1 {
                    // Code 0 encodes as an RpcStatus with code 0.
                    let st = std::slice::from_raw_parts(p, l as usize);
                    ok = aster::rpc::codec::decode_rpc_status(st)
                        .map(|s| s.code == 0)
                        .unwrap_or(false)
                        && frames == 1;
                    aster_call_buffer_release(c.rt, b);
                    break;
                }
                frames += 1;
                aster_call_buffer_release(c.rt, b);
            }
            aster_call_release(c.rt, call);
            ok
        }
        "rust-unary" => {
            let (h, conn) = &t.native;
            h.block_on(conn.unary_with(&t.header, t.request.clone(), &CallOptions::new()))
                .is_ok_and(|r| !r.is_empty())
        }
        "rust-stream" => {
            let (h, conn) = &t.native;
            h.block_on(async {
                let Ok(mut s) = conn
                    .server_stream_with(&t.header, t.request.clone(), &CallOptions::new())
                    .await
                else {
                    return false;
                };
                let mut n = 0;
                while let Some(r) = s.next().await {
                    if r.is_err() {
                        return false;
                    }
                    n += 1;
                }
                n == 1
            })
        }
        other => panic!("unknown mode {other}"),
    }
}

struct Run {
    calls: usize,
    errors: usize,
    secs: f64,
    lats: Vec<u64>,
}

fn run(c: Client, t: &Arc<Target>, mode: &str, conc: usize, secs: f64) -> Run {
    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(conc + 1));
    let workers: Vec<_> = (0..conc)
        .map(|_| {
            let (stop, start, t, mode) = (stop.clone(), start.clone(), t.clone(), mode.to_owned());
            std::thread::spawn(move || {
                let o = t.options();
                let mut lats = Vec::with_capacity(1 << 16);
                let mut errors = 0;
                start.wait();
                while !stop.load(Ordering::Relaxed) {
                    let t0 = Instant::now();
                    if unsafe { call_once(c, &t, &o, &mode) } {
                        lats.push(t0.elapsed().as_nanos() as u64);
                    } else {
                        errors += 1;
                    }
                }
                (lats, errors)
            })
        })
        .collect();
    start.wait();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    let mut lats = Vec::new();
    let mut errors = 0;
    for w in workers {
        let (l, e) = w.join().unwrap();
        lats.extend(l);
        errors += e;
    }
    let secs = t0.elapsed().as_secs_f64();
    lats.sort_unstable();
    Run {
        calls: lats.len(),
        errors,
        secs,
        lats,
    }
}

fn pct(s: &[u64], p: f64) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    s[((s.len() as f64 * p) as usize).min(s.len() - 1)] as f64 / 1e6
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

unsafe fn ffi_connect(
    rt: aster_runtime_t,
    endpoint_hex: &str,
    direct: &[String],
) -> aster_connection_t {
    let alpn: &[u8] = b"aster/1";
    let alpns = [bytes(alpn)];
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.alpns = aster_bytes_list_t {
        items: alpns.as_ptr(),
        len: 1,
    };
    let mut op = 0;
    assert_eq!(aster_node_start(rt, &cfg, 0, 0, &mut op), OK);
    let node = wait(rt, op).handle;
    let direct_b: Vec<aster_bytes_t> = direct.iter().map(|d| bytes(d.as_bytes())).collect();
    let addr = aster_node_addr_t {
        endpoint_id: bytes(endpoint_hex.as_bytes()),
        relay_url: bytes(&[]),
        direct_addresses: aster_bytes_list_t {
            items: direct_b.as_ptr(),
            len: direct_b.len(),
        },
    };
    let ccfg = aster_connect_config_t {
        struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
        flags: 0,
        node_id: bytes(endpoint_hex.as_bytes()),
        alpn: bytes(alpn),
        addr: &addr,
    };
    assert_eq!(aster_connect(rt, node, &ccfg, 0, &mut op), OK);
    wait(rt, op).handle
}

unsafe fn wait(rt: aster_runtime_t, op: aster_operation_t) -> aster_event_t {
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        let mut evs: [aster_event_t; 8] = std::mem::zeroed();
        let n = aster_poll_events(rt, evs.as_mut_ptr(), evs.len(), 250);
        for e in evs.iter().take(n) {
            if e.operation == op {
                assert_eq!(e.status, OK as u32, "operation {op} failed");
                return *e;
            }
        }
    }
    panic!("operation {op} timed out");
}

fn main() {
    let args = parse_args();
    // The server (in-process mode) runs on a runtime of its own.
    let server_rt = tokio::runtime::Runtime::new().unwrap();
    let server = server_rt.block_on(async {
        let cfg = AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .build();
        let srv = AsterServer::builder()
            .config(cfg)
            .service(EchoService)
            .start()
            .await
            .unwrap();
        for _ in 0..100 {
            if !srv.node().addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        srv
    });
    let a = server.node().addr();
    let (endpoint, direct) = (a.node_id.as_str().to_owned(), a.direct_addresses);
    let native_rt = tokio::runtime::Runtime::new().unwrap();
    let native_conn = native_rt.block_on(async {
        let node = Node::start(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .build(),
        )
        .await
        .unwrap();
        node.add_peer(server.node()).unwrap();
        let conn = node
            .rpc_connect(&server.node().addr().node_id)
            .await
            .unwrap();
        (node, conn)
    });
    let target = Target {
        service: "Bench",
        method: "echo",
        request: vec![0xab; 90],
        native: (native_rt.handle().clone(), native_conn.1.clone()),
        header: StreamHeader {
            service: "Bench".into(),
            method: "echo".into(),
            version: 1,
            ..Default::default()
        },
    };
    let target = Arc::new(target);

    let mut rt = 0;
    unsafe {
        assert_eq!(
            aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
            OK
        );
    }
    let conn = unsafe { ffi_connect(rt, &endpoint, &direct) };
    let c = Client { rt, conn };
    let o = target.options();
    for m in &args.modes {
        assert!(
            unsafe { call_once(c, &target, &o, m) },
            "first {m} call failed"
        );
    }
    if args.warmup > 0.0 {
        for m in &args.modes {
            run(c, &target, m, 16, args.warmup);
        }
    }

    // (mode, conc) -> per-run (rps, p50, p99)
    type Cell = ((String, usize), Vec<(f64, f64, f64)>);
    let mut table: Vec<Cell> = Vec::new();
    for r in 0..args.runs {
        for &conc in &args.concurrency {
            for m in &args.modes {
                let res = run(c, &target, m, conc, args.duration);
                let rps = res.calls as f64 / res.secs;
                let (p50, p99) = (pct(&res.lats, 0.5), pct(&res.lats, 0.99));
                println!(
                    "RESULT {{\"kind\":\"ffi_unary\",\"run\":{r},\"mode\":\"{m}\",\"conc\":{conc},\"calls\":{},\"errors\":{},\"secs\":{:.3},\"rps\":{rps:.1},\"p50_ms\":{p50:.3},\"p90_ms\":{:.3},\"p99_ms\":{p99:.3},\"max_ms\":{:.3}}}",
                    res.calls,
                    res.errors,
                    res.secs,
                    pct(&res.lats, 0.9),
                    pct(&res.lats, 1.0),
                );
                let key = (m.clone(), conc);
                match table.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v.push((rps, p50, p99)),
                    None => table.push((key, vec![(rps, p50, p99)])),
                }
            }
        }
    }
    println!("\n| mode | conc | rps (median) | p50 ms | p99 ms | rps min..max |");
    println!("|---|---:|---:|---:|---:|---|");
    for ((m, conc), v) in &table {
        let rps: Vec<f64> = v.iter().map(|x| x.0).collect();
        let (lo, hi) = rps
            .iter()
            .fold((f64::MAX, 0f64), |(l, h), &x| (l.min(x), h.max(x)));
        println!(
            "| {m} | {conc} | {:.0} | {:.3} | {:.3} | {lo:.0}..{hi:.0} |",
            median(rps.clone()),
            median(v.iter().map(|x| x.1).collect()),
            median(v.iter().map(|x| x.2).collect()),
        );
    }
    unsafe { aster_runtime_close(rt) };
    server_rt.block_on(server.shutdown());
}
