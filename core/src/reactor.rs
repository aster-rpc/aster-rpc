//! Server reactor -- Rust-driven accept + read loop.
//!
//! The reactor runs entirely on the tokio runtime. It accepts connections,
//! reads StreamHeader + request frames, and delivers fully-read requests
//! to the language binding via an async channel. The binding runs the
//! handler and submits response frames via an mpsc channel, one frame at a
//! time, terminated by a Trailer. The reactor writes each frame to the QUIC
//! stream as it arrives, enabling server-streaming and bidi-streaming RPCs.
//!
//! Read-side mpsc (added for client-stream / bidi support):
//! - Every QUIC bi-stream gets a single frame-reader task that owns the
//!   recv side and pushes every frame it reads (header, calls, requests)
//!   into a per-stream tokio mpsc channel.
//! - The dispatch loop (stateless or session) consumes from that channel
//!   and routes frames: header frames bootstrap the call, request frames
//!   are forwarded to a per-call `request_sender` that feeds the binding.
//! - For client-streaming or bidi calls the binding pulls additional
//!   request frames via the FFI `recv_frame` path. The reactor closes
//!   the per-call request channel when it sees `FLAG_END_STREAM` on a
//!   request frame, or when the QUIC recv stream EOFs (stateless mode
//!   only — session mode requires explicit `FLAG_END_STREAM` because
//!   the stream stays open between calls).
//!
//! Two reactor entry shapes:
//! - `start_reactor` / `start_reactor_on`: owns the accept loop (pulls from
//!   `node.accept_aster()`). Good for standalone Server use.
//! - `create_reactor`: returns a handle + feeder. The caller owns the accept
//!   loop and feeds RPC connections via `ReactorFeeder::feed`. Good for
//!   AsterServer integration where the accept loop dispatches by ALPN.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::framing::{FLAG_CALL, FLAG_END_STREAM, FLAG_HEADER, MAX_FRAME_SIZE};
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
    /// Receiver for ADDITIONAL request frames after the first one (which
    /// is delivered inline via `request_payload` / `request_flags`). Unary
    /// and server-streaming calls will see this channel close almost
    /// immediately; client-streaming and bidi-streaming calls pull frames
    /// until `FLAG_END_STREAM` or QUIC EOF closes the channel.
    pub request_receiver: mpsc::UnboundedReceiver<RequestFrame>,
}

/// One additional request frame delivered to the binding's per-call
/// `request_receiver` channel. The first request frame is delivered inline
/// in the [`IncomingCall`] descriptor; this type is for everything after
/// that, used only by client-streaming and bidi-streaming calls.
pub struct RequestFrame {
    pub payload: Vec<u8>,
    pub flags: u8,
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

/// One frame as read off the wire by the per-stream reader task.
struct WireFrame {
    payload: Vec<u8>,
    flags: u8,
}

/// Per-stream frame reader task. Owns `recv` for the lifetime of the QUIC
/// bi-stream and pushes every frame it reads into a shared mpsc that the
/// dispatch loop consumes from. Exits on the first read error (typically
/// EOF when the peer closes its send side), which closes the channel and
/// signals the dispatch loop that no more frames are coming.
async fn read_all_frames(recv: CoreRecvStream, frame_tx: mpsc::UnboundedSender<WireFrame>) {
    loop {
        match read_one_frame(&recv).await {
            Ok((payload, flags)) => {
                if frame_tx.send(WireFrame { payload, flags }).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

async fn handle_stream_inner(
    send: CoreSendStream,
    recv: CoreRecvStream,
    peer_id: String,
    call_tx: mpsc::Sender<IncomingCall>,
) -> Result<()> {
    let stream_id = next_stream_id();

    // Spawn the per-stream reader task. Everything below pulls from
    // `frame_rx`; nothing else touches `recv`.
    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel();
    let reader_handle = tokio::spawn(read_all_frames(recv, frame_tx));

    let result = dispatch_stream(&mut frame_rx, send, peer_id, stream_id, call_tx).await;

    // Stop the reader task. For well-behaved peers it has already exited
    // on EOF; for misbehaving peers we cut it off here.
    reader_handle.abort();
    result
}

async fn dispatch_stream(
    frame_rx: &mut mpsc::UnboundedReceiver<WireFrame>,
    send: CoreSendStream,
    peer_id: String,
    stream_id: u64,
    call_tx: mpsc::Sender<IncomingCall>,
) -> Result<()> {
    let header = frame_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("eof before stream header"))?;
    if header.flags & FLAG_HEADER == 0 {
        return Err(anyhow!("first frame missing HEADER flag"));
    }

    let second = frame_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("eof after stream header"))?;

    if second.flags & FLAG_CALL != 0 {
        handle_session(
            send,
            frame_rx,
            peer_id,
            stream_id,
            call_tx,
            header.payload,
            second.payload,
            second.flags,
        )
        .await
    } else {
        handle_stateless(
            send,
            frame_rx,
            peer_id,
            stream_id,
            call_tx,
            header.payload,
            header.flags,
            second.payload,
            second.flags,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_stateless(
    send: CoreSendStream,
    frame_rx: &mut mpsc::UnboundedReceiver<WireFrame>,
    peer_id: String,
    stream_id: u64,
    call_tx: mpsc::Sender<IncomingCall>,
    header_payload: Vec<u8>,
    header_flags: u8,
    first_request_payload: Vec<u8>,
    first_request_flags: u8,
) -> Result<()> {
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    let (req_tx, req_rx) = mpsc::unbounded_channel();

    // Stateless mode: if the first request frame already carries
    // FLAG_END_STREAM (or the call shape is unary/server-stream where the
    // peer will simply EOF), the binding doesn't need any more frames.
    // Either way we ALSO forward subsequent wire frames (until EOF or
    // END_STREAM) so client-streaming peers can push their full input.
    let mut request_done = first_request_flags & FLAG_END_STREAM != 0;
    let mut req_tx_opt = if request_done { None } else { Some(req_tx) };

    call_tx
        .send(IncomingCall {
            call_id: next_call_id(),
            stream_id,
            header_payload,
            header_flags,
            request_payload: first_request_payload,
            request_flags: first_request_flags,
            peer_id,
            is_session_call: false,
            response_sender: resp_tx,
            request_receiver: req_rx,
        })
        .await
        .map_err(|_| anyhow!("reactor channel closed"))?;

    loop {
        tokio::select! {
            biased;

            // Forward any additional request frames the peer sends.
            wire = frame_rx.recv(), if !request_done => {
                match wire {
                    Some(frame) => {
                        let end = frame.flags & FLAG_END_STREAM != 0;
                        if let Some(tx) = req_tx_opt.as_ref() {
                            let _ = tx.send(RequestFrame {
                                payload: frame.payload,
                                flags: frame.flags,
                            });
                        }
                        if end {
                            request_done = true;
                            req_tx_opt = None;
                        }
                    }
                    None => {
                        // Peer EOF — close the request channel.
                        request_done = true;
                        req_tx_opt = None;
                    }
                }
            }

            // Drain response frames as they arrive from the binding.
            resp = resp_rx.recv() => {
                match resp {
                    Some(OutgoingFrame::Frame(bytes)) => {
                        if !bytes.is_empty() {
                            send.write_all(bytes).await?;
                        }
                    }
                    Some(OutgoingFrame::Trailer(bytes)) => {
                        if !bytes.is_empty() {
                            send.write_all(bytes).await?;
                        }
                        send.finish().await?;
                        return Ok(());
                    }
                    None => {
                        send.finish().await?;
                        return Err(anyhow!(
                            "binding dropped response channel without trailer"
                        ));
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_session(
    send: CoreSendStream,
    frame_rx: &mut mpsc::UnboundedReceiver<WireFrame>,
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
        // Read the first request frame for this call.
        let first = match frame_rx.recv().await {
            Some(f) => f,
            None => return Ok(()),
        };
        if first.flags & FLAG_CALL != 0 {
            return Err(anyhow!(
                "expected request frame, got CallHeader (flags={:#x})",
                first.flags
            ));
        }

        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
        let (req_tx, req_rx) = mpsc::unbounded_channel();

        // Session mode CANNOT rely on QUIC EOF to end a call's request
        // stream — the bi-stream stays open between calls. Client-streaming
        // calls in session mode MUST mark the last request frame with
        // FLAG_END_STREAM so the reactor knows where one call's request
        // stream ends and the next CallHeader begins.
        let mut request_done = first.flags & FLAG_END_STREAM != 0;
        let mut req_tx_opt = if request_done { None } else { Some(req_tx) };

        call_tx
            .send(IncomingCall {
                call_id: next_call_id(),
                stream_id,
                header_payload: call_header_payload,
                header_flags: call_header_flags,
                request_payload: first.payload,
                request_flags: first.flags,
                peer_id: peer_id.clone(),
                is_session_call: true,
                response_sender: resp_tx,
                request_receiver: req_rx,
            })
            .await
            .map_err(|_| anyhow!("reactor channel closed"))?;

        // Drain request + response frames for this call. Exit the inner
        // loop on Trailer so the next iteration can read the next
        // CallHeader. A peer that sets FLAG_CALL before the previous
        // call's request stream finished is a protocol violation — we
        // detect it by seeing a CALL frame while `request_done == false`.
        loop {
            tokio::select! {
                biased;

                wire = frame_rx.recv(), if !request_done => {
                    match wire {
                        Some(frame) => {
                            if frame.flags & FLAG_CALL != 0 {
                                return Err(anyhow!(
                                    "got CallHeader before previous call's END_STREAM"
                                ));
                            }
                            let end = frame.flags & FLAG_END_STREAM != 0;
                            if let Some(tx) = req_tx_opt.as_ref() {
                                let _ = tx.send(RequestFrame {
                                    payload: frame.payload,
                                    flags: frame.flags,
                                });
                            }
                            if end {
                                request_done = true;
                                req_tx_opt = None;
                            }
                        }
                        None => return Ok(()),
                    }
                }

                resp = resp_rx.recv() => {
                    match resp {
                        Some(OutgoingFrame::Frame(bytes)) => {
                            if !bytes.is_empty() {
                                send.write_all(bytes).await?;
                            }
                        }
                        Some(OutgoingFrame::Trailer(bytes)) => {
                            if !bytes.is_empty() {
                                send.write_all(bytes).await?;
                            }
                            // The next loop iteration creates a fresh
                            // (req_tx, req_rx) pair, dropping the old one
                            // and closing any unread request channel.
                            break;
                        }
                        None => {
                            return Err(anyhow!("binding dropped session call channel"));
                        }
                    }
                }
            }
        }

        // Wait for the next CallHeader.
        let next = match frame_rx.recv().await {
            Some(f) => f,
            None => return Ok(()),
        };
        if next.flags & FLAG_CALL == 0 {
            return Err(anyhow!(
                "expected CallHeader, got flags={:#x}",
                next.flags
            ));
        }
        call_header_payload = next.payload;
        call_header_flags = next.flags;
    }
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
