//! Server reactor -- Rust-driven accept + read loop.
//!
//! The reactor runs entirely on the tokio runtime. It accepts connections,
//! reads StreamHeader + request frames, and delivers fully-read requests
//! to the language binding via an async channel. The binding runs the
//! handler and submits response frames via an mpsc channel, one frame at a
//! time, terminated by a Trailer. The reactor writes each frame to the QUIC
//! stream as it arrives, enabling server-streaming and bidi-streaming RPCs.
//!
//! Two modes:
//! - `start_reactor` / `start_reactor_on`: owns the accept loop (pulls from
//!   `node.accept_aster()`). Good for standalone Server use.
//! - `create_reactor`: returns a handle + feeder. The caller owns the accept
//!   loop and feeds RPC connections via `ReactorFeeder::feed`. Good for
//!   AsterServer integration where the accept loop dispatches by ALPN.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::framing::{FLAG_CALL, FLAG_HEADER, MAX_FRAME_SIZE};
use crate::{CoreConnection, CoreNode, CoreRecvStream, CoreSendStream};

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

fn next_call_id() -> u64 {
    NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_stream_id() -> u64 {
    NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct IncomingCall {
    pub call_id: u64,
    /// Reactor-assigned unique id for the QUIC bi-stream this call arrived
    /// on. Stateless (unary / server-stream) calls each get a fresh stream
    /// id because they always open a new bi-stream; session-mode calls on
    /// the same bi-stream share one stream id across multiple calls. Bindings
    /// use `(peer_id, stream_id, service)` as the session key so concurrent
    /// sessions from the same peer don't collapse onto one service instance.
    pub stream_id: u64,
    pub header_payload: Vec<u8>,
    pub header_flags: u8,
    pub request_payload: Vec<u8>,
    pub request_flags: u8,
    pub peer_id: String,
    pub is_session_call: bool,
    pub response_sender: mpsc::UnboundedSender<OutgoingFrame>,
}

/// Outgoing frame emitted by the binding. The reactor writes each `Frame` to
/// the stream as it arrives; `Trailer` is the terminal frame — after writing
/// it the reactor finishes the stream's send side and drops the channel.
///
/// Both variants carry already-framed bytes — `[4B LE len][1B flags][payload]`.
/// The binding is responsible for the framing so the reactor can write bytes
/// opaquely without reaching into the wire format.
pub enum OutgoingFrame {
    Frame(Vec<u8>),
    Trailer(Vec<u8>),
}

pub struct ReactorHandle {
    call_rx: mpsc::Receiver<IncomingCall>,
}

impl ReactorHandle {
    pub async fn next_call(&mut self) -> Option<IncomingCall> {
        self.call_rx.recv().await
    }
}

/// Feed connections into a reactor created with `create_reactor`.
#[derive(Clone)]
pub struct ReactorFeeder {
    call_tx: mpsc::Sender<IncomingCall>,
    rt_handle: tokio::runtime::Handle,
}

impl ReactorFeeder {
    pub fn feed(&self, conn: CoreConnection) {
        let tx = self.call_tx.clone();
        self.rt_handle.spawn(connection_loop(conn, tx));
    }
}

/// Create a reactor without an accept loop. Returns a handle for receiving
/// calls and a feeder for pushing connections from an external accept loop.
pub fn create_reactor(
    handle: &tokio::runtime::Handle,
    channel_capacity: usize,
) -> (ReactorHandle, ReactorFeeder) {
    let (call_tx, call_rx) = mpsc::channel(channel_capacity);
    (
        ReactorHandle { call_rx },
        ReactorFeeder {
            call_tx,
            rt_handle: handle.clone(),
        },
    )
}

/// Start a reactor that owns the accept loop.
pub fn start_reactor(node: CoreNode, channel_capacity: usize) -> ReactorHandle {
    let (call_tx, call_rx) = mpsc::channel(channel_capacity);
    tokio::spawn(accept_loop(node, call_tx));
    ReactorHandle { call_rx }
}

/// Same as `start_reactor` but takes an explicit runtime handle.
pub fn start_reactor_on(
    handle: &tokio::runtime::Handle,
    node: CoreNode,
    channel_capacity: usize,
) -> ReactorHandle {
    let (call_tx, call_rx) = mpsc::channel(channel_capacity);
    handle.spawn(accept_loop(node, call_tx));
    ReactorHandle { call_rx }
}

async fn accept_loop(node: CoreNode, call_tx: mpsc::Sender<IncomingCall>) {
    loop {
        match node.accept_aster().await {
            Ok((_alpn, conn)) => {
                let tx = call_tx.clone();
                tokio::spawn(connection_loop(conn, tx));
            }
            Err(e) => {
                if call_tx.is_closed() {
                    break;
                }
                tracing::warn!("reactor accept error: {}", e);
            }
        }
    }
}

async fn connection_loop(conn: CoreConnection, call_tx: mpsc::Sender<IncomingCall>) {
    let peer_id = conn.remote_id();

    while let Ok((send, recv)) = conn.accept_bi().await {
        let tx = call_tx.clone();
        let peer = peer_id.clone();
        tokio::spawn(handle_stream(send, recv, peer, tx));
    }
}

async fn handle_stream(
    send: CoreSendStream,
    recv: CoreRecvStream,
    peer_id: String,
    call_tx: mpsc::Sender<IncomingCall>,
) {
    if let Err(e) = handle_stream_inner(send, recv, peer_id, call_tx).await {
        tracing::debug!("reactor stream error: {}", e);
    }
}

async fn handle_stream_inner(
    send: CoreSendStream,
    recv: CoreRecvStream,
    peer_id: String,
    call_tx: mpsc::Sender<IncomingCall>,
) -> Result<()> {
    let stream_id = next_stream_id();
    let (header_payload, header_flags) = read_one_frame(&recv).await?;
    if header_flags & FLAG_HEADER == 0 {
        return Err(anyhow!("first frame missing HEADER flag"));
    }

    let (second_payload, second_flags) = read_one_frame(&recv).await?;

    if second_flags & FLAG_CALL != 0 {
        handle_session(
            send,
            recv,
            peer_id,
            stream_id,
            call_tx,
            header_payload,
            second_payload,
            second_flags,
        )
        .await
    } else {
        handle_stateless(
            send,
            peer_id,
            stream_id,
            call_tx,
            header_payload,
            header_flags,
            second_payload,
            second_flags,
        )
        .await
    }
}

async fn handle_stateless(
    send: CoreSendStream,
    peer_id: String,
    stream_id: u64,
    call_tx: mpsc::Sender<IncomingCall>,
    header_payload: Vec<u8>,
    header_flags: u8,
    request_payload: Vec<u8>,
    request_flags: u8,
) -> Result<()> {
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    call_tx
        .send(IncomingCall {
            call_id: next_call_id(),
            stream_id,
            header_payload,
            header_flags,
            request_payload,
            request_flags,
            peer_id,
            is_session_call: false,
            response_sender: resp_tx,
        })
        .await
        .map_err(|_| anyhow!("reactor channel closed"))?;

    // Drain every frame the binding emits, write it to the stream, and stop
    // once the Trailer lands. Supports unary (1 Frame + 1 Trailer), server
    // streaming (N Frames + 1 Trailer), and the error-only case (1 Trailer).
    while let Some(frame) = resp_rx.recv().await {
        match frame {
            OutgoingFrame::Frame(bytes) => {
                if !bytes.is_empty() {
                    send.write_all(bytes).await?;
                }
            }
            OutgoingFrame::Trailer(bytes) => {
                if !bytes.is_empty() {
                    send.write_all(bytes).await?;
                }
                send.finish().await?;
                return Ok(());
            }
        }
    }

    // Binding dropped the channel without sending a trailer — close the
    // stream cleanly so the client isn't left hanging on a dangling read.
    send.finish().await?;
    Err(anyhow!("binding dropped response channel without trailer"))
}

async fn handle_session(
    send: CoreSendStream,
    recv: CoreRecvStream,
    peer_id: String,
    stream_id: u64,
    call_tx: mpsc::Sender<IncomingCall>,
    _stream_header_payload: Vec<u8>,
    first_call_header: Vec<u8>,
    first_call_flags: u8,
) -> Result<()> {
    let mut call_header_payload = first_call_header;
    let mut call_header_flags = first_call_flags;

    loop {
        let (request_payload, request_flags) = read_one_frame(&recv).await?;

        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
        call_tx
            .send(IncomingCall {
                call_id: next_call_id(),
                stream_id,
                header_payload: call_header_payload,
                header_flags: call_header_flags,
                request_payload,
                request_flags,
                peer_id: peer_id.clone(),
                is_session_call: true,
                response_sender: resp_tx,
            })
            .await
            .map_err(|_| anyhow!("reactor channel closed"))?;

        // Drain frames for this call but DO NOT finish the stream on Trailer —
        // session mode keeps the stream open for subsequent calls. The outer
        // loop reads the next CallHeader once the trailer has been written.
        loop {
            match resp_rx.recv().await {
                Some(OutgoingFrame::Frame(bytes)) => {
                    if !bytes.is_empty() {
                        send.write_all(bytes).await?;
                    }
                }
                Some(OutgoingFrame::Trailer(bytes)) => {
                    if !bytes.is_empty() {
                        send.write_all(bytes).await?;
                    }
                    break;
                }
                None => return Err(anyhow!("binding dropped session call channel")),
            }
        }

        match read_one_frame(&recv).await {
            Ok((payload, flags)) => {
                if flags & FLAG_CALL != 0 {
                    call_header_payload = payload;
                    call_header_flags = flags;
                    continue;
                }
                return Err(anyhow!(
                    "unexpected frame flags {:#x} in session loop",
                    flags
                ));
            }
            Err(_) => {
                break;
            }
        }
    }

    Ok(())
}

async fn read_one_frame(recv: &CoreRecvStream) -> Result<(Vec<u8>, u8)> {
    // Stack-allocated length header (no heap alloc)
    let mut len_bytes = [0u8; 4];
    recv.read_exact_into(&mut len_bytes).await?;
    let frame_body_len = u32::from_le_bytes(len_bytes) as usize;

    if frame_body_len == 0 {
        return Err(anyhow!("zero-length frame"));
    }
    if frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(anyhow!("frame too large: {}", frame_body_len));
    }

    // Read flags + payload directly into a pre-allocated Vec using read_into.
    // This eliminates the `body[1..].to_vec()` copy by reading the flags byte
    // separately and the payload into its own buffer.
    let mut flags_buf = [0u8; 1];
    recv.read_exact_into(&mut flags_buf).await?;
    let flags = flags_buf[0];

    let payload_len = frame_body_len - 1;
    let mut payload = vec![0u8; payload_len];
    recv.read_exact_into(&mut payload).await?;

    Ok((payload, flags))
}
