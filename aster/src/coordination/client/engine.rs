//! The redirecting call engine (§5.4, S4-E).
//!
//! One place decides where a request goes and what an answer *is*: every
//! typed method on the public clients funnels through here, so redirect
//! handling, the loop cap, and the ambiguity classification cannot drift
//! between nineteen methods.
//!
//! ## Routing
//!
//! The engine holds the set of known member NodeIds — seeded from validated
//! tickets, grown by authenticated redirect hints — and one live connection.
//! A redirect is followed only to an authenticated destination: the hint names
//! a NodeId, the dial authenticates it on the Iroh handshake, and no follower
//! ever proxies a caller's request. Redirect hops are capped at the number of
//! known members plus two (§5.4); past the cap the call reports unavailable
//! rather than orbiting a stale hint loop.
//!
//! ## Ambiguity (§S4-E)
//!
//! For a mutation, a redirect, timeout, reset, or malformed reply is
//! *ambiguous*: the request may or may not have reached the log, so the only
//! legal continuation is to resend the **exact** journaled bytes. This module
//! reports those outcomes as [`CallFailure::Ambiguous`] and never invents a
//! success or a refusal for them. A typed reply is authoritative; a non-OK
//! status that is not a redirect is a *status denial* — non-terminal, unknown
//! reason, the sequence still open.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::coordination::ids::CoordinationGroupId;
use crate::coordination::service::{STORE_SERVICE, STORE_VERSION};
use crate::coordination::store::wire::DETAIL_LEADER_NODE_ID;
use crate::error::Error;
use crate::id::NodeId;
use crate::node::Node;
use crate::rpc::{RpcConnection, StreamHeader};

/// How long one attempt may wait for the peer before it is ambiguous.
const ATTEMPT_DEADLINE: Duration = Duration::from_secs(20);

/// What one routed call produced.
#[derive(Debug)]
pub(crate) enum CallOutcome {
    /// A payload arrived with an OK trailer: the typed reply's bytes.
    Reply(Vec<u8>),
    /// The service answered a non-OK, non-redirect status. For a mutation this
    /// is non-terminal with an unknown reason: nothing committed, the sequence
    /// stays open, and the caller decides whether to retry or surface it.
    Denied { code: i32, message: String },
}

/// Why a routed call produced nothing usable.
#[derive(Debug)]
pub(crate) enum CallFailure {
    /// Transport failure, timeout, or reset at an unknown point: ambiguous.
    /// The only legal continuation for a mutation is resending the exact
    /// journaled bytes.
    Ambiguous(String),
    /// Every candidate was tried up to the redirect cap and none would serve.
    Unavailable(String),
}

pub(crate) struct Engine {
    node: Node,
    pub(crate) group: CoordinationGroupId,
    /// Known member NodeIds: seeds plus authenticated redirect hints.
    members: Mutex<BTreeSet<[u8; 32]>>,
    /// The connection the last successful exchange used, and who it is to.
    current: Mutex<Option<([u8; 32], RpcConnection)>>,
}

impl Engine {
    pub(crate) fn new(node: Node, group: CoordinationGroupId, seeds: BTreeSet<[u8; 32]>) -> Self {
        Self {
            node,
            group,
            members: Mutex::new(seeds),
            current: Mutex::new(None),
        }
    }

    pub(crate) fn local_id(&self) -> crate::Result<[u8; 32]> {
        let hex = self.node.id().as_str().to_string();
        crate::coordination::ids::CoordinationNodeId::parse_hex(&hex)
            .map(|id| *id.as_bytes())
            .map_err(|_| {
                Error::InvalidArgument(
                    "the local endpoint identity is not a 32-byte coordination id".into(),
                )
            })
    }

    fn header(method: &str) -> StreamHeader {
        StreamHeader {
            service: STORE_SERVICE.to_string(),
            method: method.to_string(),
            version: STORE_VERSION,
            ..Default::default()
        }
    }

    async fn candidates(&self) -> Vec<[u8; 32]> {
        self.members.lock().await.iter().copied().collect()
    }

    async fn connect(&self, member: [u8; 32]) -> Result<RpcConnection, String> {
        let id = NodeId::from_hex(hex::encode(member));
        match tokio::time::timeout(ATTEMPT_DEADLINE, self.node.rpc_connect(&id)).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(err)) => Err(format!("dialing {}: {err}", id.as_str())),
            Err(_) => Err(format!("dialing {}: no answer", id.as_str())),
        }
    }

    /// Route one unary request: dial, follow authenticated redirects up to the
    /// cap, and classify what came back.
    pub(crate) async fn unary(
        &self,
        method: &str,
        request: Vec<u8>,
    ) -> Result<CallOutcome, CallFailure> {
        // §5.4: redirects are capped at known members plus two.
        let cap = self.candidates().await.len() + 2;
        let mut hops = 0usize;
        let mut tried: BTreeSet<[u8; 32]> = BTreeSet::new();
        // Start from the connection that last worked, then walk candidates.
        let mut target: Option<[u8; 32]> = self.current.lock().await.as_ref().map(|(id, _)| *id);
        let mut last_ambiguity: Option<String> = None;

        loop {
            if hops >= cap {
                return match last_ambiguity {
                    Some(detail) => Err(CallFailure::Ambiguous(detail)),
                    None => Err(CallFailure::Unavailable(format!(
                        "no member served {method} within {cap} redirect hops"
                    ))),
                };
            }
            hops += 1;

            let member = match target.take() {
                Some(member) => member,
                None => {
                    // The next candidate nobody has tried this call.
                    match self
                        .candidates()
                        .await
                        .into_iter()
                        .find(|m| !tried.contains(m))
                    {
                        Some(member) => member,
                        None => {
                            return match last_ambiguity {
                                Some(detail) => Err(CallFailure::Ambiguous(detail)),
                                None => Err(CallFailure::Unavailable(
                                    "every known member refused or was unreachable".into(),
                                )),
                            }
                        }
                    }
                }
            };
            tried.insert(member);

            let conn = {
                let held = self.current.lock().await;
                match held.as_ref() {
                    Some((id, conn)) if *id == member => Some(conn.clone()),
                    _ => None,
                }
            };
            let conn = match conn {
                Some(conn) => conn,
                None => match self.connect(member).await {
                    Ok(conn) => conn,
                    Err(detail) => {
                        // Unreachable is not ambiguous: nothing was sent.
                        // Try the next candidate.
                        let _ = detail;
                        continue;
                    }
                },
            };

            let outcome = tokio::time::timeout(
                ATTEMPT_DEADLINE,
                conn.unary(&Self::header(method), request.clone()),
            )
            .await;

            match outcome {
                Ok(Ok(payload)) => {
                    *self.current.lock().await = Some((member, conn));
                    return Ok(CallOutcome::Reply(payload));
                }
                Ok(Err(Error::Rpc {
                    code,
                    message,
                    details,
                })) => {
                    // A follower's authenticated redirect (§5.4): learn the
                    // hint and follow it. UNAVAILABLE without a hint is a
                    // leaderless or quorumless answer — try elsewhere.
                    if code == 14 {
                        if let Some((_, hint)) =
                            details.iter().find(|(key, _)| key == DETAIL_LEADER_NODE_ID)
                        {
                            if let Ok(decoded) = hex::decode(hint) {
                                if let Ok(id) = <[u8; 32]>::try_from(decoded.as_slice()) {
                                    self.members.lock().await.insert(id);
                                    // The hint is staleable: following it is
                                    // still bounded by the cap.
                                    target = Some(id);
                                }
                            }
                        }
                        continue;
                    }
                    // `FailedPrecondition` from a node whose runtime has not
                    // attached: transient, not this call's answer — move on.
                    if code == 9 && message.contains("not yet serving") {
                        continue;
                    }
                    return Ok(CallOutcome::Denied { code, message });
                }
                Ok(Err(other)) => {
                    // Transport-level failure after the send: ambiguous.
                    *self.current.lock().await = None;
                    last_ambiguity = Some(other.to_string());
                    continue;
                }
                Err(_) => {
                    *self.current.lock().await = None;
                    last_ambiguity = Some(format!("{method}: no answer in {ATTEMPT_DEADLINE:?}"));
                    continue;
                }
            }
        }
    }

    /// Route one server-streaming request and collect its frames.
    pub(crate) async fn stream(
        &self,
        method: &str,
        request: Vec<u8>,
    ) -> Result<Result<Vec<Vec<u8>>, (i32, String)>, CallFailure> {
        // Streams are reads: an ambiguous attempt is safely re-issued, so the
        // routing loop can reuse the unary walk by treating the stream open
        // like a call. Kept separate because the reply is many frames.
        let cap = self.candidates().await.len() + 2;
        let mut hops = 0usize;
        let mut tried: BTreeSet<[u8; 32]> = BTreeSet::new();
        let mut target: Option<[u8; 32]> = self.current.lock().await.as_ref().map(|(id, _)| *id);

        loop {
            if hops >= cap {
                return Err(CallFailure::Unavailable(format!(
                    "no member served {method} within {cap} redirect hops"
                )));
            }
            hops += 1;
            let member = match target.take() {
                Some(member) => member,
                None => match self
                    .candidates()
                    .await
                    .into_iter()
                    .find(|m| !tried.contains(m))
                {
                    Some(member) => member,
                    None => {
                        return Err(CallFailure::Unavailable(
                            "every known member refused or was unreachable".into(),
                        ))
                    }
                },
            };
            tried.insert(member);
            let conn = match self.connect(member).await {
                Ok(conn) => conn,
                Err(_) => continue,
            };
            let opened = tokio::time::timeout(
                ATTEMPT_DEADLINE,
                conn.server_stream(&Self::header(method), request.clone()),
            )
            .await;
            let mut response = match opened {
                Ok(Ok(response)) => response,
                Ok(Err(Error::Rpc { code, details, .. })) if code == 14 => {
                    if let Some((_, hint)) =
                        details.iter().find(|(key, _)| key == DETAIL_LEADER_NODE_ID)
                    {
                        if let Ok(decoded) = hex::decode(hint) {
                            if let Ok(id) = <[u8; 32]>::try_from(decoded.as_slice()) {
                                self.members.lock().await.insert(id);
                                target = Some(id);
                            }
                        }
                    }
                    continue;
                }
                Ok(Err(Error::Rpc { code, message, .. })) => return Ok(Err((code, message))),
                Ok(Err(_)) | Err(_) => continue,
            };
            match tokio::time::timeout(ATTEMPT_DEADLINE, response.collect()).await {
                Ok(Ok(frames)) => {
                    *self.current.lock().await = Some((member, conn));
                    return Ok(Ok(frames));
                }
                Ok(Err(Error::Rpc { code, message, .. })) => return Ok(Err((code, message))),
                Ok(Err(_)) | Err(_) => continue,
            }
        }
    }
}

/// A shared handle: both public clients hold the same engine.
pub(crate) type SharedEngine = Arc<Engine>;
