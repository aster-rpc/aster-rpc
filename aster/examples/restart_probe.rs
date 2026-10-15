//! Restart/reconnect probe: one enrolled `AsterServer` that measures how Docs,
//! blob and gossip traffic with a single peer recover after either side
//! restarts.
//!
//! Written for the cross-NAT investigation in
//! `docs/_internal/investigations/2026-10-15-restart-reconnect.md`; the
//! orchestration that drives two of these (one per host) lives next to it in
//! `restart_probe_orchestrate.py`.
//!
//! ```text
//! restart_probe init --out fleet.json
//! restart_probe run  --fleet fleet.json --name mac --peer forge \
//!                    --data DIR --relay on|off [--bind 0.0.0.0:PORT] \
//!                    --role active|passive [--warmup] [--no-docs-serve] \
//!                    --addr-out FILE --peer-addr-file FILE
//! ```
//!
//! `init` generates a root, two node keys (`mac`, `forge`), a credential for
//! each issued by the root, the fleet namespace, a shared document secret and
//! a gossip topic. `run` starts the node the way a deployment does: an
//! `AsterServer` with `[enrollment]`, a persistent data directory and a stable
//! key, so a restart keeps the identity and the stores.
//!
//! Every node writes a heartbeat into the shared document once a second
//! (`hb/<name>`), seeds 10 000 small blobs on first start and broadcasts
//! `<name>:<boot>:<seq>` on the gossip topic. An **active** node additionally,
//! every second and independently per protocol, syncs the document with the
//! peer, fetches the peer's next unseen blob and keeps a gossip subscription
//! bootstrapped from the peer. A **passive** node never dials the peer, so the
//! only traffic between them after its restart is what the active side
//! initiates. `--warmup` makes even a passive node dial once at start (to open
//! a NAT mapping when the peer cannot reach it otherwise): it retries a Docs
//! sync with the peer until one succeeds, then joins gossip through the peer.
//! The active side likewise starts gossip only after its first successful
//! Docs sync or blob fetch, because Iroh Gossip never re-dials a peer whose first dial
//! failed.
//!
//! Events go to stdout as JSON lines (`{"t": <unix ms>, "ev": ...}`); tracing
//! (default `aster::admission` and Iroh path events at debug) goes to stderr.
//! The peer's address book entry is (re)loaded from `--peer-addr-file`
//! whenever that file changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use aster::enrollment::{issue_credential, EnrollmentIssueRequest};
use aster::rpc::AsterServer;
use aster::{
    AsterConfig, BlobFormat, Doc, EnrollmentConfig, GossipEvent, GossipTopic, Hash,
    NamespaceSecret, Node, NodeAddr, NodeId, PublicKey, RelayMode, SecretKey,
};
use serde_json::{json, Value};

const AUDIENCE: &str = "restart-probe";
const SEEDED_BLOBS: u64 = 10_000;
const ROUND: Duration = Duration::from_secs(1);
const OP_TIMEOUT: Duration = Duration::from_secs(15);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// One JSON event line on stdout.
fn emit(ev: &str, fields: Value) {
    let mut line = json!({ "t": now_ms(), "ev": ev });
    if let (Value::Object(line), Value::Object(fields)) = (&mut line, fields) {
        line.extend(fields);
    }
    println!("{line}");
}

fn args() -> (String, HashMap<String, String>) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_default();
    let mut map = HashMap::new();
    let rest: Vec<String> = it.collect();
    let mut i = 0;
    while i < rest.len() {
        let key = rest[i].trim_start_matches("--").to_string();
        if i + 1 < rest.len() && !rest[i + 1].starts_with("--") {
            map.insert(key, rest[i + 1].clone());
            i += 2;
        } else {
            map.insert(key, "true".into());
            i += 1;
        }
    }
    (cmd, map)
}

fn arg<'a>(map: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    map.get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing --{key}"))
}

fn seed() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

fn unhex32(value: &Value) -> Result<[u8; 32]> {
    let s = value
        .as_str()
        .ok_or_else(|| anyhow!("expected hex string"))?;
    hex::decode(s)?
        .try_into()
        .map_err(|_| anyhow!("expected 32 bytes"))
}

fn init(out: &str) -> Result<()> {
    let root = SecretKey::generate();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut nodes = serde_json::Map::new();
    for name in ["mac", "forge"] {
        let key = SecretKey::generate();
        let public = key.public_key().to_bytes();
        let request = EnrollmentIssueRequest::new(public, AUDIENCE, now + 7 * 24 * 3600);
        let chain = issue_credential(&root, &request, now)?.into_chain();
        nodes.insert(
            name.into(),
            json!({ "seed": hex::encode(key.to_bytes()), "id": hex::encode(public), "credential": chain }),
        );
    }
    let fleet = json!({
        "root_public": hex::encode(root.public_key().to_bytes()),
        "audience": AUDIENCE,
        "fleet_namespace": NamespaceSecret::from_bytes(seed()).id().to_string(),
        "doc_secret": hex::encode(seed()),
        "topic": hex::encode(seed()),
        "nodes": nodes,
    });
    std::fs::write(out, serde_json::to_string_pretty(&fleet)?)?;
    println!("wrote {out}");
    Ok(())
}

struct Probe {
    name: String,
    peer_name: String,
    peer: NodeId,
    boot: u64,
    node: Node,
    doc: Doc,
    data: PathBuf,
    topic: [u8; 32],
    /// Last time a gossip message from the peer arrived.
    last_gossip_rx: Mutex<Option<Instant>>,
    gossip: Mutex<Option<GossipTopic>>,
    /// Set on the active side's first successful Docs sync or blob fetch.
    peer_ok_once: std::sync::atomic::AtomicBool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        "warn,restart_probe=info,aster::admission=debug,iroh::_events=debug".into()
    });
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let (cmd, map) = args();
    match cmd.as_str() {
        "init" => init(arg(&map, "out")?),
        "run" => run(&map).await,
        other => bail!("unknown command {other:?}; use init or run"),
    }
}

async fn run(map: &HashMap<String, String>) -> Result<()> {
    let boot = now_ms();
    let fleet: Value = serde_json::from_str(&std::fs::read_to_string(arg(map, "fleet")?)?)?;
    let name = arg(map, "name")?.to_string();
    let peer_name = arg(map, "peer")?.to_string();
    let me = &fleet["nodes"][&name];
    let peer = NodeId::from_hex(
        fleet["nodes"][&peer_name]["id"]
            .as_str()
            .context("peer id")?,
    );
    let data = PathBuf::from(arg(map, "data")?);
    let relay_on = match arg(map, "relay")? {
        "on" => true,
        "off" => false,
        other => bail!("--relay must be on or off, got {other}"),
    };
    let active = match arg(map, "role")? {
        "active" => true,
        "passive" => false,
        other => bail!("--role must be active or passive, got {other}"),
    };
    let warmup = map.contains_key("warmup");
    let addr_out = PathBuf::from(arg(map, "addr-out")?);
    let peer_addr_file = PathBuf::from(arg(map, "peer-addr-file")?);

    let mut builder = AsterConfig::builder()
        .relay(if relay_on {
            RelayMode::Default
        } else {
            RelayMode::Disabled
        })
        .secret_key(SecretKey::from_bytes(unhex32(&me["seed"])?))
        .root_pubkey(PublicKey::from_bytes(unhex32(&fleet["root_public"])?))
        .persistent(data.join("node"))
        .enrollment_credential_chain(me["credential"].as_str().context("credential")?)
        .enrollment(EnrollmentConfig {
            audience: Some(fleet["audience"].as_str().context("audience")?.into()),
            fleet_namespace: Some(
                fleet["fleet_namespace"]
                    .as_str()
                    .context("fleet_namespace")?
                    .into(),
            ),
            ..Default::default()
        });
    if let Some(bind) = map.get("bind") {
        builder = builder.bind_addr(bind.clone());
    }
    let config = builder.build();

    emit(
        "starting",
        json!({ "name": name, "boot": boot, "relay": relay_on, "active": active,
                "warmup": warmup, "bind": map.get("bind"), "pid": std::process::id() }),
    );
    let server = AsterServer::builder().config(config).start().await?;
    let node = server.node().clone();

    // Ready once the endpoint has addresses (and a home relay, if relays are on).
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let addr = node.addr();
        let has_relay = !relay_on || addr.relay_url.is_some();
        if (!addr.direct_addresses.is_empty() && has_relay) || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let addr = node.addr();
    let bound = node.native_endpoint().bound_sockets();
    let addr_json = json!({
        "node_id": addr.node_id.as_str(),
        "relay_url": addr.relay_url,
        "direct_addresses": addr.direct_addresses,
    });
    write_atomic(&addr_out, &serde_json::to_string(&addr_json)?)?;
    emit(
        "ready",
        json!({ "boot": boot, "startup_ms": now_ms() - boot, "addr": addr_json,
                "bound": bound.iter().map(|a| a.to_string()).collect::<Vec<_>>() }),
    );

    let doc = node
        .docs()
        .open_or_import_write_namespace(NamespaceSecret::from_bytes(unhex32(&fleet["doc_secret"])?))
        .await?;
    // Known peer address first, as an application would: serving the document
    // below makes Iroh Docs dial its stored peers straight away.
    let mut last_peer_addr = String::new();
    load_peer_addr(&node, &peer_addr_file, &mut last_peer_addr);
    tokio::spawn(watch_peer_addr(
        node.clone(),
        peer_addr_file,
        last_peer_addr,
    ));
    // Serve the document, as `share` does: Iroh Docs refuses an incoming sync
    // (`NotFound`) for a replica that is not in live-sync mode. Note that this
    // also makes Iroh Docs re-dial the peers it has synced this document with
    // before, so a restarted node reconnects on its own. `--no-docs-serve`
    // skips it, leaving a node that never dials (used for the dial-back case).
    if !map.contains_key("no-docs-serve") {
        doc.start_sync(&[]).await?;
    }
    let probe = Arc::new(Probe {
        name,
        peer_name,
        peer,
        boot,
        node,
        doc,
        data,
        topic: unhex32(&fleet["topic"])?,
        last_gossip_rx: Mutex::new(None),
        gossip: Mutex::new(None),
        peer_ok_once: std::sync::atomic::AtomicBool::new(false),
    });

    tokio::spawn(heartbeat(probe.clone()));
    tokio::spawn(seed_blobs(probe.clone()));
    tokio::spawn(status(probe.clone()));
    tokio::spawn(gossip_broadcast(probe.clone()));
    if active {
        tokio::spawn(docs_loop(probe.clone()));
        tokio::spawn(blob_loop(probe.clone()));
        tokio::spawn(gossip_active(probe.clone()));
    } else if warmup {
        // Warm-up (first start only): dial the peer until a Docs sync succeeds,
        // then join gossip through it. This opens the NAT mapping a NAT'd
        // passive node needs before the peer can reach it.
        let p = probe.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let mut ok = false;
            while !ok && started.elapsed() < Duration::from_secs(90) {
                ok = matches!(
                    tokio::time::timeout(
                        OP_TIMEOUT,
                        p.doc.sync_with_timeout(
                            std::slice::from_ref(&p.peer),
                            Duration::from_secs(10)
                        ),
                    )
                    .await,
                    Ok(Ok(()))
                );
                if !ok {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            emit(
                "warmup_docs",
                json!({ "ok": ok, "ms": started.elapsed().as_millis() as u64 }),
            );
            gossip_passive(p, true).await;
        });
    } else {
        tokio::spawn(gossip_passive(probe.clone(), false));
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    emit("shutdown", json!({ "boot": probe.boot }));
    server.shutdown().await;
    emit("stopped", json!({ "boot": probe.boot }));
    Ok(())
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load the peer's address material into the address book if the file
/// changed since `last`. This is the "address given to the peer directly"
/// channel.
fn load_peer_addr(node: &Node, path: &Path, last: &mut String) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    if content == *last {
        return;
    }
    // A partially written file fails to parse; it is retried next tick.
    let Ok(v) = serde_json::from_str::<Value>(&content) else {
        return;
    };
    let addr = NodeAddr {
        node_id: NodeId::from_hex(v["node_id"].as_str().unwrap_or_default()),
        relay_url: v["relay_url"].as_str().map(str::to_string),
        direct_addresses: v["direct_addresses"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let result = node.add_node_addr(addr);
    emit(
        "peer_addr_added",
        json!({ "addr": v, "ok": result.is_ok() }),
    );
    *last = content;
}

async fn watch_peer_addr(node: Node, path: PathBuf, mut last: String) {
    loop {
        load_peer_addr(&node, &path, &mut last);
        tokio::time::sleep(ROUND).await;
    }
}

async fn heartbeat(p: Arc<Probe>) {
    let author = match p.node.docs().default_author().await {
        Ok(a) => a,
        Err(e) => {
            emit("heartbeat_error", json!({ "err": e.to_string() }));
            return;
        }
    };
    let key = format!("hb/{}", p.name);
    let mut seq = 0u64;
    loop {
        seq += 1;
        let value = format!("{}:{}", p.boot, seq);
        if let Err(e) = p.doc.set_bytes(&author, key.clone(), value).await {
            emit("heartbeat_error", json!({ "err": e.to_string() }));
        }
        tokio::time::sleep(ROUND).await;
    }
}

fn blob_content(owner: &str, i: u64) -> String {
    format!("restart-probe-blob/{owner}/{i}")
}

fn blob_hash(content: &str) -> Hash {
    Hash::from_hex(aster::native::iroh_blobs::Hash::new(content.as_bytes()).to_hex())
}

async fn seed_blobs(p: Arc<Probe>) {
    let marker = p.data.join("blobs-seeded");
    if marker.exists() {
        return;
    }
    let started = Instant::now();
    for i in 0..SEEDED_BLOBS {
        if let Err(e) = p.node.blobs().add_bytes(blob_content(&p.name, i)).await {
            emit("seed_error", json!({ "err": e.to_string() }));
            return;
        }
    }
    let _ = std::fs::write(&marker, b"");
    emit(
        "blobs_seeded",
        json!({ "n": SEEDED_BLOBS, "ms": started.elapsed().as_millis() as u64 }),
    );
}

/// Every 2 s: what the endpoint knows about the peer (all transport addresses
/// and their usage), for path-state evidence.
async fn status(p: Arc<Probe>) {
    let Ok(peer) = p.peer.as_str().parse::<aster::native::iroh::EndpointId>() else {
        return;
    };
    let endpoint = p.node.native_endpoint();
    let mut last = String::new();
    loop {
        let info = endpoint.remote_info(peer).await;
        let own = p.node.addr();
        let line = format!(
            "{:?} | own relay={:?} direct={:?}",
            info, own.relay_url, own.direct_addresses
        );
        if line != last {
            emit("remote_info", json!({ "info": line }));
            last = line;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn docs_loop(p: Arc<Probe>) {
    let key = format!("hb/{}", p.peer_name);
    loop {
        let started = Instant::now();
        let result = tokio::time::timeout(
            OP_TIMEOUT,
            p.doc
                .sync_with_timeout(std::slice::from_ref(&p.peer), Duration::from_secs(10)),
        )
        .await;
        let ms = started.elapsed().as_millis() as u64;
        let err = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some("timeout".into()),
        };
        // The peer's heartbeat entry: its write timestamp is in the peer's
        // clock, so "written after the peer's restart" needs no clock offset.
        if err.is_none() {
            p.peer_ok_once
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let entry = p
            .doc
            .query_latest_exact(key.as_bytes().to_vec())
            .await
            .ok()
            .flatten();
        let peer_ts_ms = entry.as_ref().map(|e| e.timestamp / 1000);
        emit(
            "docs",
            json!({ "ok": err.is_none(), "err": err, "ms": ms, "peer_hb_ts": peer_ts_ms }),
        );
        tokio::time::sleep(ROUND).await;
    }
}

async fn blob_loop(p: Arc<Probe>) {
    let index_file = p.data.join(format!("blob-index-{}", p.peer_name));
    let mut i: u64 = std::fs::read_to_string(&index_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let blobs = p.node.blobs();
    loop {
        let content = blob_content(&p.peer_name, i);
        let hash = blob_hash(&content);
        let started = Instant::now();
        let result = tokio::time::timeout(
            OP_TIMEOUT,
            blobs.download_hash(&hash, &p.peer, BlobFormat::Raw),
        )
        .await;
        let ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(Ok(bytes)) if bytes == content.as_bytes() => {
                emit("blob", json!({ "ok": true, "i": i, "ms": ms }));
                p.peer_ok_once
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                i += 1;
                let _ = std::fs::write(&index_file, i.to_string());
            }
            Ok(Ok(_)) => emit(
                "blob",
                json!({ "ok": false, "i": i, "ms": ms, "err": "content mismatch" }),
            ),
            Ok(Err(e)) => emit(
                "blob",
                json!({ "ok": false, "i": i, "ms": ms, "err": e.to_string() }),
            ),
            Err(_) => emit(
                "blob",
                json!({ "ok": false, "i": i, "ms": ms, "err": "timeout" }),
            ),
        }
        tokio::time::sleep(ROUND).await;
    }
}

async fn gossip_broadcast(p: Arc<Probe>) {
    let mut seq = 0u64;
    loop {
        seq += 1;
        let topic = p.gossip.lock().unwrap().clone();
        if let Some(topic) = topic {
            let msg = format!("{}:{}:{}", p.name, p.boot, seq);
            let _ = topic.broadcast(msg.into_bytes()).await;
        }
        tokio::time::sleep(ROUND).await;
    }
}

/// Receive on a subscription until it fails, logging peer messages and
/// neighbour changes.
fn spawn_receiver(p: Arc<Probe>, topic: GossipTopic, generation: u64) {
    tokio::spawn(async move {
        loop {
            match topic.recv().await {
                Ok(GossipEvent::Received { data }) => {
                    let text = String::from_utf8_lossy(&data).to_string();
                    let mut parts = text.split(':');
                    let from = parts.next().unwrap_or_default().to_string();
                    let boot: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let seq: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    if from == p.peer_name {
                        *p.last_gossip_rx.lock().unwrap() = Some(Instant::now());
                    }
                    emit(
                        "gossip_rx",
                        json!({ "from": from, "boot": boot, "seq": seq, "gen": generation }),
                    );
                }
                Ok(GossipEvent::NeighborUp { peer }) => emit(
                    "gossip_neighbor",
                    json!({ "up": true, "peer": peer.as_str(), "gen": generation }),
                ),
                Ok(GossipEvent::NeighborDown { peer }) => emit(
                    "gossip_neighbor",
                    json!({ "up": false, "peer": peer.as_str(), "gen": generation }),
                ),
                Ok(GossipEvent::Lagged) => {}
                Err(e) => {
                    emit(
                        "gossip_recv_end",
                        json!({ "err": e.to_string(), "gen": generation }),
                    );
                    return;
                }
            }
        }
    });
}

/// The passive side subscribes once, with no bootstrap peers (or with the
/// peer, for the warm-up start), and waits to be joined.
async fn gossip_passive(p: Arc<Probe>, warmup: bool) {
    let peers = if warmup { vec![p.peer.clone()] } else { vec![] };
    match p.node.gossip().subscribe(p.topic, &peers).await {
        Ok(topic) => {
            emit(
                "gossip_subscribed",
                json!({ "bootstrap": warmup, "gen": 0 }),
            );
            *p.gossip.lock().unwrap() = Some(topic.clone());
            spawn_receiver(p.clone(), topic, 0);
        }
        Err(e) => emit("gossip_subscribe_error", json!({ "err": e.to_string() })),
    }
}

/// The active side keeps a subscription bootstrapped from the peer. When no
/// message from the peer has arrived for 8 s it subscribes again (the
/// application-level rejoin a deployment would do): each attempt waits up to
/// 10 s for a neighbour, and a failed one is retried 2 s later.
async fn gossip_active(p: Arc<Probe>) {
    // Iroh Gossip never re-dials a peer after one failed dial (the peer stays
    // `Pending` in its actor), so only start once the peer is known reachable.
    while !p.peer_ok_once.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut generation = 0u64;
    loop {
        let stale = match *p.last_gossip_rx.lock().unwrap() {
            Some(at) => at.elapsed() > Duration::from_secs(8),
            None => true,
        };
        if stale || p.gossip.lock().unwrap().is_none() {
            generation += 1;
            let started = Instant::now();
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                p.node
                    .gossip()
                    .subscribe(p.topic, std::slice::from_ref(&p.peer)),
            )
            .await;
            let ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(Ok(topic)) => {
                    emit(
                        "gossip_subscribed",
                        json!({ "bootstrap": true, "gen": generation, "ms": ms }),
                    );
                    *p.gossip.lock().unwrap() = Some(topic.clone());
                    // Grace: give the new subscription a full window to deliver.
                    *p.last_gossip_rx.lock().unwrap() = Some(Instant::now());
                    spawn_receiver(p.clone(), topic, generation);
                }
                Ok(Err(e)) => emit(
                    "gossip_subscribe_error",
                    json!({ "err": e.to_string(), "gen": generation, "ms": ms }),
                ),
                Err(_) => emit(
                    "gossip_subscribe_error",
                    json!({ "err": "timeout", "gen": generation, "ms": ms }),
                ),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else {
            tokio::time::sleep(ROUND).await;
        }
    }
}
