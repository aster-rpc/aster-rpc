//! Kasm A6 node network settings through the high-level path
//! (`AsterConfig` -> `Node::start`): the IPv6 port pin, an advertised external
//! address, discovery `none`, and start-up errors for bad relay CA settings.
//! The end-to-end private-CA relay test is `core/tests/relay_private_ca.rs`.

use std::net::{Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use aster::{AsterConfig, AsterConfigBuilder, DiscoveryMode, Node, RelayMode};

const ADVERTISED: &str = "203.0.113.10:31842";

fn base() -> AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .discovery_mode(DiscoveryMode::None)
}

#[tokio::test]
async fn node_publishes_advertised_address_with_discovery_none() {
    let node = Node::start(base().advertise_addr(ADVERTISED).build())
        .await
        .expect("node starts");
    let endpoint = node.native_endpoint();
    let advertised: SocketAddr = ADVERTISED.parse().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !endpoint.addr().ip_addrs().any(|a| *a == advertised) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "advertised address never appeared in {:?}",
            endpoint.addr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Only Aster's own in-process lookups remain; nothing global.
    let lookups = format!("{:?}", endpoint.address_lookup().unwrap());
    assert!(
        !lookups.contains("Pkarr") && !lookups.contains("Dns"),
        "{lookups}"
    );
    node.shutdown().await;
}

#[tokio::test]
async fn node_pins_the_ipv6_port() {
    let Some(port) = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0))
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port())
    else {
        eprintln!("skipping: no IPv6 loopback");
        return;
    };
    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    let node = Node::start(base().bind_addr_v6(v6.to_string()).build())
        .await
        .expect("node starts");
    let bound = node.native_endpoint().bound_sockets();
    assert!(bound.contains(&v6), "{bound:?}");
    node.shutdown().await;
}

#[tokio::test]
async fn bad_relay_ca_settings_fail_start() {
    let missing = base()
        .relay_ca_cert("/nonexistent/aster-relay-ca.pem")
        .build();
    let err = Node::start(missing).await.err().expect("missing CA file");
    assert!(err.to_string().contains("relay_ca_certs"), "{err}");

    let only_without_files = base().relay_ca_only(true).build();
    assert!(Node::start(only_without_files).await.is_err());
}
