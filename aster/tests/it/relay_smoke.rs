//! Opt-in connectivity check against Iroh's default public relay mesh.
//!
//! This exercises the native Aster/Iroh transport with a dedicated ALPN. It
//! does not exercise `aster-expose` or claim that an application relay works.
//! Run explicitly with:
//!
//! ```text
//! cargo test -p aster --test it relay_smoke:: -- --ignored --nocapture
//! ```

use std::time::Duration;

use aster::{AsterConfig, Connection, Node, PathRemote, RelayMode};
use tokio::time::timeout;

const ALPN: &[u8] = b"aster/relay-smoke/1";
const MAX_PAYLOAD: usize = 1024;
const WAIT: Duration = Duration::from_secs(20);

fn relay_only_config() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Default)
        .clear_ip_transports(true)
        .build()
}

async fn wait_for_relay(node: &Node) {
    timeout(WAIT, async {
        loop {
            if node.addr().relay_url.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("node was not assigned a default relay within 20 seconds");
}

async fn echo_once(conn: &Connection, payload: Vec<u8>) -> aster::Result<Vec<u8>> {
    let (send, recv) = conn.open_bi().await?;
    send.write_all(payload.clone()).await?;
    send.finish().await?;
    let echoed = recv.read_to_end(MAX_PAYLOAD).await?;
    assert_eq!(echoed, payload);
    Ok(echoed)
}

#[tokio::test]
#[ignore = "requires outbound access to the public default Iroh relay mesh"]
async fn default_relay_carries_native_stream_round_trips() {
    let server = timeout(
        WAIT,
        Node::start_with_alpns(relay_only_config(), vec![ALPN.to_vec()]),
    )
    .await
    .expect("server startup bounded")
    .expect("start relay-only server node");
    let client = timeout(WAIT, Node::start(relay_only_config()))
        .await
        .expect("client startup bounded")
        .expect("start relay-only client node");
    wait_for_relay(&server).await;
    wait_for_relay(&client).await;
    eprintln!(
        "assigned relays: server={:?} client={:?}",
        server.addr().relay_url,
        client.addr().relay_url
    );
    client
        .add_node_addr(server.addr())
        .expect("register server relay address");
    let server_for_accept = server.clone();
    let accept_task = tokio::spawn(async move {
        for _ in 0..2 {
            let (alpn, conn) = timeout(WAIT, server_for_accept.accept())
                .await
                .expect("accept deadline")
                .expect("accept relay connection");
            assert_eq!(alpn.as_slice(), ALPN);
            for _ in 0..2 {
                timeout(WAIT, async {
                    let (send, recv) = conn.accept_bi().await.expect("accept request stream");
                    let request = recv.read_to_end(MAX_PAYLOAD).await.expect("read request");
                    send.write_all(request).await.expect("write echo");
                    send.finish().await.expect("finish echo");
                })
                .await
                .expect("server stream deadline");
            }
            timeout(WAIT, conn.closed())
                .await
                .expect("client disconnect observed");
        }
    });
    // A deliberate close of our own connection, followed by a new connection,
    // verifies recovery without changing or faulting the public relay service.
    for cycle in 0..2 {
        let started = tokio::time::Instant::now();
        let conn = timeout(WAIT, client.connect(&server.id(), ALPN.to_vec()))
            .await
            .expect("relay connect deadline")
            .expect("connect through default relay");
        timeout(WAIT, echo_once(&conn, b"unary-style request".to_vec()))
            .await
            .expect("unary-style deadline")
            .expect("unary-style echo");
        let echoed = timeout(WAIT, async {
            let (send, recv) = conn.open_bi().await.expect("open streaming request");
            send.write_all(b"stream-".to_vec())
                .await
                .expect("first chunk");
            send.write_all(b"payload".to_vec())
                .await
                .expect("second chunk");
            send.finish().await.expect("finish streaming request");
            recv.read_to_end(MAX_PAYLOAD).await.expect("streaming echo")
        })
        .await
        .expect("streaming deadline");
        assert_eq!(echoed, b"stream-payload".to_vec());
        let selected = conn.selected_path().expect("selected Iroh path");
        assert!(
            matches!(&selected.remote, PathRemote::Relay(_)),
            "not relay: {:?}",
            selected.remote
        );
        eprintln!(
            "relay cycle {cycle}: {:?}, elapsed={:?}",
            selected.remote,
            started.elapsed()
        );
        conn.close(0, b"relay smoke complete".to_vec())
            .expect("close own connection");
    }
    timeout(WAIT, accept_task)
        .await
        .expect("server task completion deadline")
        .expect("server accept task panicked");
    timeout(WAIT, client.shutdown())
        .await
        .expect("client shutdown deadline");
    timeout(WAIT, server.shutdown())
        .await
        .expect("server shutdown deadline");
}
