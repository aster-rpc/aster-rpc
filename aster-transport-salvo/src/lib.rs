//! Aster RPC over HTTP — Salvo transport.
//!
//! Bridges HTTP requests to the transport-agnostic Aster dispatcher
//! ([`aster::rpc::Dispatcher`]), so the same services served over Iroh are
//! reachable over HTTP. The dispatcher is shared, not duplicated: build it once
//! with [`Server::dispatcher`](aster::rpc::Server::dispatcher) and hand a clone
//! here.
//!
//! ## Scope (v0)
//!
//! **Unary only.** `POST /aster/{service}/{method}` with an Aster-framed body
//! (`[u32 len][u8 flags][payload]`) in, an Aster-framed body (response frame +
//! trailer) out — the same wire framing the Iroh transport uses, so clients
//! decode identically. Streaming (server/client/bidi), TLS modes, auth
//! delegate, sessions, static files, and stream priority land in later
//! increments. Auth is **not** wired yet: `peer_id` is the remote address, not
//! an Aster identity, so only no-capability methods are reachable.
//!
//! Aster owns `/aster/*`; nest [`router`] into your own Salvo app and keep every
//! other path for yourself.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use bytes::Bytes;
use salvo::http::{ResBody, StatusCode as HttpStatus};
use salvo::prelude::*;
use tokio::sync::mpsc;

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
    Router::with_path("aster/{service}/{method}").post(UnaryHandler { dispatcher })
}

struct UnaryHandler {
    dispatcher: Dispatcher,
}

#[async_trait]
impl Handler for UnaryHandler {
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

        // Request body = one length-prefixed Aster frame (unary).
        let body = match req.payload().await {
            Ok(b) => b.to_vec(),
            Err(_) => return fail(res, HttpStatus::BAD_REQUEST, "missing request body"),
        };
        let request_payload = match decode_frame(&body) {
            Ok((payload, _flags, _consumed)) => payload,
            Err(_) => return fail(res, HttpStatus::BAD_REQUEST, "malformed request frame"),
        };

        // Construct the StreamHeader from the URL. Metadata / session / auth
        // land in later increments.
        let header = StreamHeader {
            service,
            method,
            version: 1,
            call_id: 0,
            deadline: 0,
            serialization_mode: SerializationMode::Xlang.as_i8(),
            metadata_keys: Vec::new(),
            metadata_values: Vec::new(),
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

        // Response channel: the handler emits OutgoingFrames here.
        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<OutgoingFrame>();
        // Unary: no additional request frames — hand the dispatcher a closed
        // receiver (the lone request rides inline via request_payload).
        let (req_tx, req_rx) = mpsc::unbounded_channel::<RequestFrame>();
        drop(req_tx);

        // TODO(auth): once the auth delegate lands, peer_id is the authenticated
        // principal. For now it's the remote address — NOT an Aster identity —
        // so Gate-3 sees empty attributes (only no-capability methods reachable).
        let peer_id = req.remote_addr().to_string();

        let parts = CallParts {
            peer_id,
            header_payload,
            request_payload,
            request_flags: FLAG_END_STREAM,
            response_sender: resp_tx,
            request_receiver: req_rx,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        // Unary: the dispatcher runs the handler to completion (it sends its
        // frames on the unbounded channel and returns), then we drain the
        // buffered frames into the HTTP body — already-framed bytes, mirroring
        // the Iroh wire so any Aster client decodes them identically. (Streaming
        // will interleave send + drain instead of await-then-drain.)
        self.dispatcher.dispatch_parts(parts).await;

        let mut out = Vec::new();
        while let Ok(frame) = resp_rx.try_recv() {
            match frame {
                OutgoingFrame::Frame(b)
                | OutgoingFrame::Trailer(b)
                | OutgoingFrame::CompleteUnary(b) => out.extend_from_slice(&b),
            }
        }

        res.status_code(HttpStatus::OK);
        let _ = res.add_header("content-type", ASTER_FRAMES, true);
        res.body(ResBody::Once(Bytes::from(out)));
    }
}

fn fail(res: &mut Response, code: HttpStatus, msg: &str) {
    res.status_code(code);
    res.render(msg.to_string());
}
