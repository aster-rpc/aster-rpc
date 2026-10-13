//! Stalled-probe floods against a replica (INV-E16, INV-E17). Attackers are
//! fresh identities that open enrollment connections and never send a probe.
#![cfg(all(feature = "enrollment", feature = "docs"))]

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::{AsterConfig, Node, RelayMode, SecretKey};
use aster_transport_core::{CoreEndpointConfig, CoreNetClient, CoreNodeAddr};
use tokio::sync::mpsc;

const NORMAL: &[u8] = b"enrollment.application.test/1";
const FLEET: &str = "offline-root-test";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes([seed; 32])
}
fn public(seed: u8) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
}
fn token(seed: u8) -> String {
    issue_jwt(
        &key(1),
        &EnrollmentIssueRequest::new(public(seed), FLEET),
        now(),
    )
    .unwrap()
    .into_jwt()
}
async fn node(seed: u8) -> Node {
    Node::start_with_alpns(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(seed))
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
            .build(),
        vec![ENROLLMENT_ALPN.to_vec(), NORMAL.to_vec()],
    )
    .await
    .unwrap()
}
fn enrollment(
    node: &Node,
    seed: u8,
    path: &std::path::Path,
    options: EnrollmentOptions,
) -> Enrollment {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(path, public(1), [42; 32], FLEET).unwrap()),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(token(seed)),
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        options,
    )
    .unwrap()
}
/// Route enrollment connections to `accept`; deliver admitted ordinary ones.
fn serve(node: Node, enrollment: Enrollment) -> mpsc::Receiver<aster::Connection> {
    let (send, recv) = mpsc::channel(64);
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    let _ = enrollment.accept(&connection).await;
                    connection.closed().await;
                });
            } else {
                let _ = send.send(connection).await;
            }
        }
    });
    recv
}
fn core_addr(node: &Node) -> CoreNodeAddr {
    let addr = node.addr();
    CoreNodeAddr {
        endpoint_id: addr.node_id.to_string(),
        relay_url: addr.relay_url,
        direct_addresses: addr.direct_addresses,
    }
}
/// Fresh-identity endpoints, created ahead of the flood so creation cost does
/// not throttle it.
async fn attackers(count: usize) -> Vec<CoreNetClient> {
    let mut created = tokio::task::JoinSet::new();
    for _ in 0..count {
        created.spawn(CoreNetClient::create_with_config(CoreEndpointConfig {
            relay_mode: Some("disabled".into()),
            bind_addr: Some("127.0.0.1:0".into()),
            alpns: vec![ENROLLMENT_ALPN.to_vec()],
            ..Default::default()
        }));
    }
    created
        .join_all()
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect()
}
/// One stalled enrollment connection from each of `count` fresh identities,
/// opened at `per_second`. The attackers run on their own runtime and threads:
/// in deployment they are remote, and 512 endpoints sharing the replica's
/// scheduler would measure in-process contention, not enrollment. `started`
/// fires when the first connection is opened; the thread returns how many
/// connected.
struct Flood {
    started: tokio::sync::oneshot::Receiver<()>,
    done: std::thread::JoinHandle<usize>,
}

fn flood(count: usize, target: CoreNodeAddr, per_second: u32) -> Flood {
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let done = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let clients = attackers(count).await;
                let mut ticker = tokio::time::interval(Duration::from_secs(1) / per_second);
                let mut started = Some(started);
                let mut opened = Vec::new();
                for client in clients {
                    ticker.tick().await;
                    if let Some(started) = started.take() {
                        let _ = started.send(());
                    }
                    let target = target.clone();
                    opened.push(tokio::spawn(async move {
                        let connection = client
                            .connect_node_addr(target, ENROLLMENT_ALPN.to_vec())
                            .await
                            .ok()?;
                        // Hold it, never sending a probe, until the replica
                        // gives up on it.
                        connection.closed().await;
                        Some(client)
                    }));
                }
                let mut connected = 0;
                for attacker in opened {
                    connected += usize::from(attacker.await.unwrap().is_some());
                }
                connected
            })
    });
    Flood {
        started: started_rx,
        done,
    }
}
/// Time for an admitted peer's new ordinary connection to pass Gate 0.
async fn gate_latency(
    peer: &Node,
    replica: &Node,
    inbound: &mut mpsc::Receiver<aster::Connection>,
    samples: usize,
) -> Vec<Duration> {
    let mut latencies = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let outbound = peer.connect(&replica.id(), NORMAL).await.unwrap();
        let admitted = tokio::time::timeout(Duration::from_secs(5), inbound.recv())
            .await
            .expect("gate check stalled")
            .unwrap();
        latencies.push(started.elapsed());
        drop((outbound, admitted));
    }
    latencies.sort();
    latencies
}
fn p99(sorted: &[Duration]) -> Duration {
    sorted[(sorted.len() * 99).div_ceil(100) - 1]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_probe_flood_neither_starves_joiners_nor_slows_gate_checks() {
    let files = tempfile::tempdir().unwrap();
    let replica = node(2).await;
    let replica_enroll = enrollment(
        &replica,
        2,
        &files.path().join("replica.json"),
        EnrollmentOptions::default(),
    );
    let mut inbound = serve(replica.clone(), replica_enroll.clone());
    let peer = node(5).await;
    let peer_enroll = enrollment(
        &peer,
        5,
        &files.path().join("peer.json"),
        EnrollmentOptions::default(),
    );
    peer_enroll.connect(replica.addr()).await.unwrap();
    peer.add_peer(&replica).unwrap();
    let baseline = gate_latency(&peer, &replica, &mut inbound, 20).await;

    // 512 fresh identities at 100/s, sustained about 5 s, below the default
    // 128/s eviction threshold (256 pending / 2 s).
    let mut flood = flood(512, core_addr(&replica), 100);
    (&mut flood.started).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let loaded = gate_latency(&peer, &replica, &mut inbound, 20).await;

    let joiner = node(6).await;
    let joiner_enroll = enrollment(
        &joiner,
        6,
        &files.path().join("joiner.json"),
        EnrollmentOptions::default(),
    );
    let joined = Instant::now();
    tokio::time::timeout(Duration::from_secs(10), async {
        while joiner_enroll.connect(replica.addr()).await.is_err() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a legitimate joiner was starved by stalled probes");
    let join_time = joined.elapsed();
    let stalled = tokio::task::spawn_blocking(move || flood.done.join().unwrap())
        .await
        .unwrap();
    assert_eq!(stalled, 512);
    eprintln!(
        "baseline p99 {:?}, loaded p99 {:?}, join {:?}, stalled {}",
        p99(&baseline),
        p99(&loaded),
        join_time,
        stalled
    );
    // Loopback latencies are a few milliseconds; allow scheduler noise below
    // 20 ms rather than failing on sub-millisecond jitter.
    assert!(
        p99(&loaded) <= (p99(&baseline) * 2).max(Duration::from_millis(20)),
        "gate checks slowed under flood: baseline {:?}, loaded {:?}",
        p99(&baseline),
        p99(&loaded)
    );
    joiner.shutdown().await;
    peer.shutdown().await;
    replica.shutdown().await;
}

/// Documents the bound rather than hiding it: above
/// `max_pending_probes / first_message_timeout` new connections per second, a
/// joiner whose probe takes longer than the pool takes to turn over is evicted
/// before its probe arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_above_the_eviction_threshold_evicts_slow_probes() {
    let files = tempfile::tempdir().unwrap();
    let replica = node(2).await;
    let replica_enroll = enrollment(
        &replica,
        2,
        &files.path().join("replica.json"),
        EnrollmentOptions {
            max_pending_probes: 8,
            ..Default::default()
        },
    );
    let _inbound = serve(replica.clone(), replica_enroll);
    let target = core_addr(&replica);
    // Threshold 8 / 2 s = 4/s; the flood runs at 100/s.
    let mut flood = flood(64, target.clone(), 100);
    (&mut flood.started).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let slow = attackers(1).await.remove(0);
    let connection = slow
        .connect_node_addr(target, ENROLLMENT_ALPN.to_vec())
        .await
        .unwrap();
    // 8 newer connections arrive within ~80 ms; this probe waits 500 ms.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let closed = tokio::time::timeout(Duration::from_secs(2), connection.closed())
        .await
        .expect("slow probe was not evicted");
    assert_eq!(closed.reason.unwrap(), b"enrollment busy");
    tokio::task::spawn_blocking(move || flood.done.join().unwrap())
        .await
        .unwrap();
    replica.shutdown().await;
}
