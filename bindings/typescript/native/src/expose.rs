//! Expose — local services over Aster (`aster::expose`), the slice that
//! needs no HTTP handler: L4 socket targets whose per-`(connection,
//! service)` authorization is a JavaScript function (upcall kind 48, R6), a
//! node-owned accept loop, and the consumer's raw relay stream
//! (`IrohConnection.openExposeStream`). A JavaScript HTTP *handler* (kind
//! 49) and the buffered HTTP relay against it wait for R9.

use std::sync::Mutex;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::binding_support::upcall::expose::foreign_authorize_fn;

use crate::error::to_napi_err;
use crate::net::{IrohBiStream, IrohConnection, IrohRecvStream, IrohSendStream};
use crate::node::Node;
use crate::upcalls::Upcalls;

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

    /// Stop exposing `serviceId`.
    #[napi]
    pub fn unexpose(&self, service_id: String) {
        self.inner.unexpose(&service_id);
    }
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
}
