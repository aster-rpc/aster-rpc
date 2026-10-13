#![cfg(feature = "binding-support")]
//! The `binding-support` surface (R1): a binding can get from the facade to the
//! transport-core handles it drives directly, and back.

use aster::{alpns, AsterConfig, Node, RelayMode, RPC_ALPN};

#[test]
fn rpc_alpn_is_exported() {
    assert_eq!(RPC_ALPN, b"aster/1");
    assert_eq!(alpns::RPC, RPC_ALPN);
}

#[test]
fn config_lowers_for_bindings() {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .clear_ip_transports(true)
        .alpn(RPC_ALPN.to_vec())
        .build();
    let core = cfg.core_endpoint_config();
    assert_eq!(core.relay_mode.as_deref(), Some("disabled"));
    assert!(core.clear_ip_transports);
    assert_eq!(core.alpns, vec![RPC_ALPN.to_vec()]);
}

#[tokio::test]
async fn node_and_connection_reach_core_and_back() {
    let a = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .alpn(b"bind/1".to_vec())
            .hooks(true)
            .build(),
    )
    .await
    .unwrap();
    let b = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .alpn(b"bind/1".to_vec())
            .build(),
    )
    .await
    .unwrap();
    b.add_peer(&a).unwrap();

    // Node ⇄ CoreNode.
    assert_eq!(a.core_node().node_id(), a.id().to_string());
    let rewrapped = Node::from_core_node(a.core_node().clone());
    assert_eq!(rewrapped.id(), a.id());

    // Raw hook receiver round-trips through Admission. Dropped straight away:
    // a closed hook channel falls back to the (fail-open) failure mode, so the
    // connection below is not held for the hook timeout.
    let admission = a.take_admission().expect("hooks enabled");
    let rx = admission.into_core_receiver();
    drop(aster::Admission::from_core_receiver(rx));

    // Connection ⇄ CoreConnection, on an ALPN registered via the config.
    let accept = tokio::spawn({
        let a = a.clone();
        async move { a.accept().await.map(|(alpn, conn)| (alpn, conn.peer())) }
    });
    let conn = b.connect(&a.id(), b"bind/1".to_vec()).await.unwrap();
    assert_eq!(conn.core_connection().remote_id(), a.id().to_string());
    let rewrapped = aster::Connection::from_core_connection(conn.core_connection().clone());
    assert_eq!(rewrapped.peer(), a.id());
    // Open a stream so the acceptor sees the connection.
    let (send, _recv) = conn.open_bi().await.unwrap();
    send.write_all(b"x").await.unwrap();
    let (alpn, peer) = accept.await.unwrap().unwrap();
    assert_eq!(alpn, b"bind/1");
    assert_eq!(peer, b.id());

    a.shutdown().await;
    b.shutdown().await;
}
