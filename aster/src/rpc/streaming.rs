//! Typed stream handles for streaming RPC methods.
//!
//! Server handlers receive a [`RequestStream<Req>`] (client-stream / bidi input)
//! and/or a [`ResponseSink<Resp>`] (server-stream / bidi output); clients get a
//! [`MessageStream<Resp>`] (server-stream / bidi output). Each wraps the
//! byte-level transport with the service's payload `Fory` for typed encode /
//! decode. Constructed by `#[aster::service]`-generated code.

use std::marker::PhantomData;
use std::sync::Arc;

use fory_core::{Fory, ForyDefault, Serializer};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use aster_transport_core::framing::encode_frame;
use aster_transport_core::reactor::{OutgoingFrame, RequestFrame};

use super::client::ResponseStream;
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
    tx: UnboundedSender<OutgoingFrame>,
    fory: Arc<Fory>,
    _t: PhantomData<fn() -> T>,
}

impl<T> ResponseSink<T> {
    pub(crate) fn new(tx: UnboundedSender<OutgoingFrame>, fory: Arc<Fory>) -> Self {
        Self {
            tx,
            fory,
            _t: PhantomData,
        }
    }
}

impl<T: Serializer> ResponseSink<T> {
    /// Encode and send one streaming response item.
    pub fn send(&self, item: &T) -> Result<()> {
        let bytes = self.fory.serialize(item).map_err(encode_err)?;
        let frame = encode_frame(&bytes, 0)?;
        self.tx
            .send(OutgoingFrame::Frame(frame))
            .map_err(|_| channel_closed())
    }
}

/// Server-side input stream of decoded requests (client-stream / bidi). Pull
/// with [`recv`](RequestStream::recv) until it returns `None`.
pub struct RequestStream<T> {
    first: Option<Vec<u8>>,
    rx: Option<UnboundedReceiver<RequestFrame>>,
    first_is_end: bool,
    fory: Arc<Fory>,
    _t: PhantomData<fn() -> T>,
}

impl<T> RequestStream<T> {
    pub(crate) fn new(
        first: Option<Vec<u8>>,
        rx: Option<UnboundedReceiver<RequestFrame>>,
        first_is_end: bool,
        fory: Arc<Fory>,
    ) -> Self {
        Self {
            first,
            rx,
            first_is_end,
            fory,
            _t: PhantomData,
        }
    }

    async fn recv_bytes(&mut self) -> Option<Vec<u8>> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        if self.first_is_end {
            return None;
        }
        match &mut self.rx {
            Some(rx) => rx.recv().await.map(|f| f.payload),
            None => None,
        }
    }
}

impl<T: Serializer + ForyDefault> RequestStream<T> {
    /// Receive and decode the next request, or `None` at end of stream.
    pub async fn recv(&mut self) -> Result<Option<T>> {
        match self.recv_bytes().await {
            Some(bytes) => Ok(Some(
                self.fory.deserialize::<T>(&bytes).map_err(decode_err)?,
            )),
            None => Ok(None),
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

impl<T: Serializer + ForyDefault> MessageStream<T> {
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
