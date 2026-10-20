//! The server-side per-call context handed to service handlers.
//!
//! [`CallContext`] carries who is calling, what they called, the metadata they
//! sent, when the call must be done by, and whether it is over. Handlers get
//! it two ways (`ffi_spec/handler-context-design.md`):
//!
//! - **Explicit parameter.** An `#[aster::service]` method may declare a
//!   `ctx: &CallContext` (or owned `ctx: CallContext`) parameter anywhere
//!   after `&self`; the generated dispatcher passes the call's context there.
//!   The parameter is framework injection, not a wire field: it never reaches
//!   the contract, so adding or removing it leaves the contract id unchanged.
//! - **Task-local.** The dispatcher sets the context on a tokio task-local
//!   around every native handler future, so code deeper in the call reads it
//!   with [`CallContext::current`] / [`CallContext::try_current`] without
//!   threading it through every signature. A task the handler
//!   `tokio::spawn`s does **not** inherit the task-local; clone the context
//!   into the task (it is one `Arc`) and read it from there.
//!
//! Hand-written [`ServiceDispatch`](super::ServiceDispatch) impls get the same
//! context from [`Call::context`](super::Call::context).

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster_transport_core::reactor::CancelSignal;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::codec::StreamHeader;
use super::identity::ForwardedIdentity;
use super::node_config::{AttrValue, NodeProperties};
use super::server::PeerProvenance;

tokio::task_local! {
    static CURRENT: CallContext;
}

/// How often [`CallContext::cancelled`] re-checks the cancel flag on a
/// transport that sets the flag without a wake-up signal.
const FLAG_POLL: Duration = Duration::from_millis(25);

/// The server-side context of one RPC call. Cheap to clone (one `Arc`).
///
/// Built by the dispatcher after routing, authentication and Gate 3, so every
/// field reflects what the gates decided: [`principal`](Self::principal) is
/// the [`Authenticator`](super::Authenticator)'s resolved principal and
/// [`attributes`](Self::attributes) the merged set Gate 3 checked.
///
/// For a call an edge forwarded on behalf of a user (the
/// `aster-on-behalf-of` entry, see [`identity`](super::identity)),
/// [`principal`](Self::principal) is the user (`iss|sub`),
/// [`actor`](Self::actor) and [`peer`](Self::peer) are the edge, and
/// [`attributes`](Self::attributes) carry the token's roles.
///
/// Fields are private and read through accessors, so later additions are not
/// breaking.
#[derive(Clone)]
pub struct CallContext {
    inner: Arc<Inner>,
}

struct Inner {
    header: StreamHeader,
    /// The caller id the authenticator resolved, else the transport's.
    principal: String,
    /// The transport's caller id, when an authenticator replaced it.
    transport_peer: Option<String>,
    provenance: PeerProvenance,
    attributes: HashMap<String, String>,
    session_id: Option<String>,
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
    /// The transport's wake-up for a peer cancel, when it has one.
    signal: Option<Arc<CancelSignal>>,
    /// Fired by the dispatcher when it ends the call itself (deadline, or a
    /// peer cancel it observed).
    ended: Notify,
    /// The verified on-behalf-of identity, for a forwarded call.
    forwarded: Option<Arc<ForwardedIdentity>>,
    /// This node's NodeConfig properties when the call was routed.
    node_properties: Arc<NodeProperties>,
}

pub(crate) struct CallContextParts {
    pub header: StreamHeader,
    pub principal: String,
    pub transport_peer: Option<String>,
    pub provenance: PeerProvenance,
    pub attributes: HashMap<String, String>,
    pub session_id: Option<String>,
    pub deadline: Option<Instant>,
    pub cancelled: Arc<AtomicBool>,
    pub signal: Option<Arc<CancelSignal>>,
    pub forwarded: Option<Arc<ForwardedIdentity>>,
    pub node_properties: Arc<NodeProperties>,
}

impl CallContext {
    pub(crate) fn new(p: CallContextParts) -> Self {
        let transport_peer = p.transport_peer.filter(|t| *t != p.principal);
        Self {
            inner: Arc::new(Inner {
                header: p.header,
                principal: p.principal,
                transport_peer,
                provenance: p.provenance,
                attributes: p.attributes,
                session_id: p.session_id,
                deadline: p.deadline,
                cancelled: p.cancelled,
                signal: p.signal,
                ended: Notify::new(),
                forwarded: p.forwarded,
                node_properties: p.node_properties,
            }),
        }
    }

    /// The context of the call this task is handling. Set by the dispatcher
    /// around every native handler future.
    ///
    /// # Panics
    ///
    /// Outside a handler's task, including in a task the handler spawned (a
    /// tokio task-local is not inherited). Use
    /// [`try_current`](Self::try_current) where that can happen.
    pub fn current() -> CallContext {
        Self::try_current().expect(
            "CallContext::current() called outside an RPC handler task \
             (spawned tasks do not inherit it; clone the context into them)",
        )
    }

    /// The context of the call this task is handling, or `None` outside a
    /// handler's task (including tasks the handler spawned).
    pub fn try_current() -> Option<CallContext> {
        CURRENT.try_with(Clone::clone).ok()
    }

    /// Run `fut` with `self` as [`current`](Self::current).
    pub(crate) async fn scope<F: std::future::Future>(self, fut: F) -> F::Output {
        CURRENT.scope(self, fut).await
    }

    /// The transport's caller id (Iroh: the peer's endpoint id, hex; HTTP:
    /// whatever the front end supplied). See [`provenance`](Self::provenance)
    /// for how far to trust it.
    pub fn peer(&self) -> &str {
        self.inner
            .transport_peer
            .as_deref()
            .unwrap_or(&self.inner.principal)
    }

    /// The verified caller: for a forwarded call the user, `iss|sub` of its
    /// on-behalf-of token; else the [`Authenticator`](super::Authenticator)'s
    /// `AuthOutcome.principal` when it resolved one; else [`peer`](Self::peer).
    pub fn principal(&self) -> &str {
        &self.inner.principal
    }

    /// The edge that forwarded this call on behalf of
    /// [`principal`](Self::principal) (its node id, which is also
    /// [`peer`](Self::peer)). `None` for a call made by the peer as itself.
    pub fn actor(&self) -> Option<&str> {
        self.inner
            .forwarded
            .as_deref()
            .map(ForwardedIdentity::actor)
    }

    /// How the user signed in (the token's `amr`, e.g. `webauthn`, `pwd`,
    /// `apikey`), for a forwarded call; empty otherwise. A method can demand
    /// passkey-level sign-in by checking it.
    pub fn auth_methods(&self) -> &[String] {
        self.inner
            .forwarded
            .as_deref()
            .map_or(&[], ForwardedIdentity::auth_methods)
    }

    /// Refuse the call with `PERMISSION_DENIED` unless the user signed in
    /// with `method` (one of [`auth_methods`](Self::auth_methods), e.g.
    /// [`AMR_WEBAUTHN`](super::identity::AMR_WEBAUTHN)). A call made by the
    /// peer as itself carries no sign-in method and is refused too. Use it
    /// after `requires` for methods that need a stronger sign-in:
    /// `ctx.require_auth_method(AMR_WEBAUTHN)?;`.
    pub fn require_auth_method(&self, method: &str) -> crate::Result<()> {
        if self.auth_methods().iter().any(|held| held == method) {
            return Ok(());
        }
        Err(crate::Error::rpc(
            super::StatusCode::PermissionDenied,
            format!("this method needs a sign-in with {method}"),
        ))
    }

    /// The whole verified on-behalf-of identity (issuer, subject, `jti`,
    /// expiry), for a forwarded call.
    pub fn on_behalf_of(&self) -> Option<&ForwardedIdentity> {
        self.inner.forwarded.as_deref()
    }

    /// Every claim of the verified on-behalf-of token, read-only (a provider
    /// token's custom claims such as `tenant` included), for a forwarded
    /// call; `None` for a call without one. Only a token that passed
    /// verification (issuer, signature, audience, lifetime) yields claims:
    /// never read identity from unverified metadata. Shared across the call,
    /// not copied.
    pub fn token_claims(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.inner
            .forwarded
            .as_deref()
            .map(ForwardedIdentity::claims)
    }

    /// How the transport learned [`peer`](Self::peer).
    pub fn provenance(&self) -> PeerProvenance {
        self.inner.provenance
    }

    /// The caller's authorization attributes: the
    /// [`AttributeStore`](super::AttributeStore)'s, merged with the
    /// authenticator's (the store wins on a collision) — the set Gate 3
    /// checked (e.g. `aster.role`).
    pub fn attributes(&self) -> &HashMap<String, String> {
        &self.inner.attributes
    }

    /// One attribute, by key.
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.inner.attributes.get(key).map(String::as_str)
    }

    /// **This node's** properties (name, tags, typed attributes) as the root
    /// or an issuer last set them through `aster.ops.NodeConfig`, as of when
    /// the call was routed. They describe the node, not the caller, and never
    /// authorise: Gate 3 does not read them, and they are kept apart from the
    /// caller's [`attributes`](Self::attributes) by name and by type. Empty
    /// when no record is set or the service is off.
    pub fn node_properties(&self) -> &NodeProperties {
        &self.inner.node_properties
    }

    /// One of this node's NodeConfig attributes, by key (see
    /// [`node_properties`](Self::node_properties)).
    pub fn node_attribute(&self, key: &str) -> Option<&AttrValue> {
        self.inner.node_properties.attribute(key)
    }

    /// The service being called.
    pub fn service(&self) -> &str {
        &self.inner.header.service
    }

    /// The (wire) method being called.
    pub fn method(&self) -> &str {
        &self.inner.header.method
    }

    /// The caller's call id from the request header (`0` when unset).
    pub fn call_id(&self) -> i32 {
        self.inner.header.call_id
    }

    /// The session the call runs in: the `StreamHeader` session id over a
    /// connection (Iroh), else the `aster-session-id` header (HTTP). `None`
    /// for a call outside any session.
    pub fn session_id(&self) -> Option<&str> {
        self.inner.session_id.as_deref()
    }

    /// The request metadata, as `(key, value)` pairs in header order. Over HTTP
    /// these include the request's headers (lowercased names).
    pub fn metadata(&self) -> impl Iterator<Item = (&str, &str)> {
        let h = &self.inner.header;
        h.metadata_keys
            .iter()
            .zip(h.metadata_values.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The first metadata value for `key`.
    pub fn metadata_value(&self, key: &str) -> Option<&str> {
        self.metadata().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// When the server cancels the call with `DEADLINE_EXCEEDED`: the minimum
    /// of the caller's deadline, the method's timeout and the server's handler
    /// timeout. `None` = no deadline.
    pub fn deadline(&self) -> Option<Instant> {
        self.inner.deadline
    }

    /// Time left until [`deadline`](Self::deadline) (zero once it has passed).
    pub fn remaining(&self) -> Option<Duration> {
        self.inner
            .deadline
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// Whether the call is over before the handler finished: the peer
    /// cancelled it, the stream failed under it, or its deadline passed.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
            || self.inner.deadline.is_some_and(|at| Instant::now() >= at)
    }

    /// Resolves once [`is_cancelled`](Self::is_cancelled) becomes true.
    ///
    /// The dispatcher drops a handler future when its call is cancelled, so
    /// inside the handler this is for `select!`-style early exit (the losing
    /// branch runs until its next await); work that must outlive the handler
    /// — a spawned task holding a clone of this context — sees it resolve too.
    /// Never resolves for a call that completes normally.
    pub async fn cancelled(&self) {
        let inner = &*self.inner;
        loop {
            let ended = inner.ended.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            let signal = inner.signal.as_deref().map(|s| s.notified());
            tokio::pin!(signal);
            if let Some(n) = signal.as_mut().as_pin_mut() {
                n.enable();
            }
            if self.is_cancelled() {
                return;
            }
            let has_signal = inner.signal.is_some();
            tokio::select! {
                () = &mut ended => return,
                () = async { signal.as_mut().as_pin_mut().unwrap().await }, if has_signal => return,
                () = async { tokio::time::sleep_until(inner.deadline.unwrap()).await },
                    if inner.deadline.is_some() => return,
                // A transport that only sets the flag: look again shortly.
                () = tokio::time::sleep(FLAG_POLL), if !has_signal => {}
            }
        }
    }

    /// The dispatcher ended the call (deadline or peer cancel): flag it and
    /// wake every [`cancelled`](Self::cancelled) waiter.
    pub(crate) fn end(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.ended.notify_waiters();
    }

    pub(crate) fn header(&self) -> &StreamHeader {
        &self.inner.header
    }

    pub(crate) fn cancel_flag(&self) -> &Arc<AtomicBool> {
        &self.inner.cancelled
    }
}

impl fmt::Debug for CallContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallContext")
            .field("service", &self.service())
            .field("method", &self.method())
            .field("peer", &self.peer())
            .field("principal", &self.principal())
            .field("actor", &self.actor())
            .field("provenance", &self.provenance())
            .field("attributes", &self.attributes())
            .field("session_id", &self.session_id())
            .field("deadline", &self.deadline())
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}
