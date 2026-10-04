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
//! - `start_reactor_with_limits`: owns the accept loop (pulls from
//!   `node.accept_aster()`). `aster::rpc::Server::serve` uses it.
//! - `create_reactor`: returns a handle + feeder. The caller owns the accept
//!   loop and feeds RPC connections via `ReactorFeeder::feed`
//!   (`aster::expose` and the transport-level tests).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};

use crate::framing::{FLAG_CANCEL, FLAG_END_STREAM, FLAG_HEADER};
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
    /// Clone of the `CoreConnection` this call arrived on. Lets the
    /// binding hand a `TunnelHandle` (or anything else connection-scoped)
    /// to the RPC handler via `CallContext`. Cheap — Arc bump only.
    pub connection: CoreConnection,
    pub header_payload: Vec<u8>,
    pub header_flags: u8,
    pub request_payload: Vec<u8>,
    pub request_flags: u8,
    pub peer_id: String,
    pub response_sender: ResponseSender,
    /// Receiver for ADDITIONAL request frames after the first one (which
    /// is delivered inline via `request_payload` / `request_flags`). Unary
    /// and server-streaming calls will see this channel close almost
    /// immediately; client-streaming and bidi-streaming calls pull frames
    /// until `FLAG_END_STREAM` or QUIC EOF closes the channel.
    pub request_receiver: mpsc::Receiver<RequestFrame>,
    /// Set to `true` by the reactor when a `FLAG_CANCEL` frame arrives on
    /// the wire OR when the QUIC stream errors mid-call. The binding can
    /// poll this from a long-running streaming dispatcher (typically via
    /// `aster_reactor_check_cancelled`) to stop early instead of running
    /// to natural completion. Cleared/destroyed when the call ends.
    pub cancelled: Arc<AtomicBool>,
    /// Signalled (`notify_waiters`) right after `cancelled` is set, so a
    /// consumer can await a cancellation instead of polling the flag. A
    /// waiter must enable its `Notified` future and then re-check
    /// `cancelled` before awaiting, since `notify_waiters` stores no permit.
    pub cancel_notify: Arc<Notify>,
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
    /// Unary fast-path: response frame concatenated with trailer frame, to
    /// be written in a single `send.write_all` call. Mirrors the client-side
    /// `aster_call_unary` pattern that writes header+request as one buffer.
    /// Terminal: the reactor returns `CallOutcome::Complete` after writing.
    CompleteUnary(Vec<u8>),
}

impl OutgoingFrame {
    /// Number of caller-owned bytes this item pins until the transport write
    /// completes. The FFI reserves this exact charge before it copies bytes.
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Frame(bytes) | Self::Trailer(bytes) | Self::CompleteUnary(bytes) => bytes.len(),
        }
    }
}

/// Response items allowed to exist per call, including the item currently
/// blocked in the transport writer.
///
/// This is deliberately independent of the byte budget. A high item ceiling
/// amortizes tiny-frame producer handoffs; the byte budget keeps large frames
/// inside the former `8 * MAX_FRAME_SIZE` envelope.
pub const RESPONSE_ITEM_LIMIT: usize = 256;

/// Maximum response bytes pinned per call, including the transport writer's
/// in-flight item. This is exactly the envelope previously attributed to an
/// eight-slot channel, but unlike an mpsc slot count it is an actual byte
/// invariant.
pub const RESPONSE_BYTE_LIMIT: usize =
    FRAME_CHANNEL_DEPTH * crate::framing::MAX_FRAME_SIZE as usize;

struct ResponseBudget {
    items: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}

/// Response producer for one call. Capacity is two-dimensional: every item
/// owns one item permit plus its exact encoded byte charge before it may enter
/// the queue.
#[derive(Clone)]
pub struct ResponseSender {
    tx: mpsc::Sender<BudgetedResponse>,
    budget: Arc<ResponseBudget>,
    item_limit: usize,
    byte_limit: usize,
}

/// Response consumer for one call.
pub struct ResponseReceiver {
    rx: mpsc::Receiver<BudgetedResponse>,
}

/// Capacity acquired before a producer constructs or copies response bytes.
pub struct ResponsePermit {
    queue: mpsc::OwnedPermit<BudgetedResponse>,
    item: OwnedSemaphorePermit,
    bytes: OwnedSemaphorePermit,
    encoded_len: usize,
}

/// The item returned to a transport writer. Its private lease must remain live
/// until the write completes, so both the queued and in-flight bytes count
/// against the same per-call envelope.
pub struct ResponseItem {
    frame: OutgoingFrame,
    lease: ResponseLease,
}

/// Capacity lease separated from the frame so a transport can move the bytes
/// into its write call while retaining the permits until that future resolves.
pub struct ResponseLease {
    _item: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

struct BudgetedResponse {
    frame: OutgoingFrame,
    item: OwnedSemaphorePermit,
    bytes: OwnedSemaphorePermit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseReserveError {
    Full,
    Closed,
    TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseSendError;

impl ResponseSender {
    /// Acquire both dimensions without waiting. A `Full` result owns no
    /// capacity and callers have not yet copied any bytes.
    pub fn try_reserve(&self, encoded_len: usize) -> Result<ResponsePermit, ResponseReserveError> {
        if encoded_len > self.byte_limit || encoded_len > u32::MAX as usize {
            return Err(ResponseReserveError::TooLarge);
        }

        let item = self
            .budget
            .items
            .clone()
            .try_acquire_owned()
            .map_err(|_| ResponseReserveError::Full)?;
        let bytes = self
            .budget
            .bytes
            .clone()
            .try_acquire_many_owned(encoded_len as u32)
            .map_err(|_| ResponseReserveError::Full)?;
        let queue = self
            .tx
            .clone()
            .try_reserve_owned()
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ResponseReserveError::Full,
                mpsc::error::TrySendError::Closed(_) => ResponseReserveError::Closed,
            })?;

        Ok(ResponsePermit {
            queue,
            item,
            bytes,
            encoded_len,
        })
    }

    /// Acquire both capacity dimensions asynchronously. No response bytes need
    /// to exist while this future waits.
    pub async fn reserve(
        &self,
        encoded_len: usize,
    ) -> Result<ResponsePermit, ResponseReserveError> {
        if encoded_len > self.byte_limit || encoded_len > u32::MAX as usize {
            return Err(ResponseReserveError::TooLarge);
        }

        let item = self
            .budget
            .items
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ResponseReserveError::Closed)?;
        let bytes = self
            .budget
            .bytes
            .clone()
            .acquire_many_owned(encoded_len as u32)
            .await
            .map_err(|_| ResponseReserveError::Closed)?;
        let queue = self
            .tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| ResponseReserveError::Closed)?;

        Ok(ResponsePermit {
            queue,
            item,
            bytes,
            encoded_len,
        })
    }

    pub async fn send(&self, frame: OutgoingFrame) -> Result<(), ResponseSendError> {
        let permit = self
            .reserve(frame.encoded_len())
            .await
            .map_err(|_| ResponseSendError)?;
        permit.send(frame)
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub async fn closed(&self) {
        self.tx.closed().await;
    }

    pub fn item_limit(&self) -> usize {
        self.item_limit
    }

    pub fn byte_limit(&self) -> usize {
        self.byte_limit
    }
}

impl ResponsePermit {
    pub fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    pub fn send(self, frame: OutgoingFrame) -> Result<(), ResponseSendError> {
        if frame.encoded_len() != self.encoded_len {
            return Err(ResponseSendError);
        }
        self.queue.send(BudgetedResponse {
            frame,
            item: self.item,
            bytes: self.bytes,
        });
        Ok(())
    }
}

impl ResponseReceiver {
    pub async fn recv(&mut self) -> Option<ResponseItem> {
        self.rx.recv().await.map(|response| ResponseItem {
            frame: response.frame,
            lease: ResponseLease {
                _item: response.item,
                _bytes: response.bytes,
            },
        })
    }

    pub fn try_recv(&mut self) -> Result<ResponseItem, mpsc::error::TryRecvError> {
        self.rx.try_recv().map(|response| ResponseItem {
            frame: response.frame,
            lease: ResponseLease {
                _item: response.item,
                _bytes: response.bytes,
            },
        })
    }
}

impl tokio_stream::Stream for ResponseReceiver {
    type Item = ResponseItem;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx).map(|response| {
            response.map(|response| ResponseItem {
                frame: response.frame,
                lease: ResponseLease {
                    _item: response.item,
                    _bytes: response.bytes,
                },
            })
        })
    }
}

impl ResponseItem {
    pub fn into_parts(self) -> (OutgoingFrame, ResponseLease) {
        (self.frame, self.lease)
    }
}

/// Construct the production response queue.
pub fn response_channel() -> (ResponseSender, ResponseReceiver) {
    response_channel_with_limits(RESPONSE_ITEM_LIMIT, RESPONSE_BYTE_LIMIT)
}

/// Construct a queue with explicit limits for invariant tests and transport
/// adapters. Both values must be positive; the production path uses the
/// constants above.
pub fn response_channel_with_limits(
    item_limit: usize,
    byte_limit: usize,
) -> (ResponseSender, ResponseReceiver) {
    assert!(item_limit > 0);
    assert!(byte_limit > 0);
    assert!(byte_limit <= Semaphore::MAX_PERMITS);
    let (tx, rx) = mpsc::channel(item_limit);
    let budget = Arc::new(ResponseBudget {
        items: Arc::new(Semaphore::new(item_limit)),
        bytes: Arc::new(Semaphore::new(byte_limit)),
    });
    (
        ResponseSender {
            tx,
            budget,
            item_limit,
            byte_limit,
        },
        ResponseReceiver { rx },
    )
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

/// Aster RPC ALPN. Connections negotiating this ALPN flow into the reactor's RPC
/// pipeline; connections on any other aster ALPN (e.g. `aster.consumer_admission`)
/// are forwarded to the reactor's non-rpc channel so bindings can dispatch them
/// to higher-level handlers.
pub const RPC_ALPN: &[u8] = b"aster/1";

pub struct ReactorHandle {
    event_rx: mpsc::Receiver<ReactorEvent>,
    /// Non-rpc accept queue. Populated by `accept_loop` when a connection
    /// negotiates an ALPN other than `aster/1` (e.g. `aster.consumer_admission`).
    /// Always present -- `start_reactor_with_limits` wires it even when no
    /// non-rpc ALPNs are registered; stays empty in that case, and in
    /// `create_reactor` mode, where the caller owns the accept loop.
    non_rpc_rx: mpsc::Receiver<(Vec<u8>, CoreConnection)>,
}

impl ReactorHandle {
    /// Pull the next event (call or connection-closed).
    pub async fn next_event(&mut self) -> Option<ReactorEvent> {
        self.event_rx.recv().await
    }

    /// Split this handle into its two independent receivers: the RPC event
    /// stream and the non-rpc `(alpn, connection)` queue, so each can be
    /// drained by its own task (as `aster::rpc::Server::serve` does).
    pub fn into_parts(
        self,
    ) -> (
        mpsc::Receiver<ReactorEvent>,
        mpsc::Receiver<(Vec<u8>, CoreConnection)>,
    ) {
        (self.event_rx, self.non_rpc_rx)
    }
}

/// Feed connections into a reactor created with `create_reactor`.
#[derive(Clone)]
pub struct ReactorFeeder {
    event_tx: mpsc::Sender<ReactorEvent>,
    rt_handle: tokio::runtime::Handle,
    resolve: Option<FrameLimitResolver>,
}

impl ReactorFeeder {
    pub fn feed(&self, conn: CoreConnection) {
        let tx = self.event_tx.clone();
        self.rt_handle
            .spawn(connection_loop(conn, tx, self.resolve.clone()));
    }

    /// Install the per-method body-frame cap resolver. Without one every
    /// service keeps the pre-existing global limit, so this is additive.
    pub fn with_frame_limits(mut self, resolve: FrameLimitResolver) -> Self {
        self.resolve = Some(resolve);
        self
    }
}

/// Create a reactor without an accept loop. Returns a handle for receiving
/// events and a feeder for pushing connections from an external accept loop.
pub fn create_reactor(
    handle: &tokio::runtime::Handle,
    channel_capacity: usize,
) -> (ReactorHandle, ReactorFeeder) {
    let (event_tx, event_rx) = mpsc::channel(channel_capacity);
    // The non-rpc channel is unused in `create_reactor` mode (caller owns the
    // accept loop and dispatches themselves) but we wire it so `ReactorHandle`
    // has the same shape in both entry points.
    let (_non_rpc_tx, non_rpc_rx) = mpsc::channel(channel_capacity);
    (
        ReactorHandle {
            event_rx,
            non_rpc_rx,
        },
        ReactorFeeder {
            event_tx,
            rt_handle: handle.clone(),
            resolve: None,
        },
    )
}

/// Start a reactor that owns the accept loop, with an optional per-method
/// body-frame cap resolver.
///
/// This is the production accept path -- `Server::serve` goes through it -- so
/// without the resolver a service could declare method limits that nothing
/// ever installed. `None` keeps the pre-existing global limit for every
/// service.
pub fn start_reactor_with_limits(
    node: CoreNode,
    channel_capacity: usize,
    resolve: Option<FrameLimitResolver>,
) -> ReactorHandle {
    let (event_tx, event_rx) = mpsc::channel(channel_capacity);
    let (non_rpc_tx, non_rpc_rx) = mpsc::channel(channel_capacity);
    tokio::spawn(accept_loop(node, event_tx, non_rpc_tx, resolve));
    ReactorHandle {
        event_rx,
        non_rpc_rx,
    }
}

#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    node: CoreNode,
    event_tx: mpsc::Sender<ReactorEvent>,
    non_rpc_tx: mpsc::Sender<(Vec<u8>, CoreConnection)>,
    resolve: Option<FrameLimitResolver>,
) {
    loop {
        match node.accept_aster().await {
            Ok((alpn, conn)) => {
                // Filter by ALPN: aster/1 → RPC pipeline, everything else →
                // non-rpc queue for the binding to dispatch (admission, etc.).
                // Previously the reactor accepted every aster-ALPN connection
                // and mis-routed admission streams into the RPC parser, which
                // silently dropped the handshake. See task #31.
                if alpn == RPC_ALPN {
                    let tx = event_tx.clone();
                    tokio::spawn(connection_loop(conn, tx, resolve.clone()));
                } else if non_rpc_tx.send((alpn, conn)).await.is_err() {
                    // Binding dropped the non-rpc receiver. That means nothing
                    // is listening — drop the connection. The peer will see a
                    // closed stream; it can retry or error its client.
                    tracing::debug!("non-rpc aster connection dropped: no receiver");
                }
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

/// Set once, when a connection's accept loop ends (the connection is gone).
/// Shared by every call on the connection, so a call can learn the peer left
/// without each one registering on the transport's own close future, which
/// takes the connection-state lock every stream on it contends for.
#[derive(Default)]
struct ConnectionGone {
    gone: AtomicBool,
    notify: Notify,
}

impl ConnectionGone {
    fn set(&self) {
        self.gone.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.gone.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn connection_loop(
    conn: CoreConnection,
    event_tx: mpsc::Sender<ReactorEvent>,
    resolve: Option<FrameLimitResolver>,
) {
    let peer_id = conn.remote_id();
    let connection_id = next_connection_id();
    let gone = Arc::new(ConnectionGone::default());

    // `accept_aster_bi` transparently dispatches `FLAG_TUNNEL` streams
    // via the per-connection tunnel registry, so the reactor only ever
    // sees regular RPC streams here. See `Aster-tunneling.md` §6.
    while let Ok((send, recv)) = conn.accept_aster_bi().await {
        let tx = event_tx.clone();
        let peer = peer_id.clone();
        let conn_clone = conn.clone();
        let res = resolve.clone();
        let gone = gone.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_stream_inner(send, recv, peer, connection_id, conn_clone, tx, res, gone)
                    .await
            {
                tracing::debug!("reactor stream error: {e}");
            }
        });
    }

    gone.set();

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

/// One frame as read off the wire by the per-stream reader task.
struct WireFrame {
    payload: Vec<u8>,
    flags: u8,
}

/// Resolves the body-frame cap for a call from its raw header bytes.
///
/// Core never decodes the header: it hands the bytes to a closure the `aster`
/// crate installs and gets a decision back. That keeps the payload format out
/// of the transport while still letting the cap be per-method.
pub enum FrameLimit {
    /// Header did not decode. Reset the stream; read no body.
    Reject,
    /// Decoded, but the service/method is unknown here. Use the global cap.
    Default,
    /// This method's body-frame cap.
    Limit(u32),
}

pub type FrameLimitResolver = std::sync::Arc<dyn Fn(&[u8]) -> FrameLimit + Send + Sync>;

/// How many frames may sit between the reader and the dispatch loop.
pub const FRAME_CHANNEL_DEPTH: usize = 8;

/// How long call N+1's header may wait for call N's writer task to finish
/// before it is rejected as arriving mid-call.
///
/// The writer hands the trailer to QUIC inside `write_all`, and the connection
/// driver (another task, often another thread) can put it on the wire before
/// the writer task's poll returns. So a client can read N's trailer, reuse the
/// stream, and get N+1's header back here while `writer.is_finished()` is
/// still false. Rejecting on that alone reset legal pooled calls with
/// `PROTOCOL_VIOLATION` about once in two million under load. A writer that
/// has written its terminal frame finishes within microseconds; one that is
/// blocked on flow control does not finish in this window, and that peer is
/// still rejected, only this much later.
const EARLY_HEADER_GRACE: Duration = Duration::from_secs(1);

/// Test-only fault injection (feature `test-hooks`). Never on in a shipping
/// build; the tests that need it enable it explicitly.
#[cfg(feature = "test-hooks")]
pub mod test_hooks {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static WRITER_EXIT_PAUSE_US: AtomicU64 = AtomicU64::new(0);

    /// Hold the response writer task for `pause` right after it has written a
    /// call's terminal frame, before it completes. That is a descheduled
    /// writer made deterministic: the trailer is on its way (the task yields,
    /// so the connection driver sends it), the peer can start its next call on
    /// the stream, and the task is still unfinished. Process-global.
    pub fn set_writer_exit_pause(pause: Duration) {
        WRITER_EXIT_PAUSE_US.store(pause.as_micros() as u64, Ordering::SeqCst);
    }

    pub(super) async fn writer_exit_pause() {
        let us = WRITER_EXIT_PAUSE_US.load(Ordering::SeqCst);
        if us > 0 {
            tokio::time::sleep(Duration::from_micros(us)).await;
        }
    }
}

/// Test-only counters so a test can assert on what the reader actually did --
/// how many resolver calls it made and how many body bytes it materialized --
/// rather than on the much weaker signal of whether a call was delivered.
#[cfg(feature = "ingress-metrics")]
pub mod ingress_metrics {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static RESOLVER_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static BODY_BYTES_READ: AtomicU64 = AtomicU64::new(0);
    pub static HEADER_BYTES_READ: AtomicU64 = AtomicU64::new(0);
    /// Incremented inside `read_one_frame_capped` at the point the payload
    /// buffer is allocated -- so it covers `accept_aster_bi`'s peek as well as
    /// the reactor's reads. The per-path counters above are downstream of a
    /// successful read and cannot see an allocation that then blocked.
    pub static FRAME_BYTES_ALLOCATED: AtomicU64 = AtomicU64::new(0);

    pub fn reset() {
        RESOLVER_CALLS.store(0, Ordering::Relaxed);
        BODY_BYTES_READ.store(0, Ordering::Relaxed);
        HEADER_BYTES_READ.store(0, Ordering::Relaxed);
        FRAME_BYTES_ALLOCATED.store(0, Ordering::Relaxed);
    }
    pub fn snapshot() -> (u64, u64, u64) {
        (
            RESOLVER_CALLS.load(Ordering::Relaxed),
            HEADER_BYTES_READ.load(Ordering::Relaxed),
            BODY_BYTES_READ.load(Ordering::Relaxed),
        )
    }
    /// Total bytes any frame read has allocated since `reset`.
    pub fn allocated() -> u64 {
        FRAME_BYTES_ALLOCATED.load(Ordering::Relaxed)
    }
}

/// Per-stream frame reader. Unlike a free-running reader, this **handshakes**
/// with dispatch: it reads a call header under the global header cap, waits for
/// the body cap that header implies, reads that call's body frames under it,
/// and resets to the header cap at the next call.
///
/// Free-running was wrong in two ways. It read call *n+1*'s frames under call
/// *n*'s limit, and with an unbounded channel it read them as fast as the peer
/// could send regardless of whether dispatch was keeping up. A bounded channel
/// alone fixes only the second.
/// The reader stopped on `err`. If the peer **reset** its send side, that is a
/// cancellation (it abandoned the call): forward a synthetic `FLAG_CANCEL` so
/// dispatch cancels the call in flight.
///
/// A client cancels by writing `FLAG_CANCEL` and then resetting. A reset lets
/// QUIC discard data not yet read, so the cancel frame itself is often lost --
/// and before this, the reset read like a clean EOF: the dispatch loop then
/// waited on the writer, and a handler that was not producing (an unanswered
/// unary, a client stream waiting for input) never learnt of the cancel.
async fn forward_reset_as_cancel(err: &anyhow::Error, frame_tx: &mpsc::Sender<WireFrame>) {
    let reset = err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<iroh::endpoint::ReadError>(),
            Some(iroh::endpoint::ReadError::Reset(_))
        )
    });
    if reset {
        let _ = frame_tx
            .send(WireFrame {
                payload: Vec::new(),
                flags: FLAG_CANCEL,
            })
            .await;
    }
}

async fn read_all_frames(
    recv: CoreRecvStream,
    frame_tx: mpsc::Sender<WireFrame>,
    resolve: Option<FrameLimitResolver>,
) {
    loop {
        // ── call header: always the global header cap ──
        let (payload, flags) = match recv
            .read_one_frame_capped(crate::framing::MAX_CALL_HEADER_SIZE)
            .await
        {
            Ok(f) => f,
            Err(e) => return forward_reset_as_cancel(&e, &frame_tx).await,
        };

        // Transport validity first. A frame without FLAG_HEADER is not a call
        // header whatever its bytes say, so resolving it -- and then reading a
        // body under whatever cap that produced -- would be acting on an
        // attacker-chosen limit before the frame was known to be a header at
        // all. Dispatch rejects it too, but only after the reader has already
        // gone on to the next frame.
        #[cfg(feature = "ingress-metrics")]
        ingress_metrics::HEADER_BYTES_READ
            .fetch_add(payload.len() as u64, std::sync::atomic::Ordering::Relaxed);

        if flags & crate::framing::FLAG_HEADER == 0 {
            // A cancellation for the call that just ended its request stream.
            // The reader is back in header state, so without this it would
            // silently `return` and the cancel would never reach dispatch --
            // which is exactly what made a server-streaming cancel a no-op,
            // since such a call ends its request stream on the FIRST frame.
            //
            // Forward it and stop reading: FLAG_CANCEL poisons this stream
            // (§6), so there is no next call on it to read a header for.
            if flags & FLAG_CANCEL != 0 {
                let _ = frame_tx.send(WireFrame { payload, flags }).await;
            }
            return;
        }

        #[cfg(feature = "ingress-metrics")]
        ingress_metrics::RESOLVER_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let body_cap = match &resolve {
            None => crate::framing::MAX_FRAME_SIZE,
            Some(f) => match f(&payload) {
                FrameLimit::Reject => return,
                FrameLimit::Default => crate::framing::MAX_FRAME_SIZE,
                FrameLimit::Limit(n) => n,
            },
        };

        if frame_tx.send(WireFrame { payload, flags }).await.is_err() {
            return;
        }

        // ── body frames: this call's cap, until the next header ──
        loop {
            match recv.read_one_frame_capped(body_cap).await {
                Ok((payload, flags)) => {
                    #[cfg(feature = "ingress-metrics")]
                    ingress_metrics::BODY_BYTES_READ
                        .fetch_add(payload.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    let is_header = flags & crate::framing::FLAG_HEADER != 0;
                    if frame_tx.send(WireFrame { payload, flags }).await.is_err() {
                        return;
                    }
                    if is_header {
                        // Shouldn't happen: a header arriving under a body cap
                        // means the peer framed a new call without the reader
                        // resetting. Bail rather than continue under the wrong
                        // limit.
                        return;
                    }
                    if flags & FLAG_END_STREAM != 0 {
                        break; // next iteration reads a header again
                    }
                }
                Err(e) => return forward_reset_as_cancel(&e, &frame_tx).await,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_stream_inner(
    send: CoreSendStream,
    recv: CoreRecvStream,
    peer_id: String,
    connection_id: u64,
    connection: CoreConnection,
    call_tx: mpsc::Sender<ReactorEvent>,
    resolve: Option<FrameLimitResolver>,
    gone: Arc<ConnectionGone>,
) -> Result<()> {
    let stream_id = next_stream_id();

    // Spawn the per-stream reader task. Everything below pulls from
    // `frame_rx`; nothing else touches `recv`.
    let (frame_tx, mut frame_rx) = mpsc::channel(FRAME_CHANNEL_DEPTH);
    let reader_handle = tokio::spawn(read_all_frames(recv, frame_tx, resolve));

    let result = dispatch_stream(
        &mut frame_rx,
        send,
        peer_id,
        connection_id,
        connection,
        stream_id,
        call_tx,
        &gone,
    )
    .await;

    // Stop the reader task. For well-behaved peers it has already exited
    // on EOF; for misbehaving peers we cut it off here.
    reader_handle.abort();
    result
}

/// Marks a call cancelled -- flag first, then the notify -- when dropped
/// while armed, or when fired explicitly.
struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
    armed: bool,
}

impl CancelOnDrop {
    fn fire(&mut self) {
        self.armed = false;
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.fire();
        }
    }
}

/// Outcome of dispatching one call through `dispatch_one_call`.
enum CallOutcome {
    /// The call completed (trailer sent to the peer). The caller loops to read
    /// the next `StreamHeader` on the same stream (multiplexed-stream model --
    /// every stream is multiplexed, spec §6).
    ///
    /// Carries that header when it had already arrived. The reader starts on
    /// call N+1 as soon as N's request stream ends, so with a `biased` select
    /// putting the wire first, N+1's header and N's writer completion can both
    /// be ready — and whoever polls first used to decide whether a perfectly
    /// legal reused stream was accepted or rejected as "mid-call". Holding the
    /// header until the current call's output is finished makes completed
    /// output win without dropping it.
    Complete(Option<WireFrame>),
    /// The peer closed the stream before the call finished. The caller
    /// should return `Ok(())` from the dispatch function.
    StreamEof,
    /// The peer sent `FLAG_CANCEL`. The writer task has been aborted so the
    /// response receiver is dropped and every producer woken with a closed
    /// channel, and the send direction has been **reset**.
    ///
    /// The stream is poisoned, not reusable: a response frame may have been
    /// partially written when flow control blocked, so the byte stream is no
    /// longer at a frame boundary and anything written after it would silently
    /// extend a half-frame. The connection and any connection-scoped session
    /// survive; the client acquires a fresh stream.
    Cancelled,
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
#[allow(clippy::too_many_arguments)]
async fn dispatch_stream(
    frame_rx: &mut mpsc::Receiver<WireFrame>,
    send: CoreSendStream,
    peer_id: String,
    connection_id: u64,
    connection: CoreConnection,
    stream_id: u64,
    call_tx: mpsc::Sender<ReactorEvent>,
    gone: &ConnectionGone,
) -> Result<()> {
    // Call N+1's header, when the previous call had already read it off the
    // wire while finishing its output.
    let mut next_header: Option<WireFrame> = None;

    loop {
        let header = match next_header.take() {
            Some(f) => f,
            None => match frame_rx.recv().await {
                Some(f) => f,
                // Clean EOF between calls — peer closed the multiplexed
                // stream. Don't `finish()` here; just exit so the
                // per-stream task can drop the send half naturally.
                None => return Ok(()),
            },
        };
        if header.flags & FLAG_HEADER == 0 {
            if header.flags & FLAG_CANCEL != 0 {
                // A cancel between calls: the client gave up on a call that
                // completed as it cancelled, or reset an idle stream. Nothing
                // is in flight; the stream is poisoned, so just end it.
                return Ok(());
            }
            return Err(anyhow!(
                "expected StreamHeader (FLAG_HEADER), got flags={:#x}",
                header.flags
            ));
        }

        let first_req = match frame_rx.recv().await {
            Some(f) => f,
            None => return Err(anyhow!("eof after stream header")),
        };
        if first_req.flags & FLAG_CANCEL != 0 {
            // Cancelled before the first request: there is no call to
            // dispatch (nothing was delivered). Poison the stream as for any
            // cancel.
            send.reset(crate::framing::RESET_CODE_CANCELLED)
                .await
                .context("poisoning a cancelled stream")?;
            return Ok(());
        }

        let outcome = dispatch_one_call(
            &send,
            frame_rx,
            &peer_id,
            connection_id,
            &connection,
            stream_id,
            &call_tx,
            gone,
            header.payload,
            header.flags,
            first_req.payload,
            first_req.flags,
        )
        .await?;

        match outcome {
            CallOutcome::StreamEof => return Ok(()),
            // A cancelled stream is poisoned (§6): end the stream task rather
            // than looping for a next call that must not be served on it.
            CallOutcome::Cancelled => return Ok(()),
            CallOutcome::Complete(prefetched) => next_header = prefetched,
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
    frame_rx: &mut mpsc::Receiver<WireFrame>,
    peer_id: &str,
    connection_id: u64,
    connection: &CoreConnection,
    stream_id: u64,
    call_tx: &mpsc::Sender<ReactorEvent>,
    gone: &ConnectionGone,
    header_payload: Vec<u8>,
    header_flags: u8,
    first_request_payload: Vec<u8>,
    first_request_flags: u8,
) -> Result<CallOutcome> {
    // Both queues are bounded, but on different invariants. Requests retain
    // §12.4's eight-item depth. Responses use an aggregate byte budget equal
    // to eight maximum frames plus a separate item ceiling for tiny frames.
    //
    // The request queue is fed by the reader from remote bytes, so an unbounded
    // one let a slow handler turn the bounded wire channel into an unbounded
    // remote-driven buffer -- the cap moved rather than bound anything.
    //
    // The response queue is equally remote-driven in practice: a peer can start
    // a server-streaming call and stop reading, so what accumulates is memory
    // the peer controls. It is written from a sync `extern "C"` function that
    // cannot `.await`, which is why the FFI reserves capacity with `try_reserve`
    // and returns `QUEUE_FULL` rather than blocking -- blocking there would pin
    // Java virtual-thread carriers.
    let (resp_tx, mut resp_rx) = response_channel();
    let (req_tx, req_rx) = mpsc::channel(FRAME_CHANNEL_DEPTH);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_notify = Arc::new(Notify::new());
    // Any exit from here that is not a completed call -- `FLAG_CANCEL`, a
    // failed write, a framing violation, a dropped response channel -- marks
    // the call cancelled and wakes whoever waits on it. Disarmed only once
    // the terminal frame has been written.
    let mut cancel_guard = CancelOnDrop {
        cancelled: cancelled.clone(),
        notify: cancel_notify.clone(),
        armed: true,
    };

    // No `request_done` flag any more: the wire branch is unconditionally
    // enabled, so there is nothing to gate. Whether the request stream has
    // ended is expressed by `req_tx_opt` being `None`.
    let request_ended = first_request_flags & FLAG_END_STREAM != 0;
    let mut req_tx_opt = if request_ended { None } else { Some(req_tx) };

    call_tx
        .send(ReactorEvent::Call(IncomingCall {
            call_id: next_call_id(),
            stream_id,
            connection_id,
            connection: connection.clone(),
            header_payload,
            header_flags,
            request_payload: first_request_payload,
            request_flags: first_request_flags,
            peer_id: peer_id.to_string(),
            response_sender: resp_tx,
            request_receiver: req_rx,
            cancelled: cancelled.clone(),
            cancel_notify: cancel_notify.clone(),
        }))
        .await
        .map_err(|_| anyhow!("reactor channel closed"))?;

    // The response writer runs in its OWN task, owning `resp_rx` and a clone of
    // the send half.
    //
    // This is what makes cancellation observable. Writing inline in a `select!`
    // arm meant that when the peer stopped reading, flow control blocked the
    // dispatch loop inside `write_all` — so it was not in the `select!` and
    // could not poll the wire for a `FLAG_CANCEL` at all. Racing `write_all`
    // against the read would not fix it either, because a cancelled partial
    // write leaves the byte stream off a frame boundary.
    //
    // With a separate task the dispatch loop is always able to read the wire,
    // and cancelling is `abort()` + `reset()` rather than an attempt to resume
    // a stream that is no longer framed.
    let send_writer = send.clone();
    let mut writer = tokio::spawn(async move {
        while let Some(resp) = resp_rx.recv().await {
            let (resp, _capacity_lease) = resp.into_parts();
            let (bytes, terminal) = match resp {
                OutgoingFrame::Frame(b) => (b, false),
                OutgoingFrame::Trailer(b) | OutgoingFrame::CompleteUnary(b) => (b, true),
            };
            if !bytes.is_empty() && send_writer.write_all(bytes).await.is_err() {
                return false;
            }
            if terminal {
                #[cfg(feature = "test-hooks")]
                test_hooks::writer_exit_pause().await;
                return true;
            }
        }
        // Channel closed with no trailer: the binding dropped the sender, or a
        // cancellation dropped the receiver.
        false
    });

    // Set once call N+1's header has arrived while this call's output is still
    // being written. Gates the wire branch off, because everything after it on
    // the wire belongs to N+1.
    let mut pending_header: Option<WireFrame> = None;

    loop {
        tokio::select! {
            biased;

            // Read the peer's frames. This branch stays enabled AFTER
            // `request_done`, which it did not used to: a server-streaming call
            // ends its request stream with the first frame, so gating on
            // `!request_done` meant a later FLAG_CANCEL was never read at all
            // and the producer streamed on to a peer that had cancelled.
            //
            // A completed call's next header is retained as `pending_header`
            // and handed back to `dispatch_stream`; it is never discarded.
            wire = frame_rx.recv(), if pending_header.is_none() => {
                match wire {
                    Some(frame) => {
                        if frame.flags & FLAG_CANCEL != 0 {
                            // A real cancellation. Setting the flag is not
                            // enough on its own: a producer blocked on a
                            // bounded `send` -- or an FFI caller spinning on
                            // QUEUE_FULL -- is waiting on CAPACITY, and an
                            // atomic nobody is polling never wakes it.
                            //
                            // Dropping the receiver is what wakes them. Every
                            // producer, in every binding, then observes a
                            // closed channel: `SendError` in Rust, NOT_FOUND
                            // from the C ABI. One mechanism, no per-binding
                            // cancellation plumbing.
                            cancel_guard.fire();
                            // Close the handler's input.
                            drop(req_tx_opt.take());
                            // Abort the writer and AWAIT it, so `resp_rx` is
                            // dropped before we return. That drop is the wake:
                            // every producer, in every binding, then observes a
                            // closed channel -- `SendError` in Rust, NOT_FOUND
                            // from the C ABI. One mechanism, no per-binding
                            // cancellation plumbing, and no reliance on anyone
                            // polling the atomic flag.
                            writer.abort();
                            let _ = (&mut writer).await;
                            // Poison the send direction: a response frame may
                            // have been partially written when flow control
                            // blocked, so the stream is no longer at a frame
                            // boundary and must never be written to again.
                            // Named code so the peer can tell a
                            // deliberate cancellation from a fault.
                            // Propagate: a reset that could not be issued
                            // means the stream was NOT poisoned, and silently
                            // reporting a clean cancellation would invite the
                            // reuse the poison exists to prevent.
                            send.reset(crate::framing::RESET_CODE_CANCELLED)
                                .await
                                .context("poisoning a cancelled stream")?;
                            return Ok(CallOutcome::Cancelled);
                        }
                        if frame.flags & FLAG_HEADER != 0 {
                            if req_tx_opt.is_some() {
                                // The request stream has NOT ended, so this is
                                // the peer starting the next call before
                                // finishing this one's requests. Protocol
                                // violation -- every multi-frame call MUST
                                // terminate its request stream with
                                // FLAG_END_STREAM before opening the next.
                                writer.abort();
                                let _ = (&mut writer).await;
                                // A violation, NOT a cancellation: a client
                                // must not read its own cancel as honoured
                                // when the server rejected what it sent.
                                send.reset(crate::framing::RESET_CODE_PROTOCOL_VIOLATION)
                                    .await
                                    .context("resetting a stream after a framing violation")?;
                                return Err(anyhow!(
                                    "got StreamHeader before previous call's END_STREAM"
                                ));
                            }
                            // Calls are strictly sequential (§ session-scoped
                            // services): N+1 may not begin until N's RESPONSE
                            // has finished, not merely its request.
                            //
                            // So a header here is only the benign
                            // already-finished race -- writer completion and
                            // N+1's header both ready, biased select happening
                            // to poll the wire first. Anything else is the peer
                            // running ahead.
                            //
                            // "Finished" has to allow for the writer task
                            // still being on its way out: the trailer can reach
                            // the peer, and N+1's header come back, before the
                            // task that wrote it completes (EARLY_HEADER_GRACE).
                            // Wait for it, briefly. Wire reads stop meanwhile,
                            // but only for the grace period.
                            if !writer.is_finished() {
                                if let Ok(written) =
                                    tokio::time::timeout(EARLY_HEADER_GRACE, &mut writer).await
                                {
                                    return match written {
                                        Ok(true) => {
                                            cancel_guard.armed = false;
                                            Ok(CallOutcome::Complete(Some(frame)))
                                        }
                                        Ok(false) => Err(anyhow!(
                                            "binding dropped response channel without trailer"
                                        )),
                                        Err(e) => Err(anyhow!("response writer task failed: {e}")),
                                    };
                                }
                            }
                            if !writer.is_finished() {
                                // Accepting this would stop wire reads while
                                // the writer is still blocked, so a FLAG_CANCEL
                                // arriving next would never be read and the
                                // producer would never wake:
                                //   N END_STREAM -> early N+1 header -> CANCEL
                                //
                                // Tear the writer down before returning.
                                // Dropping its JoinHandle does NOT abort the
                                // task -- it would keep holding `resp_rx` and
                                // the producer would stay blocked on a stream
                                // nobody is reading, which is the very hang
                                // this rejection exists to prevent.
                                writer.abort();
                                let _ = (&mut writer).await;
                                // A violation, NOT a cancellation: a client
                                // must not read its own cancel as honoured
                                // when the server rejected what it sent.
                                send.reset(crate::framing::RESET_CODE_PROTOCOL_VIOLATION)
                                    .await
                                    .context("resetting a stream after a framing violation")?;
                                return Err(anyhow!(
                                    "got StreamHeader before previous call's \
                                     response completed"
                                ));
                            }
                            // Writer already done: hold the header and stop
                            // reading, since every frame after it belongs to
                            // N+1 and consuming more here would steal them.
                            pending_header = Some(frame);
                            continue;
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
                                // Awaited, not dropped. On a bounded channel
                                // `send` is a future: `let _ =` would discard
                                // it and silently lose the frame -- and would
                                // also discard the backpressure this bound
                                // exists to apply.
                                let _ = tx
                                    .send(RequestFrame {
                                        payload: frame.payload,
                                        flags: frame.flags,
                                    })
                                    .await;
                            }
                        }
                        if end {
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
                        // Let the writer flush whatever the binding already
                        // queued (often the trailer) before we report EOF.
                        //
                        // EOF is also what a lost connection looks like from
                        // here. Then a handler that is not producing would
                        // keep the writer -- and this call -- alive until its
                        // deadline, never learning the peer is gone. Racing
                        // the connection's close turns that into a cancel.
                        tokio::select! {
                            biased;
                            written = &mut writer => {
                                if let Ok(true) = written {
                                    cancel_guard.armed = false;
                                }
                            }
                            () = gone.wait() => {
                                writer.abort();
                                let _ = (&mut writer).await;
                            }
                        }
                        return Ok(CallOutcome::StreamEof);
                    }
                }
            }

            // The writer finished: it wrote a terminal frame, or the binding
            // dropped the response sender.
            //
            // Don't `finish()` the send side — every stream is multiplexed, so
            // the next call (if any) on this stream still needs to write its
            // frames. The send half is dropped naturally when the peer EOFs and
            // we return cleanly from dispatch_stream.
            written = &mut writer => {
                return match written {
                    Ok(true) => {
                        cancel_guard.armed = false;
                        Ok(CallOutcome::Complete(pending_header.take()))
                    }
                    Ok(false) => Err(anyhow!(
                        "binding dropped response channel without trailer"
                    )),
                    Err(e) => Err(anyhow!("response writer task failed: {e}")),
                };
            }
        }
    }
}
