//! A persistent node re-binds the ports it last bound when they are free
//! (`AsterConfig::reuse_last_port`), so peers' cached direct addresses and a
//! NAT mapping stay valid across a restart.

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::time::Duration;

use aster::{AsterConfig, AsterConfigBuilder, Node, RelayMode, SecretKey};

const RECORD: &str = "bind-ports.json";

fn base() -> AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .secret_key(SecretKey::from_bytes([41; 32]))
}

/// Loopback IPv4 only, so a test holds exactly the port it asserts on.
fn loopback(storage: Option<&Path>) -> AsterConfigBuilder {
    let builder = base().bind_addr("127.0.0.1:0").clear_ip_transports(true);
    match storage {
        Some(dir) => builder.persistent(dir),
        None => builder,
    }
}

async fn start(config: &AsterConfig) -> Node {
    Node::start(config.clone()).await.expect("node starts")
}

fn bound(node: &Node) -> Vec<SocketAddr> {
    node.native_endpoint().bound_sockets()
}

fn v4_port(node: &Node) -> u16 {
    bound(node)
        .iter()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 socket")
        .port()
}

fn v6_port(node: &Node) -> Option<u16> {
    bound(node).iter().find(|a| a.is_ipv6()).map(|a| a.port())
}

fn record(dir: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(dir.join(RECORD)).ok()?;
    Some(serde_json::from_slice(&bytes).expect("record is JSON"))
}

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_record(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(RECORD), body).unwrap();
}

/// Whether `addr` can be bound now, as the endpoint binds it (IPv6 only).
fn bindable(addr: SocketAddr) -> bool {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .unwrap();
    if addr.is_ipv6() {
        socket.set_only_v6(true).unwrap();
    }
    socket.bind(&addr.into()).is_ok()
}

/// Shut a node down and wait until every port it bound can be bound again,
/// so a restart in the same process sees what a new process would.
async fn stop(node: Node) {
    let addrs = bound(&node);
    node.shutdown().await;
    for _ in 0..250 {
        if addrs.iter().all(|a| bindable(*a)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{addrs:?} were not released after shutdown");
}

#[tokio::test]
async fn persistent_node_rebinds_its_last_port() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("node");
    let config = loopback(Some(&data)).build();

    let node = start(&config).await;
    let port = v4_port(&node);
    let rec = record(&data).expect("the bound ports are recorded");
    assert_eq!(rec["schema"], "aster.bind-ports");
    assert_eq!(rec["version"], 1);
    assert_eq!(rec["v4"], port);
    stop(node).await;

    for _ in 0..2 {
        let node = start(&config).await;
        assert_eq!(v4_port(&node), port, "restart re-binds the same port");
        stop(node).await;
    }
}

/// The default bind (`0.0.0.0:0` plus `[::]:0`): each family keeps its port.
#[tokio::test]
async fn default_bind_rebinds_both_families() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("node");
    let config = base().persistent(&data).build();

    let node = start(&config).await;
    let ports = (v4_port(&node), v6_port(&node));
    stop(node).await;

    let node = start(&config).await;
    assert_eq!((v4_port(&node), v6_port(&node)), ports);
    stop(node).await;
}

#[tokio::test]
async fn taken_port_falls_back_to_a_fresh_one_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("node");
    let config = loopback(Some(&data)).build();

    let node = start(&config).await;
    let first = v4_port(&node);
    stop(node).await;

    // Another process takes the port meanwhile.
    let squatter = UdpSocket::bind(("127.0.0.1", first)).unwrap();
    let node = start(&config).await;
    let second = v4_port(&node);
    assert_ne!(second, first, "a taken port falls back");
    assert_eq!(
        record(&data).unwrap()["v4"],
        second,
        "the new port is recorded"
    );
    stop(node).await;
    drop(squatter);

    let node = start(&config).await;
    assert_eq!(
        v4_port(&node),
        second,
        "the next start re-binds the new port"
    );
    stop(node).await;
}

#[tokio::test]
async fn explicit_port_is_never_overridden_or_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("node");
    let recorded = free_port();
    let body = format!(r#"{{"schema":"aster.bind-ports","version":1,"v4":{recorded}}}"#);
    write_record(&data, &body);

    let explicit = free_port();
    let config = base()
        .bind_addr(format!("127.0.0.1:{explicit}"))
        .clear_ip_transports(true)
        .persistent(&data)
        .build();
    let node = start(&config).await;
    assert_eq!(
        v4_port(&node),
        explicit,
        "the explicit port is used as given"
    );
    stop(node).await;
    assert_eq!(
        std::fs::read_to_string(data.join(RECORD)).unwrap(),
        body,
        "an explicit port is never recorded"
    );
}

#[tokio::test]
async fn ephemeral_node_records_nothing_and_rebinds_fresh() {
    let config = loopback(None).build();
    let node = start(&config).await;
    let first = v4_port(&node);
    stop(node).await;
    // Hold the old port: an in-memory node has nothing to reuse, so it must
    // bind a fresh one either way.
    let _held = UdpSocket::bind(("127.0.0.1", first)).unwrap();
    let node = start(&config).await;
    assert_ne!(v4_port(&node), first);
    stop(node).await;
}

#[tokio::test]
async fn corrupt_or_old_records_are_ignored_and_replaced() {
    for body in [
        "{ not json".to_string(),
        format!(
            r#"{{"schema":"aster.bind-ports","version":0,"v4":{}}}"#,
            free_port()
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("node");
        write_record(&data, &body);
        let stale = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["v4"].as_u64());

        let config = loopback(Some(&data)).build();
        let node = start(&config).await;
        let port = v4_port(&node);
        if let Some(stale) = stale {
            assert_ne!(u64::from(port), stale, "an old-schema port is not used");
        }
        let rec = record(&data).expect("a valid record replaces the bad one");
        assert_eq!(rec["version"], 1);
        assert_eq!(rec["v4"], port);
        stop(node).await;
    }
}

#[tokio::test]
async fn opt_out_neither_records_nor_reuses() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("node");
    let recorded = free_port();
    let body = format!(r#"{{"schema":"aster.bind-ports","version":1,"v4":{recorded}}}"#);
    write_record(&data, &body);

    let config = loopback(Some(&data)).reuse_last_port(false).build();
    let node = start(&config).await;
    assert_ne!(v4_port(&node), recorded, "the recorded port is not reused");
    stop(node).await;
    assert_eq!(
        std::fs::read_to_string(data.join(RECORD)).unwrap(),
        body,
        "nothing is recorded"
    );

    // The same opt-out from `[network] reuse_last_port` in aster.toml.
    let toml = dir.path().join("aster.toml");
    std::fs::write(&toml, "[network]\nreuse_last_port = false\n").unwrap();
    assert!(!AsterConfig::from_file(&toml).unwrap().reuse_last_port);
}

/// Two nodes on one data directory are not supported, but they must not
/// fight. The second start never takes the first node's port, and it never
/// gets to record one: it waits on the store lock the first node holds.
#[tokio::test]
async fn second_node_on_the_same_data_dir_does_not_fight() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("node");
    let config = loopback(Some(&data)).build();

    let first = start(&config).await;
    let port = v4_port(&first);
    let before = std::fs::read_to_string(data.join(RECORD)).unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), Node::start(config.clone())).await;
    if let Ok(Ok(second)) = second {
        assert_ne!(v4_port(&second), port, "the second node falls back");
        second.shutdown().await;
    }
    assert_eq!(v4_port(&first), port);
    assert_eq!(
        std::fs::read_to_string(data.join(RECORD)).unwrap(),
        before,
        "the record keeps the first node's port"
    );
    stop(first).await;
}
