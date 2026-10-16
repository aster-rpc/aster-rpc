//! Node network settings for running inside a cluster (Kasm A6): an IPv6
//! bind port pin, advertised external addresses, and discovery presets.
//!
//! Relays are disabled throughout, so nothing here reaches the network beyond
//! loopback.

use std::net::{Ipv6Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster_transport_core::network_config::{discovery_plan, DiscoveryPlan};
use aster_transport_core::{CoreEndpointConfig, CoreNetClient};
use iroh::address_lookup::memory::MemoryLookup;
use iroh::address_lookup::{AddressLookup, EndpointData};
use iroh::{EndpointAddr, TransportAddr};
use tokio::net::UdpSocket;

const ALPN: &[u8] = b"aster.test/cluster-network/1";
const BUDGET: Duration = Duration::from_secs(15);

fn direct(extra: impl FnOnce(&mut CoreEndpointConfig)) -> CoreEndpointConfig {
    let mut config = CoreEndpointConfig {
        relay_mode: Some("disabled".into()),
        alpns: vec![ALPN.to_vec()],
        bind_addr: Some("127.0.0.1:0".into()),
        discovery: Some("none".into()),
        portmapper_config: Some("disabled".into()),
        ..Default::default()
    };
    extra(&mut config);
    config
}

fn free_v6_loopback_port() -> Option<u16> {
    let sock = StdUdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).ok()?;
    Some(sock.local_addr().ok()?.port())
}

#[tokio::test]
async fn bind_addr_v6_pins_the_ipv6_port() {
    let Some(port) = free_v6_loopback_port() else {
        eprintln!("skipping: no IPv6 loopback");
        return;
    };
    let v6: SocketAddr = (Ipv6Addr::LOCALHOST, port).into();
    let node = CoreNetClient::create_with_config(direct(|c| {
        c.bind_addr_v6 = Some(v6.to_string());
    }))
    .await
    .expect("start");
    let bound = node.endpoint.bound_sockets();
    assert!(bound.contains(&v6), "{bound:?} lacks {v6}");
    assert!(bound.iter().any(|a| a.is_ipv4()), "{bound:?}");
    node.endpoint.close().await;
}

#[tokio::test]
async fn bind_addr_v6_rejects_bad_combinations() {
    for (v4, v6) in [
        ("127.0.0.1:0", "127.0.0.1:0"),
        ("[::1]:0", "[::1]:0"),
        ("127.0.0.1:0", "not an address"),
    ] {
        let result = CoreNetClient::create_with_config(direct(|c| {
            c.bind_addr = Some(v4.into());
            c.bind_addr_v6 = Some(v6.into());
        }))
        .await;
        assert!(result.is_err(), "bind_addr={v4} bind_addr_v6={v6}");
    }
}

/// An address lookup that records what the endpoint publishes.
#[derive(Debug, Default)]
struct Recorder(Mutex<Vec<EndpointData>>);

impl AddressLookup for Recorder {
    fn publish(&self, data: &EndpointData) {
        self.0.lock().unwrap().push(data.clone());
    }
}

async fn wait_published(recorder: &Recorder, addr: SocketAddr) -> EndpointData {
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        if let Some(data) = recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|d| d.ip_addrs().any(|a| *a == addr))
        {
            return data.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{addr} never published"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A one-client UDP forwarder: what a NodePort or load balancer does to the
/// node's traffic. Returns the front address and a count of the datagrams
/// forwarded to the node.
async fn udp_forwarder(target: SocketAddr) -> (SocketAddr, Arc<AtomicUsize>) {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let front_addr = front.local_addr().unwrap();
    let client = Arc::new(Mutex::new(None::<SocketAddr>));
    let forwarded = Arc::new(AtomicUsize::new(0));
    {
        let (front, back, client) = (front.clone(), back.clone(), client.clone());
        let forwarded = forwarded.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, from)) = front.recv_from(&mut buf).await {
                *client.lock().unwrap() = Some(from);
                forwarded.fetch_add(1, Ordering::Relaxed);
                let _ = back.send_to(&buf[..n], target).await;
            }
        });
    }
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok(n) = back.recv(&mut buf).await {
            let to = *client.lock().unwrap();
            if let Some(to) = to {
                let _ = front.send_to(&buf[..n], to).await;
            }
        }
    });
    (front_addr, forwarded)
}

/// An advertised address is published with the discovered ones, and a peer
/// that resolves the node through discovery can reach it on that address
/// alone.
#[tokio::test]
async fn advertised_address_is_published_and_dialable() {
    // A listens on loopback; the forwarder stands in for the NodePort.
    let a_probe = CoreNetClient::create_with_config(direct(|_| {}))
        .await
        .unwrap();
    // Reserve A's port up front so the forwarder can target it.
    let a_bound = a_probe
        .endpoint
        .bound_sockets()
        .into_iter()
        .find(|s| s.is_ipv4())
        .unwrap();
    a_probe.endpoint.close().await;
    drop(a_probe);
    let (forwarded, datagrams) = udp_forwarder(a_bound).await;

    let a = CoreNetClient::create_with_config(direct(|c| {
        c.bind_addr = Some(a_bound.to_string());
        c.advertise_addrs = vec![forwarded.to_string()];
        c.clear_ip_transports = true;
    }))
    .await;
    // clear_ip_transports with advertise_addrs is refused: there would be no
    // socket to receive on.
    assert!(a.is_err());

    let a = CoreNetClient::create_with_config(direct(|c| {
        c.bind_addr = Some(a_bound.to_string());
        c.advertise_addrs = vec![forwarded.to_string()];
    }))
    .await
    .expect("start A");
    let recorder = Arc::new(Recorder::default());
    a.endpoint.address_lookup().unwrap().add(recorder.clone());
    let published = wait_published(&recorder, forwarded).await;
    assert!(
        published.ip_addrs().any(|s| s.port() == a_bound.port()),
        "discovered addresses are still published: {published:?}"
    );
    assert!(a.endpoint.addr().ip_addrs().any(|s| *s == forwarded));

    // B resolves A by bare ID through its address lookup, which holds only
    // the advertised address from A's record.
    let b = CoreNetClient::create_with_config(direct(|_| {}))
        .await
        .unwrap();
    let lookup = MemoryLookup::new();
    lookup.add_endpoint_info(EndpointAddr::from_parts(
        a.endpoint.id(),
        [TransportAddr::Ip(forwarded)],
    ));
    b.endpoint.address_lookup().unwrap().add(lookup);

    let server = a.endpoint.clone();
    tokio::spawn(async move {
        if let Some(incoming) = server.accept().await {
            if let Ok(conn) = incoming.await {
                conn.closed().await;
            }
        }
    });
    let conn = tokio::time::timeout(BUDGET, b.endpoint.connect(a.endpoint.id(), ALPN))
        .await
        .expect("dial within budget")
        .expect("dial A via its advertised address");
    assert_eq!(conn.remote_id(), a.endpoint.id());
    assert!(
        datagrams.load(Ordering::Relaxed) > 0,
        "the dial went through the advertised (forwarded) address"
    );
    conn.close(0u32.into(), b"done");
}

#[tokio::test]
async fn discovery_presets_install_only_the_configured_services() {
    let services = |extra: fn(&mut CoreEndpointConfig)| async move {
        let config = direct(extra);
        let plan = discovery_plan(&config).unwrap();
        let node = CoreNetClient::create_with_config(config).await.unwrap();
        let count = node.endpoint.address_lookup().unwrap().len();
        node.endpoint.close().await;
        (plan, count)
    };

    // `default` is not started here: it would publish to n0's servers.
    let mut default = direct(|_| {});
    default.discovery = Some("default".into());
    assert_eq!(discovery_plan(&default).unwrap(), DiscoveryPlan::Default);

    let (plan, count) = services(|c| c.discovery = Some("none".into())).await;
    assert_eq!((plan.clone(), count), (DiscoveryPlan::None, 0));
    assert!(plan.hosts().is_empty());

    let (plan, count) = services(|c| {
        c.discovery = None;
        c.discovery_pkarr_relays = vec!["http://127.0.0.1:9/pkarr".into()];
        c.discovery_dns_origin = Some("dns.example.internal".into());
    })
    .await;
    assert_eq!(count, 3, "one pkarr publisher + resolver, one DNS lookup");
    assert_eq!(count, plan.service_count());
    for host in plan.hosts() {
        assert!(
            !host.contains("iroh.link") && !host.contains("n0"),
            "{host}"
        );
    }
}
