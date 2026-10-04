//! L7 HTTP-object relay — the same-process request/response datapath
//! (design doc §2, build-order step A4; streaming bodies = cut 1, §7).
//!
//! One Aster stream carries one request/response exchange (no pool, no
//! HTTP/1.1 re-encode):
//!
//! ```text
//! request  (client -> server):  [u32 head-len][request head][body..]  then finish()
//! response (server -> client):  [u32 head-len][response head][body..] then finish()
//! ```
//!
//! The head is the thin H3-object from [`crate::codec`]; the body **streams** —
//! chunks flow off the wire as a [`RelayBody`] until the writer finishes its
//! send side (clean EOF), so SSE / chunked / long-lived HTTP/2 responses pass
//! through without buffering. The handler is a [`tower::Service`] (adapt one
//! with [`tower_handler`]); [`serve_request`] drives it over split read/write
//! halves so it is unit-testable over `tokio::io::duplex`.
//!
//! gRPC trailers are **V2** (design doc §9) — `[body…until finish]` carries no
//! trailers frame.
//!
//! **Failure is a reset, never a finish.** Because the body is delimited only
//! by the send side's finish, a producer that stops partway (its body yields an
//! error, a [`BodySender::abort`](crate::body::BodySender::abort), a size cap)
//! must not let the send half end normally: QUIC *finishes* a send stream that
//! is merely dropped, and the peer would read a truncated body as complete.
//! Both directions therefore reset the send side with
//! [`RESET_CODE_BODY_ABORTED`] on any error, which the reading side's
//! [`RelayBody`] surfaces as an `Err` frame instead of a clean end.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Result};
use bytes::{Buf, Bytes};
use http_body::Body;
use http_body_util::{BodyExt, Full};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use aster_transport_core::stream_io::{AsterStreamIo, BoxFut};
use aster_transport_core::tunnel::{
    splice_with_failover, LocalTunnelAcceptor, PeerContext, TunnelTarget,
};
use aster_transport_core::{CoreConnection, CoreRecvStream, CoreSendStream};

use crate::body::{RelayBody, RelayError, ResponseBody};
use crate::codec;

/// Streaming HTTP dispatch handler — the backend target. Takes a request whose
/// body streams off the wire ([`RelayBody`]) and returns a streaming response
/// ([`ResponseBody`]). Build one from a [`tower::Service`] with
/// [`tower_handler`], or write the closure directly.
pub type HttpHandler = Arc<
    dyn Fn(http::Request<RelayBody>) -> BoxFut<Result<http::Response<ResponseBody>>> + Send + Sync,
>;

/// Per-`(connection, service)` admission policy: given the peer's identity +
/// attached metadata ([`PeerContext`]), `Ok(())` admits. Async because real
/// policy is often a network call.
pub type AuthorizeFn = Arc<dyn Fn(PeerContext) -> BoxFut<Result<()>> + Send + Sync>;

/// The transport-authenticated identity of the peer that opened a request stream — the verified
/// remote node id (`conn.remote_id()`). The relay inserts it into each request's [`http::Extensions`]
/// AFTER decoding the head and BEFORE dispatch, so a handler can attribute the request to its
/// authenticated origin (e.g. a per-peer quota) with a provenance the peer cannot forge.
///
/// The inner value is PRIVATE and there is no public constructor — HTTP input can neither populate
/// nor override this extension; only the relay's [`accept`](ServiceAcceptor::accept) can mint it. A
/// handler reads it via `req.extensions().get::<AuthenticatedPeer>()` and must fail closed if absent.
#[derive(Clone, Debug)]
pub struct AuthenticatedPeer {
    peer_id: String,
}

impl AuthenticatedPeer {
    /// Crate-private: constructed ONLY from a verified transport connection (never from a request).
    pub(crate) fn new(peer_id: String) -> Self {
        Self { peer_id }
    }

    /// The verified remote node id.
    pub fn as_str(&self) -> &str {
        &self.peer_id
    }
}

/// Bound on the request/response head (the body is unbounded — it streams).
const MAX_HEAD: usize = 1 << 20; // 1 MiB

/// QUIC application error code an http-relay stream is reset with when its
/// exchange fails after the stream is open — most importantly when a body
/// stops partway (the producer errored or aborted, or a size cap tripped), so
/// the peer sees a failed body rather than a clean, truncated one. ("AX" 00 01;
/// distinct from the core RPC `RESET_CODE_*` values, which are "AS" 00 xx.)
pub const RESET_CODE_BODY_ABORTED: u32 = 0x41_58_00_01;

/// Upper bound on waiting for a send half's lock before resetting it. The
/// lock is free by the time the reset runs (every writer has returned); the
/// bound only keeps a bug there from parking the stream task forever.
const RESET_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Reset `send` with [`RESET_CODE_BODY_ABORTED`] after a failed exchange, so
/// the peer reads an error instead of the implicit finish a dropped QUIC send
/// half gets. A reset after the peer stopped or the connection closed fails
/// harmlessly; that is ignored.
async fn reset_failed(send: &CoreSendStream) {
    match tokio::time::timeout(RESET_LOCK_WAIT, send.reset(RESET_CODE_BODY_ABORTED)).await {
        Ok(_) => {}
        Err(_) => tracing::warn!("http-relay: send half still locked; stream not reset"),
    }
}

/// Resets an upload's send half if the request future is dropped before the
/// upload completed (the caller timed out or cancelled), which would
/// otherwise finish it and hand the server a truncated request body as
/// complete. Disarmed once the request is fully sent or reset explicitly.
struct ResetOnDrop(Option<CoreSendStream>);

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        let Some(send) = self.0.take() else { return };
        // The IO halves drop with the same future, releasing the lock; the
        // task holds the last clone, so nothing finishes the stream first.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { reset_failed(&send).await });
        }
    }
}

/// Flatten a relay/body error into `anyhow` (its `From` doesn't cover the boxed
/// `dyn Error` directly, so go through `Display`).
fn relay_anyhow(e: impl Into<RelayError>) -> anyhow::Error {
    anyhow::Error::msg(e.into().to_string())
}

async fn read_u32_le<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).await?;
    Ok(u32::from_le_bytes(b))
}

async fn read_length_prefixed<R: AsyncRead + Unpin + ?Sized>(
    r: &mut R,
    max: usize,
    what: &str,
) -> Result<Vec<u8>> {
    let len = read_u32_le(r).await? as usize;
    if len > max {
        bail!("{what} too large: {len} > {max}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Pump a [`Body`] onto an `AsyncWrite`, chunk by chunk (no buffering). Data
/// frames are written through; trailers are dropped (gRPC trailers are V2).
async fn pump_body<B, W>(mut body: B, write: &mut W) -> Result<()>
where
    B: Body + Unpin,
    B::Error: Into<RelayError>,
    W: AsyncWrite + Unpin + ?Sized,
{
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(relay_anyhow)?;
        if let Ok(mut data) = frame.into_data() {
            let bytes = data.copy_to_bytes(data.remaining());
            if !bytes.is_empty() {
                write.write_all(&bytes).await?;
            }
        }
    }
    Ok(())
}

/// Adapt a [`tower::Service`] into an [`HttpHandler`]. The service sees the
/// streaming request body and returns any [`Body`]; its response body is boxed
/// into [`ResponseBody`]. This is the ergonomic entry for Salvo / `tower`
/// stacks (Salvo is `tower`-compatible).
pub fn tower_handler<S, B>(svc: S) -> HttpHandler
where
    S: tower::Service<http::Request<RelayBody>, Response = http::Response<B>>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
    S::Error: Into<RelayError>,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<RelayError>,
{
    Arc::new(move |req| {
        let svc = svc.clone();
        Box::pin(async move {
            let resp = tower::ServiceExt::oneshot(svc, req)
                .await
                .map_err(relay_anyhow)?;
            let (parts, body) = resp.into_parts();
            let body = body.map_err(Into::into).boxed_unsync();
            Ok(http::Response::from_parts(parts, body))
        })
    })
}

/// Serve exactly one request/response exchange over split read/write halves:
/// decode the request head, hand the streaming body to `handler`, encode the
/// response head, pump the streaming response body back, then finish the send
/// side. Generic over the byte transport so it is testable without a live
/// Aster connection.
///
/// The send side is finished (`shutdown`) **only** on success. On `Err` — in
/// particular a response body that fails partway — nothing more is written
/// and the caller must reset the stream rather than finish or drop it (a
/// dropped QUIC send half finishes), or the peer reads the truncated body as
/// complete. [`ServiceAcceptor`] resets with [`RESET_CODE_BODY_ABORTED`].
pub async fn serve_request<R, W>(
    read: R,
    write: W,
    handler: &HttpHandler,
    peer: AuthenticatedPeer,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    serve_request_until(read, write, handler, peer, std::future::pending()).await
}

/// As [`serve_request`], but gives up waiting for the handler's response head
/// when `peer_gone` resolves: the handler future is **dropped** (so a foreign
/// handler's upcall is cancelled as abandoned) and the exchange fails. Once
/// the head is out, a vanished peer surfaces as a write error on the body
/// pump, which drops the response body the same way. [`ServiceAcceptor`]
/// passes "the peer stopped reading this stream, or the connection closed".
pub async fn serve_request_until<R, W, G>(
    mut read: R,
    mut write: W,
    handler: &HttpHandler,
    peer: AuthenticatedPeer,
    peer_gone: G,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
    G: std::future::Future<Output = ()>,
{
    let head = read_length_prefixed(&mut read, MAX_HEAD, "request head").await?;
    let parts = codec::decode_request_head(&head)?;
    let mut request = http::Request::from_parts(parts, RelayBody::new(Box::pin(read)));
    // Attribute the request to its transport-authenticated origin. Inserted AFTER decoding the head,
    // so it overwrites (never merges with) anything a peer might have tried to smuggle in the head.
    request.extensions_mut().insert(peer);

    // `peer_gone` is consumed by the select, so it is dropped before any
    // response byte is written (the stream watcher may hold the send half).
    let response = tokio::select! {
        r = handler(request) => r?,
        () = peer_gone => bail!("peer went away before the response head"),
    };
    let (parts, body) = response.into_parts();
    let head = codec::encode_response_head(&parts)?;

    write.write_all(&(head.len() as u32).to_le_bytes()).await?;
    write.write_all(&head).await?;
    pump_body(body, &mut write).await?;
    write.shutdown().await?; // finish() — signals response EOF to the peer
    Ok(())
}

/// Client side (`dial_http`): open an http-relay stream to `service_id` on
/// `conn`, stream `request` (head + body), and return the response with a
/// **streaming** body ([`RelayBody`]). One Aster stream per request — no
/// pooling. The edge uses this to pump the origin's response straight back to
/// the browser.
///
/// If `request`'s body fails partway, or this future is dropped before the
/// upload is sent, the request stream is reset, so the server's handler sees a
/// failed request body rather than a truncated one. Likewise a response the
/// server could not complete arrives as an `Err` from the returned body (or,
/// if it failed before its head got out, from this call).
pub async fn relay_request_streaming<B>(
    conn: &CoreConnection,
    service_id: &str,
    request: http::Request<B>,
) -> Result<http::Response<RelayBody>>
where
    B: Body + Unpin + Send + 'static,
    B::Error: Into<RelayError>,
{
    let (send, recv) = conn.open_http_relay(service_id, &[]).await?;
    let (parts, body) = request.into_parts();
    let head = codec::encode_request_head(&parts)?;

    let mut armed = ResetOnDrop(Some(send.clone()));
    let io = AsterStreamIo::new(recv, send);
    let (mut r, mut w) = tokio::io::split(io);
    let sent = async {
        w.write_all(&(head.len() as u32).to_le_bytes()).await?;
        w.write_all(&head).await?;
        pump_body(body, &mut w).await?;
        w.shutdown().await?; // half-close: signals request EOF to the server
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(e) = sent {
        // An upload that failed partway must reach the handler as a failed
        // request body, not a truncated one: reset rather than let the
        // dropped send half finish. Every write has returned, so `w` holds
        // no lock on the send half.
        drop((r, w));
        if let Some(send) = armed.0.take() {
            reset_failed(&send).await;
        }
        return Err(e);
    }
    armed.0 = None; // fully sent and finished

    let head = read_length_prefixed(&mut r, MAX_HEAD, "response head").await?;
    let parts = codec::decode_response_head(&head)?;
    Ok(http::Response::from_parts(
        parts,
        RelayBody::new(Box::pin(r)),
    ))
}

/// Buffered convenience over [`relay_request_streaming`]: send a `Vec<u8>` body
/// and collect the full response. For callers that don't need streaming.
pub async fn relay_request(
    conn: &CoreConnection,
    service_id: &str,
    request: http::Request<Vec<u8>>,
) -> Result<http::Response<Vec<u8>>> {
    let (parts, body) = request.into_parts();
    let request = http::Request::from_parts(parts, Full::new(Bytes::from(body)));
    let resp = relay_request_streaming(conn, service_id, request).await?;
    let (parts, body) = resp.into_parts();
    let body = body
        .collect()
        .await
        .map_err(relay_anyhow)?
        .to_bytes()
        .to_vec();
    Ok(http::Response::from_parts(parts, body))
}

/// A registered L7 service: the user's admission policy + streaming handler.
/// Implements core's [`LocalTunnelAcceptor`] so each admitted request-stream is
/// driven through [`serve_request`] over the split halves of an
/// [`AsterStreamIo`].
pub struct ServiceAcceptor {
    authorize: AuthorizeFn,
    handler: HttpHandler,
}

impl ServiceAcceptor {
    pub fn new(authorize: AuthorizeFn, handler: HttpHandler) -> Self {
        Self { authorize, handler }
    }
}

impl LocalTunnelAcceptor for ServiceAcceptor {
    fn authorize(&self, ctx: PeerContext) -> BoxFut<Result<()>> {
        (self.authorize)(ctx)
    }

    fn accept(
        &self,
        conn: CoreConnection,
        send: CoreSendStream,
        recv: CoreRecvStream,
    ) -> BoxFut<()> {
        let handler = self.handler.clone();
        // Kept outside the IO adapter so a failed exchange can reset the
        // stream after `serve_request_until` has released its halves.
        let reset_send = send.clone();
        // The transport-authenticated peer identity — derived here from the verified connection, the
        // only place it can come from, and handed to the handler as a request extension.
        let peer = AuthenticatedPeer::new(conn.remote_id());
        // Watches for the consumer abandoning the request while the handler
        // works on it: STOP_SENDING on our send half (the consumer dropped its
        // receive side) or the connection closing. `stopped()` holds the send
        // half's lock, which is safe only because nothing writes before the
        // response head and `serve_request_until` drops this before writing.
        let watch_send = send.clone();
        let peer_gone = async move {
            tokio::select! {
                _ = watch_send.stopped() => {}
                _ = conn.closed() => {}
            }
        };
        Box::pin(async move {
            let io = AsterStreamIo::new(recv, send);
            let (r, w) = tokio::io::split(io);
            if let Err(e) = serve_request_until(r, w, &handler, peer, peer_gone).await {
                tracing::debug!("http-relay serve error: {e}");
                // Reset, never finish: a response that failed mid-body must
                // reach the consumer as a failed response, not a truncated
                // one (the halves are dropped by now, so the lock is free).
                reset_failed(&reset_send).await;
            }
        })
    }
}

/// Expose a streaming HTTP `handler` under `service_id` on `conn`, gated by
/// `authorize`. Connection-scoped (the node-wide `expose_local_http` facade
/// that shares one registry across a node's connections is a later step).
pub fn expose_http_on_connection(
    conn: &CoreConnection,
    service_id: &str,
    authorize: AuthorizeFn,
    handler: HttpHandler,
) {
    conn.register_local_target(
        service_id,
        Arc::new(ServiceAcceptor::new(authorize, handler)),
    );
}

/// A registered L4 service: the user's admission policy + a loopback TCP
/// target. Implements core's [`LocalTunnelAcceptor`]; each admitted stream is
/// raw-spliced to `addr` (reusing core's `splice_with_failover`). The consumer
/// speaks its own protocol — HTTP/1.1, h2c, WebSocket, VNC, … — raw over the
/// stream; this acceptor is protocol-blind.
pub struct SocketAcceptor {
    authorize: AuthorizeFn,
    addr: SocketAddr,
}

impl SocketAcceptor {
    pub fn new(authorize: AuthorizeFn, addr: SocketAddr) -> Self {
        Self { authorize, addr }
    }
}

impl LocalTunnelAcceptor for SocketAcceptor {
    fn authorize(&self, ctx: PeerContext) -> BoxFut<Result<()>> {
        (self.authorize)(ctx)
    }

    fn accept(
        &self,
        _conn: CoreConnection,
        send: CoreSendStream,
        recv: CoreRecvStream,
    ) -> BoxFut<()> {
        let addr = self.addr;
        Box::pin(async move {
            if let Err(e) = splice_with_failover(send, recv, vec![TunnelTarget::Tcp { addr }]).await
            {
                tracing::debug!("socket-relay splice error: {e}");
            }
        })
    }
}

/// What a service maps to behind the door (design doc §2 modes).
pub enum LocalHttpTarget {
    /// Separate process: raw L4 splice to a loopback TCP server. Protocol-blind
    /// (HTTP/1.1, h2c, WebSocket, VNC, SSH, …). The consumer dials with
    /// [`CoreConnection::open_http_relay`] and writes its protocol raw.
    Socket(SocketAddr),
    /// Same process: buffered H3-object relay into `handler`. The consumer dials
    /// with [`relay_request`].
    Service(HttpHandler),
}

/// Unified ergonomic entry: expose `service_id` on `conn` as either a
/// separate-process socket (L4) or an in-process handler (L7), both gated by
/// the same `authorize` policy (design doc §4). Connection-scoped for now.
pub fn expose_local_http(
    conn: &CoreConnection,
    service_id: &str,
    authorize: AuthorizeFn,
    target: LocalHttpTarget,
) {
    let acceptor: Arc<dyn LocalTunnelAcceptor> = match target {
        LocalHttpTarget::Socket(addr) => Arc::new(SocketAcceptor::new(authorize, addr)),
        LocalHttpTarget::Service(handler) => Arc::new(ServiceAcceptor::new(authorize, handler)),
    };
    conn.register_local_target(service_id, acceptor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Streaming echo: drains the request body (so it exercises the streaming
    /// `RelayBody`) and reflects the path + byte count back as a `Full` body.
    fn echo_handler() -> HttpHandler {
        Arc::new(|req: http::Request<RelayBody>| {
            Box::pin(async move {
                let path = req.uri().path().to_owned();
                let body = req
                    .into_body()
                    .collect()
                    .await
                    .map_err(relay_anyhow)?
                    .to_bytes();
                let resp = http::Response::builder()
                    .status(200)
                    .header("x-echo-path", path)
                    .body(
                        Full::new(Bytes::from(format!("got {} bytes", body.len())))
                            .map_err(|e: std::convert::Infallible| -> RelayError { match e {} })
                            .boxed_unsync(),
                    )
                    .unwrap();
                Ok(resp)
            }) as BoxFut<Result<http::Response<ResponseBody>>>
        })
    }

    /// Write a request head + body onto `w`, then half-close so the server's
    /// body stream EOFs.
    async fn write_request<W: AsyncWrite + Unpin>(
        w: &mut W,
        parts: &http::request::Parts,
        body: &[u8],
    ) {
        let head = codec::encode_request_head(parts).unwrap();
        w.write_all(&(head.len() as u32).to_le_bytes())
            .await
            .unwrap();
        w.write_all(&head).await.unwrap();
        w.write_all(body).await.unwrap();
        w.shutdown().await.unwrap();
    }

    /// Read a response head + collect its streaming body.
    async fn read_response<R: AsyncRead + Unpin + Send + 'static>(
        mut r: R,
    ) -> http::Response<Vec<u8>> {
        let head = read_length_prefixed(&mut r, MAX_HEAD, "response head")
            .await
            .unwrap();
        let parts = codec::decode_response_head(&head).unwrap();
        let body = RelayBody::new(Box::pin(r))
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        http::Response::from_parts(parts, body)
    }

    #[tokio::test]
    async fn serve_request_round_trips_over_duplex() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let handler = echo_handler();
        let srv = tokio::spawn(async move {
            serve_request(sr, sw, &handler, AuthenticatedPeer::new("test-peer".into())).await
        });

        let (cr, mut cw) = tokio::io::split(client);
        let parts = http::Request::builder()
            .method("POST")
            .uri("/hello?x=1")
            .header("host", "backend.local")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        write_request(&mut cw, &parts, b"abcde").await;

        let resp = read_response(cr).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("x-echo-path").unwrap(), "/hello");
        assert_eq!(resp.body(), b"got 5 bytes");
        srv.await.unwrap().unwrap();
    }

    /// Reflects the transport-attributed peer back to the caller: `x-authed-peer` carries the
    /// value read from `req.extensions().get::<AuthenticatedPeer>()` (or `<none>` if the handler
    /// received no extension — a fail-closed handler would reject that), and `x-hdr-peer` echoes
    /// whatever an `x-authenticated-peer` HTTP header claimed, so a test can prove the header does
    /// not influence the extension.
    fn peer_reflect_handler() -> HttpHandler {
        Arc::new(|req: http::Request<RelayBody>| {
            Box::pin(async move {
                let authed = req
                    .extensions()
                    .get::<AuthenticatedPeer>()
                    .map(|p| p.as_str().to_owned())
                    .unwrap_or_else(|| "<none>".to_owned());
                let hdr = req
                    .headers()
                    .get("x-authenticated-peer")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("<none>")
                    .to_owned();
                let resp = http::Response::builder()
                    .status(200)
                    .header("x-authed-peer", authed)
                    .header("x-hdr-peer", hdr)
                    .body(
                        Full::new(Bytes::new())
                            .map_err(|e: std::convert::Infallible| -> RelayError { match e {} })
                            .boxed_unsync(),
                    )
                    .unwrap();
                Ok(resp)
            }) as BoxFut<Result<http::Response<ResponseBody>>>
        })
    }

    #[tokio::test]
    async fn handler_receives_the_exact_authenticated_peer_extension() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let handler = peer_reflect_handler();
        let srv = tokio::spawn(async move {
            serve_request(sr, sw, &handler, AuthenticatedPeer::new("test-peer".into())).await
        });

        let (cr, mut cw) = tokio::io::split(client);
        let parts = http::Request::builder()
            .uri("/whoami")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        write_request(&mut cw, &parts, b"").await;

        let resp = read_response(cr).await;
        assert_eq!(resp.status(), 200);
        // The handler saw exactly the peer that serve_request was given.
        assert_eq!(resp.headers().get("x-authed-peer").unwrap(), "test-peer");
        srv.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_forged_identity_header_does_not_alter_attribution() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let handler = peer_reflect_handler();
        // The transport attributes this connection to "real-peer".
        let srv = tokio::spawn(async move {
            serve_request(sr, sw, &handler, AuthenticatedPeer::new("real-peer".into())).await
        });

        let (cr, mut cw) = tokio::io::split(client);
        // The caller tries to spoof a different identity via an HTTP header.
        let parts = http::Request::builder()
            .uri("/whoami")
            .header("x-authenticated-peer", "attacker-peer")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        write_request(&mut cw, &parts, b"").await;

        let resp = read_response(cr).await;
        // The extension is still the transport-derived identity, untouched by the header...
        assert_eq!(resp.headers().get("x-authed-peer").unwrap(), "real-peer");
        // ...even though the forged header did ride along in the request untouched.
        assert_eq!(resp.headers().get("x-hdr-peer").unwrap(), "attacker-peer");
        srv.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn peer_gone_drops_the_pending_handler() {
        // A handler that never answers; it flags when its future is dropped.
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = dropped.clone();
        let handler: HttpHandler = Arc::new(move |_req| {
            let flag = Flag(d.clone());
            Box::pin(async move {
                let _flag = flag;
                std::future::pending::<Result<http::Response<ResponseBody>>>().await
            }) as BoxFut<Result<http::Response<ResponseBody>>>
        });

        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let (gone_tx, gone_rx) = tokio::sync::oneshot::channel::<()>();
        let srv = tokio::spawn(async move {
            let gone = async move {
                let _ = gone_rx.await;
            };
            serve_request_until(sr, sw, &handler, AuthenticatedPeer::new("p".into()), gone).await
        });
        let (_cr, mut cw) = tokio::io::split(client);
        let parts = http::Request::builder()
            .uri("/slow")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        write_request(&mut cw, &parts, b"").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));

        gone_tx.send(()).unwrap();
        let res = srv.await.unwrap();
        assert!(res.is_err(), "the exchange fails when the peer is gone");
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn empty_body_request() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let handler = echo_handler();
        let srv = tokio::spawn(async move {
            serve_request(sr, sw, &handler, AuthenticatedPeer::new("test-peer".into())).await
        });

        let (cr, mut cw) = tokio::io::split(client);
        let parts = http::Request::builder()
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        write_request(&mut cw, &parts, b"").await;

        let resp = read_response(cr).await;
        assert_eq!(resp.body(), b"got 0 bytes");
        srv.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn large_body_streams_without_buffering_cap() {
        // 256 KiB spans many 16 KiB body chunks in both directions.
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let handler = echo_handler();
        let srv = tokio::spawn(async move {
            serve_request(sr, sw, &handler, AuthenticatedPeer::new("test-peer".into())).await
        });

        let (cr, mut cw) = tokio::io::split(client);
        let parts = http::Request::builder()
            .uri("/big")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let big = vec![0xabu8; 256 * 1024];
        write_request(&mut cw, &parts, &big).await;

        let resp = read_response(cr).await;
        assert_eq!(resp.body(), format!("got {} bytes", big.len()).as_bytes());
        srv.await.unwrap().unwrap();
    }
}
