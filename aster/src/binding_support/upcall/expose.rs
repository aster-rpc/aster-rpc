//! expose's per-`(connection, service)` policy implemented by upcalling
//! (`expose`). The bytes-level `HttpHandler` (kind
//! [`UpcallKind::ExposeHttp`]) is R9's; the code is reserved here.

use std::sync::Arc;

use super::codec;
use super::{UpcallKind, Upcalls};
use crate::expose::{AuthorizeFn, PeerContext};

/// Build an [`AuthorizeFn`] for `service_id` that asks foreign code (kind
/// [`UpcallKind::ExposeAuthorize`]): a `true` decision admits, `false`
/// refuses, and no answer applies the kind's failure mode (fail-closed by
/// default).
pub fn foreign_authorize_fn(upcalls: Upcalls, service_id: &str) -> AuthorizeFn {
    let service_id: Arc<str> = service_id.into();
    Arc::new(move |peer: PeerContext| {
        let upcalls = upcalls.clone();
        let service_id = service_id.clone();
        Box::pin(async move {
            let answer = upcalls
                .call(
                    UpcallKind::ExposeAuthorize,
                    codec::encode_expose_authorize_request(
                        &service_id,
                        &peer.peer_id,
                        &peer.metadata,
                    ),
                )
                .await;
            if upcalls.decide(UpcallKind::ExposeAuthorize, answer, codec::decode_decision) {
                Ok(())
            } else {
                anyhow::bail!(
                    "expose: peer {} not authorized for {service_id}",
                    peer.peer_id
                )
            }
        })
    })
}
