#![cfg(feature = "attestation")]
//! Custom-ALPN connections + the bootstrap-ALPN admission flow:
//! unknown peer is blocked on a normal ALPN, presents an attestation chain on
//! the admission ALPN, is admitted, and is then allowed on the normal ALPN.

use aster::attestation::{attest_root_node, public_key, verify_chain, AttestOptions, Chain};
use aster::{
    alpns, AsterConfig, Credential, Gate0, Node, NodeId, PublicKey, RelayMode, SecretKey, Ticket,
};
use std::time::Duration;
use tokio::time::timeout;

const ADMISSION_ALPN: &[u8] = alpns::PRODUCER_ADMISSION;
const RPC_ALPN: &[u8] = b"portal.rpc/1";

fn cfg(hooks: bool) -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .hooks(hooks)
        .build()
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Accept one connection and echo a single bi-stream, keeping it alive until read.
fn spawn_echo_once(node: &Node) {
    let srv = node.clone();
    tokio::spawn(async move {
        if let Ok((_alpn, conn)) = srv.accept().await {
            if let Ok((send, recv)) = conn.accept_bi().await {
                let data = recv.read_to_end(1024).await.unwrap_or_default();
                let _ = send.write_all(data).await;
                let _ = send.finish().await;
            }
            conn.closed().await;
        }
    });
}

#[test]
fn gate0_policy_allows_admission_alpns_and_admitted_peers() {
    let gate = Gate0::new();
    let peer = NodeId::from_hex("ab".repeat(32));

    // Admission ALPN: open even for an unknown peer.
    assert!(gate.should_allow(&peer, ADMISSION_ALPN));
    // Normal ALPN: denied until admitted.
    assert!(!gate.should_allow(&peer, RPC_ALPN));
    gate.admit(peer.clone());
    assert!(gate.should_allow(&peer, RPC_ALPN));
    gate.revoke(&peer);
    assert!(!gate.should_allow(&peer, RPC_ALPN));
}

#[tokio::test]
async fn custom_alpn_bidi_round_trip() {
    let server = Node::start_with_alpns(cfg(false), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg(false)).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    // Server: echo one bi-stream.
    let srv = server.clone();
    tokio::spawn(async move {
        if let Ok((_alpn, conn)) = srv.accept().await {
            if let Ok((send, recv)) = conn.accept_bi().await {
                let data = recv.read_to_end(1024).await.unwrap_or_default();
                let _ = send.write_all(data).await;
                let _ = send.finish().await;
            }
            // Keep the connection alive until the client has read the reply.
            conn.closed().await;
        }
    });

    let conn = client.connect(&server.id(), RPC_ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    send.write_all(b"hello".to_vec()).await.unwrap();
    send.finish().await.unwrap();
    let echo = timeout(Duration::from_secs(10), recv.read_to_end(1024))
        .await
        .expect("timed out")
        .unwrap();
    assert_eq!(echo, b"hello");

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn connect_ticket_direct_only() {
    let server = Node::start_with_alpns(cfg(false), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg(false)).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    // No add_peer — the address material comes from a ticket. Relay is disabled,
    // so this exercises the direct-only loopback path through the ticket.
    spawn_echo_once(&server);

    // Aster owns the interpretation of the address token — the caller hands it
    // a NodeAddr and dials straight from the decoded ticket.
    let ticket = Ticket::from_node_addr(&server.addr(), Credential::Open).unwrap();
    let parsed = Ticket::from_base58(&ticket.to_base58().unwrap()).unwrap();

    let conn = client.connect_ticket(&parsed, RPC_ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    send.write_all(b"hi".to_vec()).await.unwrap();
    send.finish().await.unwrap();
    let echo = timeout(Duration::from_secs(10), recv.read_to_end(1024))
        .await
        .expect("timed out")
        .unwrap();
    assert_eq!(echo, b"hi");

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn add_ticket_addr_then_connect_by_id() {
    let server = Node::start_with_alpns(cfg(false), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg(false)).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    spawn_echo_once(&server);

    // Register the server's ticket address material, then dial by the returned
    // node id (no add_peer).
    let ticket = Ticket::from_node_addr(&server.addr(), Credential::Open).unwrap();
    let id = client.add_ticket_addr(&ticket).unwrap();
    assert_eq!(id, server.id());
    let conn = client.connect(&id, RPC_ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    send.write_all(b"yo".to_vec()).await.unwrap();
    send.finish().await.unwrap();
    let echo = timeout(Duration::from_secs(10), recv.read_to_end(1024))
        .await
        .expect("timed out")
        .unwrap();
    assert_eq!(echo, b"yo");

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn bootstrap_alpn_admission_gates_normal_alpn() {
    // Trust anchor.
    let root = SecretKey::from_bytes([5u8; 32]);
    let root_pk: PublicKey = public_key(&root);

    // Server admits via Gate 0 + an attestation handshake on the admission ALPN.
    let server =
        Node::start_with_alpns(cfg(true), vec![ADMISSION_ALPN.to_vec(), RPC_ALPN.to_vec()])
            .await
            .unwrap();
    let gate = Gate0::new(); // admission ALPNs open; everything else gated

    // Gate-0 hook loop.
    let mut adm = server.take_admission().expect("hooks enabled");
    let gate_hook = gate.clone();
    tokio::spawn(async move {
        while let Some(req) = adm.next_handshake().await {
            if gate_hook.should_allow(&req.peer, &req.alpn) {
                req.accept();
            } else {
                req.reject(403, b"not admitted".to_vec());
            }
        }
    });

    // Accept loop: verify chains on the admission ALPN; echo on the RPC ALPN.
    let srv = server.clone();
    let gate_srv = gate.clone();
    tokio::spawn(async move {
        loop {
            let (alpn, conn) = match srv.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            let gate = gate_srv.clone();
            tokio::spawn(async move {
                if alpn == ADMISSION_ALPN {
                    let Ok((send, recv)) = conn.accept_bi().await else {
                        return;
                    };
                    let raw = recv.read_to_end(8192).await.unwrap_or_default();
                    let chain = Chain::from_bytes(raw);
                    let Ok(expected) = PublicKey::from_hex(conn.peer().as_str()) else {
                        return;
                    };
                    let resp: &[u8] = match verify_chain(&chain, &[root_pk], &expected) {
                        Ok(_) => {
                            gate.admit(conn.peer());
                            b"ok"
                        }
                        Err(_) => b"no",
                    };
                    let _ = send.write_all(resp.to_vec()).await;
                    let _ = send.finish().await;
                    conn.closed().await;
                } else if alpn == RPC_ALPN {
                    if let Ok((send, recv)) = conn.accept_bi().await {
                        let data = recv.read_to_end(1024).await.unwrap_or_default();
                        let _ = send.write_all(data).await;
                        let _ = send.finish().await;
                    }
                    conn.closed().await;
                }
            });
        }
    });

    // Client node, attested under the root.
    let client = Node::start(cfg(false)).await.unwrap();
    let client_secret = client.export_secret_key().unwrap();
    let chain = attest_root_node(&root, &client_secret, &AttestOptions::default()).unwrap();

    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    // Phase 1: before admission, the RPC ALPN must NOT complete an echo.
    let pre = timeout(Duration::from_secs(5), async {
        let conn = client.connect(&server.id(), RPC_ALPN).await?;
        let (send, recv) = conn.open_bi().await?;
        send.write_all(b"ping".to_vec()).await?;
        send.finish().await?;
        recv.read_to_end(1024).await
    })
    .await;
    let echoed = matches!(&pre, Ok(Ok(b)) if b.as_slice() == b"ping");
    assert!(!echoed, "unadmitted peer must not complete an RPC echo");

    // Phase 2: present the attestation chain on the admission ALPN → admitted.
    let conn = client.connect(&server.id(), ADMISSION_ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    send.write_all(chain.into_bytes()).await.unwrap();
    send.finish().await.unwrap();
    let verdict = timeout(Duration::from_secs(10), recv.read_to_end(64))
        .await
        .expect("admission timed out")
        .unwrap();
    assert_eq!(verdict, b"ok", "valid chain must be admitted");

    // Phase 3: now the RPC ALPN is allowed (peer admitted) and echoes.
    let conn = client.connect(&server.id(), RPC_ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    send.write_all(b"ping".to_vec()).await.unwrap();
    send.finish().await.unwrap();
    let echo = timeout(Duration::from_secs(10), recv.read_to_end(1024))
        .await
        .expect("rpc timed out")
        .unwrap();
    assert_eq!(
        echo, b"ping",
        "admitted peer must be allowed on the RPC ALPN"
    );

    client.shutdown().await;
    server.shutdown().await;
}

/// Native send streams expose quinn-style send priority, so a custom-ALPN app
/// that multiplexes its own lanes (control/audio ahead of bulk/video) can keep
/// the urgent lane first — the native peer of the WebTransport per-call priority.
#[tokio::test]
async fn send_stream_priority_round_trips() {
    let server = Node::start_with_alpns(cfg(false), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg(false)).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    let srv = server.clone();
    tokio::spawn(async move {
        if let Ok((_alpn, conn)) = srv.accept().await {
            if let Ok((send, recv)) = conn.accept_bi().await {
                let echo = recv.read_to_end(1024).await.unwrap_or_default();
                let _ = send.write_all(echo).await;
                let _ = send.finish().await;
            }
            conn.closed().await;
        }
    });

    let conn = client.connect(&server.id(), RPC_ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();

    // Default priority is 0; set it and read it back on the live send half.
    assert_eq!(send.priority().await.unwrap(), 0);
    send.set_priority(7).await.unwrap();
    assert_eq!(send.priority().await.unwrap(), 7);

    // Writing still works after the priority change.
    send.write_all(b"frame".to_vec()).await.unwrap();
    send.finish().await.unwrap();
    let echo = timeout(Duration::from_secs(10), recv.read_to_end(1024))
        .await
        .expect("echo timed out")
        .unwrap();
    assert_eq!(echo, b"frame");

    client.shutdown().await;
    server.shutdown().await;
}
