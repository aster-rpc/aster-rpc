//! Native P2P media carrier over Aster — the shape `portal_desktop` needs
//! (`docs/20260626_NativeP2P_Aster_Transport.md`). Proves that the facade
//! primitives satisfy a `MediaCarrier`-style trait: uni-streams as `AsyncWrite`/
//! `AsyncRead` lanes (Gap 2), unreliable datagrams (Gap 1), and a verbatim bidi
//! control lane.
//!
//! Run it:
//! ```bash
//! cargo run -p aster --example native_carrier
//! ```
//! Two nodes in one process: a host exposes a custom ALPN, a client connects,
//! and a video frame + a control message + a timing ping flow across.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use aster::{AsterConfig, Node, RelayMode};

const ALPN: &[u8] = b"portal/media/1";

// ── Portal's carrier abstraction (trimmed to the bytes that matter) ──────────
// Portal's transport is generic over this; the native path is a third impl.
trait MediaCarrier: Send + Sync + 'static {
    type Uni: AsyncWrite + Unpin + Send + 'static; // video / audio lanes
    type BiSend: AsyncWrite + Unpin + Send + 'static; // control lane (send)
    type BiRecv: AsyncRead + Unpin + Send + 'static; // control lane (recv)

    fn open_uni(&self) -> impl Future<Output = Result<Self::Uni>> + Send;
    fn accept_bi(&self) -> impl Future<Output = Result<(Self::BiSend, Self::BiRecv)>> + Send;
    #[allow(dead_code)] // part of the carrier contract; not exercised in this demo
    fn close(&self);
}

/// The native carrier: Portal's existing wire frames over raw Aster streams.
struct IrohCarrier {
    conn: Arc<aster::Connection>,
}

impl MediaCarrier for IrohCarrier {
    // The facade stream types drop in directly — `aster::SendStream: AsyncWrite`,
    // `aster::RecvStream: AsyncRead`, both `Unpin + Send` (Gap 2).
    type Uni = aster::SendStream;
    type BiSend = aster::SendStream;
    type BiRecv = aster::RecvStream;

    async fn open_uni(&self) -> Result<Self::Uni> {
        Ok(self.conn.open_uni().await?)
    }

    async fn accept_bi(&self) -> Result<(Self::BiSend, Self::BiRecv)> {
        Ok(self.conn.accept_bi().await?)
    }

    fn close(&self) {
        let _ = self.conn.close(0, b"session ended".to_vec());
    }
}

// Portal's pump/control loop is generic over the carrier's stream types, so it
// calls the `AsyncWrite`/`AsyncRead` trait methods (the inherent `aster` methods
// only shadow them on the *concrete* types — never on a generic `W: AsyncWrite`).
async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    w.write_all(data).await?;
    Ok(())
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(buf)
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
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

#[tokio::main]
async fn main() -> Result<()> {
    let host = Node::start_with_alpns(cfg(), vec![ALPN.to_vec()]).await?;
    let client = Node::start(cfg()).await?;
    wait_for_addr(&host).await;
    wait_for_addr(&client).await;
    client.add_peer(&host)?;
    host.add_peer(&client)?;
    let host_id = host.id();

    // ── Host: accept a connection, drive lanes through the carrier ────────────
    let host_node = host.clone();
    let _host_task = tokio::spawn(async move {
        let (_alpn, conn) = host_node.accept().await?;
        let carrier = IrohCarrier {
            conn: Arc::new(conn.clone()),
        };

        // Control lane: the client opens it (matches `accept_bi`); read its
        // raw JSON verbatim (no Aster framing on a custom ALPN).
        let (mut ctl_send, mut ctl_recv) = carrier.accept_bi().await?;
        let ctl = read_all(&mut ctl_recv).await?;
        println!("[host] control received: {}", String::from_utf8_lossy(&ctl));
        write_frame(&mut ctl_send, b"{\"ack\":true}").await?;
        ctl_send.shutdown().await?;

        // Video lane: a uni-stream carrying one opaque frame. The pump just
        // writes bytes through `AsyncWrite` — Aster never parses them.
        let mut video = carrier.open_uni().await?;
        write_frame(&mut video, b"\x01\x00\x00\x00\x10<H.264 keyframe>").await?;
        video.shutdown().await?;

        // Timing ping: echo one unreliable datagram.
        if let Ok(dg) = conn.read_datagram().await {
            let _ = conn.send_datagram(dg);
        }

        conn.closed().await;
        Ok::<_, anyhow::Error>(())
    });

    // ── Client: the native consumer (raw facade, no carrier needed) ───────────
    let conn = client.connect(&host_id, ALPN).await?;

    // Control: open the bidi, send config, read the ack.
    let (ctl_send, ctl_recv) = conn.open_bi().await?;
    ctl_send
        .write_all(br#"{"role":"base","codec":"avc1.640033"}"#.to_vec())
        .await?;
    ctl_send.finish().await?;
    let ack = ctl_recv.read_to_end(4096).await?;
    println!("[client] control ack: {}", String::from_utf8_lossy(&ack));

    // Video: read the uni-stream lane via AsyncRead.
    let mut video = timeout(Duration::from_secs(10), conn.accept_uni()).await??;
    let mut frame = Vec::new();
    AsyncReadExt::read_to_end(&mut video, &mut frame).await?;
    println!("[client] video frame: {} bytes", frame.len());

    // Timing ping: unreliable datagram round-trip (retry; it may be dropped).
    println!("[client] max datagram size: {:?}", conn.max_datagram_size());
    let mut rtt_ok = false;
    for _ in 0..20 {
        conn.send_datagram(b"ping".to_vec())?;
        if let Ok(Ok(_)) = timeout(Duration::from_millis(300), conn.read_datagram()).await {
            rtt_ok = true;
            break;
        }
    }

    assert!(
        frame.ends_with(b"<H.264 keyframe>"),
        "video frame corrupted"
    );
    assert!(ack.starts_with(b"{\"ack\""), "missing control ack");
    assert!(rtt_ok, "timing ping never came back");

    // The client-side assertions above prove the host drove every lane. Exit
    // cleanly (the host task is parked on `conn.closed()` keeping the link up).
    println!("\n✓ video lane, control lane, and timing ping all flowed over Aster");
    std::process::exit(0);
}
