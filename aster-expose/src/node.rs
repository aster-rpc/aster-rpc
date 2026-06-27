//! Node-level facade — expose services and accept route registrations across
//! *every* connection a node handles, rather than wiring each connection by
//! hand (`expose_http_on_connection` / `serve_routes_on_connection`).
//!
//! [`ExposeNode`] owns one shared [`LocalTargetRegistry`]; register services
//! and the route-control service on it once, then [`attach`](ExposeNode::attach)
//! each freshly-accepted (or freshly-dialed) connection before feeding it to the
//! reactor. Because the registry is shared, one registration is visible on all
//! of them. Salvo-free — the edge data plane (`crate::edge`) is separate.

use std::net::SocketAddr;
use std::sync::Arc;

use aster_transport_core::tunnel::{LocalTargetRegistry, LocalTunnelAcceptor};
use aster_transport_core::CoreConnection;

use crate::control::{ControlAcceptor, EdgeRouter, RoutePolicy, CONTROL_SERVICE_ID};
use crate::relay::{AuthorizeFn, HttpHandler, LocalHttpTarget, ServiceAcceptor, SocketAcceptor};

/// A node-wide exposure registry. Cheap to clone (shares one `Arc`); register
/// services up front, then `attach` it to each connection the node owns.
#[derive(Clone, Default)]
pub struct ExposeNode {
    targets: Arc<LocalTargetRegistry>,
}

impl ExposeNode {
    pub fn new() -> Self {
        Self::default()
    }

    /// Expose a streaming HTTP `handler` under `service_id`, node-wide.
    pub fn expose_http(&self, service_id: &str, authorize: AuthorizeFn, handler: HttpHandler) {
        self.targets.register(
            service_id,
            Arc::new(ServiceAcceptor::new(authorize, handler)),
        );
    }

    /// Expose a raw L4 splice to a loopback `addr` under `service_id`, node-wide.
    pub fn expose_socket(&self, service_id: &str, authorize: AuthorizeFn, addr: SocketAddr) {
        self.targets
            .register(service_id, Arc::new(SocketAcceptor::new(authorize, addr)));
    }

    /// Expose either target shape under `service_id`, node-wide (mirrors
    /// `relay::expose_local_http` but on the shared registry).
    pub fn expose(&self, service_id: &str, authorize: AuthorizeFn, target: LocalHttpTarget) {
        let acceptor: Arc<dyn LocalTunnelAcceptor> = match target {
            LocalHttpTarget::Socket(addr) => Arc::new(SocketAcceptor::new(authorize, addr)),
            LocalHttpTarget::Service(handler) => Arc::new(ServiceAcceptor::new(authorize, handler)),
        };
        self.targets.register(service_id, acceptor);
    }

    /// Accept route registrations (act as an edge) node-wide: peers `request_route`
    /// into `router` subject to `policy`. Reciprocal of exposing a service.
    pub fn serve_routes(&self, router: EdgeRouter, policy: RoutePolicy) {
        self.targets.register(
            CONTROL_SERVICE_ID,
            Arc::new(ControlAcceptor::new(router, policy)),
        );
    }

    /// Stop exposing `service_id` node-wide.
    pub fn unexpose(&self, service_id: &str) {
        self.targets.unregister(service_id);
    }

    /// Point `conn` at this node's shared registry. Call once per connection,
    /// **before** it is fed to the reactor (i.e. before any stream is accepted
    /// on it), so every service registered here is reachable on it.
    pub fn attach(&self, conn: &mut CoreConnection) {
        conn.set_local_targets(self.targets.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_registry() {
        // Two clones backed by the same Arc registry — registering through one
        // is visible to the other (proven structurally; the relay path is in
        // tests/node_integration.rs).
        let a = ExposeNode::new();
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.targets, &b.targets));
    }
}
