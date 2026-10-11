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

use crate::listener::TaskScope;
use aster::rpc::codec::{decode_stream_header, StreamHeader};
use aster::rpc::{CallParts, Dispatcher, OutgoingFrame, PeerProvenance, RequestFrame};
use aster_transport_core::framing::{FLAG_END_STREAM, MAX_CALL_HEADER_SIZE, MAX_FRAME_SIZE};
use aster_transport_core::reactor::{
    response_channel, CancelSignal, FrameLimit, FRAME_CHANNEL_DEPTH,
};

/// Mount the Aster WebTransport endpoint at `/aster/wt`. Nest into your app
/// router; a browser opens a WebTransport session to this path and runs one
/// bidi stream per RPC call.
pub fn wt_router(dispatcher: Dispatcher) -> Router {
    wt_router_with_scope(dispatcher, TaskScope::new())
}

pub(crate) fn wt_router_with_scope(dispatcher: Dispatcher, scope: TaskScope) -> Router {
    Router::with_path("aster/wt").goal(WtHandler { dispatcher, scope })
}

struct WtHandler {
    dispatcher: Dispatcher,
    scope: TaskScope,
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
        // Transport hint only; the dispatcher authenticates each RPC from its
        // StreamHeader metadata before authorization or session creation.
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
                    self.scope.spawn(async move {
                        // Observe STOP_SENDING/connection loss even before the
                        // header arrives, and while a unary handler has no output.
                        let stopped = send.transport_acked();
                        let call = async move {
                            let header_payload =
                                match read_frame(&mut recv, MAX_CALL_HEADER_SIZE).await {
                                    Ok(Some((payload, _flags))) => payload,
                                    _ => return,
                                };
                            if let Ok(header) = decode_stream_header(&header_payload) {
                                if let Some(urgency) = urgency_from(&header) {
                                    let _ = send.set_priority(urgency_to_priority(urgency));
                                }
                            }
                            serve_call(dispatcher, peer, header_payload, send, recv).await;
                        };
                        tokio::select! {
                            _ = stopped => {},
                            () = call => {},
                        }
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
    let limit = match dispatcher.frame_limit_resolver()(&header_payload) {
        FrameLimit::Reject => return,
        FrameLimit::Default => MAX_FRAME_SIZE,
        FrameLimit::Limit(limit) => limit.min(MAX_FRAME_SIZE),
    };
    let (first_payload, first_flags) = match read_frame(&mut recv, limit).await {
        Ok(Some(f)) => f,
        _ => return,
    };
    let (req_tx, req_rx) = mpsc::channel::<RequestFrame>(FRAME_CHANNEL_DEPTH);
    let (resp_tx, mut resp_rx) = response_channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = Arc::new(CancelSignal::new());
    let _cancel_on_drop = CancelOnDrop {
        cancelled: cancelled.clone(),
        signal: signal.clone(),
    };
    let mut parts = CallParts::new(
        peer_id,
        PeerProvenance::Other,
        header_payload,
        first_payload,
        first_flags,
        resp_tx,
        req_rx,
        cancelled.clone(),
    );
    parts.cancel_notify = Some(signal.clone());

    // All three futures belong to this call; none can outlive its stream.
    // A clean request END_STREAM half-close still allows a response.
    let pump = async {
        if first_flags & FLAG_END_STREAM == 0 {
            loop {
                match read_frame(&mut recv, limit).await {
                    Ok(Some((payload, flags))) => {
                        if req_tx.send(RequestFrame { payload, flags }).await.is_err() {
                            break;
                        }
                        if flags & FLAG_END_STREAM != 0 {
                            break;
                        }
                    }
                    _ => {
                        signal.cancel(&cancelled);
                        break;
                    }
                }
            }
        }
        drop(req_tx);
        std::future::pending::<()>().await;
    };
    let responses = async {
        while let Some(item) = resp_rx.recv().await {
            let terminal = item.is_terminal();
            let (frame, _lease) = item.into_parts();
            if send.write_all(&raw_bytes(frame)).await.is_err() {
                signal.cancel(&cancelled);
                return;
            }
            if terminal {
                break;
            }
        }
        let _ = send.flush().await;
        let _ = send.shutdown().await;
    };
    let dispatch_and_responses = async {
        tokio::join!(dispatcher.dispatch_parts(parts), responses);
    };
    tokio::select! {
        () = dispatch_and_responses => {},
        () = pump => unreachable!("request pump remains pending until call completion"),
    }
}

struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    signal: Arc<CancelSignal>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.signal.cancel(&self.cancelled);
    }
}

/// Read one length-prefixed Aster frame. `Ok(None)` on clean EOF.
async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    limit: u32,
) -> std::io::Result<Option<(Vec<u8>, u8)>> {
    let mut len = [0u8; 4];
    // Only EOF before any prefix byte is clean; truncated prefixes are errors.
    if r.read(&mut len[..1]).await? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut len[1..]).await?;
    let body_len = u32::from_le_bytes(len) as usize;
    if body_len == 0 || body_len > limit.min(MAX_FRAME_SIZE) as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame length is zero or exceeds the configured limit",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_limits_are_checked_before_reading_body() {
        for (length, limit) in [
            (0, MAX_FRAME_SIZE),
            (MAX_FRAME_SIZE + 1, MAX_FRAME_SIZE),
            (MAX_CALL_HEADER_SIZE + 1, MAX_CALL_HEADER_SIZE),
            (33, 32),
        ] {
            let bytes = length.to_le_bytes();
            let err = read_frame(&mut &bytes[..], limit).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
        let bytes = [3, 0, 0, 0, FLAG_END_STREAM, 7, 8];
        assert_eq!(
            read_frame(&mut &bytes[..], 3).await.unwrap(),
            Some((vec![7, 8], FLAG_END_STREAM))
        );
    }

    #[tokio::test]
    async fn only_empty_input_is_clean_eof() {
        assert_eq!(
            read_frame(&mut &[][..], MAX_FRAME_SIZE).await.unwrap(),
            None
        );
        for bytes in [&[1][..], &[1, 0, 0][..], &[2, 0, 0, 0, 0][..]] {
            assert_eq!(
                read_frame(&mut &bytes[..], MAX_FRAME_SIZE)
                    .await
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        }
    }
}
