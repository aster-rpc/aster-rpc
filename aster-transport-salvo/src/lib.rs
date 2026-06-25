//! Aster RPC over HTTP — Salvo transport.
//!
//! Bridges HTTP requests to the transport-agnostic Aster dispatcher
//! ([`aster::rpc::Dispatcher`]), so the same services served over Iroh are
//! reachable over HTTP. The dispatcher is shared, not duplicated: build it once
//! with [`Server::dispatcher`](aster::rpc::Server::dispatcher) and hand a clone
//! here.
//!
//! ## Scope
//!
//! **All four call patterns** under `POST /aster/{service}/{method}`, one
//! handler. Bodies are Aster frames (`[u32 len][u8 flags][payload]`) — the same
//! wire framing the Iroh transport uses, so any Aster client decodes them
//! identically. The request body is read in full and its frames fed to the
//! dispatcher (the Rust client is eager for client-stream / bidi inputs, so
//! buffering requests matches its behaviour); the **response** is streamed frame
//! by frame as the handler emits it, so server-stream / bidi don't wait for the
//! handler to finish.
//!
//! Not yet: TLS modes, the auth delegate, sessions, static files, stream
//! priority, true incremental request streaming. Auth is **not** wired — the
//! `peer_id` is the remote address, not an Aster identity, so Gate-3 sees empty
//! attributes (only no-capability methods reachable).
//!
//! Aster owns `/aster/*`; nest [`router`] into your own Salvo app and keep every
//! other path for yourself.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use salvo::http::{ResBody, StatusCode as HttpStatus};
use salvo::prelude::*;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use aster::rpc::codec::{encode_stream_header, SerializationMode, StreamHeader};
use aster::rpc::{CallParts, Dispatcher, OutgoingFrame, RequestFrame};
use aster_transport_core::framing::{decode_frame, FLAG_END_STREAM};

/// Content-type for Aster framed bodies (request and response).
pub const ASTER_FRAMES: &str = "application/aster-frames";

/// Build a Salvo [`Router`] serving Aster RPC under `/aster/{service}/{method}`.
/// Nest it into your own app router; everything outside `/aster/*` is yours.
///
/// ```ignore
/// let app = salvo::Router::new()
///     .push(aster_transport_salvo::router(dispatcher.clone()))
///     .push(salvo::Router::with_path("healthz").get(health));
/// ```
pub fn router(dispatcher: Dispatcher) -> Router {
    Router::with_path("aster/{service}/{method}").post(AsterHandler { dispatcher })
}

struct AsterHandler {
    dispatcher: Dispatcher,
}

#[async_trait]
impl Handler for AsterHandler {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let Some(service) = req.param::<String>("service") else {
            return fail(res, HttpStatus::BAD_REQUEST, "missing service");
        };
        let Some(method) = req.param::<String>("method") else {
            return fail(res, HttpStatus::BAD_REQUEST, "missing method");
        };

        // Request body = a sequence of Aster frames. Read in full (the Rust
        // client is eager for client-stream / bidi inputs), then parse frames.
        let body = match req.payload().await {
            Ok(b) => b.to_vec(),
            Err(_) => return fail(res, HttpStatus::BAD_REQUEST, "missing request body"),
        };
        let frames = match parse_frames(&body) {
            Ok(f) => f,
            Err(_) => return fail(res, HttpStatus::BAD_REQUEST, "malformed request frame"),
        };

        // HTTP request headers → Aster metadata (lowercased names), so a
        // server-side Authenticator sees `authorization` etc.
        let mut metadata_keys = Vec::new();
        let mut metadata_values = Vec::new();
        for (name, value) in req.headers().iter() {
            if let Ok(v) = value.to_str() {
                metadata_keys.push(name.as_str().to_string());
                metadata_values.push(v.to_string());
            }
        }

        // Construct the StreamHeader from the URL + headers. Session land later.
        let header = StreamHeader {
            service,
            method,
            version: 1,
            call_id: 0,
            deadline: 0,
            serialization_mode: SerializationMode::Xlang.as_i8(),
            metadata_keys,
            metadata_values,
            session_id: 0,
        };
        let header_payload = match encode_stream_header(&header) {
            Ok(b) => b,
            Err(_) => {
                return fail(
                    res,
                    HttpStatus::INTERNAL_SERVER_ERROR,
                    "header encode failed",
                )
            }
        };

        // Request channel: the first frame rides inline; the rest are forwarded,
        // then the channel is closed so `Call::recv_request` terminates.
        let (req_tx, req_rx) = mpsc::unbounded_channel::<RequestFrame>();
        let mut iter = frames.into_iter();
        let (request_payload, request_flags) = iter.next().unwrap_or((Vec::new(), FLAG_END_STREAM));
        for (payload, flags) in iter {
            let _ = req_tx.send(RequestFrame { payload, flags });
        }
        drop(req_tx);

        // Response channel → streamed HTTP body (frames flow as the handler emits
        // them; server-stream / bidi don't wait for completion).
        let (resp_tx, resp_rx) = mpsc::unbounded_channel::<OutgoingFrame>();

        // TODO(auth): once the auth delegate lands, peer_id is the authenticated
        // principal. For now it's the remote address — NOT an Aster identity.
        let peer_id = req.remote_addr().to_string();

        let parts = CallParts {
            peer_id,
            header_payload,
            request_payload,
            request_flags,
            response_sender: resp_tx,
            request_receiver: req_rx,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        // Run dispatch concurrently with the response stream. Detached: it owns
        // everything it needs and ends when the handler returns (dropping
        // resp_tx, which terminates the body stream).
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            dispatcher.dispatch_parts(parts).await;
        });

        res.status_code(HttpStatus::OK);
        let _ = res.add_header("content-type", ASTER_FRAMES, true);
        let stream = UnboundedReceiverStream::new(resp_rx)
            .map(|frame| Ok::<Bytes, std::io::Error>(frame_bytes(frame)));
        res.body(ResBody::stream(stream));
    }
}

/// Already-framed bytes from an outgoing frame (all variants carry framed bytes).
fn frame_bytes(f: OutgoingFrame) -> Bytes {
    match f {
        OutgoingFrame::Frame(b) | OutgoingFrame::Trailer(b) | OutgoingFrame::CompleteUnary(b) => {
            Bytes::from(b)
        }
    }
}

/// Split a body of concatenated length-prefixed Aster frames into
/// `(payload, flags)` pairs.
fn parse_frames(mut buf: &[u8]) -> Result<Vec<(Vec<u8>, u8)>, ()> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let (payload, flags, consumed) = decode_frame(buf).map_err(|_| ())?;
        if consumed == 0 {
            return Err(());
        }
        out.push((payload, flags));
        buf = &buf[consumed..];
    }
    Ok(out)
}

fn fail(res: &mut Response, code: HttpStatus, msg: &str) {
    res.status_code(code);
    res.render(msg.to_string());
}
