//! Gate 0 admission over the upcall channel.
//!
//! Gate 0 already is event + respond: [`Admission`] yields
//! [`ConnectRequest`](crate::ConnectRequest)s /
//! [`HandshakeRequest`](crate::HandshakeRequest)s that the PyO3 / NAPI / C
//! ABI layers surface as their own iterators (P3, T3, F1). [`serve_admission`]
//! routes them through [`Upcalls`] instead, so a binding that already drains
//! one upcall queue can answer admission on it too (kinds
//! [`UpcallKind::AdmissionConnect`] / [`UpcallKind::AdmissionHandshake`]) --
//! one queue, one respond function. The existing iterators stay; this is an
//! alternative, not a replacement.
//!
//! Two timeouts apply: the node's hook timeout (`AsterConfig`, with its own
//! failure modes) and the upcall policy's. Keep the upcall timeout shorter,
//! so the upcall's failure mode decides rather than the node dropping the
//! request.

use tokio::task::JoinHandle;

use super::codec::{self, HandshakeDecision};
use super::{UpcallKind, Upcalls};
use crate::admission::AnyAdmissionRequest;
use crate::Admission;

/// The close code and reason a fail-closed handshake upcall rejects with
/// (the same values the node's own hook fallback uses).
pub const UNAVAILABLE_CODE: u32 = 403;
pub const UNAVAILABLE_REASON: &[u8] = b"hook unavailable";

/// Answer every admission request on `admission` by upcall until the node
/// closes the hook channels. Each request runs on its own task, so a slow
/// answer does not hold up the next request.
///
/// # Panics
/// Outside a tokio runtime.
pub fn serve_admission(mut admission: Admission, upcalls: Upcalls) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = admission.next_any().await {
            let upcalls = upcalls.clone();
            match req {
                AnyAdmissionRequest::Connect(req) => {
                    tokio::spawn(async move {
                        let answer = upcalls
                            .call(
                                UpcallKind::AdmissionConnect,
                                codec::encode_admission_connect_request(
                                    req.peer.as_str(),
                                    &req.alpn,
                                ),
                            )
                            .await;
                        if upcalls.decide(
                            UpcallKind::AdmissionConnect,
                            answer,
                            codec::decode_decision,
                        ) {
                            req.accept();
                        } else {
                            req.reject();
                        }
                    });
                }
                AnyAdmissionRequest::Handshake(req) => {
                    tokio::spawn(async move {
                        let answer = upcalls
                            .call(
                                UpcallKind::AdmissionHandshake,
                                codec::encode_admission_handshake_request(
                                    req.peer.as_str(),
                                    &req.alpn,
                                    req.is_alive,
                                ),
                            )
                            .await;
                        match handshake_decision(&upcalls, answer) {
                            HandshakeDecision::Accept => req.accept(),
                            HandshakeDecision::Reject { code, reason } => req.reject(code, reason),
                        }
                    });
                }
            }
        }
    })
}

/// Decode a handshake answer, or apply the kind's failure mode.
pub fn handshake_decision(
    upcalls: &Upcalls,
    answer: Result<Vec<u8>, super::UpcallError>,
) -> HandshakeDecision {
    let mut decided = None;
    let accept = upcalls.decide(UpcallKind::AdmissionHandshake, answer, |bytes| {
        let d = codec::decode_admission_handshake_response(bytes)?;
        let accept = d == HandshakeDecision::Accept;
        decided = Some(d);
        Ok(accept)
    });
    match decided {
        Some(d) => d,
        None if accept => HandshakeDecision::Accept,
        None => HandshakeDecision::Reject {
            code: UNAVAILABLE_CODE,
            reason: UNAVAILABLE_REASON.to_vec(),
        },
    }
}
