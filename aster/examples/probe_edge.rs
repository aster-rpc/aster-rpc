//! Connectivity spike — the **edge** (prober) node.
//!
//! The other half of the direct-connection spike (see `probe_local` and
//! `docs/_internal/working_ideas/aster-expose-portal-webrtc.md` §11). It dials
//! the `probe_local` node, sends ~4 KB (SDP-offer-sized) echo round-trips on a
//! loop, and reports — per ping — whether the selected QUIC path is **Direct**
//! (UDP hole-punched) or **Relay**, plus the application round-trip latency. The
//! point is to answer: from this host's network (a Fly.io container vs. a CF
//! container vs. local), does Aster reach the home node *directly*, and how much
//! faster is direct than relay?
//!
//! Run it:
//! ```bash
//! # locally (baseline — will go Direct over LAN almost immediately):
//! PROBE_ADDR_JSON='{"node_id":"…","relay_url":"…","direct":[…]}' \
//!   cargo run -p aster --example probe_edge
//!
//! # on Fly: set PROBE_ADDR_JSON as a secret, deploy, `fly logs`.
//! ```
//!
//! Env:
//! - `PROBE_ADDR_JSON` (required) — the line `probe_local` printed.
//! - `PROBE_PINGS` (optional) — stop after N pings and print a summary;
//!   `0`/unset = run forever (rolling summary every 20 pings), for `fly logs`.
//! - `PROBE_INTERVAL_MS` (optional, default 500) — gap between pings.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use aster::{AsterConfig, Node, NodeAddr, NodeId, PathRemote, RelayMode};

const ALPN: &[u8] = b"aster/probe/echo/0";
const PAYLOAD_LEN: usize = 4096; // SDP-offer-sized
const MAX_FRAME: usize = 1 << 20;

#[derive(Default)]
struct Stats {
    relay_us: Vec<u128>,
    direct_us: Vec<u128>,
    first_direct_after_ms: Option<u128>,
    last_class: Option<&'static str>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let target = parse_target().context("parse PROBE_ADDR_JSON")?;
    let pings: u64 = env_num("PROBE_PINGS", 0);
    let interval = Duration::from_millis(env_num("PROBE_INTERVAL_MS", 500));

    println!(
        "[edge] dialing {} (relay {:?})",
        target.node_id.as_str(),
        target.relay_url
    );

    // The edge is a fresh ephemeral node on the public relay mesh.
    let cfg = AsterConfig::builder().relay(RelayMode::Default).build();
    let node = Node::start(cfg).await?;

    // Dial with retry — relay registration on the far side can take a moment.
    let mut conn = connect_with_retry(&node, &target).await?;
    println!(
        "[edge] connected; probing every {} ms\n",
        interval.as_millis()
    );

    let payload = vec![0xABu8; PAYLOAD_LEN];
    let connected_at = Instant::now();
    let mut stats = Stats::default();
    let mut i: u64 = 0;

    loop {
        i += 1;
        // One echo round-trip = open bidi, write payload, read it back.
        let t0 = Instant::now();
        let rtt = match echo_once(&conn, &payload).await {
            Ok(()) => t0.elapsed(),
            Err(e) => {
                println!("[edge] ping {i} failed: {e} — reconnecting");
                conn = connect_with_retry(&node, &target).await?;
                continue;
            }
        };

        // Classify the path that carried it.
        let (class, where_, quic_rtt_us) = classify(&conn);
        record(&mut stats, class, rtt.as_micros(), connected_at);
        log_ping(i, class, where_, rtt, quic_rtt_us, &stats);

        if pings != 0 && i >= pings {
            break;
        }
        if pings == 0 && i % 20 == 0 {
            print_summary(&stats);
        }
        tokio::time::sleep(interval).await;
    }

    print_summary(&stats);
    Ok(())
}

/// One echo round-trip on a fresh bidi stream.
async fn echo_once(conn: &aster::Connection, payload: &[u8]) -> Result<()> {
    let (send, recv) = conn.open_bi().await?;
    send.write_all(payload.to_vec()).await?;
    send.finish().await?;
    let echoed = recv.read_to_end(MAX_FRAME).await?;
    anyhow::ensure!(
        echoed.len() == payload.len(),
        "echo length mismatch: sent {} got {}",
        payload.len(),
        echoed.len()
    );
    Ok(())
}

/// Map the connection's selected path to (class, remote-string, quic-rtt-µs).
fn classify(conn: &aster::Connection) -> (&'static str, String, Option<u64>) {
    match conn.selected_path() {
        Some(p) => match &p.remote {
            PathRemote::Direct(a) => ("direct", a.to_string(), p.rtt_micros),
            PathRemote::Relay(u) => ("relay", u.clone(), p.rtt_micros),
            PathRemote::Other(s) => ("other", s.clone(), p.rtt_micros),
        },
        None => ("none", "-".to_string(), None),
    }
}

fn record(stats: &mut Stats, class: &'static str, us: u128, connected_at: Instant) {
    match class {
        "direct" => {
            if stats.first_direct_after_ms.is_none() {
                stats.first_direct_after_ms = Some(connected_at.elapsed().as_millis());
            }
            stats.direct_us.push(us);
        }
        "relay" => stats.relay_us.push(us),
        _ => {}
    }
    if stats.last_class != Some(class) {
        if let Some(prev) = stats.last_class {
            println!("    ── path changed: {prev} → {class} ──");
        }
        stats.last_class = Some(class);
    }
}

fn log_ping(
    i: u64,
    class: &str,
    where_: String,
    rtt: Duration,
    quic_rtt_us: Option<u64>,
    _stats: &Stats,
) {
    let quic = match quic_rtt_us {
        Some(us) => format!("{:.1}ms", us as f64 / 1000.0),
        None => "—".to_string(),
    };
    println!(
        "[edge] ping {i:>4}  path={class:<6} app_rtt={:>6.1}ms  quic_rtt={quic:<7}  via {where_}",
        rtt.as_micros() as f64 / 1000.0
    );
}

fn print_summary(stats: &Stats) {
    println!("\n──────── summary ────────");
    println!(
        "reached direct : {}",
        match stats.first_direct_after_ms {
            Some(ms) => format!("yes, after {ms} ms"),
            None => "NO — relay only".to_string(),
        }
    );
    print_bucket("relay ", &stats.relay_us);
    print_bucket("direct", &stats.direct_us);
    println!("─────────────────────────\n");
}

fn print_bucket(name: &str, samples: &[u128]) {
    if samples.is_empty() {
        println!("{name} app_rtt : (none)");
        return;
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let pct = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)];
    println!(
        "{name} app_rtt : n={:<4} min={:.1}ms  p50={:.1}ms  p90={:.1}ms  max={:.1}ms",
        s.len(),
        s[0] as f64 / 1000.0,
        pct(0.50) as f64 / 1000.0,
        pct(0.90) as f64 / 1000.0,
        s[s.len() - 1] as f64 / 1000.0,
    );
}

async fn connect_with_retry(node: &Node, target: &NodeAddr) -> Result<aster::Connection> {
    let mut delay = Duration::from_millis(500);
    for attempt in 1..=12 {
        match node.connect_addr(target.clone(), ALPN).await {
            Ok(c) => return Ok(c),
            Err(e) => {
                println!(
                    "[edge] connect attempt {attempt} failed: {e} — retrying in {:?}",
                    delay
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(8));
            }
        }
    }
    anyhow::bail!("could not connect after retries")
}

/// Parse the compact `<node_id>@<relay_url>@<addr,addr>` token from
/// `probe_local` (relay and addrs may be empty).
fn parse_target() -> Result<NodeAddr> {
    let raw = std::env::var("PROBE_TICKET")
        .ok()
        .or_else(|| std::env::args().nth(1))
        .context("set PROBE_TICKET env var (or pass it as arg 1)")?;
    let mut parts = raw.trim().splitn(3, '@');
    let node_id = parts
        .next()
        .filter(|s| !s.is_empty())
        .context("missing node_id")?;
    let relay_url = parts.next().filter(|s| !s.is_empty()).map(String::from);
    let direct_addresses = parts
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();
    Ok(NodeAddr {
        node_id: NodeId::from_hex(node_id),
        relay_url,
        direct_addresses,
    })
}

fn env_num(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
