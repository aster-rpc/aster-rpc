//! Gate 0 admission: an inbound peer is surfaced post-handshake and can be
//! rejected (blocking the connection) or accepted (letting it through).

use aster::{AsterConfig, BlobFormat, Node, RelayMode};
use std::time::Duration;
use tokio::time::timeout;

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

#[tokio::test]
async fn rejected_peer_cannot_download() {
    // Server admits via post-handshake hook and rejects everyone.
    let server = Node::start(cfg(true)).await.unwrap();
    let mut adm = server.take_admission().expect("hooks enabled");
    tokio::spawn(async move {
        while let Some(req) = adm.next_handshake().await {
            req.reject(403, b"denied".to_vec());
        }
    });

    let data = b"secret payload".to_vec();
    let hash = server.blobs().add_bytes(data.clone()).await.unwrap();

    let client = Node::start(cfg(false)).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    // The download must NOT succeed — either it errors or it never completes.
    let res = timeout(
        Duration::from_secs(10),
        client
            .blobs()
            .download_hash(&hash, &server.id(), BlobFormat::Raw),
    )
    .await;
    if let Ok(Ok(bytes)) = res {
        panic!("rejected peer should not have downloaded: {bytes:?}");
    }

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn accepted_peer_can_download() {
    // Server admits via post-handshake hook and accepts everyone.
    let server = Node::start(cfg(true)).await.unwrap();
    let mut adm = server.take_admission().expect("hooks enabled");
    tokio::spawn(async move {
        while let Some(req) = adm.next_handshake().await {
            req.accept();
        }
    });

    let data = b"public payload".to_vec();
    let hash = server.blobs().add_bytes(data.clone()).await.unwrap();

    let client = Node::start(cfg(false)).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    let got = timeout(
        Duration::from_secs(20),
        client
            .blobs()
            .download_hash(&hash, &server.id(), BlobFormat::Raw),
    )
    .await
    .expect("download timed out")
    .expect("accepted peer should download");
    assert_eq!(got, data);

    client.shutdown().await;
    server.shutdown().await;
}
