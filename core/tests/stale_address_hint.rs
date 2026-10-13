//! A stale address hint never reaches another node.
//!
//! Pod IPs (and any other socket addresses) are reused. If node A's address
//! hint is stale and the socket now belongs to node B (a different key),
//! dialing A by id must not end up talking to B. Iroh's TLS verifier compares
//! the server's raw Ed25519 public key with the dialed endpoint id
//! (`iroh/src/tls/verifier.rs`), so the handshake fails and B never gets an
//! established connection from that attempt.
//!
//! Relays are disabled and only direct loopback addresses are used, so the
//! only path to anything is the address hint we supply.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster_transport_core::{CoreEndpointConfig, CoreNetClient, CoreNodeAddr};

const ALPN: &[u8] = b"aster.test/stale-hint/1";
const DIAL_BUDGET: Duration = Duration::from_secs(15);
/// Time for B to observe the end of a failed handshake.
const SETTLE: Duration = Duration::from_millis(500);

async fn direct_node() -> CoreNetClient {
    CoreNetClient::create_with_config(CoreEndpointConfig {
        relay_mode: Some("disabled".into()),
        alpns: vec![ALPN.to_vec()],
        bind_addr: Some("127.0.0.1:0".into()),
        ..Default::default()
    })
    .await
    .expect("start direct-only endpoint")
}

fn loopback_addr(node: &CoreNetClient) -> SocketAddr {
    node.endpoint
        .bound_sockets()
        .into_iter()
        .find(|a| a.is_ipv4() && a.ip().is_loopback())
        .expect("endpoint bound to 127.0.0.1")
}

/// What a listening node saw: every incoming handshake attempt, and the
/// remote id of every connection that actually completed.
#[derive(Default)]
struct Observed {
    incoming: usize,
    established: Vec<String>,
}

fn spawn_recorder(node: &CoreNetClient) -> Arc<Mutex<Observed>> {
    let seen = Arc::new(Mutex::new(Observed::default()));
    let endpoint = node.endpoint.clone();
    let record = seen.clone();
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            record.lock().unwrap().incoming += 1;
            let record = record.clone();
            tokio::spawn(async move {
                let Ok(connecting) = incoming.accept() else {
                    return;
                };
                if let Ok(conn) = connecting.await {
                    record
                        .lock()
                        .unwrap()
                        .established
                        .push(conn.remote_id().to_string());
                    // Hold the connection so the dialer can inspect it.
                    conn.closed().await;
                }
            });
        }
    });
    seen
}

fn hint(id: &str, addr: SocketAddr) -> CoreNodeAddr {
    CoreNodeAddr {
        endpoint_id: id.to_string(),
        relay_url: None,
        direct_addresses: vec![addr.to_string()],
    }
}

/// A is not running anywhere; its stale hint points at live node B.
#[tokio::test]
async fn stale_address_hint_never_reaches_another_node() {
    let b = direct_node().await;
    let b_seen = spawn_recorder(&b);
    let b_addr = loopback_addr(&b);
    let dialer = direct_node().await;

    let a_id = iroh::SecretKey::from_bytes(&[0xA5; 32])
        .public()
        .to_string();
    assert_ne!(a_id, b.endpoint_id());

    let attempt = tokio::time::timeout(
        DIAL_BUDGET,
        dialer.connect_node_addr(hint(&a_id, b_addr), ALPN.to_vec()),
    )
    .await;
    match attempt {
        Err(_elapsed) => {}
        Ok(Err(e)) => {
            // Rejected by the identity check, not by some unrelated failure.
            let text = format!("{e:#}");
            assert!(text.contains("invalid peer certificate"), "{text}");
        }
        Ok(Ok(conn)) => panic!(
            "dial of {a_id} via B's address succeeded; remote is {}",
            conn.remote_id()
        ),
    }

    tokio::time::sleep(SETTLE).await;
    {
        let seen = b_seen.lock().unwrap();
        assert!(
            seen.incoming >= 1,
            "the hint was not used: B saw no handshake, so the test proves nothing"
        );
        assert!(
            seen.established.is_empty(),
            "B accepted a connection meant for A: {:?}",
            seen.established
        );
    }

    // Control: B's accept loop is live, so the assertion above is not vacuous.
    let conn = tokio::time::timeout(
        DIAL_BUDGET,
        dialer.connect_node_addr(hint(&b.endpoint_id(), b_addr), ALPN.to_vec()),
    )
    .await
    .expect("control dial timed out")
    .expect("control dial to B by its own id must succeed");
    assert_eq!(conn.remote_id(), b.endpoint_id());
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        b_seen.lock().unwrap().established,
        vec![dialer.endpoint_id()]
    );

    dialer.close().await;
    b.close().await;
}

/// A is alive at another address, but the dialer is handed only B's address.
/// The dial may fail, or reach the real A through an address the dialer
/// already knows; it must never reach B.
#[tokio::test]
async fn stale_address_hint_with_live_target_never_reaches_another_node() {
    let a = direct_node().await;
    let a_seen = spawn_recorder(&a);
    let a_addr = loopback_addr(&a);
    let b = direct_node().await;
    let b_seen = spawn_recorder(&b);
    let b_addr = loopback_addr(&b);
    let a_id = a.endpoint_id();

    // Fresh dialer: knows nothing about A beyond the stale hint.
    // Warm dialer: has already talked to A at its real address.
    let fresh = direct_node().await;
    let warm = direct_node().await;
    let warm_conn = warm
        .connect_node_addr(hint(&a_id, a_addr), ALPN.to_vec())
        .await
        .expect("warm-up dial to A at its real address");
    assert_eq!(warm_conn.remote_id(), a_id);

    for (label, dialer) in [("fresh", &fresh), ("warm", &warm)] {
        let attempt = tokio::time::timeout(
            DIAL_BUDGET,
            dialer.connect_node_addr(hint(&a_id, b_addr), ALPN.to_vec()),
        )
        .await;
        if let Ok(Ok(conn)) = attempt {
            assert_eq!(
                conn.remote_id(),
                a_id,
                "{label} dialer: a dial of A must only ever reach A"
            );
        }
    }

    tokio::time::sleep(SETTLE).await;
    let b_established = b_seen.lock().unwrap().established.clone();
    assert!(
        b_established.is_empty(),
        "B accepted a connection meant for A: {b_established:?}"
    );
    // Whatever A accepted came from the dialers, by their own ids.
    let a_established = a_seen.lock().unwrap().established.clone();
    for remote in &a_established {
        assert!(
            *remote == fresh.endpoint_id() || *remote == warm.endpoint_id(),
            "A accepted an unexpected peer {remote}"
        );
    }

    drop(warm_conn);
    for node in [&fresh, &warm, &a, &b] {
        node.close().await;
    }
}
