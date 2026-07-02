//! Connectivity spike — the **local** (home-NAT) node.
//!
//! Half of the CF-vs-Fly direct-connection spike for `aster-expose-portal-webrtc`
//! (see `docs/_internal/working_ideas/aster-expose-portal-webrtc.md` §11). This
//! node runs on a machine behind a home NAT and acts as the dial target: it
//! starts on the public relay mesh, prints its address, and echoes back whatever
//! a peer sends on a bidi stream. The **edge** node (`probe_edge`) dials it from
//! a Fly.io container and measures whether the path upgrades to **Direct** (UDP
//! hole-punched) or stays **Relay**, plus round-trip latency.
//!
//! Run it (leave it running, copy the `PROBE_ADDR_JSON=` line it prints):
//! ```bash
//! cargo run -p aster --example probe_local
//! ```
//!
//! It does NOT terminate — Ctrl-C to stop.

use std::time::Duration;

use anyhow::{Context, Result};
use aster::{AsterConfig, Node, RelayMode};

/// Same ALPN both ends register/dial. Bespoke protocol, no Aster RPC framing.
const ALPN: &[u8] = b"aster/probe/echo/0";

/// Bound on a single echo request (an SDP offer is a few KB; we send ~4 KB).
const MAX_FRAME: usize = 1 << 20;

#[tokio::main]
async fn main() -> Result<()> {
    // Real relay mesh (RelayMode::Default) + default bind (all interfaces, not
    // loopback) so a remote edge can reach us via relay and then hole-punch.
    let cfg = AsterConfig::builder().relay(RelayMode::Default).build();
    let node = Node::start_with_alpns(cfg, vec![ALPN.to_vec()]).await?;

    let addr = wait_for_relay(&node)
        .await
        .context("node never picked up a home relay — check connectivity")?;

    // Compact one-token handoff: `<node_id>@<relay_url>@<addr,addr>`. Shell-safe
    // (no quoting). We DON'T use the idiomatic `aster1` ticket here because its
    // compact format can only store a relay as an IP:port and *drops* a
    // DNS-hostname relay (which the default relays are) — and Aster has no global
    // (n0/DNS) discovery, only mDNS, so a remote edge can't resolve the relay by
    // node id either. The relay URL is exactly what bootstraps the cross-NAT
    // hole-punch, so it must travel in the token. Direct addrs are a bonus
    // (the public reflexive one may speed the direct path); the relay does the work.
    let relay = addr.relay_url.clone().unwrap_or_default();
    let addrs = addr.direct_addresses.join(",");
    let token = format!("{}@{}@{}", addr.node_id.as_str(), relay, addrs);

    println!("\n┌─ probe_local ready (behind whatever NAT this machine is on) ─");
    println!("│ node id   : {}", addr.node_id.as_str());
    println!("│ relay url : {:?}", addr.relay_url);
    println!("│ direct    : {:?}", addr.direct_addresses);
    println!("│");
    println!("│ Hand this one token to the edge (set as a Fly secret, no quotes needed):");
    println!("│");
    println!("PROBE_TICKET={token}");
    println!("│");
    println!(
        "└─ echoing on ALPN {} — leave running, Ctrl-C to stop ─\n",
        String::from_utf8_lossy(ALPN)
    );

    // Accept loop: every inbound connection gets its own echo task.
    loop {
        let (_alpn, conn) = node.accept().await?;
        let peer = conn.peer();
        println!("[local] connection from {}", peer.as_str());
        tokio::spawn(async move {
            // One echo per bidi stream: read to end, write the same bytes back.
            loop {
                let (send, recv) = match conn.accept_bi().await {
                    Ok(pair) => pair,
                    Err(_) => break, // peer closed
                };
                let data = match recv.read_to_end(MAX_FRAME).await {
                    Ok(d) => d,
                    Err(_) => break,
                };
                if send.write_all(data).await.is_err() {
                    break;
                }
                let _ = send.finish().await;
            }
            println!("[local] connection from {} closed", peer.as_str());
        });
    }
}

/// Poll `node.addr()` until the home relay is assigned (RelayMode::Default
/// connects to the nearest relay asynchronously after start).
async fn wait_for_relay(node: &Node) -> Option<aster::NodeAddr> {
    for _ in 0..200 {
        let addr = node.addr();
        if addr.relay_url.is_some() {
            return Some(addr);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Fall back to whatever we have (direct addrs only) after ~20s.
    let addr = node.addr();
    if addr.relay_url.is_some() || !addr.direct_addresses.is_empty() {
        Some(addr)
    } else {
        None
    }
}
