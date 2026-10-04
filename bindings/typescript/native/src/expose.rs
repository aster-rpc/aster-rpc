//! Expose — local services over Aster (`aster::expose`): L4 socket targets
//! and JavaScript HTTP handlers (upcall kind 49, R9) whose per-`(connection,
//! service)` authorization is a JavaScript function (kind 48, R6), edge
//! route policies (kind 50), a node-owned accept loop, and the consumer's
//! raw relay stream, buffered HTTP relay and route registration
//! (`IrohConnection.openExposeStream` / `relayHttp` / `requestRoute`).
//!
//! HTTP bodies never travel over upcalls: each exchange has a body handle
//! (`HttpBodyHandle`, from `Upcalls.httpBody(streamId)`) whose `read` /
//! `write` are napi async functions over the async `HttpBodies` API, so a
//! full response queue is an unresolved promise (backpressure is `await`) and
//! no `_blocking` call ever runs on the JavaScript thread.

use std::sync::Mutex;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::binding_support::upcall::expose::{
    foreign_authorize_fn, foreign_route_policy, BodyError, ForeignHttpHandler, ForeignHttpOptions,
    HttpBodies,
};
use aster::expose::{AuthorizeFn, PeerContext, RoutePolicy, RouteSpec};

use crate::error::to_napi_err;
use crate::net::{IrohBiStream, IrohConnection, IrohRecvStream, IrohSendStream};
use crate::node::Node;
use crate::upcalls::{
    headers_in, headers_out, route_spec_from_core, route_spec_to_core, ExposeHeader,
    ExposeRouteSpec, Upcalls,
};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// Default largest single body chunk (`ForeignHttpOptions::max_chunk`).
const DEFAULT_MAX_CHUNK: usize = 1 << 20;

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

/// A node-wide exposure registry (`node.expose()`). Register services, then
/// `node.serveExpose(expose, alpns)`.
#[napi]
pub struct Expose {
    pub(crate) inner: aster::expose::Expose,
}

impl Expose {
    pub(crate) fn create() -> Self {
        // `Expose::new` spawns its reactor drain task.
        Self {
            inner: within_runtime_if_available(aster::expose::Expose::new),
        }
    }
}

#[napi]
impl Expose {
    /// Expose a raw L4 splice to the loopback `addr` (`"127.0.0.1:8080"`)
    /// as `serviceId`. Each `(connection, service)` is authorized once by
    /// the JavaScript authorizer on `upcalls` (kind 48); no answer applies
    /// the kind's failure mode (fail-closed by default).
    #[napi]
    pub fn expose_socket(&self, service_id: String, addr: String, upcalls: &Upcalls) -> Result<()> {
        let addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|_| invalid(format!("addr must be ip:port, got {addr:?}")))?;
        self.inner.expose_socket(
            &service_id,
            foreign_authorize_fn(upcalls.table(), &service_id),
            addr,
        );
        Ok(())
    }

    /// Expose a JavaScript HTTP handler as `serviceId` (R9): one kind-49
    /// upcall per request on `upcalls`, bodies through its body table.
    /// `authorize` (`true`): ask the kind-48 authorizer on `upcalls`;
    /// `false` admits every peer that reaches the ALPN.
    #[napi]
    pub fn expose_http(
        &self,
        service_id: String,
        upcalls: &Upcalls,
        authorize: bool,
        options: Option<ExposeHttpOptions>,
    ) -> Result<()> {
        let options = options.unwrap_or_default();
        let max_chunk = options.max_chunk.map_or(DEFAULT_MAX_CHUNK, |c| c as usize);
        if max_chunk == 0 {
            return Err(invalid("maxChunk must be > 0"));
        }
        let opts = ForeignHttpOptions {
            inline_request_max: options.inline_request_max.map_or(64 * 1024, |c| c as usize),
            max_request_body: cap_in(options.max_request_body, "maxRequestBody")?,
            max_response_body: cap_in(options.max_response_body, "maxResponseBody")?,
            max_chunk,
            response_buffer: options.response_buffer.map_or(8, |c| (c as usize).max(1)),
        };
        let auth: AuthorizeFn = if authorize {
            foreign_authorize_fn(upcalls.table(), &service_id)
        } else {
            std::sync::Arc::new(|_: PeerContext| Box::pin(async { Ok(()) }))
        };
        let handler =
            ForeignHttpHandler::new(upcalls.table(), upcalls.bodies.clone(), &service_id, opts);
        within_runtime_if_available(|| {
            self.inner.expose_http(&service_id, auth, handler.handler())
        });
        Ok(())
    }

    /// Act as an edge: accept route registrations from peers into
    /// `router`. `policy` (`true`): ask the kind-50 route policy on
    /// `upcalls` (no usable answer applies its failure mode: fail-closed
    /// grants nothing, fail-open everything); `false` grants every
    /// requested route.
    #[napi]
    pub fn serve_routes(&self, router: &EdgeRouter, upcalls: &Upcalls, policy: bool) {
        let policy: RoutePolicy = if policy {
            foreign_route_policy(upcalls.table())
        } else {
            std::sync::Arc::new(|_: PeerContext, specs: Vec<RouteSpec>| {
                Box::pin(async move { Ok(specs) })
            })
        };
        within_runtime_if_available(|| self.inner.serve_routes(router.inner.clone(), policy));
    }

    /// Stop exposing `serviceId`.
    #[napi]
    pub fn unexpose(&self, service_id: String) {
        self.inner.unexpose(&service_id);
    }
}

/// Options of `Expose.exposeHttp` (all optional).
#[napi(object)]
#[derive(Default)]
pub struct ExposeHttpOptions {
    /// A request with a declared `content-length` of at most this many bytes
    /// arrives inline; larger ones stream (default 65 536).
    pub inline_request_max: Option<u32>,
    /// Cap on the request body: a declared length over it is a `413`
    /// without an upcall (default none).
    pub max_request_body: Option<f64>,
    /// Cap on the response body: inline over it is a `502`, streamed is
    /// aborted (default none).
    pub max_response_body: Option<f64>,
    /// Largest single body chunk: reads return at most this many bytes and
    /// `write` splits bigger chunks (default 1 MiB, must be > 0).
    pub max_chunk: Option<u32>,
    /// Response chunks queued before `write` waits (default 8).
    pub response_buffer: Option<u32>,
}

fn cap_in(v: Option<f64>, what: &str) -> Result<Option<u64>> {
    match v {
        None => Ok(None),
        Some(n)
            if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= MAX_SAFE_INTEGER as f64 =>
        {
            Ok(Some(n as u64))
        }
        Some(_) => Err(invalid(format!(
            "{what} must be a non-negative safe integer"
        ))),
    }
}

// ── HTTP body handles ───────────────────────────────────────────────────────

fn body_error_kind(e: &BodyError) -> &'static str {
    match e {
        BodyError::UnknownStream(_) => "unknown_stream",
        BodyError::Cancelled => "cancelled",
        BodyError::TooLarge(_) => "too_large",
        BodyError::ChunkTooLarge { .. } => "chunk_too_large",
        BodyError::Finished => "finished",
        BodyError::Timeout => "timeout",
        BodyError::Read(_) => "read",
        BodyError::InvalidTrailers(_) => "invalid_trailers",
        BodyError::BlockingOnRuntime => "blocking_on_runtime",
    }
}

/// `[body:<kind>:<code>] message`: `HttpBodyError` on the JavaScript side
/// parses the tag.
fn body_err(e: BodyError) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("[body:{}:{}] {e}", body_error_kind(&e), e.code()),
    )
}

/// One HTTP exchange's body handle: read the request body, write the
/// response body. Everything async here is a napi async function: nothing
/// blocks the JavaScript thread.
#[napi]
pub struct HttpBodyHandle {
    bodies: HttpBodies,
    stream_id: u64,
    /// The exchange handler's `max_chunk`.
    max_chunk: usize,
}

#[napi]
impl HttpBodyHandle {
    #[napi(getter)]
    pub fn stream_id(&self) -> f64 {
        self.stream_id as f64
    }

    /// The next request-body chunk (at most `max` bytes and at most the
    /// handler's `maxChunk`), or `null` at the end.
    #[napi]
    pub async fn read(&self, max: Option<u32>) -> Result<Option<Buffer>> {
        let bodies = self.bodies.clone();
        let id = self.stream_id;
        let max = (max.unwrap_or(65_536) as usize).clamp(1, self.max_chunk);
        Ok(bodies
            .read(id, max)
            .await
            .map_err(body_err)?
            .map(Buffer::from))
    }

    /// Queue a response-body chunk (split at `maxChunk`), waiting while the
    /// queue is full: awaiting it is the backpressure. Write after the
    /// response head went out.
    #[napi]
    pub async fn write(&self, chunk: Buffer) -> Result<()> {
        let bodies = self.bodies.clone();
        let id = self.stream_id;
        let max_chunk = self.max_chunk;
        let data = chunk.to_vec();
        for piece in data.chunks(max_chunk) {
            bodies.write(id, piece.to_vec()).await.map_err(body_err)?;
        }
        Ok(())
    }

    /// Queue a chunk (at most `maxChunk` bytes) without waiting; `false`
    /// when the queue is full (nothing queued).
    #[napi]
    pub fn try_write(&self, chunk: Buffer) -> Result<bool> {
        self.bodies
            .try_write(self.stream_id, chunk.to_vec())
            .map_err(body_err)
    }

    /// End the response body, optionally with trailers.
    #[napi]
    pub fn finish(&self, trailers: Option<Vec<ExposeHeader>>) -> Result<()> {
        let trailers = headers_in(&trailers.unwrap_or_default(), "trailers")?;
        self.bodies
            .finish(self.stream_id, trailers)
            .map_err(body_err)
    }

    /// Abort the response: the consumer sees a failed (reset) response, not
    /// a truncated one.
    #[napi]
    pub fn abort(&self, message: Option<String>) -> Result<()> {
        self.bodies
            .abort(self.stream_id, message.as_deref().unwrap_or("aborted"))
            .map_err(body_err)
    }
}

#[napi]
impl Upcalls {
    /// The body handle of the HTTP exchange `streamId` (a kind-49
    /// request's `streamId`); `maxChunk` is the handler's.
    #[napi]
    pub fn http_body(&self, stream_id: f64, max_chunk: Option<u32>) -> Result<HttpBodyHandle> {
        if !stream_id.is_finite()
            || stream_id < 0.0
            || stream_id.fract() != 0.0
            || stream_id > MAX_SAFE_INTEGER as f64
        {
            return Err(invalid("streamId must be a non-negative safe integer"));
        }
        Ok(HttpBodyHandle {
            bodies: self.bodies.clone(),
            stream_id: stream_id as u64,
            max_chunk: max_chunk.map_or(DEFAULT_MAX_CHUNK, |c| (c as usize).max(1)),
        })
    }
}

// ── edge routes ─────────────────────────────────────────────────────────────

/// An edge's `(host, port) -> origin route` table, filled by
/// `Expose.serveRoutes` as peers register routes.
#[napi]
pub struct EdgeRouter {
    pub(crate) inner: aster::expose::EdgeRouter,
}

/// A live route: where requests for `(host, port)` go.
#[napi(object)]
pub struct EdgeRouteInfo {
    pub service_id: String,
    pub protocol: String,
    /// The origin node (hex node id).
    pub node_id: String,
}

#[napi]
impl EdgeRouter {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: aster::expose::EdgeRouter::new(),
        }
    }

    /// The live route for `(host, port)` (then `(host, 0)`), or `null`.
    #[napi]
    pub fn lookup(&self, host: String, port: Option<u32>) -> Result<Option<EdgeRouteInfo>> {
        let port =
            u16::try_from(port.unwrap_or(0)).map_err(|_| invalid("port must be 0..=65535"))?;
        Ok(self.inner.lookup(&host, port).map(|r| EdgeRouteInfo {
            service_id: r.service_id,
            protocol: match r.protocol {
                aster::expose::RouteProtocol::Http => "http",
                aster::expose::RouteProtocol::Tcp => "tcp",
            }
            .into(),
            node_id: r.conn.remote_id(),
        }))
    }

    /// Drop the route for `(host, port)`.
    #[napi]
    pub fn unregister(&self, host: String, port: Option<u32>) -> Result<()> {
        let port =
            u16::try_from(port.unwrap_or(0)).map_err(|_| invalid("port must be 0..=65535"))?;
        self.inner.unregister(&host, port);
        Ok(())
    }
}

/// A live route registration (`requestRoute`): the routes stay bound while
/// it is open. `close()` ends it; dropping it does too.
#[napi]
pub struct RouteRegistration {
    granted: Vec<RouteSpec>,
    inner: Mutex<Option<aster::expose::RouteRegistration>>,
}

#[napi]
impl RouteRegistration {
    /// The requested routes the edge granted.
    #[napi(getter)]
    pub fn granted(&self) -> Vec<ExposeRouteSpec> {
        self.granted.iter().map(route_spec_from_core).collect()
    }

    #[napi(getter)]
    pub fn closed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    }

    /// End the registration (the edge evicts the routes). Idempotent.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let reg = self.inner.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(reg) = reg {
            reg.close().await.map_err(to_napi_err)?;
        }
        Ok(())
    }
}

/// A buffered HTTP request for `IrohConnection.relayHttp`.
#[napi(object)]
pub struct RelayHttpRequest {
    pub method: Option<String>,
    pub uri: Option<String>,
    pub headers: Option<Vec<ExposeHeader>>,
    pub body: Option<Buffer>,
}

/// The collected response of `IrohConnection.relayHttp`.
#[napi(object)]
pub struct RelayHttpResponse {
    pub status: u32,
    pub headers: Vec<ExposeHeader>,
    pub body: Buffer,
}

/// A running expose accept loop; `close()` stops it.
#[napi]
pub struct ExposeServer {
    pub(crate) handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[napi]
impl ExposeServer {
    /// Stop accepting (idempotent). Open relays finish on their own.
    #[napi]
    pub fn close(&self) {
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            h.abort();
        }
    }
}

#[napi]
impl Node {
    /// A fresh exposure registry.
    #[napi]
    pub fn expose(&self) -> Expose {
        Expose::create()
    }

    /// Own this node's accept loop for expose: every inbound connection on
    /// one of `alpns` (all, when empty) is fed to `expose`. Start the node
    /// with those ALPNs in `alpns`; don't also run an RPC server or
    /// `acceptAster` loop on it.
    #[napi]
    pub fn serve_expose(&self, expose: &Expose, alpns: Vec<String>) -> ExposeServer {
        let node = self.aster().clone();
        let expose = expose.inner.clone();
        let handle = within_runtime_if_available(move || {
            tokio::spawn(async move {
                let alpns: Vec<&[u8]> = alpns.iter().map(|a| a.as_bytes()).collect();
                let _ = node.serve_expose(&expose, &alpns).await;
            })
        });
        ExposeServer {
            handle: Mutex::new(Some(handle)),
        }
    }
}

#[napi]
impl IrohConnection {
    /// Consumer side of expose: open a raw relay stream to the peer's
    /// exposed `serviceId` (a socket target), attaching `metadata` for its
    /// authorizer. Bytes written are spliced to the target's TCP socket; an
    /// unauthorized stream is closed by the peer.
    #[napi]
    pub async fn open_expose_stream(
        &self,
        service_id: String,
        metadata: Option<Buffer>,
    ) -> Result<IrohBiStream> {
        let metadata = metadata.map(|m| m.to_vec()).unwrap_or_default();
        let (send, recv) = self
            .inner
            .clone()
            .open_http_relay(&service_id, &metadata)
            .await
            .map_err(to_napi_err)?;
        Ok(IrohBiStream {
            send: Some(IrohSendStream { inner: send }),
            recv: Some(IrohRecvStream { inner: recv }),
        })
    }

    /// Consumer side: relay one buffered HTTP request to the HTTP service
    /// `serviceId` and collect the response. Trailers do not cross the
    /// relay hop yet. A response that fails mid-body is an error, not a
    /// truncated body.
    #[napi]
    pub async fn relay_http(
        &self,
        service_id: String,
        request: RelayHttpRequest,
    ) -> Result<RelayHttpResponse> {
        use aster::expose::http;
        let headers = headers_in(&request.headers.unwrap_or_default(), "headers")?;
        let mut builder = http::Request::builder()
            .method(request.method.as_deref().unwrap_or("GET"))
            .uri(request.uri.as_deref().unwrap_or("/"));
        for (name, value) in &headers {
            builder = builder.header(name.as_str(), value.as_slice());
        }
        let req = builder
            .body(request.body.map(|b| b.to_vec()).unwrap_or_default())
            .map_err(|e| invalid(format!("invalid HTTP request: {e}")))?;
        let conn = aster::Connection::from_core_connection(self.inner.clone());
        let resp = aster::expose::relay_request(&conn, &service_id, req)
            .await
            .map_err(|e| to_napi_err(e.into()))?;
        let status = resp.status().as_u16().into();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect();
        Ok(RelayHttpResponse {
            status,
            headers: headers_out(headers),
            body: resp.into_body().into(),
        })
    }

    /// Consumer side: ask the edge at the other end to route `specs` to
    /// this node, attaching `metadata` for its route policy. Hold the
    /// registration for the routes to stay live.
    #[napi]
    pub async fn request_route(
        &self,
        specs: Vec<ExposeRouteSpec>,
        metadata: Option<Buffer>,
    ) -> Result<RouteRegistration> {
        let specs = specs
            .iter()
            .map(route_spec_to_core)
            .collect::<Result<Vec<_>>>()?;
        let conn = aster::Connection::from_core_connection(self.inner.clone());
        let reg = aster::expose::request_route(
            &conn,
            specs,
            metadata.map(|m| m.to_vec()).unwrap_or_default(),
        )
        .await
        .map_err(|e| to_napi_err(e.into()))?;
        Ok(RouteRegistration {
            granted: reg.granted().to_vec(),
            inner: Mutex::new(Some(reg)),
        })
    }
}
