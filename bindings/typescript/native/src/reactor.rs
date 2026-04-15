//! Server reactor surface for TypeScript.
//!
//! napi-rs mirror of the `ReactorHandle` / `ReactorEvent` / sender / receiver
//! family in `bindings/python/rust/src/net.rs:999+`. The reactor itself lives
//! in `aster_transport_core::reactor`; this module wraps it so JS can drain
//! events, dispatch calls on a pool of worker tasks, and reap per-connection
//! state on `ConnectionClosed` events (spec §7.5).

use std::sync::{Arc, Mutex as StdMutex};

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::reactor as core_reactor;

use crate::net::IrohConnection;
use crate::node::IrohNode;

// ============================================================================
// ReactorResponseSender
// ============================================================================

/// Per-call response sender surfaced to the binding's dispatcher. Carries
/// the unbounded channel that the reactor drains to write frames to the
/// wire. For unary calls use [`submit`] (single write of frame+trailer);
/// for streaming calls call [`sendFrame`] for each frame and [`sendTrailer`]
/// to terminate.
#[napi]
pub struct ReactorResponseSender {
    inner: StdMutex<Option<tokio::sync::mpsc::UnboundedSender<core_reactor::OutgoingFrame>>>,
}

#[napi]
impl ReactorResponseSender {
    /// Unary fast-path: one Frame + one Trailer, atomically. After this
    /// call the sender is consumed and further submits fail.
    #[napi]
    pub fn submit(&self, response_frame: Buffer, trailer_frame: Buffer) -> Result<()> {
        let sender = self
            .inner
            .lock()
            .map_err(|_| Error::from_reason("lock poisoned".to_string()))?
            .take()
            .ok_or_else(|| Error::from_reason("response already submitted".to_string()))?;
        let response = response_frame.to_vec();
        if !response.is_empty() {
            let _ = sender.send(core_reactor::OutgoingFrame::Frame(response));
        }
        let _ = sender.send(core_reactor::OutgoingFrame::Trailer(trailer_frame.to_vec()));
        Ok(())
    }

    /// Streaming: send one response data frame. The sender stays open for
    /// further `sendFrame` / `sendTrailer` calls.
    #[napi]
    pub fn send_frame(&self, frame: Buffer) -> Result<()> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| Error::from_reason("lock poisoned".to_string()))?;
        let sender = guard
            .as_ref()
            .ok_or_else(|| Error::from_reason("response stream already closed".to_string()))?;
        sender
            .send(core_reactor::OutgoingFrame::Frame(frame.to_vec()))
            .map_err(|_| Error::from_reason("reactor dropped response channel".to_string()))?;
        Ok(())
    }

    /// Streaming terminator: send the trailer frame and consume the sender.
    /// Further sends fail.
    #[napi]
    pub fn send_trailer(&self, trailer_frame: Buffer) -> Result<()> {
        let sender = self
            .inner
            .lock()
            .map_err(|_| Error::from_reason("lock poisoned".to_string()))?
            .take()
            .ok_or_else(|| Error::from_reason("response stream already closed".to_string()))?;
        sender
            .send(core_reactor::OutgoingFrame::Trailer(trailer_frame.to_vec()))
            .map_err(|_| Error::from_reason("reactor dropped response channel".to_string()))?;
        Ok(())
    }
}

// ============================================================================
// ReactorRequestReceiver
// ============================================================================

/// Per-call additional-request-frame stream. The first request frame
/// arrives inline on the [`ReactorEvent`]; this wrapper yields every
/// frame after that until the stream ends or a `FLAG_END_STREAM` frame
/// arrives. Only client-streaming and bidi dispatchers need this.
#[napi]
pub struct ReactorRequestReceiver {
    inner: Arc<
        tokio::sync::Mutex<
            Option<tokio::sync::mpsc::UnboundedReceiver<core_reactor::RequestFrame>>,
        >,
    >,
}

/// Result of [`ReactorRequestReceiver::recv`]. `payload` is empty and
/// `flags` is `0` when the stream has finished (i.e. when the Python
/// equivalent would return `None`); callers should also check `done`.
#[napi(object)]
pub struct ReactorRequestFrame {
    pub payload: Buffer,
    pub flags: u8,
    pub done: bool,
}

#[napi]
impl ReactorRequestReceiver {
    /// Await the next request frame. `done: true` means the peer has
    /// finished sending and the channel is closed.
    #[napi]
    pub async fn recv(&self) -> Result<ReactorRequestFrame> {
        let inner = self.inner.clone();
        let mut guard = inner.lock().await;
        let Some(rx) = guard.as_mut() else {
            return Ok(ReactorRequestFrame {
                payload: Buffer::from(Vec::<u8>::new()),
                flags: 0,
                done: true,
            });
        };
        match rx.recv().await {
            Some(frame) => Ok(ReactorRequestFrame {
                payload: Buffer::from(frame.payload),
                flags: frame.flags,
                done: false,
            }),
            None => {
                *guard = None;
                Ok(ReactorRequestFrame {
                    payload: Buffer::from(Vec::<u8>::new()),
                    flags: 0,
                    done: true,
                })
            }
        }
    }
}

// ============================================================================
// ReactorCancelFlag
// ============================================================================

/// Sync cancel flag wrapping the reactor's per-call `Arc<AtomicBool>`.
/// Streaming dispatchers poll this between iterations to stop early when
/// the peer sends `FLAG_CANCEL` or the QUIC stream errors mid-call.
#[napi]
pub struct ReactorCancelFlag {
    inner: Arc<std::sync::atomic::AtomicBool>,
}

#[napi]
impl ReactorCancelFlag {
    #[napi(getter)]
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(std::sync::atomic::Ordering::Acquire)
    }
}

// ============================================================================
// ReactorEvent
// ============================================================================

/// Event surfaced by [`ReactorHandle::nextEvent`]. `kind` is
/// `"call"` or `"connection_closed"`. Call-specific fields are
/// only populated on `call`; `close*` fields only on `connection_closed`.
#[napi]
pub struct ReactorEvent {
    kind: u8, // 0 = Call, 1 = ConnectionClosed
    connection_id: u64,
    peer_id: String,
    // Call-only fields (kind == 0).
    call_id: u64,
    header_payload: Option<Vec<u8>>,
    header_flags: u8,
    request_payload: Option<Vec<u8>>,
    request_flags: u8,
    sender: StdMutex<Option<ReactorResponseSender>>,
    request_receiver: StdMutex<Option<ReactorRequestReceiver>>,
    cancel_flag: Option<ReactorCancelFlag>,
    // ConnectionClosed-only fields (kind == 1).
    close_kind: Option<String>,
    close_code: Option<u64>,
    close_reason: Option<Vec<u8>>,
}

#[napi]
impl ReactorEvent {
    /// `"call"` or `"connection_closed"`.
    #[napi(getter)]
    pub fn kind(&self) -> String {
        match self.kind {
            0 => "call".to_string(),
            _ => "connection_closed".to_string(),
        }
    }

    #[napi(getter)]
    pub fn connection_id(&self) -> BigInt {
        BigInt::from(self.connection_id)
    }

    #[napi(getter)]
    pub fn peer_id(&self) -> String {
        self.peer_id.clone()
    }

    #[napi(getter)]
    pub fn call_id(&self) -> BigInt {
        BigInt::from(self.call_id)
    }

    #[napi(getter)]
    pub fn header_payload(&self) -> Option<Buffer> {
        self.header_payload
            .as_ref()
            .map(|b| Buffer::from(b.clone()))
    }

    #[napi(getter)]
    pub fn header_flags(&self) -> u8 {
        self.header_flags
    }

    #[napi(getter)]
    pub fn request_payload(&self) -> Option<Buffer> {
        self.request_payload
            .as_ref()
            .map(|b| Buffer::from(b.clone()))
    }

    #[napi(getter)]
    pub fn request_flags(&self) -> u8 {
        self.request_flags
    }

    /// Take the response sender out of this Call event. May be called
    /// once; returns `null` on subsequent calls or for non-Call events.
    #[napi]
    pub fn take_sender(&self) -> Option<ReactorResponseSender> {
        let mut guard = self.sender.lock().ok()?;
        guard.take()
    }

    /// Take the request receiver out of this Call event. May be called
    /// once; returns `null` on subsequent calls or for non-Call events.
    /// Only client-streaming and bidi dispatchers need this.
    #[napi]
    pub fn take_request_receiver(&self) -> Option<ReactorRequestReceiver> {
        let mut guard = self.request_receiver.lock().ok()?;
        guard.take()
    }

    /// Take the cancel flag (returns a fresh handle sharing the same
    /// atomic). Streaming dispatchers poll it to stop early on
    /// `FLAG_CANCEL` or stream reset.
    #[napi]
    pub fn take_cancel_flag(&self) -> Option<ReactorCancelFlag> {
        self.cancel_flag.as_ref().map(|f| ReactorCancelFlag {
            inner: f.inner.clone(),
        })
    }

    #[napi(getter)]
    pub fn close_kind(&self) -> Option<String> {
        self.close_kind.clone()
    }

    #[napi(getter)]
    pub fn close_code(&self) -> Option<BigInt> {
        self.close_code.map(BigInt::from)
    }

    #[napi(getter)]
    pub fn close_reason(&self) -> Option<Buffer> {
        self.close_reason.as_ref().map(|b| Buffer::from(b.clone()))
    }
}

// ============================================================================
// ReactorHandle
// ============================================================================

/// Consumer handle for a running reactor. Bindings poll [`nextEvent`] in
/// a loop to drain calls and connection-closed signals.
#[napi]
pub struct ReactorHandle {
    inner: Arc<tokio::sync::Mutex<core_reactor::ReactorHandle>>,
}

#[napi]
impl ReactorHandle {
    /// Pull the next reactor event. Returns `null` when the reactor has
    /// shut down and no more events will arrive.
    #[napi]
    pub async fn next_event(&self) -> Result<Option<ReactorEvent>> {
        let handle = self.inner.clone();
        let mut guard = handle.lock().await;
        match guard.next_event().await {
            Some(core_reactor::ReactorEvent::Call(call)) => {
                let sender = ReactorResponseSender {
                    inner: StdMutex::new(Some(call.response_sender)),
                };
                let request_receiver = ReactorRequestReceiver {
                    inner: Arc::new(tokio::sync::Mutex::new(Some(call.request_receiver))),
                };
                let cancel_flag = ReactorCancelFlag {
                    inner: call.cancelled,
                };
                Ok(Some(ReactorEvent {
                    kind: 0,
                    connection_id: call.connection_id,
                    peer_id: call.peer_id,
                    call_id: call.call_id,
                    header_payload: Some(call.header_payload),
                    header_flags: call.header_flags,
                    request_payload: Some(call.request_payload),
                    request_flags: call.request_flags,
                    sender: StdMutex::new(Some(sender)),
                    request_receiver: StdMutex::new(Some(request_receiver)),
                    cancel_flag: Some(cancel_flag),
                    close_kind: None,
                    close_code: None,
                    close_reason: None,
                }))
            }
            Some(core_reactor::ReactorEvent::ConnectionClosed {
                peer_id,
                connection_id,
                info,
            }) => Ok(Some(ReactorEvent {
                kind: 1,
                connection_id,
                peer_id,
                call_id: 0,
                header_payload: None,
                header_flags: 0,
                request_payload: None,
                request_flags: 0,
                sender: StdMutex::new(None),
                request_receiver: StdMutex::new(None),
                cancel_flag: None,
                close_kind: Some(info.kind),
                close_code: info.code,
                close_reason: info.reason,
            })),
            None => Ok(None),
        }
    }
}

// ============================================================================
// ReactorFeeder
// ============================================================================

/// Pushes accepted connections into a reactor created via `createReactor`.
/// Used when the caller owns the accept loop and dispatches by ALPN before
/// forwarding the RPC connections.
#[napi]
pub struct ReactorFeeder {
    inner: core_reactor::ReactorFeeder,
}

#[napi]
impl ReactorFeeder {
    #[napi]
    pub fn feed(&self, conn: &IrohConnection) {
        self.inner.feed(conn.core_clone());
    }
}

// ============================================================================
// createReactor / startReactor (pair of factory functions)
// ============================================================================

/// Result of [`createReactor`]. napi classes can't be returned by value
/// through an `#[napi(object)]` field, so we wrap the pair in a small
/// holder with `takeHandle` / `takeFeeder` accessors — each may be called
/// exactly once.
#[napi]
pub struct ReactorPair {
    handle: StdMutex<Option<ReactorHandle>>,
    feeder: StdMutex<Option<ReactorFeeder>>,
}

#[napi]
impl ReactorPair {
    #[napi]
    pub fn take_handle(&self) -> Result<ReactorHandle> {
        self.handle
            .lock()
            .map_err(|_| Error::from_reason("lock poisoned".to_string()))?
            .take()
            .ok_or_else(|| Error::from_reason("handle already taken".to_string()))
    }

    #[napi]
    pub fn take_feeder(&self) -> Result<ReactorFeeder> {
        self.feeder
            .lock()
            .map_err(|_| Error::from_reason("lock poisoned".to_string()))?
            .take()
            .ok_or_else(|| Error::from_reason("feeder already taken".to_string()))
    }
}

/// Start a reactor that owns its own accept loop on the given node.
/// Suitable when the server only serves the Aster ALPN.
#[napi]
pub fn start_reactor(node: &IrohNode, channel_capacity: u32) -> ReactorHandle {
    let rt_handle = tokio::runtime::Handle::current();
    let core_handle =
        core_reactor::start_reactor_on(&rt_handle, node.core_clone(), channel_capacity as usize);
    ReactorHandle {
        inner: Arc::new(tokio::sync::Mutex::new(core_handle)),
    }
}

/// Create a reactor that receives externally-fed connections. Returns the
/// handle for event polling and the feeder for pushing accepted connections,
/// bundled in a [`ReactorPair`] for take-once extraction.
#[napi]
pub fn create_reactor(channel_capacity: u32) -> ReactorPair {
    let rt_handle = tokio::runtime::Handle::current();
    let (core_handle, core_feeder) =
        core_reactor::create_reactor(&rt_handle, channel_capacity as usize);
    ReactorPair {
        handle: StdMutex::new(Some(ReactorHandle {
            inner: Arc::new(tokio::sync::Mutex::new(core_handle)),
        })),
        feeder: StdMutex::new(Some(ReactorFeeder { inner: core_feeder })),
    }
}
