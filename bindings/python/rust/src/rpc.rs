//! RPC on `aster::rpc` dynamic dispatch (plan P4, design
//! `docs/_internal/bindings/dynamic-dispatch-design.md` §4.2).
//!
//! **Server.** [`DispatchServer`] serves Python-implemented services through
//! `aster::rpc::Server`: Python registers each service in its JSON form
//! (design §1.5, produced by `aster/_lowering.py`), Rust runs every gate
//! (header decode, lookup, sessions, Gate 3, deadlines, frame limits) and
//! delivers events through a `RingSink`. Python drains them in batches --
//! one crossing per batch -- and answers each call through its
//! [`ServerCall`] with payloads and statuses, never frames. `respond`,
//! `send` and `finish` are synchronous; only `recv_batch` and
//! `wait_writable` await. Baseline services (`aster.ops.NodeInfo`) are
//! served in Rust next to the Python ones.
//!
//! **Client.** [`RpcClient`] wraps the pooled `aster::rpc::RpcConnection`
//! (design §3): unary in one crossing, streaming calls as handles whose
//! responses a pump task reads ahead so Python takes them in batches.

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use pyo3_async_runtimes::tokio::future_into_py;

use aster::binding_support::dispatch::{
    CallHandle, CancelReason, DispatchEvent, InboundTrace, PeerAttributes, RingConsumer, RingSink,
    SubmitError,
};
use aster::binding_support::CoreConnection;
use aster::rpc::baseline::{Attr, NodeIdentity, NodeInfo, NodeInfoClient, NodeInfoServer};
use aster::rpc::{
    AttributeStore, AuthContext, AuthOutcome, Authenticator, CallOptions, DynamicService,
    RequestSink, ResponseFuture, ResponseStream, RpcConnection, RpcStatus, Server, ServerHandle,
    StreamHeader,
};

use crate::admission::GatePolicy;
use crate::error::aster_err_to_py;
use crate::net::IrohConnection;
use crate::PyBytesResult;

// ── event kinds (keep in sync with aster/_dispatch.py) ──────────────────────

/// `next_batch` item: a `(EV_CANCELLED, call_id, reason)` tuple.
const EV_CANCELLED: u8 = 2;
/// `(EV_RESPONSE_WRITABLE, call_id)`.
const EV_RESPONSE_WRITABLE: u8 = 3;
/// `(EV_RESPONSE_CLOSED, call_id)`.
const EV_RESPONSE_CLOSED: u8 = 4;
/// `(EV_SESSION_CLOSED, session_key)`.
const EV_SESSION_CLOSED: u8 = 5;
/// `(EV_CONNECTION_CLOSED, connection_id, peer)`.
const EV_CONNECTION_CLOSED: u8 = 6;
/// `(EV_NON_RPC, alpn, IrohConnection)`.
const EV_NON_RPC: u8 = 7;

/// `Cancelled` reasons.
const CANCEL_PEER: u8 = 1;
const CANCEL_DEADLINE: u8 = 2;
const CANCEL_INVALID_REQUEST: u8 = 3;

/// Poll a future once without a real waker: `Some(output)` if it is ready
/// now. Only for cancel-safe futures (dropping a pending one loses nothing).
fn poll_once<F: Future>(fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

fn status_of(code: i32, message: &str, details: Option<HashMap<String, String>>) -> RpcStatus {
    let (detail_keys, detail_values) = details.unwrap_or_default().into_iter().unzip();
    RpcStatus {
        code,
        message: message.to_owned(),
        detail_keys,
        detail_values,
    }
}

/// `respond` / `send` / `finish` result: `0` sent, `-1` the call is gone
/// (cancelled, closed, already finished), `n > 0` the queue is full and
/// `wait_writable(n)` then the same operation will succeed.
fn submit_result(r: Result<(), SubmitError>) -> PyResult<i64> {
    match r {
        Ok(()) => Ok(0),
        Err(SubmitError::QueueFull { required_len }) => Ok(required_len.max(1) as i64),
        Err(SubmitError::NotFound) | Err(SubmitError::Closed) => Ok(-1),
        Err(SubmitError::TooLarge) => Err(PyValueError::new_err("response frame too large")),
        Err(other) => Err(PyValueError::new_err(other.to_string())),
    }
}

// ── server ──────────────────────────────────────────────────────────────────

/// The baseline `aster.ops.NodeInfo` served for a Python server.
struct StaticNodeInfo(NodeIdentity);

#[aster::rpc::async_trait]
impl NodeInfo for StaticNodeInfo {
    async fn describe(&self) -> aster::Result<NodeIdentity> {
        Ok(self.0.clone())
    }
}

/// Gate 3 reads the peer's Gate-0 admission attributes: the bridge from an
/// application-supplied `GatePolicy` (whose store records per-peer
/// attributes, e.g. `gate.admit(peer, attributes)`) to Rust-enforced
/// `requires` (design §2.6). With Enrollment the role source is the
/// enrollment session instead (`enrollment=`).
struct GateAttributes(Arc<RwLock<aster::GatePolicy>>);

#[aster::rpc::async_trait]
impl Authenticator for GateAttributes {
    async fn authenticate(&self, ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        let attributes = self
            .0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .attributes(&ctx.peer_id);
        Ok(AuthOutcome {
            principal: None,
            attributes,
        })
    }
}

/// Python services served by `aster::rpc::Server` (design §4.2).
///
/// `DispatchServer.start(node, services, ...)` registers the services (JSON
/// strings, design §1.5; `service_idx` = list position) and starts serving
/// on `node`, which must have been started with `aster/1`. Drain events with
/// `poll_batch` / `await next_batch`.
#[pyclass(frozen, module = "aster._aster")]
pub struct DispatchServer {
    consumer: Arc<tokio::sync::Mutex<RingConsumer>>,
    handle: Mutex<Option<ServerHandle>>,
    attributes: AttributeStore,
    table: aster::binding_support::dispatch::CallTable,
    dispatcher: Mutex<Option<aster::rpc::Dispatcher>>,
    http: Arc<crate::http::HttpChildren>,
}

#[pymethods]
impl DispatchServer {
    /// Register `services` (DynamicService JSON strings) and serve them on
    /// `node`.
    ///
    /// - `node_info`: the `aster.ops.NodeInfo` identity as `(name, tags,
    ///   roles)`; `None` disables the baseline service.
    /// - `node_attributes`: text attributes served in that identity.
    /// - `gate`: with `gate_attributes=True`, Gate 3 reads the caller's
    ///   attributes from this policy's admission store.
    /// - `enrollment`: an installed `Enrollment`: Gate 3 reads each peer's
    ///   roles from its live enrollment session, the enrollment ALPN is
    ///   served, and a configured delegated issuer serves renewals.
    /// - `handler_timeout`: cap on any handler, seconds (`None` = the Rust
    ///   default, 300 s; `<= 0` = no cap).
    #[staticmethod]
    #[pyo3(signature = (node, services, *, node_info=Some((String::new(), Vec::new(), Vec::new())), node_attributes=Vec::new(), gate=None, gate_attributes=false, enrollment=None, max_sessions_per_connection=None, handler_timeout=None, ring_capacity=256, builtin_topology=false, topology_requires=None))]
    #[allow(clippy::too_many_arguments)]
    fn start(
        node: PyRef<'_, crate::node::Node>,
        services: Vec<String>,
        node_info: Option<(String, Vec<String>, Vec<String>)>,
        node_attributes: Vec<(String, String)>,
        gate: Option<PyRef<'_, GatePolicy>>,
        gate_attributes: bool,
        enrollment: Option<PyRef<'_, crate::enrollment::Enrollment>>,
        max_sessions_per_connection: Option<usize>,
        handler_timeout: Option<f64>,
        ring_capacity: usize,
        builtin_topology: bool,
        topology_requires: Option<&str>,
    ) -> PyResult<Self> {
        if builtin_topology && !node.inner.topology().has_monitoring() {
            return Err(PyValueError::new_err(
                "builtin topology requires monitoring",
            ));
        }
        let requirement = if let Some(json) = topology_requires {
            if json.len() > 1024 * 1024 {
                return Err(PyValueError::new_err("topology requirement exceeds 1 MiB"));
            }
            serde_json::from_str::<Option<aster::rpc::CapabilityRequirement>>(json).map_err(
                |error| PyValueError::new_err(format!("invalid topology requirement: {error}")),
            )?
        } else {
            None
        };
        crate::ensure_tokio_runtime();
        let rt = pyo3_async_runtimes::tokio::get_runtime();
        let _guard = rt.enter();

        let (sink, consumer) = RingSink::new_on(rt.handle(), ring_capacity.max(2));
        let attributes = AttributeStore::new();
        let mut server = Server::new(&node.inner).attributes(attributes.clone());
        if builtin_topology {
            server = server.with_builtin_topology(&node.inner, requirement);
        }
        if let Some((node_name, tags, roles)) = node_info {
            server = server.register(NodeInfoServer::new(StaticNodeInfo(NodeIdentity {
                node_id: node.inner.id().to_string(),
                node_name,
                tags,
                roles,
                attributes: node_attributes
                    .into_iter()
                    .map(|(name, value)| Attr::new(name, value))
                    .collect(),
            })));
        }
        if let Some(enrollment) = &enrollment {
            if gate.is_some() {
                return Err(PyValueError::new_err(
                    "enrollment owns Gate 0: pass either gate or enrollment, not both",
                ));
            }
            server = enrollment.serve_on(server, &attributes);
        }
        if gate_attributes {
            if let Some(gate) = &gate {
                server = server.authenticator(GateAttributes(gate.shared.clone()));
            }
        }
        if let Some(max) = max_sessions_per_connection {
            server = server.max_sessions_per_connection(max);
        }
        if let Some(secs) = handler_timeout {
            server = server.handler_timeout(if secs > 0.0 && secs.is_finite() {
                Some(Duration::from_secs_f64(secs))
            } else {
                None
            });
        }
        server = server.dispatch_sink(sink);
        for json in services {
            let svc = DynamicService::from_json(&json).map_err(aster_err_to_py)?;
            server = server.dynamic_service(svc).map_err(aster_err_to_py)?;
        }
        let table = server.call_table();
        let dispatcher = server.dispatcher();
        let handle = server.serve();
        Ok(DispatchServer {
            consumer: Arc::new(tokio::sync::Mutex::new(consumer)),
            handle: Mutex::new(Some(handle)),
            attributes,
            table,
            dispatcher: Mutex::new(Some(dispatcher)),
            http: Arc::new(crate::http::HttpChildren::default()),
        })
    }

    /// Events ready now, without waiting (up to `max`): a list (possibly
    /// empty), or `None` once the server has stopped and nothing is left.
    fn poll_batch(&self, py: Python<'_>, max: usize) -> PyResult<Py<PyAny>> {
        let Ok(mut consumer) = self.consumer.try_lock() else {
            // A `next_batch` is waiting: it will take what is there.
            return Ok(PyList::empty(py).into_any().unbind());
        };
        let mut events = Vec::new();
        consumer.drain(max.max(1), |e| events.push(e));
        if events.is_empty() && consumer.is_closed() {
            return Ok(py.None());
        }
        drop(consumer);
        Ok(EventBatch(Some(events)).into_pyobject(py)?.unbind())
    }

    /// Await the next batch of up to `max` events (one crossing per batch).
    /// Resolves to a list, or `None` once the server has stopped.
    ///
    /// Items: a `ServerCall` per call, otherwise a tuple whose first element
    /// is the event kind (see `aster/_dispatch.py`).
    fn next_batch<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Bound<'py, PyAny>> {
        let consumer = self.consumer.clone();
        future_into_py(py, async move {
            let mut consumer = consumer.lock().await;
            loop {
                consumer.ready().await;
                let mut events = Vec::new();
                consumer.drain(max.max(1), |e| events.push(e));
                if !events.is_empty() {
                    return Ok(EventBatch(Some(events)));
                }
                if consumer.is_closed() {
                    return Ok(EventBatch(None));
                }
            }
        })
    }

    /// Set Gate-3 attributes for `peer` (hex node id) in the server's
    /// attribute store. For a peer with a live enrollment session the
    /// credential sets the maximum roles: `aster.role` here raises
    /// `ValueError` (use `restrict_peer_roles`).
    fn set_peer_attributes(
        &self,
        peer: String,
        attributes: HashMap<String, String>,
    ) -> PyResult<()> {
        self.attributes
            .set(peer, attributes)
            .map_err(|e| PyValueError::new_err(e.to_string()))
    }

    /// Narrow an enrolled `peer`'s roles to `roles` intersected with its
    /// credential roles, from its next call. Raises `LookupError` without a
    /// live enrollment session.
    fn restrict_peer_roles(&self, peer: &str, roles: Vec<String>) -> PyResult<()> {
        self.attributes
            .restrict_roles(peer, roles)
            .map_err(|e| pyo3::exceptions::PyLookupError::new_err(e.to_string()))
    }

    /// Remove `peer`'s role restriction; returns whether one was set.
    fn clear_peer_restriction(&self, peer: &str) -> bool {
        self.attributes.clear_restriction(peer)
    }

    /// Remove `peer`'s entry from the server's attribute store.
    fn remove_peer_attributes(&self, peer: &str) {
        self.attributes.remove(peer);
    }

    /// Number of calls Rust is still tracking (in flight, unanswered).
    fn live_calls(&self) -> usize {
        self.table.len()
    }

    /// Bind an authenticated PEM listener over these registered services.
    #[pyo3(signature = (authenticator, bind_addr, cert_pem, key_pem, *, webtransport=false, max_pending_auth=256, max_auth_payload_bytes=65536))]
    #[allow(clippy::too_many_arguments)]
    fn start_http<'py>(
        &self,
        py: Python<'py>,
        authenticator: &Bound<'_, PyAny>,
        bind_addr: String,
        cert_pem: Vec<u8>,
        key_pem: Vec<u8>,
        webtransport: bool,
        max_pending_auth: usize,
        max_auth_payload_bytes: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        let auth =
            crate::http::authenticator(authenticator, max_pending_auth, max_auth_payload_bytes)?;
        let children = self.http.clone();
        let dispatcher = self
            .dispatcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("server is closed"))?;
        future_into_py(py, async move {
            crate::http::start(
                children,
                dispatcher,
                crate::http::ListenerPolicy::Authenticated(auth),
                bind_addr,
                cert_pem,
                key_pem,
                webtransport,
            )
            .await
        })
    }

    /// Bind an explicitly anonymous PEM listener; session services are rejected.
    #[pyo3(signature = (bind_addr, cert_pem, key_pem, *, webtransport=false))]
    fn start_public_http<'py>(
        &self,
        py: Python<'py>,
        bind_addr: String,
        cert_pem: Vec<u8>,
        key_pem: Vec<u8>,
        webtransport: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let children = self.http.clone();
        let dispatcher = self
            .dispatcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("server is closed"))?;
        future_into_py(py, async move {
            crate::http::start(
                children,
                dispatcher,
                crate::http::ListenerPolicy::Public,
                bind_addr,
                cert_pem,
                key_pem,
                webtransport,
            )
            .await
        })
    }

    /// Join HTTP children before stopping the Python dispatch pump.
    fn close_http<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let children = self.http.clone();
        children.close();
        future_into_py(py, async move { children.stop_all().await })
    }

    /// Stop serving: the accept/dispatch loops are aborted. In-flight calls
    /// end when their `ServerCall`s are answered or dropped.
    fn close(&self) {
        self.http.force_cleanup();
        self.dispatcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(handle) = self.handle.lock().unwrap().take() {
            handle.abort();
        }
    }
}

impl Drop for DispatchServer {
    fn drop(&mut self) {
        self.http.force_cleanup();
        if let Some(handle) = self.handle.get_mut().ok().and_then(Option::take) {
            handle.abort();
        }
    }
}

/// A drained batch, converted to Python on the event-loop thread. `None` =
/// the server stopped.
struct EventBatch(Option<Vec<DispatchEvent>>);

impl<'py> IntoPyObject<'py> for EventBatch {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let Some(events) = self.0 else {
            return Ok(py.None().into_bound(py));
        };
        let list = PyList::empty(py);
        for ev in events {
            if let Some(obj) = event_to_py(py, ev)? {
                list.append(obj)?;
            }
        }
        Ok(list.into_any())
    }
}

fn event_to_py(py: Python<'_>, ev: DispatchEvent) -> PyResult<Option<Bound<'_, PyAny>>> {
    let obj = match ev {
        DispatchEvent::Call(call) => {
            let header = call.header;
            Bound::new(
                py,
                ServerCall {
                    call_id: call.call_id,
                    service_idx: call.service_idx,
                    method_idx: call.method_idx,
                    flags: call.flags,
                    session_key: call.session_key,
                    connection_id: call.connection_id,
                    wire_call_id: call.wire_call_id,
                    deadline_ms: call.deadline_unix_ms,
                    deadline_secs: header.deadline,
                    peer: call.peer_id.to_string(),
                    request: PyBytes::new(py, &call.request).unbind(),
                    header,
                    attrs: call.attributes,
                    connection: call.connection,
                    trace_parent: call.trace_context.as_ref().map(|c| {
                        crate::observability::call_traceparent(c.trace_id, c.span_id, c.trace_flags)
                    }),
                    inbound_trace: call.inbound_trace,
                    handle: call.handle,
                },
            )?
            .into_any()
        }
        DispatchEvent::Cancelled { call_id, reason } => {
            let reason = match reason {
                CancelReason::Deadline => CANCEL_DEADLINE,
                CancelReason::InvalidRequest => CANCEL_INVALID_REQUEST,
                _ => CANCEL_PEER,
            };
            (EV_CANCELLED, call_id, reason)
                .into_pyobject(py)?
                .into_any()
        }
        DispatchEvent::ResponseWritable { call_id } => (EV_RESPONSE_WRITABLE, call_id)
            .into_pyobject(py)?
            .into_any(),
        DispatchEvent::ResponseClosed { call_id } => {
            (EV_RESPONSE_CLOSED, call_id).into_pyobject(py)?.into_any()
        }
        DispatchEvent::SessionClosed { session_key } => (EV_SESSION_CLOSED, session_key)
            .into_pyobject(py)?
            .into_any(),
        DispatchEvent::ConnectionClosed {
            connection_id,
            peer_id,
        } => (EV_CONNECTION_CLOSED, connection_id, peer_id.to_string())
            .into_pyobject(py)?
            .into_any(),
        DispatchEvent::NonRpc { alpn, connection } => {
            let conn = Bound::new(py, IrohConnection::from(connection))?;
            PyTuple::new(
                py,
                [
                    EV_NON_RPC.into_pyobject(py)?.into_any(),
                    PyBytes::new(py, &alpn).into_any(),
                    conn.into_any(),
                ],
            )?
            .into_any()
        }
        _ => return Ok(None),
    };
    Ok(Some(obj))
}

/// One call delivered to Python. Holds the owning call handle: dropping the
/// object before the call is answered ends it with `INTERNAL "handler
/// dropped call"`.
///
/// `respond` / `send` / `finish` return `0` when sent, `-1` when the call is
/// gone (cancelled, closed or already finished), and `n > 0` when the
/// response queue is full: `await wait_writable(n)`, then repeat the same
/// operation.
#[pyclass(frozen, module = "aster._aster")]
pub struct ServerCall {
    /// Correlation key (per server, from 1).
    #[pyo3(get)]
    call_id: u64,
    /// Index of the service in the list passed to `DispatchServer.start`.
    #[pyo3(get)]
    service_idx: u32,
    /// Index of the method in the service's `methods`.
    #[pyo3(get)]
    method_idx: u32,
    /// `FIRST_IS_END` (1) | `NEW_SESSION` (2) | `IROH_AUTHENTICATED` (4).
    #[pyo3(get)]
    flags: u32,
    /// `0` for shared services; the Rust-assigned session key otherwise.
    #[pyo3(get)]
    session_key: u64,
    #[pyo3(get)]
    connection_id: u64,
    /// `StreamHeader.call_id` (request id for logs).
    #[pyo3(get)]
    wire_call_id: i32,
    /// When Rust cancels the call with `DEADLINE_EXCEEDED`, Unix ms (0 = never).
    #[pyo3(get)]
    deadline_ms: u64,
    /// The caller's relative deadline from the header (seconds, 0 = none).
    #[pyo3(get)]
    deadline_secs: i16,
    /// Peer endpoint id (or authenticator principal).
    #[pyo3(get)]
    peer: String,
    /// The first request payload, decompressed.
    #[pyo3(get)]
    request: Py<PyBytes>,
    header: Arc<StreamHeader>,
    attrs: Arc<PeerAttributes>,
    connection: Option<CoreConnection>,
    /// W3C `traceparent` of the call's server span (set when the span is
    /// exported; the parent of binding spans and the source of log trace ids).
    trace_parent: Option<String>,
    /// Inbound W3C `baggage` / `tracestate` (hidden from the metadata).
    inbound_trace: Option<InboundTrace>,
    handle: CallHandle,
}

#[pymethods]
impl ServerCall {
    /// The server span's `traceparent`, or `None` when tracing is off.
    #[getter]
    fn trace_parent(&self) -> Option<String> {
        self.trace_parent.clone()
    }

    /// The caller's raw W3C `baggage` header (within the 8192-byte / 64-member
    /// limits), or `None`. Set whether or not the call's span is exported; the
    /// reserved keys are not in `metadata()`.
    #[getter]
    fn baggage(&self) -> Option<String> {
        self.inbound_trace
            .as_ref()
            .and_then(|t| t.baggage.as_deref().map(str::to_owned))
    }

    /// The caller's raw W3C `tracestate` header, or `None`.
    #[getter]
    fn tracestate(&self) -> Option<String> {
        self.inbound_trace
            .as_ref()
            .and_then(|t| t.tracestate.as_deref().map(str::to_owned))
    }

    /// Single-shot terminal response (unary, client-stream): one data frame
    /// (omitted when `payload` is empty) plus the status trailer.
    #[pyo3(signature = (payload, code=0, message="", details=None))]
    fn respond(
        &self,
        payload: &[u8],
        code: i32,
        message: &str,
        details: Option<HashMap<String, String>>,
    ) -> PyResult<i64> {
        submit_result(
            self.handle
                .try_respond(payload, &status_of(code, message, details)),
        )
    }

    /// One stream data frame (server-stream, bidi). `payload` must not be
    /// empty.
    fn send(&self, payload: &[u8]) -> PyResult<i64> {
        submit_result(self.handle.try_send(payload))
    }

    /// The terminal status trailer.
    #[pyo3(signature = (code=0, message="", details=None))]
    fn finish(
        &self,
        code: i32,
        message: &str,
        details: Option<HashMap<String, String>>,
    ) -> PyResult<i64> {
        submit_result(self.handle.try_finish(&status_of(code, message, details)))
    }

    /// After a full queue: resolve `True` once `required_len` bytes of
    /// response capacity are held (repeat the operation now), `False` if
    /// the call ended.
    fn wait_writable<'py>(
        &self,
        py: Python<'py>,
        required_len: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        let call = self.handle.to_ref();
        future_into_py(py, async move {
            Ok(call.wait_writable(required_len).await.unwrap_or(false))
        })
    }

    /// Up to `max` further request payloads (client-stream, bidi), as soon
    /// as at least one is available; `[]` at the end of the request stream.
    #[pyo3(signature = (max=64))]
    fn recv_batch<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Bound<'py, PyAny>> {
        let call = self.handle.to_ref();
        let max = max.max(1);
        future_into_py(py, async move {
            let mut out = Vec::new();
            if let Some(first) = call.recv().await {
                out.push(first);
                while out.len() < max {
                    // `recv` is cancel-safe (an mpsc receive behind a lock),
                    // so a pending poll here loses nothing.
                    match poll_once(call.recv()) {
                        Some(Some(next)) => out.push(next),
                        _ => break,
                    }
                }
            }
            Ok(BytesList(out))
        })
    }

    /// Whether the peer cancelled, the stream failed, or the deadline passed.
    #[getter]
    fn cancelled(&self) -> bool {
        self.handle.is_cancelled()
    }

    /// Whether the call has ended (answered, cancelled or abandoned).
    #[getter]
    fn finished(&self) -> bool {
        self.handle.is_finished()
    }

    #[getter]
    fn service(&self) -> &str {
        &self.header.service
    }

    #[getter]
    fn method(&self) -> &str {
        &self.header.method
    }

    /// The header's session id (0 for shared services).
    #[getter]
    fn session_id(&self) -> i32 {
        self.header.session_id
    }

    #[getter]
    fn serialization_mode(&self) -> i8 {
        self.header.serialization_mode
    }

    /// The header metadata as a dict (empty when none).
    fn metadata<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (k, v) in self
            .header
            .metadata_keys
            .iter()
            .zip(self.header.metadata_values.iter())
        {
            d.set_item(k, v)?;
        }
        Ok(d)
    }

    /// Whether the header carries any metadata.
    fn has_metadata(&self) -> bool {
        !self.header.metadata_keys.is_empty()
    }

    /// The caller's Gate-3 attributes (attribute store + gate, if bridged).
    fn attributes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (k, v) in self.attrs.iter() {
            d.set_item(k, v)?;
        }
        Ok(d)
    }

    /// The connection the call arrived on (tunnels, transport snapshot).
    fn connection(&self) -> Option<IrohConnection> {
        self.connection.clone().map(IrohConnection::from)
    }

    /// End the call with `INTERNAL "handler dropped call"` unless it ended.
    fn abandon(&self) {
        self.handle.abandon();
    }

    fn __repr__(&self) -> String {
        format!(
            "ServerCall(call_id={}, {}.{}, peer={})",
            self.call_id, self.header.service, self.header.method, self.peer
        )
    }
}

struct BytesList(Vec<Vec<u8>>);

impl<'py> IntoPyObject<'py> for BytesList {
    type Target = PyList;
    type Output = Bound<'py, PyList>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, self.0.into_iter().map(|b| PyBytes::new(py, &b)))
    }
}

// ── client ──────────────────────────────────────────────────────────────────

/// Map a client-side `aster::Error` onto the Python exceptions the
/// transport has always raised: `RpcError` (by status code) for RPC
/// failures, `ConnectionLostError` for broken streams and connections,
/// `TransportError` for misuse.
fn client_err(e: aster::Error) -> PyErr {
    Python::attach(|py| {
        let made = match e {
            aster::Error::Rpc {
                code,
                message,
                details,
            } => py.import("aster.status").and_then(|m| {
                let d = PyDict::new(py);
                for (k, v) in details {
                    d.set_item(k, v)?;
                }
                m.getattr("_from_wire")?.call1((code, message, d))
            }),
            aster::Error::InvalidArgument(msg) => py
                .import("aster.transport.base")
                .and_then(|m| m.getattr("TransportError")?.call1((msg,))),
            other => {
                let msg = other.to_string();
                py.import("aster.transport.base")
                    .and_then(|m| m.getattr("ConnectionLostError")?.call1((msg,)))
            }
        };
        match made {
            Ok(exc) => PyErr::from_value(exc),
            Err(err) => err,
        }
    })
}

fn make_header(
    service: String,
    method: String,
    version: i32,
    mode: i8,
    metadata: Option<Vec<(String, String)>>,
) -> StreamHeader {
    let (metadata_keys, metadata_values) = metadata.unwrap_or_default().into_iter().unzip();
    StreamHeader {
        service,
        method,
        version,
        call_id: 0,
        deadline: 0,
        serialization_mode: mode,
        metadata_keys,
        metadata_values,
        session_id: 0,
    }
}

fn make_options(session_id: u32, deadline: f64) -> CallOptions {
    let mut o = CallOptions::new().session(session_id);
    if deadline > 0.0 && deadline.is_finite() {
        o = o.deadline(Duration::from_secs_f64(deadline));
    }
    o
}

/// An RPC client over one `aster/1` connection: the pooled
/// `aster::rpc::RpcConnection`. Every client on the connection (Rust or
/// Python) shares its stream pool and session-id counter.
#[pyclass(frozen, module = "aster._aster")]
pub struct RpcClient {
    conn: RpcConnection,
}

#[pymethods]
impl RpcClient {
    #[new]
    fn new(connection: PyRef<'_, IrohConnection>) -> Self {
        RpcClient {
            conn: RpcConnection::from_core_connection(connection.core_clone()),
        }
    }

    /// Call the peer's baseline `aster.ops.NodeInfo.describe`. Returns
    /// `{"node_id", "node_name", "tags", "roles", "attributes"}`, with the
    /// attributes as a `{name: str}` dict (non-text values in their display
    /// form, a declared null as `None`).
    fn describe_node<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.conn.clone();
        future_into_py(py, async move {
            let identity = NodeInfoClient::new(conn)
                .describe()
                .await
                .map_err(client_err)?;
            Python::attach(|py| {
                let attributes = PyDict::new(py);
                for attr in &identity.attributes {
                    let value: Option<String> = attr.value.as_ref().map(|value| {
                        if let Some(text) = value.as_text() {
                            text.to_string()
                        } else if let Some(v) = value.as_int() {
                            v.to_string()
                        } else if let Some(v) = value.as_float() {
                            v.to_string()
                        } else if let Some(v) = value.as_bool() {
                            v.to_string()
                        } else {
                            String::new()
                        }
                    });
                    attributes.set_item(&attr.name, value)?;
                }
                let out = PyDict::new(py);
                out.set_item("node_id", &identity.node_id)?;
                out.set_item("node_name", &identity.node_name)?;
                out.set_item("tags", &identity.tags)?;
                out.set_item("roles", &identity.roles)?;
                out.set_item("attributes", attributes)?;
                Ok::<Py<PyAny>, PyErr>(out.into_any().unbind())
            })
        })
    }

    /// A fresh session id from the connection's shared counter (never 0).
    fn next_session_id(&self) -> PyResult<u32> {
        self.conn.open_session().map(|s| s.id()).map_err(client_err)
    }

    /// Unary calls kept on the wire at once (the SHARED pool size).
    #[getter]
    fn shared_pool_size(&self) -> usize {
        self.conn.shared_pool_size()
    }

    #[setter]
    fn set_shared_pool_size(&self, size: usize) {
        self.conn.set_shared_pool_size(size);
    }

    /// Unary call: returns the (decompressed) response payload; `b""` when
    /// the server answered OK without a response frame. `deadline` is the
    /// total budget in seconds (0 = none).
    #[pyo3(signature = (service, method, payload, *, version=1, session_id=0, deadline=0.0, serialization_mode=0, metadata=None))]
    #[allow(clippy::too_many_arguments)]
    fn unary<'py>(
        &self,
        py: Python<'py>,
        service: String,
        method: String,
        payload: Vec<u8>,
        version: i32,
        session_id: u32,
        deadline: f64,
        serialization_mode: i8,
        metadata: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.conn.clone();
        let header = make_header(service, method, version, serialization_mode, metadata);
        let options = make_options(session_id, deadline);
        future_into_py(py, async move {
            conn.unary_with(&header, payload, &options)
                .await
                .map(PyBytesResult)
                .map_err(client_err)
        })
    }

    /// Server-streaming call; resolves to a `ClientResponseStream`.
    #[pyo3(signature = (service, method, payload, *, version=1, session_id=0, deadline=0.0, serialization_mode=0, metadata=None))]
    #[allow(clippy::too_many_arguments)]
    fn server_stream<'py>(
        &self,
        py: Python<'py>,
        service: String,
        method: String,
        payload: Vec<u8>,
        version: i32,
        session_id: u32,
        deadline: f64,
        serialization_mode: i8,
        metadata: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.conn.clone();
        let header = make_header(service, method, version, serialization_mode, metadata);
        let options = make_options(session_id, deadline);
        future_into_py(py, async move {
            let stream = conn
                .server_stream_with(&header, payload, &options)
                .await
                .map_err(client_err)?;
            Ok(ClientResponseStream::spawn(stream))
        })
    }

    /// Client-streaming call with every request known up front (written in
    /// batched frames): resolves to the response payload.
    #[pyo3(signature = (service, method, payloads, *, version=1, session_id=0, deadline=0.0, serialization_mode=0, metadata=None))]
    #[allow(clippy::too_many_arguments)]
    fn client_stream<'py>(
        &self,
        py: Python<'py>,
        service: String,
        method: String,
        payloads: Vec<Vec<u8>>,
        version: i32,
        session_id: u32,
        deadline: f64,
        serialization_mode: i8,
        metadata: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.conn.clone();
        let header = make_header(service, method, version, serialization_mode, metadata);
        let options = make_options(session_id, deadline);
        future_into_py(py, async move {
            conn.client_stream_with(&header, payloads, &options)
                .await
                .map(PyBytesResult)
                .map_err(client_err)
        })
    }

    /// Incremental client-streaming call: resolves to `(ClientRequestSink,
    /// ClientResponseFuture)`.
    #[pyo3(signature = (service, method, *, version=1, session_id=0, deadline=0.0, serialization_mode=0, metadata=None))]
    #[allow(clippy::too_many_arguments)]
    fn client_stream_open<'py>(
        &self,
        py: Python<'py>,
        service: String,
        method: String,
        version: i32,
        session_id: u32,
        deadline: f64,
        serialization_mode: i8,
        metadata: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.conn.clone();
        let header = make_header(service, method, version, serialization_mode, metadata);
        let options = make_options(session_id, deadline);
        future_into_py(py, async move {
            let (sink, response) = conn
                .client_stream_open(&header, &options)
                .await
                .map_err(client_err)?;
            Ok((
                ClientRequestSink {
                    sink: Arc::new(sink),
                },
                ClientResponseFuture {
                    fut: Arc::new(tokio::sync::Mutex::new(Some(response))),
                },
            ))
        })
    }

    /// Bidirectional call: resolves to `(ClientRequestSink,
    /// ClientResponseStream)`.
    #[pyo3(signature = (service, method, *, version=1, session_id=0, deadline=0.0, serialization_mode=0, metadata=None))]
    #[allow(clippy::too_many_arguments)]
    fn bidi_open<'py>(
        &self,
        py: Python<'py>,
        service: String,
        method: String,
        version: i32,
        session_id: u32,
        deadline: f64,
        serialization_mode: i8,
        metadata: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.conn.clone();
        let header = make_header(service, method, version, serialization_mode, metadata);
        let options = make_options(session_id, deadline);
        future_into_py(py, async move {
            let (sink, stream) = conn
                .bidi_open_with(&header, &options)
                .await
                .map_err(client_err)?;
            Ok((
                ClientRequestSink {
                    sink: Arc::new(sink),
                },
                ClientResponseStream::spawn(stream),
            ))
        })
    }
}

/// The response half of a server-stream or bidi call. Read-ahead is bounded by
/// 64 items and 8 MiB of charged payload; one legal larger frame is allowed
/// only alone. Together with one in-flight native frame, raw read-ahead is at
/// most 24 MiB under the native 16 MiB frame limit. A returned batch is bounded
/// separately (64 items / 8 MiB plus one legal crossing frame, at most 24 MiB).
/// Raw read-ahead plus one Rust batch is bounded separately from Python copies,
/// codec objects and RSS. Conversion consumes each Vec after copying it into
/// Python; batches retained by the application are caller-owned.
#[pyclass(frozen, module = "aster._aster")]
pub struct ClientResponseStream {
    queue: Arc<crate::response_queue::ResponseReadAhead>,
}

impl ClientResponseStream {
    fn spawn(stream: ResponseStream) -> Self {
        Self {
            queue: Arc::new(crate::response_queue::ResponseReadAhead::spawn(stream)),
        }
    }
}

#[pymethods]
impl ClientResponseStream {
    /// Up to `max` response payloads, capped at 64 items and an 8 MiB batch
    /// threshold (one legal frame may cross it); `[]` at the OK trailer.
    /// A non-OK trailer raises after the payloads that preceded it were returned.
    #[pyo3(signature = (max=64))]
    fn next_batch<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Bound<'py, PyAny>> {
        let queue = self.queue.clone();
        future_into_py(py, async move {
            queue
                .next_batch(max)
                .await
                .map(BytesList)
                .map_err(client_err)
        })
    }

    /// Cancel the call (`FLAG_CANCEL` + reset). A no-op after the trailer.
    fn cancel(&self) {
        self.queue.cancel();
    }
}

impl Drop for ClientResponseStream {
    fn drop(&mut self) {
        self.queue.cancel();
    }
}

/// The request half of a client-stream or bidi call.
#[pyclass(frozen, module = "aster._aster")]
pub struct ClientRequestSink {
    sink: Arc<RequestSink>,
}

#[pymethods]
impl ClientRequestSink {
    /// Send one request payload (flushed at once).
    fn send<'py>(&self, py: Python<'py>, payload: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let sink = self.sink.clone();
        future_into_py(
            py,
            async move { sink.send(payload).await.map_err(client_err) },
        )
    }

    /// Send the last request (it also ends the request stream).
    fn send_last<'py>(&self, py: Python<'py>, payload: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let sink = self.sink.clone();
        future_into_py(py, async move {
            sink.send_last(payload).await.map_err(client_err)
        })
    }

    /// End the request stream. Idempotent.
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sink = self.sink.clone();
        future_into_py(py, async move { sink.finish().await.map_err(client_err) })
    }

    /// Cancel the call.
    fn cancel<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sink = self.sink.clone();
        future_into_py(py, async move {
            sink.cancel().await;
            Ok(())
        })
    }
}

/// The single response of an incremental client-streaming call.
#[pyclass(frozen, module = "aster._aster")]
pub struct ClientResponseFuture {
    fut: Arc<tokio::sync::Mutex<Option<ResponseFuture>>>,
}

#[pymethods]
impl ClientResponseFuture {
    /// Await the response payload (once).
    fn result<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let fut = self.fut.clone();
        future_into_py(py, async move {
            let taken = fut.lock().await.take();
            match taken {
                Some(f) => f.await.map(PyBytesResult).map_err(client_err),
                None => Err(PyRuntimeError::new_err("response already taken")),
            }
        })
    }
}

/// Validate a `DynamicService` JSON registration (design §1.5) and return
/// the contract id Rust derives from it.
#[pyfunction]
fn dynamic_service_contract_id(json: &str) -> PyResult<String> {
    DynamicService::from_json(json)
        .and_then(|svc| svc.contract_id())
        .map_err(aster_err_to_py)
}

pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    crate::http::register(m)?;
    m.add_function(wrap_pyfunction!(dynamic_service_contract_id, m)?)?;
    m.add_class::<DispatchServer>()?;
    m.add_class::<ServerCall>()?;
    m.add_class::<RpcClient>()?;
    m.add_class::<ClientResponseStream>()?;
    m.add_class::<ClientRequestSink>()?;
    m.add_class::<ClientResponseFuture>()?;
    Ok(())
}
