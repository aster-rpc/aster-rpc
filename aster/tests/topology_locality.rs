#![cfg(all(feature = "rpc", feature = "docs"))]
//! Declared locality through the shared topology doc: published, verified
//! per node, folded into peer views as a bound, and answered by `locate` for
//! a peer that was never dialed. Verification is stubbed here (the real
//! source, `Enrollment::topology_locality`, is covered by the delegation and
//! enrollment tests); this exercises the swarm plumbing and the ladder.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aster::rpc::AsterServer;
use aster::topology::{
    DeclaredLocality, LadderLevel, LevelSource, LocalitySource, TopoConfig, TopologySwarmOptions,
};
use aster::{AsterConfig, NamespaceSecret, RelayMode, SecretKey};
use tokio::time::timeout;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .monitoring(true)
        .build()
}

/// What the "root" vouches for, by node. A published chain counts only when
/// it is the one issued to that node, standing in for `verify_credential`'s
/// subject binding.
type Roster = Arc<HashMap<String, DeclaredLocality>>;

fn source(self_hex: String, roster: Roster) -> LocalitySource {
    LocalitySource {
        own_chain: Arc::new(move || Some(format!("chain-for-{self_hex}"))),
        verify: Arc::new(move |node_hex: &str, chain: &str| {
            (chain == format!("chain-for-{node_hex}"))
                .then(|| roster.get(node_hex).cloned())
                .flatten()
        }),
    }
}

fn opts(locality: LocalitySource) -> TopologySwarmOptions {
    TopologySwarmOptions {
        position_refresh: Duration::from_millis(500),
        view_refresh: Duration::from_millis(250),
        timing: TopoConfig {
            min_hold: Duration::ZERO,
            liveness_ttl: Duration::from_secs(60),
            ..TopoConfig::default()
        },
        locality: Some(locality),
        ..Default::default()
    }
}

fn zone(region: &str, zone: &str) -> DeclaredLocality {
    DeclaredLocality {
        region: region.into(),
        zone: Some(zone.into()),
    }
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(20), f)
        .await
        .expect("operation timed out")
}

async fn wait_for_addr(n: &aster::Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn converge<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..150 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn declared_zones_bound_levels_and_locate_unmet_peers() {
    let a = AsterServer::builder().config(cfg()).start().await.unwrap();
    let b = AsterServer::builder().config(cfg()).start().await.unwrap();
    let c = AsterServer::builder().config(cfg()).start().await.unwrap();
    let c_id = c.id();
    let roster: Roster = Arc::new(HashMap::from([
        (a.id().to_string(), zone("eu-west-1", "eu-west-1a")),
        (b.id().to_string(), zone("eu-west-1", "eu-west-1a")),
        (c_id.to_string(), zone("eu-west-1", "eu-west-1b")),
    ]));
    let secret = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes());
    let join = |s: &AsterServer| {
        let locality = source(s.id().to_string(), roster.clone());
        let topology = s.topology();
        let secret = secret.clone();
        async move {
            topology
                .enable_shared(&secret, opts(locality))
                .await
                .unwrap()
        }
    };

    // A and C meet. Cross-zone on a loopback path: measured same-host,
    // declared exactly same-region.
    wait_for_addr(a.node()).await;
    wait_for_addr(c.node()).await;
    join(&a).await;
    join(&c).await;
    a.node().add_peer(c.node()).unwrap();
    c.node().add_peer(a.node()).unwrap();
    let ac = within(a.node().rpc_connect(&c_id)).await.unwrap();
    let c_from_a = converge("A's declared view of C", || {
        a.topology()
            .peer(&c_id)
            .filter(|v| v.level_source == LevelSource::Declared)
    })
    .await;
    assert_eq!(c_from_a.level, LadderLevel::SameRegion);
    assert_eq!(
        c_from_a.level_reason,
        "declared same region, different zone"
    );
    assert_eq!(c_from_a.declared_zone.as_deref(), Some("eu-west-1b"));
    let located = a.topology().locate(&c_id).unwrap();
    assert_eq!(located.level, LadderLevel::SameRegion);
    assert_eq!(located.level_source, LevelSource::Declared);

    // C leaves. B joins later and meets only A, so everything it knows about
    // C came through A's copy of the topology doc.
    drop(ac);
    c.shutdown().await;
    wait_for_addr(b.node()).await;
    join(&b).await;
    a.node().add_peer(b.node()).unwrap();
    b.node().add_peer(a.node()).unwrap();
    let _ab = within(a.node().rpc_connect(&b.id())).await.unwrap();

    let c_from_b = converge("B locating C from declarations", || {
        b.topology()
            .locate(&c_id)
            .filter(|e| e.level_source == LevelSource::Declared)
    })
    .await;
    assert_eq!(c_from_b.level, LadderLevel::SameRegion);
    assert_eq!(c_from_b.declared_zone.as_deref(), Some("eu-west-1b"));
    assert!(b.topology().peer(&c_id).is_none(), "B never measured C");

    // Same zone: the closer measurement stands, the declaration is reported.
    let b_from_a = converge("A's view of B with B's declaration", || {
        a.topology()
            .peer(&b.id())
            .filter(|v| v.declared_zone.is_some())
    })
    .await;
    assert_eq!(b_from_a.level, LadderLevel::SameHost);
    assert_eq!(b_from_a.level_source, LevelSource::Measured);
    assert_eq!(b_from_a.declared_region.as_deref(), Some("eu-west-1"));
}

/// A record whose chain was not issued to its publisher is "no declaration":
/// the measured level stands and nothing is reported as declared.
#[tokio::test]
async fn unverified_declarations_are_ignored() {
    let servers = [
        AsterServer::builder().config(cfg()).start().await.unwrap(),
        AsterServer::builder().config(cfg()).start().await.unwrap(),
    ];
    let [a, b] = &servers;
    let roster: Roster = Arc::new(HashMap::from([
        (a.id().to_string(), zone("eu-west-1", "eu-west-1a")),
        (b.id().to_string(), zone("us-east-1", "us-east-1a")),
    ]));
    let secret = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes());
    wait_for_addr(a.node()).await;
    wait_for_addr(b.node()).await;
    a.topology()
        .enable_shared(&secret, opts(source(a.id().to_string(), roster.clone())))
        .await
        .unwrap();
    // B publishes someone else's chain.
    let forged = LocalitySource {
        own_chain: Arc::new(|| Some("chain-for-someone-else".to_string())),
        verify: source(b.id().to_string(), roster.clone()).verify,
    };
    b.topology()
        .enable_shared(&secret, opts(forged))
        .await
        .unwrap();
    a.node().add_peer(b.node()).unwrap();
    b.node().add_peer(a.node()).unwrap();
    let _ab = within(a.node().rpc_connect(&b.id())).await.unwrap();

    // Positive control: B's records have replicated to A (B is a live
    // vertex of A's derived view; its locality record is written in the
    // same beat as its position). Then let a few view refreshes pass.
    let b_hex = b.id().to_string();
    for _ in 0..150 {
        let clusters = a.topology().clusters().await.unwrap();
        if clusters.iter().any(|c| c.members.contains(&b_hex)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let view = converge("A measuring B", || a.topology().peer(&b.id())).await;
    assert_eq!(view.level_source, LevelSource::Measured);
    assert_eq!(
        view.level,
        LadderLevel::SameHost,
        "different region ignored"
    );
    assert!(view.declared_region.is_none());
}
