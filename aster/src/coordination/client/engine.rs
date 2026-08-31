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

/// The **base** absolute budget for one stream attempt. The per-frame idle
/// timeout alone is not a deadline — a peer dripping one small frame every
/// nineteen seconds would hold the future forever while never going idle —
/// so every stream carries an absolute budget: this base, plus an allowance
/// derived from the **declared work** once the consumer knows it
/// ([`BoundedFrames::allow_bytes`], [`BoundedFrames::allow_items`]). The
/// protocol declares no minimum throughput, so the allowance encodes one
/// explicitly rather than asserting any fixed wall time is universally
/// healthy: a full §11.4 export earns its transfer time, a drip never earns
/// more than its declared bytes.
const STREAM_BASE_DEADLINE: Duration = Duration::from_secs(60);

/// The minimum sustained throughput a declared export body earns budget at:
/// one mebibyte per second, so the largest legal body (512 MiB) earns
/// ~8.5 minutes on top of the base.
const MIN_STREAM_THROUGHPUT_BYTES_PER_SEC: u64 = 1024 * 1024;

/// The per-item allowance a declared audit limit earns: 25 ms per row, so
/// the §5.2 cap of 10,000 rows earns 250 s on top of the base.
const STREAM_ITEM_ALLOWANCE: Duration = Duration::from_millis(25);

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
                                    // A hint that names **the answerer itself**
                                    // is not followed. Everything else is,
                                    // including a hint naming a member this
                                    // call has already asked: that one may have
                                    // become true since — a group mid-election
                                    // hands out the old leader's id until the
                                    // new one is known, and following it again
                                    // a moment later is how a read survives a
                                    // leadership change.
                                    //
                                    // A self-hint cannot become true that way.
                                    // The answerer has just said two things at
                                    // once — "I lead" and "I cannot serve you"
                                    // — and asking it again gets the same pair.
                                    // A voter removed by a committed membership
                                    // change keeps naming itself leader until
                                    // it steps down, and following that
                                    // consumed the whole redirect budget while
                                    // the real leader, a candidate this walk had
                                    // not yet tried, was never reached. The cap
                                    // bounds termination, not fairness.
                                    if id != member {
                                        target = Some(id);
                                    }
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

    /// Open a server stream and hand back a **deadline-bounded** frame
    /// source once a member accepts it.
    ///
    /// The same routing walk as [`unary`](Self::unary) up to the open; the
    /// frames are not collected, so the caller consumes them incrementally
    /// and a mid-stream failure surfaces to the caller instead of being
    /// retried here. A Store stream is a read: re-issuing the whole call is
    /// always safe, so the caller loses nothing but the walk. Every frame
    /// wait is bounded by [`BoundedFrames`] — a peer that accepts the stream
    /// and then sends neither data nor trailer parks nothing.
    pub(crate) async fn open_stream(
        &self,
        method: &str,
        request: Vec<u8>,
    ) -> Result<Result<BoundedFrames, (i32, String)>, CallFailure> {
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
            match opened {
                Ok(Ok(response)) => {
                    *self.current.lock().await = Some((member, conn));
                    return Ok(Ok(BoundedFrames::new(response)));
                }
                Ok(Err(Error::Rpc {
                    code: 14, details, ..
                })) => {
                    if let Some((_, hint)) =
                        details.iter().find(|(key, _)| key == DETAIL_LEADER_NODE_ID)
                    {
                        if let Ok(decoded) = hex::decode(hint) {
                            if let Ok(id) = <[u8; 32]>::try_from(decoded.as_slice()) {
                                self.members.lock().await.insert(id);
                                // Not the answerer itself, for the reason
                                // `unary` gives: a member that says "I lead"
                                // and "I cannot serve you" in one answer gives
                                // the same pair when asked again, and one such
                                // hint can spend the whole budget.
                                if id != member {
                                    target = Some(id);
                                }
                            }
                        }
                    }
                    continue;
                }
                Ok(Err(Error::Rpc { code, message, .. })) => return Ok(Err((code, message))),
                Ok(Err(_)) | Err(_) => continue,
            }
        }
    }
}

/// A server stream whose every frame wait is bounded by the attempt
/// deadline.
///
/// The open is deadline-bounded by the routing walk, but a stream is frames:
/// a peer that accepts and then parks — neither data nor trailer — would
/// otherwise hang the public future forever, since the transport's own
/// `next()` has no timeout. Each `next` here waits at most
/// [`ATTEMPT_DEADLINE`] and then reports the stall as a transport error; the
/// caller re-issues the read like any other ambiguous stream failure.
pub(crate) struct BoundedFrames {
    inner: crate::rpc::ResponseStream,
    /// One absolute deadline for the whole attempt, fixed at the open.
    deadline: tokio::time::Instant,
}

impl BoundedFrames {
    fn new(inner: crate::rpc::ResponseStream) -> Self {
        Self {
            inner,
            deadline: tokio::time::Instant::now() + STREAM_BASE_DEADLINE,
        }
    }

    /// Extend the absolute budget for `bytes` of declared body, at the
    /// declared minimum throughput. Called once the manifest has passed
    /// validation, so the allowance is bounded by the §11.4 ceiling.
    pub(crate) fn allow_bytes(&mut self, bytes: u64) {
        let seconds = bytes.div_ceil(MIN_STREAM_THROUGHPUT_BYTES_PER_SEC);
        self.deadline += Duration::from_secs(seconds);
    }

    /// Extend the absolute budget for `items` declared rows. Bounded by the
    /// §5.2 audit cap, which the caller has already enforced.
    pub(crate) fn allow_items(&mut self, items: u64) {
        self.deadline += STREAM_ITEM_ALLOWANCE * (items as u32);
    }

    pub(crate) async fn next(&mut self) -> Option<crate::Result<Vec<u8>>> {
        // Each wait is the shorter of the idle bound and what remains of the
        // absolute budget — so a stalled peer is cut off within the idle
        // bound, and a dripping one within the absolute one.
        let remaining = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Some(Err(crate::error::Error::Transport(anyhow::anyhow!(
                "the stream did not complete within its absolute budget"
            ))));
        }
        match tokio::time::timeout(ATTEMPT_DEADLINE.min(remaining), self.inner.next()).await {
            Ok(frame) => frame,
            Err(_) => Some(Err(crate::error::Error::Transport(anyhow::anyhow!(
                "the stream produced no frame within its deadline"
            )))),
        }
    }
}

/// A shared handle: both public clients hold the same engine.
pub(crate) type SharedEngine = Arc<Engine>;
