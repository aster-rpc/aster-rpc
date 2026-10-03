//! Dispatch benchmark: the Rust load driver (`benchmarks/dispatch/`).
//!
//! Drives any `InteropEcho` server (`tests/interop/`, contract in
//! `tests/interop/rust/contract.rs`) over one connection and prints one
//! `RESULT {json}` line per measurement. Built as the `dispatch_driver` example
//! of the `aster` crate (features `rpc,binding-support`); orchestrated by
//! `benchmarks/dispatch/run.py`.
//!
//! ```text
//! dispatch_driver <aster1-ticket> [--mode pooled|rpc] [--duration S] [--warmup S]
//!                 [--concurrency 1,10,50,100] [--stream-frames 100,10000]
//!                 [--client-stream 10000] [--skip-streams] [--online]
//! ```
//!
//! Unary modes:
//! - `pooled` (default): the binding fast path. `CoreConnection::acquire_stream`
//!   on the SHARED pool (default 8 multiplexed streams per connection), one
//!   `write_all` of header+request, read frames to the trailer, return the
//!   stream to the pool. This is what `ffi::aster_call_unary`, Python
//!   `AsterCall.unary_fast_path` and the TS `unaryFastPath` do.
//! - `rpc`: `aster::rpc::RpcConnection::unary` (generated client stub). Opens a
//!   fresh bi-stream per call (not pooled; see dynamic-dispatch-design §0.3).
//!
//! Streaming always uses the generated client (`generate` / `ingest`), which
//! opens a dedicated substream per call, like every binding.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aster::rpc::{
    decode_rpc_status, encode_stream_header, RpcConnection, SerializationMode, StreamHeader,
};
use aster::{AsterConfig, Node, RelayMode, Ticket};
use aster_transport_core::framing::{
    encode_frame, encode_payload_frame, payload_of, FLAG_END_STREAM, FLAG_HEADER, FLAG_ROW_SCHEMA,
    FLAG_TRAILER, MAX_FRAME_SIZE,
};
use aster_transport_core::CoreRecvStream;

#[path = "../../tests/interop/rust/contract.rs"]
mod contract;
use contract::{EchoReply, EchoRequest, InteropEchoClient};

/// The unary request every client in the benchmark sends (keep in sync with
/// `bench_client.py`, `bench_client.ts`, `InteropEchoBench.java`).
fn bench_request() -> EchoRequest {
    EchoRequest {
        message: "bench".into(),
        count: 1,
        data: vec![0xab; 64],
    }
}

/// Server-stream request: `frames` replies of 64 bytes of data each.
fn stream_request(frames: i32) -> EchoRequest {
    EchoRequest {
        message: "s".into(),
        count: frames,
        data: vec![0xcd; 64],
    }
}

struct Args {
    ticket: String,
    mode: String,
    duration: f64,
    warmup: f64,
    concurrency: Vec<usize>,
    stream_frames: Vec<i32>,
    client_stream: Vec<usize>,
    skip_streams: bool,
    online: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        ticket: String::new(),
        mode: "pooled".into(),
        duration: 5.0,
        warmup: 1.0,
        concurrency: vec![1, 10, 50, 100],
        stream_frames: vec![100, 10_000],
        client_stream: vec![10_000],
        skip_streams: false,
        online: false,
    };
    let list = |s: &str| -> Vec<usize> {
        s.split(',')
            .filter(|x| !x.is_empty())
            .map(|x| x.parse().expect("number"))
            .collect()
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mode" => a.mode = it.next().expect("--mode value"),
            "--duration" => a.duration = it.next().expect("value").parse().expect("secs"),
            "--warmup" => a.warmup = it.next().expect("value").parse().expect("secs"),
            "--concurrency" => a.concurrency = list(&it.next().expect("value")),
            "--stream-frames" => {
                a.stream_frames = list(&it.next().expect("value"))
                    .into_iter()
                    .map(|n| n as i32)
                    .collect()
            }
            "--client-stream" => a.client_stream = list(&it.next().expect("value")),
            "--skip-streams" => a.skip_streams = true,
            "--online" => a.online = true,
            t if !t.starts_with("--") && a.ticket.is_empty() => a.ticket = t.to_string(),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    if a.ticket.is_empty() || !matches!(a.mode.as_str(), "pooled" | "rpc") {
        eprintln!("usage: dispatch_driver <aster1-ticket> [--mode pooled|rpc] [--duration S] [--warmup S] [--concurrency 1,10] [--stream-frames 100,10000] [--client-stream 10000] [--skip-streams]");
        std::process::exit(2);
    }
    a
}

/// Pooled unary over the binding fast path (mirrors `ffi::aster_call_unary`).
struct PooledUnary {
    conn: RpcConnection,
    request_pair: Vec<u8>,
}

impl PooledUnary {
    fn new(conn: RpcConnection, req: &EchoRequest) -> Self {
        let header = StreamHeader {
            service: "InteropEcho".into(),
            method: "echo".into(),
            version: 1,
            call_id: 0,
            deadline: 0,
            serialization_mode: SerializationMode::Xlang.as_i8(),
            metadata_keys: vec![],
            metadata_values: vec![],
            session_id: 0,
        };
        // Same per-root Fory runtime the generated stub builds.
        let mut reg = aster::rpc::PayloadRegistry::new();
        <EchoRequest as aster::rpc::WireField>::register_payload(&mut reg);
        let fory = reg.into_fory();
        let payload = fory.serialize(req).expect("encode request");
        let mut request_pair =
            encode_frame(&encode_stream_header(&header).expect("header"), FLAG_HEADER)
                .expect("frame");
        request_pair.extend(encode_payload_frame(&payload, FLAG_END_STREAM).expect("frame"));
        Self { conn, request_pair }
    }

    async fn call(&self) -> Result<Vec<u8>, String> {
        let core = self.conn.core_connection();
        let handle = core
            .acquire_stream(None)
            .await
            .map_err(|e| format!("acquire: {e:?}"))?;
        let (send, recv) = {
            let pair = handle.get();
            (pair.0.clone(), pair.1.clone())
        };
        if let Err(e) = send.write_all(self.request_pair.clone()).await {
            handle.discard();
            return Err(format!("write: {e}"));
        }
        let mut response: Option<(Vec<u8>, u8)> = None;
        let trailer = loop {
            match read_one_frame(&recv).await {
                Ok((payload, flags)) => {
                    if flags & FLAG_TRAILER != 0 {
                        break payload;
                    }
                    if flags & FLAG_ROW_SCHEMA != 0 {
                        continue;
                    }
                    if response.is_none() {
                        response = Some((payload, flags));
                    }
                }
                Err(e) => {
                    handle.discard();
                    return Err(e);
                }
            }
        };
        drop(handle);
        if !trailer.is_empty() {
            let st = decode_rpc_status(&trailer).map_err(|e| format!("trailer: {e}"))?;
            if !st.is_ok() {
                return Err(format!("status {} {}", st.code, st.message));
            }
        }
        let (payload, flags) = response.ok_or("no response frame")?;
        payload_of(payload, flags).map_err(|e| format!("payload: {e}"))
    }
}

async fn read_one_frame(recv: &CoreRecvStream) -> Result<(Vec<u8>, u8), String> {
    let mut len = [0u8; 4];
    recv.read_exact_into(&mut len)
        .await
        .map_err(|e| format!("read: {e}"))?;
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > MAX_FRAME_SIZE as usize {
        return Err(format!("bad frame length {n}"));
    }
    let mut flags = [0u8; 1];
    recv.read_exact_into(&mut flags)
        .await
        .map_err(|e| format!("read: {e}"))?;
    let mut payload = vec![0u8; n - 1];
    recv.read_exact_into(&mut payload)
        .await
        .map_err(|e| format!("read: {e}"))?;
    Ok((payload, flags[0]))
}

enum Unary {
    Pooled(Arc<PooledUnary>),
    Rpc(Arc<InteropEchoClient>),
}

impl Unary {
    async fn call(&self) -> Result<(), String> {
        match self {
            Unary::Pooled(p) => p.call().await.map(|_| ()),
            Unary::Rpc(c) => {
                let r: EchoReply = c.echo(bench_request()).await.map_err(|e| e.to_string())?;
                if r.count != 2 {
                    return Err(format!("bad reply count {}", r.count));
                }
                Ok(())
            }
        }
    }
    fn clone_ref(&self) -> Unary {
        match self {
            Unary::Pooled(p) => Unary::Pooled(p.clone()),
            Unary::Rpc(c) => Unary::Rpc(c.clone()),
        }
    }
}

fn pct(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p).floor() as usize;
    sorted[idx.min(sorted.len() - 1)] as f64 / 1e6
}

/// Closed-loop load: `conc` workers each issue calls back to back for
/// `duration`; returns (calls, errors, elapsed, sorted latencies ns).
async fn run_unary(u: &Unary, conc: usize, duration: Duration) -> (u64, u64, f64, Vec<u64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let mut tasks = Vec::with_capacity(conc);
    for _ in 0..conc {
        let u = u.clone_ref();
        let stop = stop.clone();
        tasks.push(tokio::spawn(async move {
            let mut lats = Vec::with_capacity(4096);
            let mut errors = 0u64;
            let mut first_err: Option<String> = None;
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                match u.call().await {
                    Ok(()) => lats.push(t.elapsed().as_nanos() as u64),
                    Err(e) => {
                        errors += 1;
                        first_err.get_or_insert(e);
                    }
                }
            }
            (lats, errors, first_err)
        }));
    }
    tokio::time::sleep(duration).await;
    stop.store(true, Ordering::Relaxed);
    let mut all = Vec::new();
    let mut errors = 0;
    for t in tasks {
        let (l, e, fe) = t.await.expect("worker");
        if let Some(fe) = fe {
            eprintln!("worker error: {fe}");
        }
        all.extend(l);
        errors += e;
    }
    let elapsed = start.elapsed().as_secs_f64();
    all.sort_unstable();
    (all.len() as u64, errors, elapsed, all)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    // Default: loopback only, relay off. `--online` keeps the default relay +
    // address lookup, for servers whose ticket carries no direct addresses
    // (the Java/FFI node cannot be configured for loopback-only today).
    let cfg = if args.online {
        AsterConfig::builder().build()
    } else {
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .build()
    };
    let node = Node::start(cfg).await.expect("node start");
    let ticket = Ticket::from_base58(&args.ticket).expect("ticket");
    let id = node.add_ticket_addr(&ticket).expect("ticket addr");
    let conn = tokio::time::timeout(Duration::from_secs(20), node.rpc_connect(&id))
        .await
        .expect("connect timeout")
        .expect("connect");
    let client = Arc::new(InteropEchoClient::new(conn.clone()));

    let unary = if args.mode == "pooled" {
        Unary::Pooled(Arc::new(PooledUnary::new(conn.clone(), &bench_request())))
    } else {
        Unary::Rpc(client.clone())
    };
    // Correctness check + warm the path.
    unary.call().await.expect("first unary call");
    let warm = Duration::from_secs_f64(args.warmup);
    if args.warmup > 0.0 {
        run_unary(&unary, 10, warm).await;
    }

    for &conc in &args.concurrency {
        if args.warmup > 0.0 {
            run_unary(&unary, conc, Duration::from_secs_f64(args.warmup.min(1.0))).await;
        }
        let (n, errors, elapsed, lats) =
            run_unary(&unary, conc, Duration::from_secs_f64(args.duration)).await;
        println!(
            "RESULT {{\"kind\":\"unary\",\"mode\":\"{}\",\"conc\":{},\"calls\":{},\"errors\":{},\"secs\":{:.3},\"rps\":{:.1},\"p50_ms\":{:.3},\"p90_ms\":{:.3},\"p99_ms\":{:.3}}}",
            args.mode,
            conc,
            n,
            errors,
            elapsed,
            n as f64 / elapsed,
            pct(&lats, 0.50),
            pct(&lats, 0.90),
            pct(&lats, 0.99),
        );
    }

    if !args.skip_streams {
        for &frames in &args.stream_frames {
            // Repeat until ~2s spent (min 3 iterations), report the median rate.
            let mut rates = Vec::new();
            let budget = Instant::now();
            while rates.len() < 3
                || (budget.elapsed() < Duration::from_secs(2) && rates.len() < 200)
            {
                let t = Instant::now();
                let mut s = client
                    .generate(stream_request(frames))
                    .await
                    .expect("generate");
                let mut got = 0;
                while let Some(r) = s.recv().await {
                    r.expect("stream item");
                    got += 1;
                }
                assert_eq!(got, frames, "server-stream frame count");
                rates.push(frames as f64 / t.elapsed().as_secs_f64());
            }
            println!(
                "RESULT {{\"kind\":\"server_stream\",\"frames\":{},\"iterations\":{},\"frames_per_s\":{:.1}}}",
                frames,
                rates.len(),
                median(rates)
            );
        }
        for &n in &args.client_stream {
            let mut rates = Vec::new();
            for _ in 0..3 {
                let reqs: Vec<EchoRequest> = (0..n).map(|_| stream_request(1)).collect();
                let t = Instant::now();
                let r = client.ingest(reqs).await.expect("ingest");
                assert_eq!(r.count as usize, n, "client-stream count");
                rates.push(n as f64 / t.elapsed().as_secs_f64());
            }
            println!(
                "RESULT {{\"kind\":\"client_stream\",\"messages\":{},\"iterations\":3,\"msgs_per_s\":{:.1}}}",
                n,
                median(rates)
            );
        }
    }
    node.shutdown().await;
}
