//! Integration tests for the tunnel primitive — see
//! `ffi_spec/Aster-tunneling.md`.
//!
//! These run the full pipeline: a real QUIC connection between two
//! `CoreNetClient` endpoints, the reactor's first-frame dispatch
//! branching on `FLAG_TUNNEL`, the per-connection ticket registry,
//! and the TCP splice pumping bytes against an in-process echo
//! server.
//!
//! Unit tests for the registry alone live in `core/src/tunnel.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use aster_transport_core::reactor::{create_reactor, ReactorHandle};
use aster_transport_core::tunnel::{TunnelTarget, TunnelTicket};
use aster_transport_core::{CoreConnection, CoreNetClient};

const TEST_ALPN: &[u8] = b"aster-test/tunnel";
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

type ServerConns = Arc<Mutex<Vec<CoreConnection>>>;

async fn start_server_reactor(
    server: CoreNetClient,
) -> (ReactorHandle, tokio::task::JoinHandle<()>, ServerConns) {
    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    // Capture every accepted CoreConnection so the test can call
    // `authorize_tunnel` on it. In real use the binding would expose
    // this via the reactor event stream + a connection table; for an
    // integration test we just stash the live connections in a mutex.
    let conns: ServerConns = Arc::new(Mutex::new(Vec::new()));
    let conns_for_task = conns.clone();
    let accept_task = tokio::spawn(async move {
        loop {
            match server.accept().await {
                Ok(conn) => {
                    conns_for_task
                        .lock()
                        .expect("server conns mutex")
                        .push(conn.clone());
                    feeder.feed(conn);
                }
                Err(_) => return,
            }
        }
    });
    (handle, accept_task, conns)
}

async fn setup_pair() -> Result<(
    CoreConnection,
    CoreNetClient,
    CoreNetClient,
    tokio::task::JoinHandle<()>,
    ReactorHandle,
    ServerConns,
)> {
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();
    let (reactor, accept_task, server_conns) = start_server_reactor(server.clone()).await;
    let conn = client.connect(server_id, TEST_ALPN.to_vec()).await?;
    Ok((conn, server, client, accept_task, reactor, server_conns))
}

async fn wait_for_one_server_conn(server_conns: &ServerConns) -> Result<CoreConnection> {
    for _ in 0..50 {
        if let Some(c) = server_conns
            .lock()
            .expect("server conns mutex")
            .first()
            .cloned()
        {
            return Ok(c);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(anyhow::anyhow!(
        "server reactor never observed an inbound connection"
    ))
}

/// Long-lived TCP echo. Accepts every incoming connection and pipes
/// reads back to the writer until either side closes.
async fn spawn_echo() -> Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut sock, _)) => {
                    tokio::spawn(async move {
                        let (mut r, mut w) = sock.split();
                        let _ = tokio::io::copy(&mut r, &mut w).await;
                    });
                }
                Err(_) => return,
            }
        }
    });
    Ok(addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_round_trip_tcp_echo() -> Result<()> {
    let (client_conn, _server, _client, _accept, _reactor, server_conns) = setup_pair().await?;
    let server_conn = wait_for_one_server_conn(&server_conns).await?;

    // Stand up a TCP echo server and authorize a tunnel to it.
    let echo_addr = spawn_echo().await?;
    let ticket = server_conn.authorize_tunnel(
        vec![TunnelTarget::Tcp { addr: echo_addr }],
        Duration::from_secs(10),
    )?;

    // Client redeems the ticket and exchanges bytes through the tunnel.
    let (send, recv) = timeout(STEP_TIMEOUT, client_conn.open_tunnel(ticket)).await??;
    send.write_all(b"hello tunnel".to_vec()).await?;
    send.finish().await?;

    // Drain the echoed bytes off the QUIC recv stream.
    let echoed = timeout(STEP_TIMEOUT, recv.read_to_end(64 * 1024)).await??;
    assert_eq!(echoed, b"hello tunnel");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_unknown_ticket_closes_stream() -> Result<()> {
    let (client_conn, _server, _client, _accept, _reactor, server_conns) = setup_pair().await?;
    let _server_conn = wait_for_one_server_conn(&server_conns).await?;

    // Fabricate a ticket the server has never issued.
    let ticket = TunnelTicket([0xABu8; 32]);
    let (send, recv) = timeout(STEP_TIMEOUT, client_conn.open_tunnel(ticket)).await??;

    // The server should drop the stream without writing anything.
    let _ = send.write_all(b"never-arrives".to_vec()).await;
    let _ = send.finish().await;
    let drained = timeout(STEP_TIMEOUT, recv.read_to_end(1024)).await?;
    match drained {
        Ok(bytes) => assert!(bytes.is_empty(), "got unexpected bytes back: {bytes:?}"),
        Err(_) => { /* stream was reset — also acceptable */ }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_redeem_is_one_shot() -> Result<()> {
    let (client_conn, _server, _client, _accept, _reactor, server_conns) = setup_pair().await?;
    let server_conn = wait_for_one_server_conn(&server_conns).await?;

    let echo_addr = spawn_echo().await?;
    let ticket = server_conn.authorize_tunnel(
        vec![TunnelTarget::Tcp { addr: echo_addr }],
        Duration::from_secs(10),
    )?;

    // First redemption: drive a full round-trip to *fully* consume
    // the ticket. Critical for ordering — issuing the replay before
    // the server has processed the first redeem leaves the registry
    // in an indeterminate state (whichever stream's tunnel handler
    // calls `registry.redeem` first wins).
    let (send, recv) = timeout(STEP_TIMEOUT, client_conn.open_tunnel(ticket)).await??;
    send.write_all(b"first".to_vec()).await?;
    send.finish().await?;
    let echoed = timeout(STEP_TIMEOUT, recv.read_to_end(64 * 1024)).await??;
    assert_eq!(echoed, b"first");

    // Now the registry has popped the ticket — second redemption must
    // be rejected by the server.
    let (send2, recv2) = timeout(STEP_TIMEOUT, client_conn.open_tunnel(ticket)).await??;
    let _ = send2.write_all(b"replay".to_vec()).await;
    let _ = send2.finish().await;
    let drained = timeout(STEP_TIMEOUT, recv2.read_to_end(1024)).await?;
    match drained {
        Ok(bytes) => assert!(
            bytes.is_empty(),
            "replay must not echo bytes (got: {bytes:?})"
        ),
        Err(_) => { /* stream reset — also acceptable */ }
    }
    Ok(())
}

/// Per-connection isolation — a ticket issued on connection A cannot be
/// redeemed on connection B. The registry is keyed by `CoreConnection`,
/// not anything the peer sends, so a leaked ticket is unredeemable
/// from any other connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_ticket_is_per_connection() -> Result<()> {
    // Stand up one server, two clients.
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client_a = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client_b = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();
    let (_reactor, _accept, server_conns) = start_server_reactor(server.clone()).await;

    let conn_a = client_a
        .connect(server_id.clone(), TEST_ALPN.to_vec())
        .await?;
    let conn_b = client_b.connect(server_id, TEST_ALPN.to_vec()).await?;

    // Wait for both server-side connections to register.
    for _ in 0..50 {
        if server_conns.lock().expect("server conns mutex").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let server_side: Vec<CoreConnection> = server_conns.lock().expect("server conns mutex").clone();
    assert_eq!(server_side.len(), 2, "expected both inbound conns to land");

    let echo_addr = spawn_echo().await?;
    // Authorise a ticket on the server-side connection for client A.
    // Both server-side conns are functionally equivalent; the test
    // only needs to know which connection (server-side) it issued
    // against, and which connection (client-side) it redeems on.
    let ticket = server_side[0].authorize_tunnel(
        vec![TunnelTarget::Tcp { addr: echo_addr }],
        Duration::from_secs(10),
    )?;

    // Pick the OTHER client connection to redeem on. To do that we
    // need to know which server_side conn corresponds to which client.
    // Their identity is the client's endpoint_id; match on that.
    let client_a_id = client_a.endpoint_id().to_string();
    let server_side_for_a = if server_side[0].remote_id() == client_a_id {
        0
    } else {
        1
    };
    let issuer_was_a = server_side_for_a == 0;
    let redeem_on = if issuer_was_a { &conn_b } else { &conn_a };

    let (send, recv) = timeout(STEP_TIMEOUT, redeem_on.open_tunnel(ticket)).await??;
    let _ = send.write_all(b"cross-conn".to_vec()).await;
    let _ = send.finish().await;
    let drained = timeout(STEP_TIMEOUT, recv.read_to_end(1024)).await?;
    match drained {
        Ok(bytes) => assert!(
            bytes.is_empty(),
            "ticket leaked across connections (got: {bytes:?})"
        ),
        Err(_) => { /* stream reset — also acceptable */ }
    }
    Ok(())
}

/// Multi-target ticket: first target is unreachable, second is the
/// echo server. The acceptor must fall through to the second target
/// and produce an echo round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_multi_target_falls_over_to_reachable() -> Result<()> {
    let (client_conn, _server, _client, _accept, _reactor, server_conns) = setup_pair().await?;
    let server_conn = wait_for_one_server_conn(&server_conns).await?;

    let echo_addr = spawn_echo().await?;
    // Pick a port that is *almost certainly* not listening. Port 1 is
    // reserved (TCPMUX) and never bound by tests on a developer box.
    let unreachable: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();

    let ticket = server_conn.authorize_tunnel(
        vec![
            TunnelTarget::Tcp { addr: unreachable },
            TunnelTarget::Tcp { addr: echo_addr },
        ],
        Duration::from_secs(10),
    )?;

    let (send, recv) = timeout(STEP_TIMEOUT, client_conn.open_tunnel(ticket)).await??;
    send.write_all(b"failover wins".to_vec()).await?;
    send.finish().await?;
    let echoed = timeout(STEP_TIMEOUT, recv.read_to_end(64 * 1024)).await??;
    assert_eq!(echoed, b"failover wins");
    Ok(())
}

/// Multi-target ticket where every target is unreachable. The
/// acceptor must close the stream silently — no bytes flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_multi_target_all_fail_closes_stream() -> Result<()> {
    let (client_conn, _server, _client, _accept, _reactor, server_conns) = setup_pair().await?;
    let server_conn = wait_for_one_server_conn(&server_conns).await?;

    let unreachable_a: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let unreachable_b: std::net::SocketAddr = "127.0.0.1:2".parse().unwrap();
    let ticket = server_conn.authorize_tunnel(
        vec![
            TunnelTarget::Tcp {
                addr: unreachable_a,
            },
            TunnelTarget::Tcp {
                addr: unreachable_b,
            },
        ],
        Duration::from_secs(10),
    )?;

    let (send, recv) = timeout(STEP_TIMEOUT, client_conn.open_tunnel(ticket)).await??;
    let _ = send.write_all(b"never-arrives".to_vec()).await;
    let _ = send.finish().await;
    let drained = timeout(STEP_TIMEOUT, recv.read_to_end(1024)).await?;
    match drained {
        Ok(bytes) => assert!(bytes.is_empty(), "unexpected bytes back: {bytes:?}"),
        Err(_) => { /* stream reset — also acceptable */ }
    }
    Ok(())
}

/// Helper used inside `tunnel_round_trip_tcp_echo` to talk to the
/// echo server directly — confirms the echo fixture is correct
/// independent of any tunnel plumbing.
#[tokio::test]
async fn echo_fixture_sanity() -> Result<()> {
    let addr = spawn_echo().await?;
    let mut s = TcpStream::connect(addr).await?;
    s.write_all(b"ping").await?;
    s.shutdown().await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    assert_eq!(buf, b"ping");
    Ok(())
}
