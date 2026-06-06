#![cfg(feature = "rpc")]
//! Protocol-level test for the remote-shell shape (see `examples/shell.rs`).
//!
//! No real PTY here — the backend is a deterministic echo "shell" so the test is
//! fast and runs everywhere (including the Windows CI leg, which has no PTY/tmux).
//! It asserts the three things the wire contract must guarantee:
//!   1. bidi data flows both ways, in order,
//!   2. a `Resize` frame is delivered in-band, and
//!   3. the terminating `eof` frame carries an exit code.
//!
//! The resize is made observable by having the backend echo the last resize's
//! `rows` as the exit code — if the Resize frame were dropped, the code is 0.

use std::future::Future;
use std::time::Duration;

use aster::rpc::{async_trait, RequestStream, ResponseSink, RpcConnection, Server, RPC_ALPN};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::time::timeout;

const KIND_DATA: i32 = 0;
const KIND_RESIZE: i32 = 1;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "shell/ShellIn")]
struct ShellIn {
    kind: i32,
    data: Vec<u8>,
    rows: i32,
    cols: i32,
}

impl ShellIn {
    fn data(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: KIND_DATA,
            data: bytes.into(),
            rows: 0,
            cols: 0,
        }
    }
    fn resize(rows: i32, cols: i32) -> Self {
        Self {
            kind: KIND_RESIZE,
            data: Vec::new(),
            rows,
            cols,
        }
    }
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "shell/ShellOut")]
struct ShellOut {
    data: Vec<u8>,
    eof: bool,
    exit_code: i32,
}

#[aster::service(name = "Shell", version = 1)]
trait Shell {
    #[rpc(bidi_stream)]
    async fn open(&self, input: RequestStream<ShellIn>, out: ResponseSink<ShellOut>) -> Result<()>;
}

/// Echoes `Data` straight back; remembers the last `Resize` and reports its
/// `rows` as the exit code when the client closes the input stream.
struct EchoShell;

#[async_trait]
impl Shell for EchoShell {
    async fn open(
        &self,
        mut input: RequestStream<ShellIn>,
        out: ResponseSink<ShellOut>,
    ) -> Result<()> {
        let mut last_rows = 0;
        while let Some(m) = input.recv().await? {
            match m.kind {
                KIND_DATA => out.send(&ShellOut {
                    data: m.data,
                    eof: false,
                    exit_code: 0,
                })?,
                KIND_RESIZE => last_rows = m.rows,
                _ => {}
            }
        }
        out.send(&ShellOut {
            data: Vec::new(),
            eof: true,
            exit_code: last_rows,
        })?;
        Ok(())
    }
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

async fn within<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

#[tokio::test]
async fn shell_bidi_data_resize_and_exit() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    let _h = Server::new(&server)
        .register(ShellServer::new(EchoShell))
        .serve();

    let conn: RpcConnection = within(client_node.rpc_connect(&server.id())).await.unwrap();
    let client = ShellClient::new(conn);

    let program = vec![
        ShellIn::data("hello"),
        ShellIn::resize(40, 100),
        ShellIn::data("world"),
    ];
    let frames = within(within(client.open(program)).await.unwrap().collect())
        .await
        .unwrap();

    // (1)+(2): data echoed back in order, across the interleaved resize frame.
    let echoed: Vec<u8> = frames
        .iter()
        .filter(|f| !f.eof)
        .flat_map(|f| f.data.clone())
        .collect();
    assert_eq!(echoed, b"helloworld");

    // (3): exactly one eof frame, and it proves the Resize(40, …) was delivered.
    let eofs: Vec<_> = frames.iter().filter(|f| f.eof).collect();
    assert_eq!(eofs.len(), 1);
    assert_eq!(eofs[0].exit_code, 40);

    client_node.shutdown().await;
    server.shutdown().await;
}

/// The interactive path: `open_streaming()` hands back a `BidiCall` sender so the
/// client can send a frame, observe its echo, then send the next frame *based on
/// what it saw* — the ping-pong that the prebuilt-`Vec` bidi stub cannot express.
/// This is the capability a live terminal (the `shell` example) is built on.
#[tokio::test]
async fn shell_streaming_is_incremental() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    let _h = Server::new(&server)
        .register(ShellServer::new(EchoShell))
        .serve();

    let conn: RpcConnection = within(client_node.rpc_connect(&server.id())).await.unwrap();
    let client = ShellClient::new(conn);

    // Incremental sender + response stream, both live at once.
    let (sink, mut out) = within(client.open_streaming()).await.unwrap();

    // Ping-pong: each send's echo must arrive before the next send is decided.
    within(sink.send(&ShellIn::data("ping"))).await.unwrap();
    let echo = within(out.recv()).await.unwrap().unwrap();
    assert_eq!(echo.data, b"ping");

    within(sink.send(&ShellIn::data("pong"))).await.unwrap();
    let echo = within(out.recv()).await.unwrap().unwrap();
    assert_eq!(echo.data, b"pong");

    // A resize mid-stream, then close the send side. EchoShell reports the last
    // resize rows as the exit code in its terminating eof frame.
    within(sink.send(&ShellIn::resize(24, 80))).await.unwrap();
    within(sink.finish()).await.unwrap();

    let last = within(out.recv()).await.unwrap().unwrap();
    assert!(last.eof);
    assert_eq!(last.exit_code, 24);
    assert!(within(out.recv()).await.is_none()); // OK trailer → stream end

    client_node.shutdown().await;
    server.shutdown().await;
}
