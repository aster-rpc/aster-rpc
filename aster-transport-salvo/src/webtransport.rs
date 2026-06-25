//! Aster RPC over **WebTransport** (HTTP/3).
//!
//! Each Aster call is one WebTransport **bidirectional stream** — the same
//! bi-stream-per-call shape as the Iroh transport. The client opens a bidi
//! stream and writes a header frame (the `StreamHeader`) followed by request
//! frames; the server decodes the header, dispatches through the shared
//! [`Dispatcher`] as ordinary Fory, and writes response frames + a status
//! trailer back on the same stream. All four call patterns ride it.
//!
//! Per-call **stream priority** (RFC 9218 urgency `0`–`7` in the
//! `aster-priority` metadata header; `0` = most urgent) is applied to the
//! server's send half via the fork's `SendStream::set_priority`. This is the
//! dynamic, per-call form; the static manifest form lands with the manifest
//! plumbing.
//!
//! Wire framing is identical to Iroh / `application/aster-frames`:
//! `[u32 LE (1+len)][u8 flags][payload]`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use salvo::prelude::*;
use salvo::proto::quic::BidiStream as _; // brings the `split` trait method into scope
use salvo::proto::webtransport::server::AcceptedBi;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use aster::rpc::codec::{decode_stream_header, StreamHeader};
use aster::rpc::{CallParts, Dispatcher, OutgoingFrame, RequestFrame};
use aster_transport_core::framing::FLAG_END_STREAM;

/// Mount the Aster WebTransport endpoint at `/aster/wt`. Nest into your app
/// router; a browser opens a WebTransport session to this path and runs one
/// bidi stream per RPC call.
pub fn wt_router(dispatcher: Dispatcher) -> Router {
    Router::with_path("aster/wt").goal(WtHandler { dispatcher })
}

struct WtHandler {
    dispatcher: Dispatcher,
}

#[async_trait]
impl Handler for WtHandler {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        // TODO(auth): resolve a principal from the session; remote addr for now.
        let peer_id = req.remote_addr().to_string();
        let session = match req.web_transport_mut().await {
            Ok(s) => s,
            Err(_) => {
                res.status_code(salvo::http::StatusCode::BAD_REQUEST);
                return;
            }
        };

        loop {
            match session.accept_bi().await {
                Ok(Some(AcceptedBi::BidiStream(_sid, stream))) => {
                    let (send, mut recv) = stream.split();
                    let dispatcher = self.dispatcher.clone();
                    let peer = peer_id.clone();
                    tokio::spawn(async move {
                        // First frame = StreamHeader; set per-call priority on the
                        // concrete send half before handing off to the generic IO.
                        let header_payload = match read_frame(&mut recv).await {
                            Ok(Some((payload, _flags))) => payload,
                            _ => return,
                        };
                        if let Ok(header) = decode_stream_header(&header_payload) {
                            if let Some(urgency) = urgency_from(&header) {
                                let _ = send.set_priority(urgency_to_priority(urgency));
                            }
                        }
                        serve_call(dispatcher, peer, header_payload, send, recv).await;
                    });
                }
                // A nested HTTP request over the WT session — not used by Aster.
                Ok(Some(AcceptedBi::Request(..))) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
}

/// Pump request frames in, dispatch, stream response frames out. Generic over
/// the WT stream halves (priority is already set by the caller).
async fn serve_call<Snd, Rcv>(
    dispatcher: Dispatcher,
    peer_id: String,
    header_payload: Vec<u8>,
    mut send: Snd,
    mut recv: Rcv,
) where
    Snd: AsyncWrite + Unpin + Send + 'static,
    Rcv: AsyncRead + Unpin + Send + 'static,
{
    // First request frame rides inline; the rest stream via a channel.
    let (first_payload, first_flags) = match read_frame(&mut recv).await {
        Ok(Some(f)) => f,
        _ => (Vec::new(), FLAG_END_STREAM),
    };
    let (req_tx, req_rx) = mpsc::unbounded_channel::<RequestFrame>();
    if first_flags & FLAG_END_STREAM == 0 {
        tokio::spawn(async move {
            while let Ok(Some((payload, flags))) = read_frame(&mut recv).await {
                if req_tx.send(RequestFrame { payload, flags }).is_err() {
                    break;
                }
                if flags & FLAG_END_STREAM != 0 {
                    break;
                }
            }
        });
    } else {
        drop(req_tx);
    }

    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<OutgoingFrame>();
    let parts = CallParts {
        peer_id,
        header_payload,
        request_payload: first_payload,
        request_flags: first_flags,
        response_sender: resp_tx,
        request_receiver: req_rx,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    tokio::spawn(async move { dispatcher.dispatch_parts(parts).await });

    // Stream response frames out as the handler emits them.
    while let Some(frame) = resp_rx.recv().await {
        if send.write_all(&raw_bytes(frame)).await.is_err() {
            return;
        }
    }
    let _ = send.flush().await;
    let _ = send.shutdown().await;
}

/// Read one length-prefixed Aster frame. `Ok(None)` on clean EOF.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<(Vec<u8>, u8)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let body_len = u32::from_le_bytes(len) as usize;
    if body_len == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "zero-length frame",
        ));
    }
    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body).await?;
    Ok(Some((body[1..].to_vec(), body[0])))
}

fn raw_bytes(f: OutgoingFrame) -> Vec<u8> {
    match f {
        OutgoingFrame::Frame(b) | OutgoingFrame::Trailer(b) | OutgoingFrame::CompleteUnary(b) => b,
    }
}

/// `aster-priority` urgency (0–7) from the header metadata.
fn urgency_from(h: &StreamHeader) -> Option<u8> {
    h.metadata_keys
        .iter()
        .position(|k| k == "aster-priority")
        .and_then(|i| h.metadata_values.get(i))
        .and_then(|v| v.parse::<u8>().ok())
}

/// RFC 9218 urgency (0 = most urgent) → quinn priority (higher = more urgent).
fn urgency_to_priority(urgency: u8) -> i32 {
    7 - urgency.min(7) as i32
}
