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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::framing::{FLAG_CALL, FLAG_CANCEL, FLAG_END_STREAM, FLAG_HEADER, MAX_FRAME_SIZE};
use crate::{CoreClosedInfo, CoreConnection, CoreNode, CoreRecvStream, CoreSendStream};

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
    /// Set to `true` by the reactor when a `FLAG_CANCEL` frame arrives on
    /// the wire OR when the QUIC stream errors mid-call. The binding can
    /// poll this from a long-running streaming dispatcher (typically via
    /// `aster_reactor_check_cancelled`) to stop early instead of running
    /// to natural completion. Cleared/destroyed when the call ends.
    pub cancelled: Arc<AtomicBool>,
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

/// An event dispatched from the reactor to its consumer. In addition to
/// incoming calls the reactor now surfaces connection-closed events so
/// bindings can reap per-connection state (e.g. the session map and
/// graveyard described in spec §7.5). Emitted exactly once per
/// connection, after every stream on it has been accepted or the
/// connection itself has errored/been closed.
pub enum ReactorEvent {
    Call(IncomingCall),
    ConnectionClosed {
        peer_id: String,
        info: CoreClosedInfo,
    },
}

pub struct ReactorHandle {
    event_rx: mpsc::Receiver<ReactorEvent>,
}

impl ReactorHandle {
    /// Pull the next event (call or connection-closed). Bindings that
    /// need to reap per-connection state on disconnect should consume
    /// from this API.
    pub async fn next_event(&mut self) -> Option<ReactorEvent> {
        self.event_rx.recv().await
    }

    /// Back-compat convenience: pull the next **Call** event,
    /// silently draining (and logging at debug) any
    /// `ConnectionClosed` events that arrive. Existing FFI consumers
    /// that don't yet track connection lifecycle can continue to use
    /// this shape unchanged.
    pub async fn next_call(&mut self) -> Option<IncomingCall> {
        loop {
            match self.event_rx.recv().await {
                Some(ReactorEvent::Call(c)) => return Some(c),
                Some(ReactorEvent::ConnectionClosed { peer_id, .. }) => {
                    tracing::debug!(
                        peer = %peer_id,
                        "ConnectionClosed event dropped by next_call (binding has not migrated to next_event)"
                    );
                    continue;
                }
                None => return None,
            }
        }
    }
}

/// Feed connections into a reactor created with `create_reactor`.
#[derive(Clone)]
pub struct ReactorFeeder {
    event_tx: mpsc::Sender<ReactorEvent>,
    rt_handle: tokio::runtime::Handle,
}

impl ReactorFeeder {
    pub fn feed(&self, conn: CoreConnection) {
        let tx = self.event_tx.clone();
        self.rt_handle.spawn(connection_loop(conn, tx));
    }
}

/// Create a reactor without an accept loop. Returns a handle for receiving
/// events and a feeder for pushing connections from an external accept loop.
pub fn create_reactor(
    handle: &tokio::runtime::Handle,
    channel_capacity: usize,
) -> (ReactorHandle, ReactorFeeder) {
    let (event_tx, event_rx) = mpsc::channel(channel_capacity);
    (
        ReactorHandle { event_rx },
        ReactorFeeder {
            event_tx,
            rt_handle: handle.clone(),
        },
    )
}

/// Start a reactor that owns the accept loop.
pub fn start_reactor(node: CoreNode, channel_capacity: usize) -> ReactorHandle {
    let (event_tx, event_rx) = mpsc::channel(channel_capacity);
    tokio::spawn(accept_loop(node, event_tx));
    ReactorHandle { event_rx }
}

/// Same as `start_reactor` but takes an explicit runtime handle.
pub fn start_reactor_on(
    handle: &tokio::runtime::Handle,
    node: CoreNode,
    channel_capacity: usize,
) -> ReactorHandle {
    let (event_tx, event_rx) = mpsc::channel(channel_capacity);
    handle.spawn(accept_loop(node, event_tx));
    ReactorHandle { event_rx }
}

async fn accept_loop(node: CoreNode, event_tx: mpsc::Sender<ReactorEvent>) {
    loop {
        match node.accept_aster().await {
            Ok((_alpn, conn)) => {
                let tx = event_tx.clone();
                tokio::spawn(connection_loop(conn, tx));
            }
            Err(e) => {
                if event_tx.is_closed() {
                    break;
                }
                tracing::warn!("reactor accept error: {}", e);
            }
        }
    }
}

async fn connection_loop(conn: CoreConnection, event_tx: mpsc::Sender<ReactorEvent>) {
    let peer_id = conn.remote_id();

    while let Ok((send, recv)) = conn.accept_bi().await {
        let tx = event_tx.clone();
        let peer = peer_id.clone();
        tokio::spawn(handle_stream(send, recv, peer, tx));
    }

    // Connection closed or accept_bi returned an error — emit a
    // ConnectionClosed event so the binding can reap per-connection
    // state (spec §7.5). We await the remote side's close-info so
    // the event carries the termination reason. `await` here is a
    // bounded wait: if the connection is already fully closed the
    // future resolves immediately; otherwise it resolves on the next
    // close frame.
    let info = conn.closed().await;
    let _ = event_tx
        .send(ReactorEvent::ConnectionClosed { peer_id, info })
        .await;
}

async fn handle_stream(
    send: CoreSendStream,
    recv: CoreRecvStream,
    peer_id: String,
    call_tx: mpsc::Sender<ReactorEvent>,
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
    call_tx: mpsc::Sender<ReactorEvent>,
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

/// Outcome of dispatching one call through `dispatch_one_call`.
enum CallOutcome {
    /// The call completed (trailer sent to the peer). For session
    /// streams the caller should loop to read the next `CallHeader`
    /// on the same stream; for stateless streams the caller should
    /// finish the send side and return.
    Complete,
    /// The peer closed the stream before the call finished. The caller
    /// should return `Ok(())` from the dispatch function.
    StreamEof,
}

/// Unified multiplexed-call reader. Every inbound stream — stateless
/// or session — goes through this function. It:
///
/// 1. Reads the `StreamHeader` (first frame, must carry `FLAG_HEADER`).
/// 2. Reads the second frame and uses its flag shape to decide whether
///    this is a session stream (next frame is a `CallHeader`) or a
///    stateless stream (next frame is already the first request frame).
///    This is the "presence of CallHeader frames after StreamHeader"
///    discriminator from spec §6.
/// 3. Loops on `dispatch_one_call`:
///    - Stateless streams run exactly one iteration and return.
///    - Session streams run until the peer EOFs or a protocol error
///      occurs; between iterations the reader reads the next
///      `CallHeader` + first request frame for the next call.
///
/// Today the `is_session_call` bool passed to the binding is derived
/// directly from the frame-shape discriminator. When the migration
/// lands `StreamHeader.sessionId` and `aster_call_*`, routing moves
/// to the session id but the loop structure in this function stays.
async fn dispatch_stream(
    frame_rx: &mut mpsc::UnboundedReceiver<WireFrame>,
    send: CoreSendStream,
    peer_id: String,
    stream_id: u64,
    call_tx: mpsc::Sender<ReactorEvent>,
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

    // Frame-shape discriminator: a `CALL` frame after the stream
    // header means "session stream with multi-call framing"; anything
    // else means "stateless one-shot stream, this frame is the first
    // request data frame".
    let is_session = second.flags & FLAG_CALL != 0;

    // Seed the first iteration: stateless uses the `StreamHeader`
    // payload as the per-call header and the second frame as the first
    // request; session needs to also read the first request frame, and
    // uses the `CallHeader` as the per-call header.
    let (mut call_header_payload, mut call_header_flags, mut first_req) = if is_session {
        let first_req = match frame_rx.recv().await {
            Some(f) => f,
            None => return Ok(()),
        };
        if first_req.flags & FLAG_CALL != 0 {
            return Err(anyhow!(
                "expected request frame, got CallHeader (flags={:#x})",
                first_req.flags
            ));
        }
        (second.payload, second.flags, first_req)
    } else {
        (header.payload, header.flags, second)
    };

    loop {
        let outcome = dispatch_one_call(
            &send,
            frame_rx,
            &peer_id,
            stream_id,
            &call_tx,
            call_header_payload,
            call_header_flags,
            first_req.payload,
            first_req.flags,
            is_session,
        )
        .await?;

        match outcome {
            CallOutcome::StreamEof => return Ok(()),
            CallOutcome::Complete => {
                if !is_session {
                    // Stateless: one call per stream. Finish and go.
                    send.finish().await?;
                    return Ok(());
                }
                // Session: read the next CallHeader and first request
                // frame, then loop.
                let next_call = match frame_rx.recv().await {
                    Some(f) => f,
                    None => return Ok(()),
                };
                if next_call.flags & FLAG_CALL == 0 {
                    return Err(anyhow!(
                        "expected CallHeader, got flags={:#x}",
                        next_call.flags
                    ));
                }
                let next_req = match frame_rx.recv().await {
                    Some(f) => f,
                    None => return Ok(()),
                };
                if next_req.flags & FLAG_CALL != 0 {
                    return Err(anyhow!(
                        "expected request frame, got CallHeader (flags={:#x})",
                        next_req.flags
                    ));
                }
                call_header_payload = next_call.payload;
                call_header_flags = next_call.flags;
                first_req = next_req;
            }
        }
    }
}

/// Dispatch one call on an open multiplexed stream: send the
/// `IncomingCall` to the binding, forward additional request frames
/// until `FLAG_END_STREAM`/QUIC EOF, drain response frames from the
/// binding, and return when the terminal trailer is written.
///
/// The `is_session` flag affects three behaviours, consolidated from
/// the old `handle_stateless` / `handle_session` split:
///
/// - **Request-channel EOF.** Stateless streams can rely on QUIC EOF
///   to close the request channel (one call per stream, no more frames
///   possible). Session streams must see `FLAG_END_STREAM` explicitly
///   because the stream stays open for the next call, and treat QUIC
///   EOF as "stream is over, stop dispatching entirely" (`StreamEof`).
/// - **Unexpected `CallHeader` mid-call.** In session mode this is a
///   protocol violation (peer tried to start a new call before ending
///   the previous request stream). In stateless mode it cannot happen
///   because the discriminator placed us on the non-session branch.
/// - **Trailer handling.** Stateless streams `finish()` the send side
///   after the trailer (caller does this, not us). Session streams
///   leave the send side open so the next call on the stream can write
///   its own frames.
#[allow(clippy::too_many_arguments)]
async fn dispatch_one_call(
    send: &CoreSendStream,
    frame_rx: &mut mpsc::UnboundedReceiver<WireFrame>,
    peer_id: &str,
    stream_id: u64,
    call_tx: &mpsc::Sender<ReactorEvent>,
    header_payload: Vec<u8>,
    header_flags: u8,
    first_request_payload: Vec<u8>,
    first_request_flags: u8,
    is_session: bool,
) -> Result<CallOutcome> {
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel();
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let cancelled = Arc::new(AtomicBool::new(false));

    let mut request_done = first_request_flags & FLAG_END_STREAM != 0;
    let mut req_tx_opt = if request_done { None } else { Some(req_tx) };

    call_tx
        .send(ReactorEvent::Call(IncomingCall {
            call_id: next_call_id(),
            stream_id,
            header_payload,
            header_flags,
            request_payload: first_request_payload,
            request_flags: first_request_flags,
            peer_id: peer_id.to_string(),
            is_session_call: is_session,
            response_sender: resp_tx,
            request_receiver: req_rx,
            cancelled: cancelled.clone(),
        }))
        .await
        .map_err(|_| anyhow!("reactor channel closed"))?;

    loop {
        tokio::select! {
            biased;

            // Forward additional request frames from the peer.
            wire = frame_rx.recv(), if !request_done => {
                match wire {
                    Some(frame) => {
                        if frame.flags & FLAG_CANCEL != 0 {
                            // Peer signalled cancellation: surface to
                            // the binding, close the request channel,
                            // and stop forwarding. The binding's
                            // eventual trailer closes the response side.
                            cancelled.store(true, Ordering::Release);
                            request_done = true;
                            req_tx_opt = None;
                            continue;
                        }
                        if is_session && frame.flags & FLAG_CALL != 0 {
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
                    None => {
                        if is_session {
                            // QUIC EOF on a session stream: the whole
                            // stream is over, not just this call.
                            return Ok(CallOutcome::StreamEof);
                        }
                        // Stateless: peer EOF just means "no more
                        // request frames"; keep draining responses.
                        request_done = true;
                        req_tx_opt = None;
                    }
                }
            }

            // Drain response frames from the binding.
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
                        // Stateless `finish()` happens in the caller
                        // after we return so the two call paths share
                        // one exit shape. Session streams keep the
                        // send side open for the next call.
                        return Ok(CallOutcome::Complete);
                    }
                    None => {
                        return Err(anyhow!(
                            "binding dropped response channel without trailer"
                        ));
                    }
                }
            }
        }
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
