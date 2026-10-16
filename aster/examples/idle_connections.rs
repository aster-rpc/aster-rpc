//! Idle-connection cost: server RSS per connection and idle CPU.
//!
//! Brings up one `AsterServer` in a child process (so its RSS and CPU time
//! are its own), then opens N `aster/1` connections to it from a few client
//! nodes in this process (loopback, relays off), makes one RPC on each
//! (`aster.ops.NodeInfo.describe`, so each connection has a pooled stream as a
//! real client's would), lets them sit idle, and reports:
//!
//! - server RSS before connecting, after connecting and settling, and the
//!   delta per connection;
//! - server CPU% while idle over the window (CPU time from `ps`);
//! - how many connections were still open at the end.
//!
//! ```bash
//! cargo run --release -p aster --example idle_connections -- \
//!     --conns 100,500,1000,2000 --idle-secs 60 --profile default
//! cargo run --release -p aster --example idle_connections -- \
//!     --conns 100,500,1000,2000 --idle-secs 60 --profile small
//! ```
//!
//! Profiles (server side; clients use defaults):
//! - `default`: stock `TransportConfig`.
//! - `small`: per-stream receive window 64 KiB, connection receive window
//!   256 KiB, send window 256 KiB, 16 bidi / 4 uni concurrent streams.
//!
//! `--no-call` connects without making an RPC (no pooled stream), to separate
//! the connection's own cost from its idle stream's.
//!
//! Loopback has no loss, no reordering and microsecond RTTs, so it shows the
//! resting cost of state, not buffer growth under load; see
//! `docs/_internal/investigations/2026-10-16-idle-connection-cost.md`.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use aster::rpc::baseline::NodeInfoClient;
use aster::rpc::{AsterServer, RpcConnection};
use aster::{AsterConfig, Node, RelayMode, Ticket};
use tokio::io::{AsyncBufReadExt, BufReader};

const CONNS_PER_CLIENT_NODE: usize = 250;

struct Args {
    role_server: bool,
    profile: String,
    conns: Vec<usize>,
    idle_secs: u64,
    /// Connect only: no RPC, so no pooled stream (`--no-call`).
    no_call: bool,
}

fn args() -> Result<Args> {
    let mut a = Args {
        role_server: false,
        profile: "default".into(),
        conns: vec![100, 500, 1000, 2000],
        idle_secs: 60,
        no_call: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().context("missing value");
        match k.as_str() {
            "--role" => a.role_server = v()? == "server",
            "--profile" => a.profile = v()?,
            "--conns" => {
                a.conns = v()?
                    .split(',')
                    .map(|s| s.trim().parse())
                    .collect::<Result<_, _>>()?
            }
            "--idle-secs" => a.idle_secs = v()?.parse()?,
            "--no-call" => a.no_call = true,
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

fn base_config() -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
}

fn server_config(profile: &str) -> Result<AsterConfig> {
    let b = base_config();
    Ok(match profile {
        "default" => b.build(),
        "small" => b
            .transport_stream_receive_window(64 * 1024)
            .transport_receive_window(256 * 1024)
            .transport_send_window(256 * 1024)
            .transport_max_concurrent_bidi_streams(16)
            .transport_max_concurrent_uni_streams(4)
            .build(),
        other => bail!("unknown profile {other}"),
    })
}

async fn run_server(profile: &str) -> Result<()> {
    let srv = AsterServer::builder()
        .config(server_config(profile)?)
        .start()
        .await?;
    // Wait for a direct address so the ticket is dialable.
    for _ in 0..100 {
        if !srv.node().addr().direct_addresses.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("ADDR={}", srv.address()?);
    // Serve until the parent kills us (or closes stdin).
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(_)) = stdin.next_line().await {}
    srv.shutdown().await;
    Ok(())
}

/// (RSS KiB, CPU seconds) of `pid`, from `ps`.
fn sample(pid: u32) -> Result<(u64, f64)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()?;
    let text = String::from_utf8(out.stdout)?;
    let mut parts = text.split_whitespace();
    let rss: u64 = parts.next().context("rss")?.parse()?;
    let time = parts.next().context("time")?;
    Ok((rss, parse_cpu_time(time)?))
}

/// `[[dd-]hh:]mm:ss[.ff]` → seconds.
fn parse_cpu_time(s: &str) -> Result<f64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>()?, r),
        None => (0.0, s),
    };
    let mut secs = 0.0;
    for part in rest.split(':') {
        secs = secs * 60.0 + part.parse::<f64>()?;
    }
    Ok(days * 86400.0 + secs)
}

struct Row {
    conns: usize,
    open_at_end: usize,
    rss_base_kib: u64,
    rss_conn_kib: u64,
    rss_end_kib: u64,
    cpu_pct: f64,
    connect_secs: f64,
}

async fn measure(profile: &str, n: usize, idle: Duration, call: bool) -> Result<Row> {
    let exe = std::env::current_exe()?;
    let mut child = tokio::process::Command::new(exe)
        .args(["--role", "server", "--profile", profile])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().context("child pid")?;
    let mut lines = BufReader::new(child.stdout.take().context("stdout")?).lines();
    let ticket = loop {
        let line = lines.next_line().await?.context("server exited early")?;
        if let Some(t) = line.strip_prefix("ADDR=") {
            break t.to_string();
        }
    };
    let addr = Ticket::from_base58(&ticket)?.to_node_addr()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (rss_base_kib, _) = sample(pid)?;

    let nodes_needed = n.div_ceil(CONNS_PER_CLIENT_NODE);
    let mut nodes = Vec::with_capacity(nodes_needed);
    for _ in 0..nodes_needed {
        let node = Node::start(base_config().build()).await?;
        node.add_node_addr(addr.clone())?;
        nodes.push(node);
    }
    let started = Instant::now();
    let mut conns: Vec<RpcConnection> = Vec::with_capacity(n);
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..n {
        let node = nodes[i / CONNS_PER_CLIENT_NODE].clone();
        let id = addr.node_id.clone();
        tasks.spawn(async move {
            let conn = node.rpc_connect(&id).await?;
            if call {
                NodeInfoClient::new(conn.clone()).describe().await?;
            }
            anyhow::Ok(conn)
        });
        // Bounded fan-out: at most 64 handshakes in flight.
        while tasks.len() >= 64 {
            conns.push(tasks.join_next().await.context("join")???);
        }
    }
    while let Some(r) = tasks.join_next().await {
        conns.push(r??);
    }
    let connect_secs = started.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (rss_conn_kib, cpu0) = sample(pid)?;
    let t0 = Instant::now();
    tokio::time::sleep(idle).await;
    let (rss_end_kib, cpu1) = sample(pid)?;
    let cpu_pct = (cpu1 - cpu0) / t0.elapsed().as_secs_f64() * 100.0;
    let open_at_end = conns.iter().filter(|c| c.is_open()).count();

    drop(conns);
    for node in nodes {
        node.shutdown().await;
    }
    drop(child);
    Ok(Row {
        conns: n,
        open_at_end,
        rss_base_kib,
        rss_conn_kib,
        rss_end_kib,
        cpu_pct,
        connect_secs,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = args()?;
    if a.role_server {
        return run_server(&a.profile).await;
    }
    println!(
        "profile={} idle={}s rpc_call={}  (server RSS in KiB; per-conn = (after idle - base) / N)",
        a.profile, a.idle_secs, !a.no_call
    );
    println!("| N | open at end | RSS base | RSS after connect | RSS after idle | KiB/conn | idle CPU % | connect s |");
    println!("|---|---|---|---|---|---|---|---|");
    for n in &a.conns {
        let r = measure(&a.profile, *n, Duration::from_secs(a.idle_secs), !a.no_call).await?;
        let per = (r.rss_end_kib.saturating_sub(r.rss_base_kib)) as f64 / r.conns as f64;
        println!(
            "| {} | {} | {} | {} | {} | {:.1} | {:.2} | {:.1} |",
            r.conns,
            r.open_at_end,
            r.rss_base_kib,
            r.rss_conn_kib,
            r.rss_end_kib,
            per,
            r.cpu_pct,
            r.connect_secs
        );
    }
    Ok(())
}
