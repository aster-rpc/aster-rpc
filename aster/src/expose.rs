//! Expose local HTTP services over Aster (`feature = "expose"`).
//!
//! Bridges the high-level [`Node`](crate::Node) to the
//! [`aster-expose`](aster_expose) L7 terminate-and-relay layer, which otherwise
//! operates one level down on `CoreConnection`s. Register services on an
//! [`Expose`] handle, then either let the node own the accept loop
//! ([`Node::serve_expose`](crate::Node::serve_expose)) or route your own
//! connections into it ([`Expose::handle`]).
//!
//! ```no_run
//! use aster::expose::{Expose, AuthorizeFn, HttpHandler};
//! # async fn run(node: aster::Node, authorize: AuthorizeFn, handler: HttpHandler) -> aster::Result<()> {
//! const ALPN: &[u8] = b"aster-expose/demo";
//!
//! let expose = node.expose();
//! expose.expose_http("signaling", authorize, handler);
//!
//! // Dedicated host — let the node own the accept loop:
//! node.serve_expose(&expose, &[ALPN]).await?;
//!
//! // …or compose with RPC / a manual accept loop:
//! let (alpn, conn) = node.accept().await?;
//! if alpn == ALPN {
//!     expose.handle(&conn);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! See the full guide in `docs/aster-expose-getstarted.md`.

use std::net::SocketAddr;

use aster_transport_core::reactor::{create_reactor, ReactorFeeder, ReactorHandle};

use crate::error::Result;
use crate::net::Connection;

// Re-exports so consumers needn't depend on `aster-expose` (or `http`) directly.
pub use aster_expose::body::{RelayBody, RelayError, ResponseBody};
use aster_expose::control::request_route as core_request_route;
pub use aster_expose::control::{
    EdgeRouter, RoutePolicy, RouteProtocol, RouteRegistration, RouteSpec,
};
pub use aster_expose::relay::{
    tower_handler, AuthenticatedPeer, AuthorizeFn, HttpHandler, LocalHttpTarget, ServiceAcceptor,
    SocketAcceptor,
};
/// Boxed future alias used in [`AuthorizeFn`] / [`HttpHandler`] signatures.
pub use aster_transport_core::stream_io::BoxFut;
/// The verified peer identity + attached metadata handed to an [`AuthorizeFn`]
/// / [`RoutePolicy`]. Re-exported from the transport core.
pub use aster_transport_core::tunnel::PeerContext;
/// Re-exported so consumers can build requests/responses without adding `http`.
pub use http;

#[cfg(feature = "expose-edge")]
pub use aster_expose::edge::{serve_edge, EdgeConfig, EdgeTls, RelayHandler};

/// A node-wide exposure handle: a shared service registry plus a dedicated
/// reactor that dispatches inbound relay streams. Cheap to clone (both parts are
/// `Arc`-backed) — register services up front, then point connections at it.
///
/// Obtain one with [`Node::expose`](crate::Node::expose).
#[derive(Clone)]
pub struct Expose {
    node: aster_expose::node::ExposeNode,
    feeder: ReactorFeeder,
}

impl Default for Expose {
    fn default() -> Self {
        Self::new()
    }
}

impl Expose {
    /// Build a fresh exposure handle with its own reactor. Equivalent to
    /// [`Node::expose`](crate::Node::expose) — the handle is independent of any
    /// node (it only binds to connections when you [`handle`](Self::handle) them),
    /// so build it directly when you need it *before* a node exists (e.g. to pass
    /// to [`AsterServerBuilder::with_expose`](crate::rpc::AsterServerBuilder::with_expose)).
    ///
    /// Must be called within a Tokio runtime context (it spawns the reactor's
    /// drain task). A background task drains the reactor's event channel — expose
    /// connections carry only tunnel streams (dispatched internally), so the only
    /// events here are connection-closed notices, which we discard.
    pub fn new() -> Self {
        let node = aster_expose::node::ExposeNode::new();
        let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
        tokio::spawn(drain_reactor(handle));
        Self { node, feeder }
    }

    /// Expose a streaming HTTP `handler` under `service_id`, node-wide. The
    /// `authorize` policy runs once per `(connection, service)` — see
    /// [`AuthorizeFn`].
    pub fn expose_http(&self, service_id: &str, authorize: AuthorizeFn, handler: HttpHandler) {
        self.node.expose_http(service_id, authorize, handler);
    }

    /// Expose a raw L4 splice to a loopback `addr` under `service_id`, node-wide.
    pub fn expose_socket(&self, service_id: &str, authorize: AuthorizeFn, addr: SocketAddr) {
        self.node.expose_socket(service_id, authorize, addr);
    }

    /// Expose either target shape under `service_id`, node-wide.
    pub fn expose(&self, service_id: &str, authorize: AuthorizeFn, target: LocalHttpTarget) {
        self.node.expose(service_id, authorize, target);
    }

    /// Accept route registrations (act as an edge) node-wide: peers
    /// [`request_route`] into `router` subject to
    /// `policy`. To also serve public browser traffic, run `serve_edge` on the
    /// same `router` (requires the `expose-edge` feature).
    pub fn serve_routes(&self, router: EdgeRouter, policy: RoutePolicy) {
        self.node.serve_routes(router, policy);
    }

    /// Stop exposing `service_id` node-wide.
    pub fn unexpose(&self, service_id: &str) {
        self.node.unexpose(service_id);
    }

    /// Attach this registry to `conn` and feed it to the exposure reactor, so
    /// inbound relay streams reach the exposed services. Call once per
    /// connection, before opening any stream on it.
    ///
    /// This is the composable primitive: route your own accept loop's (or
    /// RPC's non-RPC) connections here by ALPN. For a node that serves *only*
    /// expose, [`Node::serve_expose`](crate::Node::serve_expose) wraps the
    /// accept loop for you.
    ///
    /// The same `conn` remains usable for the consumer side
    /// ([`relay_request`] / [`request_route`]) — feeding clones the underlying
    /// connection rather than consuming it.
    pub fn handle(&self, conn: &Connection) {
        let mut core = conn.core_clone();
        self.node.attach(&mut core);
        self.feeder.feed(core);
    }
}

/// Consumer side: relay one buffered HTTP request to `service_id` on `conn` and
/// collect the full response. For streaming bodies use
/// [`relay_request_streaming`]. The consumer needs no [`Expose`] handle — it
/// only opens an outbound relay stream.
pub async fn relay_request(
    conn: &Connection,
    service_id: &str,
    request: http::Request<Vec<u8>>,
) -> Result<http::Response<Vec<u8>>> {
    Ok(aster_expose::relay::relay_request(conn.core_ref(), service_id, request).await?)
}

/// Consumer side: relay an HTTP request whose body streams off the wire,
/// returning a response with a streaming body. One Aster stream per request.
pub async fn relay_request_streaming<B>(
    conn: &Connection,
    service_id: &str,
    request: http::Request<B>,
) -> Result<http::Response<RelayBody>>
where
    B: http_body::Body + Unpin + Send + 'static,
    B::Error: Into<RelayError>,
{
    Ok(aster_expose::relay::relay_request_streaming(conn.core_ref(), service_id, request).await?)
}

/// Consumer side: ask `conn`'s peer (an edge) to route `specs` to us, attaching
/// `metadata` for the edge's [`RoutePolicy`]. Hold the returned
/// [`RouteRegistration`] for the routes to stay live.
pub async fn request_route(
    conn: &Connection,
    specs: Vec<RouteSpec>,
    metadata: Vec<u8>,
) -> Result<RouteRegistration> {
    Ok(core_request_route(conn.core_ref(), specs, metadata).await?)
}

/// Drain a reactor's event channel forever so it never backs up. Ends when the
/// feeder (and thus all fed connections) are dropped.
async fn drain_reactor(mut handle: ReactorHandle) {
    while handle.next_event().await.is_some() {}
}
