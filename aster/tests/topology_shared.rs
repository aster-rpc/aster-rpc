#![cfg(all(feature = "rpc", feature = "docs"))]
//! Topology v2: the shared topology namespace, cluster derivation, bridge
//! agreement, admission filtering, attribution enforcement, and the RPC
//! surface. All nodes run on 127.0.0.1, so every pair measures sub-ms RTT
//! and the swarm must converge to a single cluster.

use std::sync::Arc;
use std::time::Duration;

use aster::grants::{open_namespace_grant, seal_namespace_grant, GrantContext};
use aster::rpc::baseline::{
    SeparatedQuery, TopologyClient, SEPARATED_CONNECTED, SEPARATED_UNKNOWN,
};
use aster::rpc::AsterServer;
use aster::topology::{ClusterView, SeparationVerdict, TopoConfig, TopologySwarmOptions};
use aster::{AsterConfig, NamespaceCapability, NamespaceSecret, NodeId, RelayMode, SecretKey};
use tokio::time::timeout;

use aster_transport_core::topology::records;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .monitoring(true)
        .build()
}

/// Short timings so tests don't wait out the production defaults; min_hold
/// is zero because the hold band still requires a measured sub-enter RTT.
fn opts() -> TopologySwarmOptions {
    TopologySwarmOptions {
        position_refresh: Duration::from_millis(500),
        timing: TopoConfig {
            min_hold: Duration::ZERO,
            liveness_ttl: Duration::from_secs(60),
            ..TopoConfig::default()
        },
        ..Default::default()
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

/// Fully interconnect the servers with held RPC connections so the v1
/// sampler continuously measures every pair.
async fn interconnect(servers: &[AsterServer]) -> Vec<aster::rpc::RpcConnection> {
    for s in servers {
        wait_for_addr(s.node()).await;
    }
    let mut held = Vec::new();
    for i in 0..servers.len() {
        for j in 0..servers.len() {
            if i != j {
                servers[i].node().add_peer(servers[j].node()).unwrap();
            }
        }
    }
    for i in 0..servers.len() {
        for j in (i + 1)..servers.len() {
            let conn = within(servers[i].node().rpc_connect(&servers[j].id()))
                .await
                .unwrap();
            held.push(conn);
        }
    }
    held
}

/// Poll until `f` yields `Some` (view cache is ~1s, publisher 500ms).
async fn converge<T, F>(what: &str, mut f: F) -> T
where
    F: AsyncFnMut() -> Option<T>,
{
    for _ in 0..150 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

fn single_full_cluster(clusters: &[ClusterView], expect: &[String]) -> Option<ClusterView> {
    let c = clusters.iter().find(|c| c.members.len() == expect.len())?;
    let mut want = expect.to_vec();
    want.sort();
    (c.members == want).then(|| c.clone())
}

#[tokio::test]
async fn swarm_converges_with_sealed_grant_distribution() {
    // Root mints the topology namespace secret and seals it per member —
    // the capability-distribution path the design doc prescribes.
    let servers = [
        AsterServer::builder().config(cfg()).start().await.unwrap(),
        AsterServer::builder().config(cfg()).start().await.unwrap(),
        AsterServer::builder().config(cfg()).start().await.unwrap(),
    ];
    let root = &servers[0];
    let topo_secret = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes());
    let namespace_id = topo_secret.id();

    let root_id = root.id();
    for s in &servers {
        let recipient = s.id();
        let ctx = GrantContext {
            app: "aster/topo/v1",
            granter: &root_id,
            recipient: &recipient,
            resource: &namespace_id.to_bytes(),
            path: "/topo/v1/grants",
            role: "write",
        };
        let sealed =
            seal_namespace_grant(&ctx, &NamespaceCapability::write(topo_secret.clone())).unwrap();

        // Each node opens the grant with its own identity secret …
        let identity = s.node().export_secret_key().unwrap();
        let capability = open_namespace_grant(&identity, &ctx, &sealed).unwrap();
        let NamespaceCapability::Write(secret) = capability else {
            panic!("expected write capability");
        };

        // … and joins the swarm with the recovered secret.
        s.topology().enable_shared(&secret, opts()).await.unwrap();
    }

    let _held = interconnect(&servers).await;

    // Convergence: every node independently derives the same single
    // 3-member cluster with the same bridge.
    let member_ids: Vec<String> = servers.iter().map(|s| s.id().to_string()).collect();
    let cluster = converge("all nodes agreeing on one 3-cluster", async || {
        let mut agreed: Option<ClusterView> = None;
        for s in &servers {
            let clusters = s.topology().clusters().await.ok()?;
            let c = single_full_cluster(&clusters, &member_ids)?;
            match &agreed {
                None => agreed = Some(c),
                Some(prev) if *prev == c => {}
                Some(_) => return None, // transient disagreement — keep polling
            }
        }
        agreed
    })
    .await;

    assert!(cluster.members.contains(&cluster.bridge));
    assert_eq!(cluster.witnesses.len(), 3);
    assert_eq!(cluster.witnesses[0], cluster.bridge);

    // my_cluster() agrees with clusters() on every node.
    for s in &servers {
        let mine = s.topology().my_cluster().await.unwrap().unwrap();
        assert_eq!(mine, cluster);
    }

    // Attribution: a member forges records under another identity's path —
    // its own author key can't vouch for them, so readers must drop them.
    let forger = servers[1].node();
    let fake = NodeId::from_hex("0d".repeat(32));
    let doc = forger
        .docs()
        .open_or_import_write_namespace(topo_secret.clone())
        .await
        .unwrap();
    let author = forger.docs().default_author().await.unwrap();
    let forged = records::NetworkPosition {
        node_id: vec![0x0d; 32],
        updated_unix_ms: (std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis()) as i64,
        ..Default::default()
    };
    doc.set_bytes(
        &author,
        records::position_key(fake.as_str()),
        records::encode_position(&forged),
    )
    .await
    .unwrap();

    // Give the forged record time to replicate + a few view refreshes: the
    // fake vertex must never appear, and the real cluster must be intact.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for s in &servers {
        let clusters = s.topology().clusters().await.unwrap();
        assert!(
            clusters
                .iter()
                .all(|c| !c.members.contains(&fake.as_str().to_string())),
            "forged position must not create a vertex"
        );
        assert!(
            single_full_cluster(&clusters, &member_ids).is_some(),
            "real cluster survives the forgery"
        );
    }

    for s in servers {
        s.shutdown().await;
    }
}

#[tokio::test]
async fn admission_filter_excludes_producer_at_read_time() {
    let servers = [
        AsterServer::builder().config(cfg()).start().await.unwrap(),
        AsterServer::builder().config(cfg()).start().await.unwrap(),
        AsterServer::builder().config(cfg()).start().await.unwrap(),
    ];
    let ids: Vec<String> = servers.iter().map(|s| s.id().to_string()).collect();
    let secret = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes());

    // Node 0 refuses to count node 2 as admitted; nodes 1 and 2 accept all.
    let banned = ids[2].clone();
    let filtering = TopologySwarmOptions {
        admitted: Some(Arc::new(move |n: &str| n != banned)),
        ..opts()
    };
    servers[0]
        .topology()
        .enable_shared(&secret, filtering)
        .await
        .unwrap();
    for s in &servers[1..] {
        s.topology().enable_shared(&secret, opts()).await.unwrap();
    }

    let _held = interconnect(&servers).await;

    // Unfiltered node 1 sees all three in one cluster …
    converge("node 1 seeing the full 3-cluster", async || {
        let clusters = servers[1].topology().clusters().await.ok()?;
        single_full_cluster(&clusters, &ids)
    })
    .await;

    // … while node 0's read-time filter yields a 2-cluster without node 2.
    let two = vec![ids[0].clone(), ids[1].clone()];
    let cluster = converge("node 0 deriving the filtered 2-cluster", async || {
        let clusters = servers[0].topology().clusters().await.ok()?;
        if clusters.iter().any(|c| c.members.contains(&ids[2])) {
            return None;
        }
        single_full_cluster(&clusters, &two)
    })
    .await;
    assert!(cluster.members.contains(&cluster.bridge));

    for s in servers {
        s.shutdown().await;
    }
}

#[tokio::test]
async fn topology_rpc_serves_clusters_and_separated() {
    let secret = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes());
    let servers = [
        AsterServer::builder()
            .config(cfg())
            .builtin_topology(true)
            .start()
            .await
            .unwrap(),
        AsterServer::builder().config(cfg()).start().await.unwrap(),
    ];
    for s in &servers {
        s.topology().enable_shared(&secret, opts()).await.unwrap();
    }
    let _held = interconnect(&servers).await;
    let ids: Vec<String> = servers.iter().map(|s| s.id().to_string()).collect();

    // Client-side RPC view of the server's derived clusters.
    let conn = within(servers[1].node().rpc_connect(&servers[0].id()))
        .await
        .unwrap();
    let topo = TopologyClient::new(conn);

    converge("RPC clusters() serving the 2-cluster", async || {
        let list = within(topo.clusters()).await.ok()?;
        let views: Vec<ClusterView> = list
            .clusters
            .iter()
            .map(|c| ClusterView {
                members: c.members.clone(),
                bridge: c.bridge.clone(),
                witnesses: c.witnesses.clone(),
            })
            .collect();
        single_full_cluster(&views, &ids)
    })
    .await;

    // Same-cluster pair → Connected; unknown node → Unknown.
    let verdict = within(topo.separated(SeparatedQuery {
        node_a: ids[0].clone(),
        node_b: ids[1].clone(),
    }))
    .await
    .unwrap();
    assert_eq!(verdict.verdict, SEPARATED_CONNECTED);

    let verdict = within(topo.separated(SeparatedQuery {
        node_a: ids[0].clone(),
        node_b: "0e".repeat(32),
    }))
    .await
    .unwrap();
    assert_eq!(verdict.verdict, SEPARATED_UNKNOWN);

    // In-process separated() agrees.
    let a = servers[0].id();
    let b = servers[1].id();
    assert_eq!(
        servers[0].topology().separated(&a, &b).await.unwrap(),
        SeparationVerdict::Connected
    );

    for s in servers {
        s.shutdown().await;
    }
}
