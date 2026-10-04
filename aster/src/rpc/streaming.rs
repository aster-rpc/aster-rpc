//! Typed stream handles for streaming RPC methods.
//!
//! Server handlers receive a [`RequestStream<Req>`] (client-stream / bidi input)
//! and/or a [`ResponseSink<Resp>`] (server-stream / bidi output); clients get a
//! [`MessageStream<Resp>`] (server-stream / bidi output). Each wraps the
//! byte-level transport with the service's payload `Fory` for typed encode /
//! decode. Constructed by `#[aster::service]`-generated code.

use std::marker::PhantomData;
use std::sync::Arc;

use fory_core::{Fory, Serializer};
use tokio::sync::mpsc::Receiver;

use aster_transport_core::framing::encode_payload_frame;
use aster_transport_core::reactor::{OutgoingFrame, RequestFrame, ResponseSender};

use super::client::{BidiSink, ResponseStream};
use super::codec::RpcStatus;
use super::server::{corrupt_frame_status, next_request_frame, CallFailer, NextFrame};
use super::status::{status_to_error, StatusCode};
use crate::error::{Error, Result};

fn encode_err(e: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("payload encode: {e}"))
}
fn decode_err(e: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("payload decode: {e}"))
}
fn channel_closed() -> Error {
    Error::Connection("rpc response channel closed (call cancelled or connection gone)".into())
}

/// Server-side output sink for streaming responses (server-stream / bidi). Send
/// items with [`send`](ResponseSink::send); the dispatcher writes the terminal
/// trailer once the handler returns.
pub struct ResponseSink<T> {
    tx: ResponseSender,
    fory: Arc<Fory>,
    _t: PhantomData<fn() -> T>,
}

impl<T> ResponseSink<T> {
    pub(crate) fn new(tx: ResponseSender, fory: Arc<Fory>) -> Self {
        Self {
            tx,
            fory,
            _t: PhantomData,
        }
    }
}

impl<T: Serializer<Target = T>> ResponseSink<T> {
    /// Encode and send one streaming response item.
    /// Async since §12.4 bounded the response channel; see [`Call::send`].
    pub async fn send(&self, item: &T) -> Result<()> {
        let bytes = self.fory.serialize(item).map_err(encode_err)?;
        let frame = encode_payload_frame(&bytes, 0)?;
        self.tx
            .send(OutgoingFrame::Frame(frame))
            .await
            .map_err(|_| channel_closed())
    }
}

/// Client-side send half of a bidi call, typed (see
/// [`RpcConnection::bidi_open`](crate::rpc::RpcConnection::bidi_open) and the
/// generated `*_streaming` stub). Encode-and-send each request as it is produced;
/// read the paired [`MessageStream`] concurrently. `&self` methods, so share it
/// across tasks via `Arc`.
pub struct BidiCall<T> {
    sink: BidiSink,
    fory: Arc<Fory>,
    _t: PhantomData<fn() -> T>,
}

impl<T> BidiCall<T> {
    /// Wrap a raw [`BidiSink`] with a payload `Fory`. Called by generated stubs.
    pub fn new(sink: BidiSink, fory: Arc<Fory>) -> Self {
        Self {
            sink,
            fory,
            _t: PhantomData,
        }
    }

    /// Signal end-of-requests (see [`BidiSink::finish`]).
    pub async fn finish(&self) -> Result<()> {
        self.sink.finish().await
    }
}

impl<T: Serializer<Target = T>> BidiCall<T> {
    /// Encode and send one request, flushed immediately.
    pub async fn send(&self, item: &T) -> Result<()> {
        let bytes = self.fory.serialize(item).map_err(encode_err)?;
        self.sink.send(bytes).await
    }
}

/// Server-side input stream of decoded requests (client-stream / bidi). Pull
/// with [`recv`](RequestStream::recv) until it returns `Ok(None)`.
///
/// A request that is not a valid payload -- a frame that does not inflate, or
/// bytes that do not decode as `T` -- fails the call: the server sends
/// `INVALID_ARGUMENT` to the caller itself, logs a warning (target
/// `aster::rpc::dispatch`), and `recv` returns that status as
/// [`Error::Rpc`]. The stream ends there; later `recv`s return `Ok(None)`
/// and later responses find the call finished.
pub struct RequestStream<T> {
    first: Option<Vec<u8>>,
    rx: Option<Receiver<RequestFrame>>,
    first_is_end: bool,
    fory: Arc<Fory>,
    failer: CallFailer,
    /// Requests read so far (for the corrupt-frame log).
    read: u64,
    failed: bool,
    _t: PhantomData<fn() -> T>,
}

impl<T> RequestStream<T> {
    pub(crate) fn new(
        first: Option<Vec<u8>>,
        rx: Option<Receiver<RequestFrame>>,
        first_is_end: bool,
        fory: Arc<Fory>,
        failer: CallFailer,
    ) -> Self {
        Self {
            first,
            rx,
            first_is_end,
            fory,
            failer,
            read: 0,
            failed: false,
            _t: PhantomData,
        }
    }

    async fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        if self.failed {
            return Ok(None);
        }
        if let Some(first) = self.first.take() {
            return Ok(Some(first));
        }
        if self.first_is_end {
            return Ok(None);
        }
        let Some(rx) = &mut self.rx else {
            return Ok(None);
        };
        match next_request_frame(rx).await {
            NextFrame::Frame(p) => Ok(Some(p)),
            NextFrame::End => Ok(None),
            NextFrame::Corrupt(err) => {
                let status = corrupt_frame_status(&err);
                Err(self.fail(self.read, &err, status).await)
            }
        }
    }

    /// Fail the call with `status` (sent by the server) and return it as the
    /// handler's error.
    async fn fail(&mut self, frame: u64, err: &str, status: RpcStatus) -> Error {
        self.failed = true;
        if let Some(rx) = &mut self.rx {
            rx.close();
        }
        self.failer.fail(frame, err, &status).await;
        status_to_error(status)
    }
}

impl<T: Serializer<Target = T>> RequestStream<T> {
    /// Receive and decode the next request, or `None` at end of stream. A
    /// corrupt request fails the call (see the type docs) and returns
    /// `Err(Error::Rpc { code: INVALID_ARGUMENT, .. })`.
    pub async fn recv(&mut self) -> Result<Option<T>> {
        let Some(bytes) = self.recv_bytes().await? else {
            return Ok(None);
        };
        // This request's index in the stream (0 = the first frame).
        let frame = self.read;
        self.read += 1;
        match self.fory.deserialize::<T>(&bytes) {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                let err = e.to_string();
                let status = RpcStatus::error(
                    StatusCode::InvalidArgument,
                    format!("request decode: {err}"),
                );
                Err(self.fail(frame, &err, status).await)
            }
        }
    }
}

/// Client-side stream of decoded responses (server-stream / bidi). Read with
/// [`recv`](MessageStream::recv), or gather everything with
/// [`collect`](MessageStream::collect).
pub struct MessageStream<T> {
    inner: ResponseStream,
    fory: Arc<Fory>,
    _t: PhantomData<fn() -> T>,
}

impl<T> MessageStream<T> {
    /// Wrap a raw response stream with a payload `Fory` for typed decoding.
    /// Called by `#[aster::service]`-generated client stubs.
    pub fn new(inner: ResponseStream, fory: Arc<Fory>) -> Self {
        Self {
            inner,
            fory,
            _t: PhantomData,
        }
    }
}

impl<T: Serializer<Target = T>> MessageStream<T> {
    /// The next decoded response: `Some(Ok(_))` per item, `None` at the OK
    /// trailer / EOF, `Some(Err(_))` on a non-OK trailer or a decode error.
    pub async fn recv(&mut self) -> Option<Result<T>> {
        match self.inner.next().await {
            Some(Ok(bytes)) => Some(self.fory.deserialize::<T>(&bytes).map_err(decode_err)),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    }

    /// Collect all responses, short-circuiting on the first error.
    pub async fn collect(&mut self) -> Result<Vec<T>> {
        let mut out = Vec::new();
        while let Some(item) = self.recv().await {
            out.push(item?);
        }
        Ok(out)
    }
}
