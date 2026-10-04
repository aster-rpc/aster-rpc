//! Expose local services over Aster, with the Python halves answered by
//! upcall (plan R6 / R9; design `upcall-design.md` section 6).
//!
//! ```python
//! expose = node.expose()
//! expose.expose_socket("api", "127.0.0.1:8080", authorize=check)  # async def check(service_id, peer, metadata) -> bool
//! expose.expose_http("app", handler, authorize=check)             # async def handler(request) -> HttpResponse
//! expose.serve_routes(EdgeRouter(), policy)                       # an edge: async def grant(peer, metadata, specs)
//! await expose.serve([b"aster-expose/api"])                       # or expose.handle(conn)
//! # consumer side
//! send, recv = await open_relay(conn, "api", b"token")            # raw stream to a socket target
//! resp = await relay_http(conn, "app", "GET", "/")                # one buffered HTTP exchange
//! reg = await request_route(conn, [RouteSpec("a.example", 443)])  # ask an edge for routes
//! ```
//!
//! `authorize` (kind `expose_authorize`), the HTTP handler (kind
//! `expose_http`, one upcall per request; bodies stream through the
//! runtime's [`HttpBodies`] table via [`HttpBodyStream`]) and the route
//! policy (kind `expose_route_policy`) are all answered by upcall.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use pyo3::exceptions::PyStopAsyncIteration;
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes, PyDict, PyMemoryView, PyString, PyTuple};
use pyo3_async_runtimes::tokio::future_into_py;

use aster::binding_support::upcall::codec::{
    ExposeHttpRequest, ExposeHttpResponse, ExposeRouteSpec, HttpBodyMode, HttpHeader,
};
use aster::binding_support::upcall::expose::{
    foreign_authorize_fn, foreign_route_policy, BodyError, ForeignHttpHandler, ForeignHttpOptions,
    HttpBodies,
};
use aster::expose::{AuthorizeFn, PeerContext, RoutePolicy, RouteProtocol, RouteSpec};

use crate::error::{aster_err_to_py, err_to_py};
use crate::keys::{invalid, node_hex, node_id_out, py_value};
use crate::net::{IrohConnection, IrohRecvStream, IrohSendStream};
use crate::upcall::{bytes_out, items, raw_bytes, str_in};

/// Default largest single body chunk (`ForeignHttpOptions::max_chunk`);
/// [`HttpBodyStream::write`] splits bigger chunks.
pub(crate) const DEFAULT_MAX_CHUNK: usize = 1 << 20;

/// UCv1 HTTP version codes, by index.
const HTTP_VERSIONS: [&str; 5] = ["HTTP/0.9", "HTTP/1.0", "HTTP/1.1", "HTTP/2", "HTTP/3"];

fn allow_all() -> AuthorizeFn {
    Arc::new(|_: PeerContext| Box::pin(async { Ok(()) }))
}

/// A node's expose registry (`node.expose()`): services exposed here are
/// reachable over any connection handed to `handle` (or accepted by
/// `serve`).
#[pyclass(frozen, module = "aster._aster")]
pub struct Expose {
    inner: aster::expose::Expose,
    node: aster::Node,
}

impl Expose {
    pub(crate) fn new(node: aster::Node) -> Self {
        Self {
            inner: node.expose(),
            node,
        }
    }
}

#[pymethods]
impl Expose {
    /// Expose the local TCP socket `addr` (`"host:port"`) as `service_id`.
    /// `authorize`: `None` admits every peer that reaches the ALPN; else an
    /// `aster.expose.ExposeAuthorizer` (`async def authorize(service_id,
    /// peer, metadata) -> bool`) or a callable with that signature, asked
    /// once per `(connection, service)` by upcall.
    #[pyo3(signature = (service_id, addr, *, authorize=None))]
    fn expose_socket(
        &self,
        service_id: &str,
        addr: &str,
        authorize: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let addr: SocketAddr = addr
            .parse()
            .map_err(|_| invalid(format!("addr must be host:port, got {addr:?}")))?;
        let auth = match authorize.filter(|a| !a.is_none()) {
            Some(a) => {
                foreign_authorize_fn(crate::upcall::bind(a, "expose_authorize")?, service_id)
            }
            None => allow_all(),
        };
        self.inner.expose_socket(service_id, auth, addr);
        Ok(())
    }

    /// Expose a Python HTTP handler as `service_id` (R9). `handler`: an
    /// `aster.expose.HttpHandler` (`async def handle(request) ->
    /// HttpResponse`) or a callable with that signature, asked once per
    /// request by upcall (kind `expose_http`; its timeout bounds the time to
    /// the response head). `authorize` as for `expose_socket`. Requests with
    /// a declared body of at most `inline_request_max` bytes arrive inline;
    /// larger ones stream (`request.body` is an `HttpBodyStream`).
    /// `max_request_body` / `max_response_body` cap bodies (`413` / `502`);
    /// `response_buffer` is how many `max_chunk`-sized chunks a streamed
    /// response queues before `write` waits. `max_chunk` (default 1 MiB,
    /// must be > 0) is the largest single body chunk: reads return at most
    /// that many bytes and `write` splits bigger chunks. A handler object
    /// exposed twice shares one `max_chunk` (the last call wins).
    #[pyo3(signature = (
        service_id, handler, *, authorize=None, inline_request_max=65536,
        max_request_body=None, max_response_body=None, response_buffer=8,
        max_chunk=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn expose_http(
        &self,
        service_id: &str,
        handler: &Bound<'_, PyAny>,
        authorize: Option<&Bound<'_, PyAny>>,
        inline_request_max: usize,
        max_request_body: Option<u64>,
        max_response_body: Option<u64>,
        response_buffer: usize,
        max_chunk: Option<usize>,
    ) -> PyResult<()> {
        if handler.is_none() {
            return Err(invalid("expose_http needs a handler"));
        }
        let max_chunk = max_chunk.unwrap_or(DEFAULT_MAX_CHUNK);
        if max_chunk == 0 {
            return Err(invalid("max_chunk must be > 0"));
        }
        let auth = match authorize.filter(|a| !a.is_none()) {
            Some(a) => {
                foreign_authorize_fn(crate::upcall::bind(a, "expose_authorize")?, service_id)
            }
            None => allow_all(),
        };
        let (upcalls, bodies) = crate::upcall::bind_http(handler, "expose_http", max_chunk)?;
        let options = ForeignHttpOptions {
            inline_request_max,
            max_request_body,
            max_response_body,
            max_chunk,
            response_buffer: response_buffer.max(1),
        };
        let h = ForeignHttpHandler::new(upcalls, bodies, service_id, options);
        let _guard = crate::upcall::tokio_guard();
        self.inner.expose_http(service_id, auth, h.handler());
        Ok(())
    }

    /// Act as an edge: accept route registrations from peers into `router`.
    /// `policy`: an `aster.expose.RoutePolicy` (`async def grant(peer,
    /// metadata, specs)` returning the granted specs or their indexes) or a
    /// callable with that signature, asked by upcall (kind
    /// `expose_route_policy`; no answer applies its failure mode:
    /// fail-closed grants nothing, fail-open everything). `None` grants
    /// every requested route.
    #[pyo3(signature = (router, policy=None))]
    fn serve_routes(&self, router: &EdgeRouter, policy: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        let policy: RoutePolicy = match policy.filter(|p| !p.is_none()) {
            Some(p) => foreign_route_policy(crate::upcall::bind(p, "expose_route_policy")?),
            None => {
                Arc::new(|_: PeerContext, specs: Vec<RouteSpec>| Box::pin(async move { Ok(specs) }))
            }
        };
        let _guard = crate::upcall::tokio_guard();
        self.inner.serve_routes(router.inner.clone(), policy);
        Ok(())
    }

    /// Stop exposing `service_id`.
    fn unexpose(&self, service_id: &str) {
        self.inner.unexpose(service_id);
    }

    /// Serve expose requests arriving on `conn` (from your own accept
    /// loop).
    fn handle(&self, conn: &IrohConnection) {
        let _guard = crate::upcall::tokio_guard();
        self.inner.handle(&conn.sdk());
    }

    /// Own the node's accept loop: feed every inbound connection on one of
    /// `alpns` to this registry until the node closes. Awaitable; start the
    /// node with those ALPNs and run nothing else that accepts on it.
    fn serve<'py>(&self, py: Python<'py>, alpns: Vec<Vec<u8>>) -> PyResult<Bound<'py, PyAny>> {
        let node = self.node.clone();
        let expose = self.inner.clone();
        future_into_py(py, async move {
            let refs: Vec<&[u8]> = alpns.iter().map(Vec::as_slice).collect();
            node.serve_expose(&expose, &refs)
                .await
                .map_err(aster_err_to_py)
        })
    }
}

/// Consumer side: open a raw relay stream to the socket service
/// `service_id` on `conn`, attaching `metadata` (what the host's authorize
/// hook receives). Awaitable `(IrohSendStream, IrohRecvStream)`: write the
/// target's protocol raw (e.g. HTTP/1.1). A refused authorization closes
/// the stream without data.
#[pyfunction]
#[pyo3(signature = (conn, service_id, metadata=Vec::new()))]
fn open_relay<'py>(
    py: Python<'py>,
    conn: &IrohConnection,
    service_id: String,
    metadata: Vec<u8>,
) -> PyResult<Bound<'py, PyAny>> {
    let core = conn.core().clone();
    future_into_py(py, async move {
        let (send, recv) = core
            .open_http_relay(&service_id, &metadata)
            .await
            .map_err(err_to_py)?;
        Ok((IrohSendStream::from(send), IrohRecvStream::from(recv)))
    })
}

/// Consumer side: relay one buffered HTTP request to the HTTP service
/// `service_id` on `conn`. Awaitable `aster.expose.HttpResponse` (body
/// `bytes`). Trailers do not cross the relay hop yet.
#[pyfunction]
#[pyo3(signature = (conn, service_id, method="GET", uri="/", headers=None, body=None))]
fn relay_http<'py>(
    py: Python<'py>,
    conn: &IrohConnection,
    service_id: String,
    method: &str,
    uri: &str,
    headers: Option<&Bound<'py, PyAny>>,
    body: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    use aster::expose::http;
    let headers = match headers.filter(|h| !h.is_none()) {
        Some(h) => headers_in(h, "headers")?,
        None => Vec::new(),
    };
    let body = match body.filter(|b| !b.is_none()) {
        Some(b) => match body_in(b)? {
            HttpBodyMode::Inline(v) => v,
            HttpBodyMode::Stream => return Err(invalid("relay_http body must be bytes or str")),
        },
        None => Vec::new(),
    };
    let mut builder = http::Request::builder().method(method).uri(uri);
    for (name, value) in &headers {
        builder = builder.header(name.as_str(), value.as_slice());
    }
    let request = builder
        .body(body)
        .map_err(|e| invalid(format!("invalid HTTP request: {e}")))?;
    let conn = conn.sdk();
    future_into_py(py, async move {
        let resp = aster::expose::relay_request(&conn, &service_id, request)
            .await
            .map_err(aster_err_to_py)?;
        let status = resp.status().as_u16();
        let headers: Vec<HttpHeader> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect();
        let body = resp.into_body();
        Python::attach(|py| {
            http_response_out(
                py,
                ExposeHttpResponse {
                    status,
                    headers,
                    body: HttpBodyMode::Inline(body),
                },
            )
            .map(Bound::unbind)
        })
    })
}

// ============================================================================
// Edge routes
// ============================================================================

/// An edge's `(host, port) -> origin route` table, filled by
/// `Expose.serve_routes` as peers register routes.
#[pyclass(frozen, module = "aster._aster")]
pub struct EdgeRouter {
    inner: aster::expose::EdgeRouter,
}

#[pymethods]
impl EdgeRouter {
    #[new]
    fn new() -> Self {
        Self {
            inner: aster::expose::EdgeRouter::new(),
        }
    }

    /// The live route for `(host, port)` (then `(host, 0)`):
    /// `(service_id, protocol, origin NodeId)`, or `None`.
    #[pyo3(signature = (host, port=0))]
    fn lookup(&self, host: &str, port: u16) -> Option<(String, &'static str, crate::ids::NodeId)> {
        self.inner.lookup(host, port).map(|r| {
            (
                r.service_id,
                protocol_name(r.protocol),
                node_id_out(r.conn.remote_id()),
            )
        })
    }

    /// Drop the route for `(host, port)`.
    #[pyo3(signature = (host, port=0))]
    fn unregister(&self, host: &str, port: u16) {
        self.inner.unregister(host, port);
    }
}

/// A live route registration (`request_route`): the routes stay bound
/// while it is open. `close()` (awaitable) ends it; dropping it does too.
#[pyclass(frozen, module = "aster._aster")]
pub struct RouteRegistration {
    granted: Vec<ExposeRouteSpec>,
    inner: Arc<Mutex<Option<aster::expose::RouteRegistration>>>,
}

#[pymethods]
impl RouteRegistration {
    /// The requested routes the edge granted (`RouteSpec`s).
    #[getter]
    fn granted<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        route_specs_out(py, &self.granted)
    }

    #[getter]
    fn closed(&self) -> bool {
        crate::upcall::lock_mutex(&self.inner).is_none()
    }

    /// End the registration (the edge evicts the routes). Idempotent.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let reg = crate::upcall::lock_mutex(&self.inner).take();
        future_into_py(py, async move {
            if let Some(reg) = reg {
                reg.close().await.map_err(err_to_py)?;
            }
            Ok(())
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "RouteRegistration(granted={}, closed={})",
            self.granted.len(),
            self.closed()
        )
    }
}

/// Consumer side: ask the edge at the other end of `conn` to route `specs`
/// (`RouteSpec`s) to this node, attaching `metadata` for its route policy.
/// Awaitable `RouteRegistration`; hold it for the routes to stay live.
#[pyfunction]
#[pyo3(signature = (conn, specs, metadata=None))]
fn request_route<'py>(
    py: Python<'py>,
    conn: &IrohConnection,
    specs: &Bound<'py, PyAny>,
    metadata: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let wire = route_specs_in(specs)?;
    let metadata = match metadata.filter(|m| !m.is_none()) {
        Some(m) => raw_bytes(m, "metadata")?,
        None => Vec::new(),
    };
    let specs: Vec<RouteSpec> = wire.iter().map(spec_to_core).collect();
    let conn = conn.sdk();
    future_into_py(py, async move {
        let reg = aster::expose::request_route(&conn, specs, metadata)
            .await
            .map_err(aster_err_to_py)?;
        let granted = reg.granted().iter().map(spec_to_wire).collect();
        Ok(RouteRegistration {
            granted,
            inner: Arc::new(Mutex::new(Some(reg))),
        })
    })
}

fn protocol_name(p: RouteProtocol) -> &'static str {
    match p {
        RouteProtocol::Http => "http",
        RouteProtocol::Tcp => "tcp",
    }
}

fn spec_to_core(s: &ExposeRouteSpec) -> RouteSpec {
    RouteSpec {
        host: s.host.clone(),
        port: s.port,
        protocol: if s.protocol == 1 {
            RouteProtocol::Tcp
        } else {
            RouteProtocol::Http
        },
        service_id: s.service_id.clone(),
    }
}

fn spec_to_wire(s: &RouteSpec) -> ExposeRouteSpec {
    ExposeRouteSpec {
        host: s.host.clone(),
        port: s.port,
        protocol: match s.protocol {
            RouteProtocol::Http => 0,
            RouteProtocol::Tcp => 1,
        },
        service_id: s.service_id.clone(),
    }
}

// ============================================================================
// HTTP body handles
// ============================================================================

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

/// `aster.expose.HttpBodyError` for `e`.
fn body_err(e: BodyError) -> PyErr {
    Python::attach(|py| {
        let build = || -> PyResult<PyErr> {
            let cls = py.import("aster.expose")?.getattr("HttpBodyError")?;
            let kw = PyDict::new(py);
            kw.set_item("kind", body_error_kind(&e))?;
            kw.set_item("code", e.code())?;
            Ok(PyErr::from_value(cls.call((e.to_string(),), Some(&kw))?))
        };
        build().unwrap_or_else(|err| err)
    })
}

/// One HTTP exchange's body handle (`request.handle`; also `request.body`
/// when the request body streams). Read the request body with `await
/// read()` or `async for chunk in stream`; the pump writes a streamed
/// response through it after the head (`write` / `finish` / `abort` are
/// there for handlers that drive it themselves). A handle decoded outside
/// an exchange (`ucv1_decode`) is detached: every call raises.
#[pyclass(frozen, module = "aster._aster")]
pub struct HttpBodyStream {
    bodies: Option<HttpBodies>,
    stream_id: u64,
    /// The exchange handler's `max_chunk`.
    max_chunk: usize,
}

impl HttpBodyStream {
    fn table(&self) -> PyResult<HttpBodies> {
        self.bodies
            .clone()
            .ok_or_else(|| body_err(BodyError::UnknownStream(self.stream_id)))
    }
}

#[pymethods]
impl HttpBodyStream {
    #[getter]
    fn stream_id(&self) -> u64 {
        self.stream_id
    }

    /// Whether this handle belongs to a live body table.
    #[getter]
    fn attached(&self) -> bool {
        self.bodies.is_some()
    }

    /// Awaitable: the next request-body chunk (at most `max` bytes, and at
    /// most the handler's `max_chunk`), or `None` at the end.
    #[pyo3(signature = (max=65536))]
    fn read<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Bound<'py, PyAny>> {
        let bodies = self.table()?;
        let id = self.stream_id;
        future_into_py(py, async move {
            let chunk = bodies.read(id, max.max(1)).await.map_err(body_err)?;
            Ok(chunk.map(|c| Python::attach(|py| PyBytes::new(py, &c).unbind())))
        })
    }

    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let bodies = self.table()?;
        let id = self.stream_id;
        let max_chunk = self.max_chunk;
        future_into_py(py, async move {
            match bodies.read(id, max_chunk).await.map_err(body_err)? {
                Some(c) => Ok(Python::attach(|py| PyBytes::new(py, &c).unbind())),
                None => Err(PyStopAsyncIteration::new_err(())),
            }
        })
    }

    /// Awaitable: queue a response-body chunk, waiting while the queue is
    /// full (backpressure). Write after the response head went out.
    fn write<'py>(
        &self,
        py: Python<'py>,
        chunk: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let bodies = self.table()?;
        let id = self.stream_id;
        let data = chunk_in(chunk)?;
        let max_chunk = self.max_chunk;
        future_into_py(py, async move {
            for piece in data.chunks(max_chunk) {
                bodies.write(id, piece.to_vec()).await.map_err(body_err)?;
            }
            Ok(())
        })
    }

    /// Queue a chunk (at most `max_chunk` bytes) without waiting: `False` when the
    /// queue is full (nothing queued).
    fn try_write(&self, chunk: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.table()?
            .try_write(self.stream_id, chunk_in(chunk)?)
            .map_err(body_err)
    }

    /// End the response body, optionally with trailers (`(name, value)`
    /// pairs).
    #[pyo3(signature = (trailers=None))]
    fn finish(&self, trailers: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        let trailers = match trailers.filter(|t| !t.is_none()) {
            Some(t) => headers_in(t, "trailers")?,
            None => Vec::new(),
        };
        self.table()?
            .finish(self.stream_id, trailers)
            .map_err(body_err)
    }

    /// Abort the response: the consumer sees a failed (reset) response,
    /// not a truncated one.
    #[pyo3(signature = (message="aborted"))]
    fn abort(&self, message: &str) -> PyResult<()> {
        self.table()?
            .abort(self.stream_id, message)
            .map_err(body_err)
    }

    fn __repr__(&self) -> String {
        format!(
            "HttpBodyStream(stream_id={}, attached={})",
            self.stream_id,
            self.attached()
        )
    }
}

// ============================================================================
// UCv1 <-> Python for expose (used by `crate::upcall`'s codec)
// ============================================================================

fn chunk_in(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(s.to_cow()?.as_bytes().to_vec());
    }
    raw_bytes(obj, "a body chunk")
}

/// `bytes` / `bytearray` / `memoryview` / `str` travel inline; anything
/// else (an iterable of chunks, `None`) streams.
pub(crate) fn body_in(obj: &Bound<'_, PyAny>) -> PyResult<HttpBodyMode> {
    if obj.is_instance_of::<PyBytes>()
        || obj.is_instance_of::<PyByteArray>()
        || obj.is_instance_of::<PyMemoryView>()
        || obj.is_instance_of::<PyString>()
    {
        return Ok(HttpBodyMode::Inline(chunk_in(obj)?));
    }
    Ok(HttpBodyMode::Stream)
}

pub(crate) fn headers_out<'py>(py: Python<'py>, h: &[HttpHeader]) -> PyResult<Bound<'py, PyAny>> {
    let pairs = h
        .iter()
        .map(|(n, v)| {
            PyTuple::new(
                py,
                [
                    n.into_pyobject(py)?.into_any(),
                    PyBytes::new(py, v).into_any(),
                ],
            )
        })
        .collect::<PyResult<Vec<_>>>()?;
    Ok(PyTuple::new(py, pairs)?.into_any())
}

/// `(name, value)` pairs (or a mapping): names are lowercased; values are
/// `bytes` or `str`.
pub(crate) fn headers_in(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Vec<HttpHeader>> {
    let it = if obj.cast::<PyDict>().is_ok() || obj.hasattr("items")? {
        obj.call_method0("items")?.try_iter()?
    } else {
        obj.try_iter()
            .map_err(|_| invalid(format!("{what} must be (name, value) pairs")))?
    };
    let mut out = Vec::new();
    for pair in it {
        let p = items(&pair?, 2, what)?;
        let name = str_in(&p[0], "a header name")?.to_ascii_lowercase();
        if name.is_empty() {
            return Err(invalid(format!("{what}: empty header name")));
        }
        out.push((name, chunk_in(&p[1])?));
    }
    Ok(out)
}

fn version_in(obj: &Bound<'_, PyAny>) -> PyResult<u8> {
    if let Ok(code) = obj.extract::<u8>() {
        if (code as usize) < HTTP_VERSIONS.len() {
            return Ok(code);
        }
    } else if let Ok(s) = obj.extract::<String>() {
        if let Some(i) = HTTP_VERSIONS.iter().position(|v| *v == s) {
            return Ok(i as u8);
        }
    }
    Err(invalid(format!(
        "HTTP version must be one of {HTTP_VERSIONS:?}"
    )))
}

pub(crate) fn http_request_out<'py>(
    py: Python<'py>,
    req: ExposeHttpRequest,
    bodies: Option<(HttpBodies, usize)>,
) -> PyResult<Bound<'py, PyAny>> {
    let (bodies, max_chunk) = match bodies {
        Some((b, m)) => (Some(b), m),
        None => (None, DEFAULT_MAX_CHUNK),
    };
    let handle = Bound::new(
        py,
        HttpBodyStream {
            bodies,
            stream_id: req.stream_id,
            max_chunk,
        },
    )?
    .into_any();
    let body = match &req.body {
        HttpBodyMode::Inline(b) => bytes_out(py, b),
        HttpBodyMode::Stream => handle.clone(),
    };
    py_value!(py, "aster.expose", "HttpRequest" {
        service_id: req.service_id,
        peer: req.peer_id.map(node_id_out),
        stream_id: req.stream_id,
        version: HTTP_VERSIONS[req.version as usize],
        method: req.method,
        uri: req.uri,
        headers: headers_out(py, &req.headers)?,
        body: body,
        handle: handle,
    })
}

pub(crate) fn http_request_in(obj: &Bound<'_, PyAny>) -> PyResult<ExposeHttpRequest> {
    let peer = obj.getattr("peer")?;
    let method = str_in(&obj.getattr("method")?, "method")?;
    if method.is_empty() {
        return Err(invalid("HTTP method must be non-empty"));
    }
    Ok(ExposeHttpRequest {
        service_id: str_in(&obj.getattr("service_id")?, "service_id")?,
        peer_id: if peer.is_none() {
            None
        } else {
            Some(node_hex(&peer)?)
        },
        stream_id: obj
            .getattr("stream_id")?
            .extract()
            .map_err(|_| invalid("stream_id must be a non-negative int"))?,
        version: version_in(&obj.getattr("version")?)?,
        method,
        uri: str_in(&obj.getattr("uri")?, "uri")?,
        headers: headers_in(&obj.getattr("headers")?, "headers")?,
        body: body_in(&obj.getattr("body")?)?,
    })
}

/// A decoded response: `body` is `bytes` (inline) or `None` (streamed).
pub(crate) fn http_response_out<'py>(
    py: Python<'py>,
    resp: ExposeHttpResponse,
) -> PyResult<Bound<'py, PyAny>> {
    let body = match &resp.body {
        HttpBodyMode::Inline(b) => bytes_out(py, b),
        HttpBodyMode::Stream => py.None().into_bound(py),
    };
    py_value!(py, "aster.expose", "HttpResponse" {
        status: resp.status,
        headers: headers_out(py, &resp.headers)?,
        body: body,
    })
}

pub(crate) fn http_response_in(obj: &Bound<'_, PyAny>) -> PyResult<ExposeHttpResponse> {
    let status: u16 = obj
        .getattr("status")
        .map_err(|_| invalid("an HTTP response must be an aster.expose.HttpResponse"))?
        .extract()
        .map_err(|_| invalid("HTTP status must be an int"))?;
    if !(100..=999).contains(&status) {
        return Err(invalid(format!(
            "HTTP status must be 100..=999, got {status}"
        )));
    }
    Ok(ExposeHttpResponse {
        status,
        headers: headers_in(&obj.getattr("headers")?, "headers")?,
        body: body_in(&obj.getattr("body")?)?,
    })
}

pub(crate) fn route_specs_out<'py>(
    py: Python<'py>,
    specs: &[ExposeRouteSpec],
) -> PyResult<Bound<'py, PyAny>> {
    let v = specs
        .iter()
        .map(|s| {
            py_value!(py, "aster.expose", "RouteSpec" {
                host: s.host.clone(),
                port: s.port,
                protocol: if s.protocol == 1 { "tcp" } else { "http" },
                service_id: s.service_id.clone(),
            })
        })
        .collect::<PyResult<Vec<_>>>()?;
    Ok(PyTuple::new(py, v)?.into_any())
}

pub(crate) fn route_specs_in(obj: &Bound<'_, PyAny>) -> PyResult<Vec<ExposeRouteSpec>> {
    let mut out = Vec::new();
    for s in obj
        .try_iter()
        .map_err(|_| invalid("route specs must be an iterable of RouteSpec"))?
    {
        let s = s?;
        let proto = s.getattr("protocol")?;
        let protocol = match proto.extract::<String>().ok().as_deref() {
            Some("http") => 0,
            Some("tcp") => 1,
            _ => match proto.extract::<u8>() {
                Ok(p @ 0..=1) => p,
                _ => return Err(invalid("RouteSpec.protocol must be 'http' or 'tcp'")),
            },
        };
        out.push(ExposeRouteSpec {
            host: str_in(&s.getattr("host")?, "RouteSpec.host")?,
            port: s
                .getattr("port")?
                .extract()
                .map_err(|_| invalid("RouteSpec.port must be 0..=65535"))?,
            protocol,
            service_id: str_in(&s.getattr("service_id")?, "RouteSpec.service_id")?,
        });
    }
    Ok(out)
}

/// Granted indexes: any iterable of ints, sorted and de-duplicated.
pub(crate) fn grant_in(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u32>> {
    let mut v = obj
        .try_iter()
        .map_err(|_| invalid("a route grant must be an iterable of indexes"))?
        .map(|i| {
            let i = i?;
            if i.extract::<bool>().is_ok() {
                return Err(invalid("a route index must be an int, not a bool"));
            }
            i.extract::<u32>()
                .map_err(|_| invalid("a route index must be a non-negative int"))
        })
        .collect::<PyResult<Vec<u32>>>()?;
    v.sort_unstable();
    v.dedup();
    Ok(v)
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Expose>()?;
    m.add_class::<EdgeRouter>()?;
    m.add_class::<RouteRegistration>()?;
    m.add_class::<HttpBodyStream>()?;
    m.add_function(wrap_pyfunction!(open_relay, m)?)?;
    m.add_function(wrap_pyfunction!(relay_http, m)?)?;
    m.add_function(wrap_pyfunction!(request_route, m)?)?;
    Ok(())
}
