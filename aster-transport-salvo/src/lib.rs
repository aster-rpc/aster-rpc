//! Aster RPC over HTTP — Salvo transport.
//!
//! Bridges HTTP requests to the transport-agnostic Aster dispatcher
//! ([`aster::rpc::Dispatcher`]), so the same services served over Iroh are
//! reachable over HTTP. Build the dispatcher once with
//! [`Server::dispatcher`](aster::rpc::Server::dispatcher) and hand a clone here.
//!
//! ## Two tiers (see the design doc's "Content negotiation")
//!
//! - **Canonical** `application/aster-frames` (default): real Aster RPC — the
//!   Aster frame envelope (`[u32 len][u8 flags][payload]`) + status trailer
//!   frame, Fory payloads, same dispatcher semantics as Iroh. All four call
//!   patterns. This is the only representation with transport parity.
//! - **Browser projections** (`application/json`, `application/x-ndjson`):
//!   transcoded into the canonical Fory path via a [`ProjectionRegistry`]. The
//!   dispatcher only ever sees Fory; `serialization_mode` is untouched. Unary
//!   and client-stream return JSON; server-stream returns NDJSON. Bidi is not
//!   projected (use WebTransport) → `406`.
//!
//! Negotiation follows HTTP: `Content-Type` (request) → unsupported = `415`;
//! `Accept` (response) → unsatisfiable = `406`. Responses set
//! `Vary: Accept, Accept-Encoding`.
//!
//! Supported: HTTPS (H1/H2/H3) + WebTransport, TLS (PEM/ACME/self-signed +
//! WebTransport `serverCertificateHashes`), per-call stream priority (WT),
//! session-scoped services (`aster-session-id`), filesystem static files, and
//! pluggable auth via [`Server::authenticator`](aster::rpc::Server::authenticator)
//! (the transport copies HTTP headers into Aster metadata). Not yet: true
//! incremental request streaming (request bodies are buffered), HTTP
//! compression rules.
//!
//! Aster owns `/aster/*`; nest [`router`] into your own Salvo app.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use salvo::http::{ResBody, StatusCode as HttpStatus};
use salvo::prelude::*;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use aster::rpc::codec::{
    decode_rpc_status, encode_stream_header, RpcStatus, SerializationMode, StreamHeader,
};
use aster::rpc::{
    CallParts, Dispatcher, MethodPattern, OutgoingFrame, Projection, ProjectionRegistry,
    RequestFrame,
};
use aster_transport_core::framing::{decode_frame, FLAG_END_STREAM, FLAG_TRAILER};

pub mod tls;
pub mod webtransport;
pub use tls::{
    generate_self_signed, generate_webtransport_cert, rustls_config, serve_https, serve_https_with,
    GeneratedCert, TlsMaterial, TransportConfig,
};
pub use webtransport::wt_router;

/// HTTP transport config for `AsterServer::builder().with_http(...)`. Serves the
/// node's services over HTTPS (H1/H2/H3) — and WebTransport when enabled — on
/// the shared dispatcher. Implements [`aster::rpc::HttpTransport`].
pub struct HttpConfig {
    /// Bind address, e.g. `"[::]:443"`.
    pub addr: String,
    /// TLS material (PEM / ACME / self-signed).
    pub tls: TlsMaterial,
    /// Browser JSON/NDJSON projections (empty = canonical Fory only).
    pub projections: ProjectionRegistry,
    /// Also mount the WebTransport endpoint at `/aster/wt`.
    pub webtransport: bool,
    /// Static-file mounts: `(path_prefix, directory)`. Filesystem only.
    pub static_mounts: Vec<(String, std::path::PathBuf)>,
}

impl HttpConfig {
    /// Minimal config: address + TLS, canonical Fory only, no WebTransport.
    pub fn new(addr: impl Into<String>, tls: TlsMaterial) -> Self {
        Self {
            addr: addr.into(),
            tls,
            projections: ProjectionRegistry::new(),
            webtransport: false,
            static_mounts: Vec::new(),
        }
    }

    /// Enable browser JSON/NDJSON projections.
    pub fn projections(mut self, projections: ProjectionRegistry) -> Self {
        self.projections = projections;
        self
    }

    /// Mount the WebTransport endpoint (`/aster/wt`).
    pub fn webtransport(mut self, on: bool) -> Self {
        self.webtransport = on;
        self
    }

    /// Serve files from `dir` under `path_prefix` (filesystem static serving).
    pub fn static_mount(
        mut self,
        path_prefix: impl Into<String>,
        dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.static_mounts.push((path_prefix.into(), dir.into()));
        self
    }
}

impl aster::rpc::HttpTransport for HttpConfig {
    fn serve(self: Box<Self>, dispatcher: aster::rpc::Dispatcher) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut app =
                salvo::Router::new().push(router_with(dispatcher.clone(), self.projections));
            if self.webtransport {
                app = app.push(wt_router(dispatcher));
            }
            // Static mounts last so /aster/* and specific routes win over a
            // catch-all directory mount.
            for (prefix, dir) in self.static_mounts {
                app = app.push(static_router(&prefix, dir));
            }
            let service = salvo::Service::new(app);
            if let Err(e) = serve_https(&self.addr, self.tls, service).await {
                eprintln!("aster-transport-salvo: HTTP serve error: {e}");
            }
        })
    }
}

/// Canonical Aster-over-HTTP media type (Fory frames).
pub const ASTER_FRAMES: &str = "application/aster-frames";

/// A Salvo [`Router`] serving files from a filesystem directory under
/// `path_prefix`. Directories fall back to `index.html`. Plain static serving —
/// no iroh-blobs. Nest it into your app (or use [`HttpConfig::static_mount`]);
/// keep `/aster/*` for RPC.
///
/// ```ignore
/// let app = salvo::Router::new()
///     .push(aster_transport_salvo::router(dispatcher))
///     .push(aster_transport_salvo::static_router("/app", "./dist"));
/// ```
pub fn static_router(path_prefix: &str, dir: impl Into<std::path::PathBuf>) -> Router {
    let dir = dir.into();
    let p = path_prefix.trim_matches('/');
    let pattern = if p.is_empty() {
        "{*path}".to_string()
    } else {
        format!("{p}/{{*path}}")
    };
    Router::with_path(pattern).get(
        salvo::serve_static::StaticDir::new([dir.to_string_lossy().into_owned()])
            .defaults(["index.html"]),
    )
}
const JSON: &str = "application/json";
const NDJSON: &str = "application/x-ndjson";

/// Build a Salvo [`Router`] serving canonical Aster RPC (`application/aster-frames`)
/// under `/aster/{service}/{method}`. For browser JSON/NDJSON projections use
/// [`router_with`].
pub fn router(dispatcher: Dispatcher) -> Router {
    router_with(dispatcher, ProjectionRegistry::new())
}

/// Like [`router`], plus a [`ProjectionRegistry`] enabling browser media types
/// (`application/json` / `application/x-ndjson`) for the registered services.
pub fn router_with(dispatcher: Dispatcher, projections: ProjectionRegistry) -> Router {
    Router::with_path("aster/{service}/{method}").post(AsterHandler {
        dispatcher,
        projections,
    })
}

struct AsterHandler {
    dispatcher: Dispatcher,
    projections: ProjectionRegistry,
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
        let _ = res.add_header("vary", "Accept, Accept-Encoding", true);

        let ctype = req.content_type().map(|m| m.essence_str().to_owned());
        match ctype.as_deref() {
            None | Some(ASTER_FRAMES) => self.serve_frames(req, res, service, method).await,
            Some(m) if is_projection_media(m) => {
                let media_in = m.to_owned();
                self.serve_projection(req, res, service, method, media_in)
                    .await;
            }
            Some(_) => fail(
                res,
                HttpStatus::UNSUPPORTED_MEDIA_TYPE,
                "unsupported Content-Type; use application/aster-frames or application/json",
            ),
        }
    }
}

impl AsterHandler {
    /// Canonical path: request body = Aster frames in, response body = Aster
    /// frames out (streamed), Fory throughout.
    async fn serve_frames(
        &self,
        req: &mut Request,
        res: &mut Response,
        service: String,
        method: String,
    ) {
        let body = match req.payload().await {
            Ok(b) => b.to_vec(),
            Err(_) => return fail(res, HttpStatus::BAD_REQUEST, "missing request body"),
        };
        let frames = match parse_frames(&body) {
            Ok(f) => f,
            Err(_) => return fail(res, HttpStatus::BAD_REQUEST, "malformed request frame"),
        };
        let header_payload = match build_header(req, service, method) {
            Ok(h) => h,
            Err(_) => {
                return fail(
                    res,
                    HttpStatus::INTERNAL_SERVER_ERROR,
                    "header encode failed",
                )
            }
        };
        let resp_rx = self.spawn_dispatch(req.remote_addr().to_string(), header_payload, frames);

        res.status_code(HttpStatus::OK);
        let _ = res.add_header("content-type", ASTER_FRAMES, true);
        let stream = UnboundedReceiverStream::new(resp_rx)
            .map(|f| Ok::<Bytes, std::io::Error>(Bytes::from(raw_bytes(f))));
        res.body(ResBody::stream(stream));
    }

    /// Projection path: transcode a browser media type to/from canonical Fory.
    async fn serve_projection(
        &self,
        req: &mut Request,
        res: &mut Response,
        service: String,
        method: String,
        media_in: String,
    ) {
        let Some(proj) = self.projections.get(&service).cloned() else {
            return fail(
                res,
                HttpStatus::UNSUPPORTED_MEDIA_TYPE,
                "no JSON projection for this service",
            );
        };
        let pattern = match proj.pattern(&method) {
            Some(p) => p,
            None => return fail(res, HttpStatus::NOT_FOUND, "unknown method"),
        };
        if pattern == MethodPattern::BidiStream {
            return fail(
                res,
                HttpStatus::NOT_ACCEPTABLE,
                "bidi is not available over JSON; use WebTransport",
            );
        }

        let media_out = match req
            .first_accept()
            .map(|m| m.essence_str().to_owned())
            .as_deref()
        {
            None | Some("*/*") | Some("application/*") => default_out(pattern).to_owned(),
            Some(m) if is_projection_media(m) => m.to_owned(),
            Some(_) => return fail(res, HttpStatus::NOT_ACCEPTABLE, "cannot satisfy Accept"),
        };

        let body = match req.payload().await {
            Ok(b) => b.to_vec(),
            Err(_) => return fail(res, HttpStatus::BAD_REQUEST, "missing request body"),
        };
        let payloads = split_request(&media_in, &body);

        // Transcode each request payload into canonical Fory bytes.
        let mut frames = Vec::with_capacity(payloads.len());
        let n = payloads.len();
        for (i, p) in payloads.into_iter().enumerate() {
            match proj.request_to_fory(&method, &media_in, &p) {
                Ok(fory) => frames.push((fory, if i + 1 == n { FLAG_END_STREAM } else { 0 })),
                Err(e) => return fail(res, HttpStatus::BAD_REQUEST, &format!("request: {e}")),
            }
        }
        let header_payload = match build_header(req, service, method.clone()) {
            Ok(h) => h,
            Err(_) => {
                return fail(
                    res,
                    HttpStatus::INTERNAL_SERVER_ERROR,
                    "header encode failed",
                )
            }
        };
        let resp_rx = self.spawn_dispatch(req.remote_addr().to_string(), header_payload, frames);

        if media_out == NDJSON {
            self.respond_ndjson(res, resp_rx, proj, method);
        } else {
            self.respond_json(res, resp_rx, proj, method, media_out)
                .await;
        }
    }

    /// Stream NDJSON: one JSON line per response payload as it arrives.
    fn respond_ndjson(
        &self,
        res: &mut Response,
        resp_rx: mpsc::UnboundedReceiver<OutgoingFrame>,
        proj: Arc<dyn Projection>,
        method: String,
    ) {
        res.status_code(HttpStatus::OK);
        let _ = res.add_header("content-type", NDJSON, true);
        let stream = UnboundedReceiverStream::new(resp_rx).map(move |of| {
            let mut out = Vec::new();
            for_each_frame(raw_bytes(of), |payload, flags| {
                if flags & FLAG_TRAILER != 0 {
                    let st = decode_rpc_status(payload).unwrap_or_default();
                    if st.code != 0 {
                        out.extend_from_slice(
                            format!("{{\"aster_status\":{}}}\n", st.code).as_bytes(),
                        );
                    }
                } else if let Ok(json) = proj.response_from_fory(&method, NDJSON, payload) {
                    out.extend_from_slice(&json);
                    out.push(b'\n');
                }
            });
            Ok::<Bytes, std::io::Error>(Bytes::from(out))
        });
        res.body(ResBody::stream(stream));
    }

    /// Buffer JSON: collect payloads, then a bare object (1) or array (N); the
    /// Aster trailer status maps to the HTTP status.
    async fn respond_json(
        &self,
        res: &mut Response,
        mut resp_rx: mpsc::UnboundedReceiver<OutgoingFrame>,
        proj: Arc<dyn Projection>,
        method: String,
        media_out: String,
    ) {
        let mut data: Vec<Vec<u8>> = Vec::new();
        let mut status = RpcStatus::ok();
        while let Some(of) = resp_rx.recv().await {
            let mut failed: Option<String> = None;
            for_each_frame(raw_bytes(of), |payload, flags| {
                if flags & FLAG_TRAILER != 0 {
                    status = decode_rpc_status(payload).unwrap_or_default();
                } else if failed.is_none() {
                    match proj.response_from_fory(&method, &media_out, payload) {
                        Ok(json) => data.push(json),
                        Err(e) => failed = Some(e),
                    }
                }
            });
            if let Some(e) = failed {
                return fail(
                    res,
                    HttpStatus::INTERNAL_SERVER_ERROR,
                    &format!("response: {e}"),
                );
            }
        }

        if status.code != 0 {
            res.status_code(status_to_http(status.code));
            let _ = res.add_header("aster-status", status.code.to_string(), true);
            if !status.message.is_empty() {
                let _ = res.add_header("aster-message", status.message.clone(), true);
            }
            return;
        }

        res.status_code(HttpStatus::OK);
        let _ = res.add_header("content-type", media_out, true);
        let body = if data.len() == 1 {
            data.into_iter().next().unwrap()
        } else {
            json_array(&data)
        };
        res.body(ResBody::Once(Bytes::from(body)));
    }

    /// Build a [`CallParts`], spawn dispatch, and return the response channel.
    fn spawn_dispatch(
        &self,
        peer_id: String,
        header_payload: Vec<u8>,
        req_frames: Vec<(Vec<u8>, u8)>,
    ) -> mpsc::UnboundedReceiver<OutgoingFrame> {
        let (req_tx, req_rx) = mpsc::unbounded_channel::<RequestFrame>();
        let mut iter = req_frames.into_iter();
        let (request_payload, request_flags) = iter.next().unwrap_or((Vec::new(), FLAG_END_STREAM));
        for (payload, flags) in iter {
            let _ = req_tx.send(RequestFrame { payload, flags });
        }
        drop(req_tx);

        let (resp_tx, resp_rx) = mpsc::unbounded_channel::<OutgoingFrame>();
        let parts = CallParts {
            peer_id,
            header_payload,
            request_payload,
            request_flags,
            response_sender: resp_tx,
            request_receiver: req_rx,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            dispatcher.dispatch_parts(parts).await;
        });
        resp_rx
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn is_projection_media(m: &str) -> bool {
    m == JSON || m == NDJSON
}

fn default_out(pattern: MethodPattern) -> &'static str {
    match pattern {
        MethodPattern::ServerStream => NDJSON,
        _ => JSON,
    }
}

/// Split a projection request body into per-payload byte slices.
fn split_request(media_in: &str, body: &[u8]) -> Vec<Vec<u8>> {
    if media_in == NDJSON {
        body.split(|&b| b == b'\n')
            .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
            .filter(|l| !l.is_empty())
            .map(|l| l.to_vec())
            .collect()
    } else {
        vec![body.to_vec()]
    }
}

/// Walk the (possibly multiple) length-prefixed frames in `raw`.
fn for_each_frame(raw: Vec<u8>, mut f: impl FnMut(&[u8], u8)) {
    let mut buf = &raw[..];
    while !buf.is_empty() {
        match decode_frame(buf) {
            Ok((payload, flags, consumed)) if consumed > 0 => {
                f(&payload, flags);
                buf = &buf[consumed..];
            }
            _ => break,
        }
    }
}

/// Already-framed bytes from an outgoing frame (all variants carry framed bytes).
fn raw_bytes(f: OutgoingFrame) -> Vec<u8> {
    match f {
        OutgoingFrame::Frame(b) | OutgoingFrame::Trailer(b) | OutgoingFrame::CompleteUnary(b) => b,
    }
}

/// Concatenate JSON payloads into a JSON array.
fn json_array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(item);
    }
    out.push(b']');
    out
}

/// Map an Aster status code to an HTTP status (gRPC-style codes).
fn status_to_http(code: i32) -> HttpStatus {
    match code {
        0 => HttpStatus::OK,
        3 => HttpStatus::BAD_REQUEST,      // INVALID_ARGUMENT
        5 => HttpStatus::NOT_FOUND,        // NOT_FOUND
        7 => HttpStatus::FORBIDDEN,        // PERMISSION_DENIED
        12 => HttpStatus::NOT_IMPLEMENTED, // UNIMPLEMENTED
        16 => HttpStatus::UNAUTHORIZED,    // UNAUTHENTICATED
        _ => HttpStatus::INTERNAL_SERVER_ERROR,
    }
}

/// Parse concatenated length-prefixed Aster frames into `(payload, flags)`.
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

/// Build the StreamHeader (XLANG), copying HTTP headers into Aster metadata
/// (lowercased) so a server-side `Authenticator` sees `authorization` etc.
fn build_header(req: &Request, service: String, method: String) -> Result<Vec<u8>, ()> {
    let mut metadata_keys = Vec::new();
    let mut metadata_values = Vec::new();
    for (name, value) in req.headers().iter() {
        if let Ok(v) = value.to_str() {
            metadata_keys.push(name.as_str().to_string());
            metadata_values.push(v.to_string());
        }
    }
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
    encode_stream_header(&header).map_err(|_| ())
}

fn fail(res: &mut Response, code: HttpStatus, msg: &str) {
    res.status_code(code);
    res.render(msg.to_string());
}
