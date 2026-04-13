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

use crate::framing::{FLAG_CANCEL, FLAG_END_STREAM, FLAG_HEADER, MAX_FRAME_SIZE};
use crate::{CoreClosedInfo, CoreConnection, CoreNode, CoreRecvStream, CoreSendStream};

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

fn next_call_id() -> u64 {
    NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_stream_id() -> u64 {
    NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct IncomingCall {
    pub call_id: u64,
    /// Reactor-assigned unique id for the QUIC bi-stream this call arrived
    /// on. With multiplexed streams (spec §6) a single bi-stream may carry
    /// many calls, so callers should NOT key per-session state on stream_id;
    /// use `connection_id + StreamHeader.sessionId` instead (spec §7.5).
    pub stream_id: u64,
    /// Reactor-assigned unique id for the QUIC connection this call arrived
    /// on. Sessions are scoped per-`(peer, connection)` (spec §7.5). When the
    /// connection drops, the reactor emits a `ConnectionClosed` event with
    /// the same `connection_id` so the binding can reap state.
    pub connection_id: u64,
    pub header_payload: Vec<u8>,
    pub header_flags: u8,
    pub request_payload: Vec<u8>,
    pub request_flags: u8,
    pub peer_id: String,
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
        connection_id: u64,
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
                Some(ReactorEvent::ConnectionClosed {
                    peer_id,
                    connection_id,
                    ..
                }) => {
                    tracing::debug!(
                        peer = %peer_id,
                        connection_id = connection_id,
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
    let connection_id = next_connection_id();

    while let Ok((send, recv)) = conn.accept_bi().await {
        let tx = event_tx.clone();
        let peer = peer_id.clone();
        tokio::spawn(handle_stream(send, recv, peer, connection_id, tx));
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
        .send(ReactorEvent::ConnectionClosed {
            peer_id,
            connection_id,
            info,
        })
        .await;
}

async fn handle_stream(
    send: CoreSendStream,
    recv: CoreRecvStream,
    peer_id: String,
    connection_id: u64,
    call_tx: mpsc::Sender<ReactorEvent>,
) {
    if let Err(e) = handle_stream_inner(send, recv, peer_id, connection_id, call_tx).await {
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
    connection_id: u64,
    call_tx: mpsc::Sender<ReactorEvent>,
) -> Result<()> {
    let stream_id = next_stream_id();

    // Spawn the per-stream reader task. Everything below pulls from
    // `frame_rx`; nothing else touches `recv`.
    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel();
    let reader_handle = tokio::spawn(read_all_frames(recv, frame_tx));

    let result = dispatch_stream(
        &mut frame_rx,
        send,
        peer_id,
        connection_id,
        stream_id,
        call_tx,
    )
    .await;

    // Stop the reader task. For well-behaved peers it has already exited
    // on EOF; for misbehaving peers we cut it off here.
    reader_handle.abort();
    result
}

/// Outcome of dispatching one call through `dispatch_one_call`.
enum CallOutcome {
    /// The call completed (trailer sent to the peer). The caller
    /// loops to read the next `StreamHeader` on the same stream
    /// (multiplexed-stream model — every stream is multiplexed, spec §6).
    Complete,
    /// The peer closed the stream before the call finished. The caller
    /// should return `Ok(())` from the dispatch function.
    StreamEof,
}

/// Unified multiplexed-call reader (spec §6 — every stream is multiplexed).
///
/// The loop is:
///
/// 1. Read a `StreamHeader` (must carry `FLAG_HEADER`). On EOF here
///    the stream is done — exit cleanly without `finish()`.
/// 2. Read the first request frame.
/// 3. Dispatch the call: deliver to the binding, forward additional
///    request frames, drain responses, write the trailer.
/// 4. Loop back to (1) — the same stream may carry additional calls
///    if the client is reusing it from its multiplexed-stream pool.
///
/// Routing between SHARED-pool and session-bound calls happens entirely
/// in the binding via the `StreamHeader.sessionId` field; core stays
/// payload-opaque and treats every stream identically.
async fn dispatch_stream(
    frame_rx: &mut mpsc::UnboundedReceiver<WireFrame>,
    send: CoreSendStream,
    peer_id: String,
    connection_id: u64,
    stream_id: u64,
    call_tx: mpsc::Sender<ReactorEvent>,
) -> Result<()> {
    loop {
        let header = match frame_rx.recv().await {
            Some(f) => f,
            // Clean EOF between calls — peer closed the multiplexed
            // stream. Don't `finish()` here; just exit so the
            // per-stream task can drop the send half naturally.
            None => return Ok(()),
        };
        if header.flags & FLAG_HEADER == 0 {
            return Err(anyhow!(
                "expected StreamHeader (FLAG_HEADER), got flags={:#x}",
                header.flags
            ));
        }

        let first_req = match frame_rx.recv().await {
            Some(f) => f,
            None => return Err(anyhow!("eof after stream header")),
        };

        let outcome = dispatch_one_call(
            &send,
            frame_rx,
            &peer_id,
            connection_id,
            stream_id,
            &call_tx,
            header.payload,
            header.flags,
            first_req.payload,
            first_req.flags,
        )
        .await?;

        if matches!(outcome, CallOutcome::StreamEof) {
            return Ok(());
        }
        // Otherwise loop: read the next call's StreamHeader on this
        // multiplexed stream.
    }
}

/// Dispatch one call on an open multiplexed stream: send the
/// `IncomingCall` to the binding, forward additional request frames
/// until `FLAG_END_STREAM`/QUIC EOF, drain response frames from the
/// binding, and return when the terminal trailer is written.
#[allow(clippy::too_many_arguments)]
async fn dispatch_one_call(
    send: &CoreSendStream,
    frame_rx: &mut mpsc::UnboundedReceiver<WireFrame>,
    peer_id: &str,
    connection_id: u64,
    stream_id: u64,
    call_tx: &mpsc::Sender<ReactorEvent>,
    header_payload: Vec<u8>,
    header_flags: u8,
    first_request_payload: Vec<u8>,
    first_request_flags: u8,
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
            connection_id,
            header_payload,
            header_flags,
            request_payload: first_request_payload,
            request_flags: first_request_flags,
            peer_id: peer_id.to_string(),
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
                        if frame.flags & FLAG_HEADER != 0 {
                            // A fresh StreamHeader mid-call means the
                            // peer started the next call before ending
                            // the previous request stream. Protocol
                            // violation — every multi-frame call MUST
                            // terminate its request stream with
                            // FLAG_END_STREAM before opening the next.
                            return Err(anyhow!(
                                "got StreamHeader before previous call's END_STREAM"
                            ));
                        }
                        let end = frame.flags & FLAG_END_STREAM != 0;
                        // An empty FLAG_END_STREAM frame is the
                        // "no more requests" sentinel — bindings that
                        // emit it (e.g. Java's BidiCall.complete())
                        // would otherwise hand the dispatcher a zero
                        // length payload that decodes to a null
                        // request. Drop the empty frame and rely on
                        // the channel close to signal EOS.
                        let forward = !(end && frame.payload.is_empty());
                        if forward {
                            if let Some(tx) = req_tx_opt.as_ref() {
                                let _ = tx.send(RequestFrame {
                                    payload: frame.payload,
                                    flags: frame.flags,
                                });
                            }
                        }
                        if end {
                            request_done = true;
                            req_tx_opt = None;
                        }
                    }
                    None => {
                        // QUIC EOF on the multiplexed stream: the whole
                        // stream is over. Close the per-call request
                        // channel by dropping the sender, then finish
                        // draining responses from the binding before
                        // returning StreamEof so the trailer (if any)
                        // still flushes to the wire.
                        drop(req_tx_opt.take());
                        // Drain any remaining response frames (the
                        // binding may already have queued the trailer).
                        while let Some(resp) = resp_rx.recv().await {
                            match resp {
                                OutgoingFrame::Frame(bytes) => {
                                    if !bytes.is_empty() {
                                        let _ = send.write_all(bytes).await;
                                    }
                                }
                                OutgoingFrame::Trailer(bytes) => {
                                    if !bytes.is_empty() {
                                        let _ = send.write_all(bytes).await;
                                    }
                                    break;
                                }
                            }
                        }
                        return Ok(CallOutcome::StreamEof);
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
                        // Don't `finish()` the send side — every stream
                        // is multiplexed, so the next call (if any) on
                        // this stream still needs to write its frames.
                        // The send half is dropped naturally when the
                        // peer EOFs and we return cleanly from
                        // dispatch_stream.
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
