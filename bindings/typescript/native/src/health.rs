//! `HealthServer` (design `OBSERVABILITY_HEALTH_DESIGN.md` §6, O7): the NAPI
//! class over `aster::ops`, serving `/healthz`, `/readyz` and `/metrics`
//! (OTel Prometheus text) from Rust. Replaces the TypeScript `health.ts`.

use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi::Status;
use napi_derive::napi;

use aster::ops::{HealthConfig, HealthServer as RsHealthServer, HealthState, ReadinessCheck};

use crate::config::aster_err;
use crate::node::Node;

/// Where the listener binds.
#[napi(object)]
#[derive(Default)]
pub struct HealthOptions {
    /// Bind host. Default `127.0.0.1` (localhost only; pods need `0.0.0.0`).
    pub host: Option<String>,
    /// TCP port; `0` binds an ephemeral one (read it back with `addr`).
    pub port: Option<u32>,
}

/// A named readiness check (`HealthServer.check`). The server is ready only
/// while the application is ready and every check is `true`. Created failing.
#[napi]
pub struct ReadinessCheckHandle {
    inner: ReadinessCheck,
}

#[napi]
impl ReadinessCheckHandle {
    /// Set the check.
    #[napi]
    pub fn set(&self, ok: bool) {
        self.inner.set(ok);
    }

    /// The check's current value.
    #[napi(getter)]
    pub fn ok(&self) -> bool {
        self.inner.is_ok()
    }

    #[napi(getter)]
    pub fn name(&self) -> String {
        self.inner.name().to_string()
    }
}

/// The health / readiness / metrics HTTP listener. Start with
/// `HealthServer.start(node, options)`; `stop()` releases the port.
#[napi]
pub struct HealthServer {
    server: Arc<RsHealthServer>,
    state: HealthState,
    addr: String,
}

#[napi]
impl HealthServer {
    /// Bind and serve for `node`. Joins (or starts, from the environment) the
    /// observability pipeline and gives it the Prometheus pull reader, so
    /// `/metrics` is never empty.
    #[napi(ts_return_type = "Promise<HealthServer>")]
    pub fn start<'env>(
        env: &'env Env,
        node: &Node,
        options: Option<HealthOptions>,
    ) -> Result<PromiseRaw<'env, HealthServer>> {
        let options = options.unwrap_or_default();
        let port = match options.port {
            None => 0,
            Some(p) => u16::try_from(p).map_err(|_| {
                Error::new(
                    Status::InvalidArg,
                    format!("port must be 0..=65535, got {p}"),
                )
            })?,
        };
        let config = HealthConfig::new(
            options.host.unwrap_or_else(|| "127.0.0.1".to_string()),
            port,
        );
        let node = node.aster().clone();
        env.spawn_future(async move {
            let server = RsHealthServer::start(&node, config)
                .await
                .map_err(aster_err)?;
            Ok(HealthServer {
                state: server.state(),
                addr: server.addr().to_string(),
                server: Arc::new(server),
            })
        })
    }

    /// The bound `host:port`.
    #[napi(getter)]
    pub fn addr(&self) -> String {
        self.addr.clone()
    }

    /// The bound port.
    #[napi(getter)]
    pub fn port(&self) -> u32 {
        u32::from(self.server.addr().port())
    }

    /// Liveness (`/healthz`).
    #[napi]
    pub fn set_healthy(&self, healthy: bool) {
        self.server.set_healthy(healthy);
    }

    /// Readiness of the application itself (`/readyz` also needs every
    /// check true).
    #[napi]
    pub fn set_ready(&self, ready: bool) {
        self.server.set_ready(ready);
    }

    /// Services reported by `/readyz`.
    #[napi]
    pub fn set_services(&self, count: u32) {
        self.state.set_services(count);
    }

    /// Whether the application and every check are ready.
    #[napi]
    pub fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    #[napi]
    pub fn is_healthy(&self) -> bool {
        self.state.is_healthy()
    }

    /// A named readiness check (created failing until it says `true`).
    #[napi]
    pub fn check(&self, name: String) -> ReadinessCheckHandle {
        ReadinessCheckHandle {
            inner: self.server.check(&name),
        }
    }

    /// Drop a named check. `false` when it did not exist.
    #[napi]
    pub fn remove_check(&self, name: String) -> bool {
        self.state.remove_check(&name)
    }

    /// Stop listening and wait until the port is released. Idempotent.
    #[napi]
    pub async fn stop(&self) -> Result<()> {
        self.server.set_ready(false);
        self.server.shutdown().await;
        Ok(())
    }
}
