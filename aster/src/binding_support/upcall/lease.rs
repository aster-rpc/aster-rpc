//! Lease extension points implemented by upcalling: a foreign
//! [`LeaseSerializer`] backend, a foreign [`FencedResource`], and a foreign
//! lease authorizer.

use std::sync::Arc;

use async_trait::async_trait;

use super::codec;
use super::{UpcallError, UpcallKind, Upcalls};
use crate::lease::{
    DenyReason, Fence, FenceRejection, FencedResource, LeaseAuthorizer, LeaseOp, LeaseSerializer,
    LeaseSnapshot, TransitionOutcome,
};

fn decode_err(e: anyhow::Error) -> UpcallError {
    UpcallError::Decode(e.to_string())
}

/// A [`LeaseSerializer`] whose serialization point is foreign code (a
/// Python/JS/JVM store applying `lease_transition` under its own lock).
/// Kinds [`UpcallKind::LeaseTransition`] and [`UpcallKind::LeaseSnapshot`];
/// a failed upcall is an `Err` (a backend fault), never a guessed outcome.
#[derive(Clone, Debug)]
pub struct ForeignLeaseSerializer {
    upcalls: Upcalls,
}

impl ForeignLeaseSerializer {
    pub fn new(upcalls: Upcalls) -> Self {
        Self { upcalls }
    }
}

#[async_trait]
impl LeaseSerializer for ForeignLeaseSerializer {
    async fn transition(&self, resource: &str, op: LeaseOp) -> anyhow::Result<TransitionOutcome> {
        let bytes = self
            .upcalls
            .call(
                UpcallKind::LeaseTransition,
                codec::encode_lease_transition_request(resource, &op),
            )
            .await?;
        codec::decode_transition_outcome(&bytes).map_err(|e| decode_err(e).into())
    }

    async fn snapshot(&self, resource: &str) -> anyhow::Result<LeaseSnapshot> {
        let bytes = self
            .upcalls
            .call(
                UpcallKind::LeaseSnapshot,
                codec::encode_lease_snapshot_request(resource),
            )
            .await?;
        codec::decode_lease_snapshot(&bytes).map_err(|e| decode_err(e).into())
    }
}

/// A [`FencedResource`] whose mutation runs in foreign code, which must
/// evaluate the fence rule (`check_fence`) atomically with the write.
///
/// `Op` is the binding's opaque operation bytes. `Output` is
/// `Result<bytes, UpcallError>`: `Err(FenceRejection)` means the resource
/// refused the fence; `Ok(Err(_))` means there was no answer (timeout,
/// handler error, shutdown) and **the mutation's outcome is unknown** -- a
/// failure mode cannot apply to a write, so the caller decides (kind
/// [`UpcallKind::FencedMutate`]).
#[derive(Clone, Debug)]
pub struct ForeignFencedResource {
    upcalls: Upcalls,
}

impl ForeignFencedResource {
    pub fn new(upcalls: Upcalls) -> Self {
        Self { upcalls }
    }
}

#[async_trait]
impl FencedResource for ForeignFencedResource {
    type Op = Vec<u8>;
    type Output = Result<Vec<u8>, UpcallError>;

    async fn mutate(
        &self,
        fence: &Fence,
        writer: &str,
        op: Vec<u8>,
    ) -> Result<Self::Output, FenceRejection> {
        let answer = self
            .upcalls
            .call(
                UpcallKind::FencedMutate,
                codec::encode_fenced_mutate_request(fence, writer, &op),
            )
            .await;
        match answer {
            Ok(bytes) => match codec::decode_fenced_mutate_response(&bytes) {
                Ok(Ok(out)) => Ok(Ok(out)),
                Ok(Err(rejection)) => Err(rejection),
                Err(e) => Ok(Err(decode_err(e))),
            },
            Err(e) => Ok(Err(e)),
        }
    }
}

/// A foreign lease authorization hook (kind [`UpcallKind::LeaseAuthorize`];
/// no answer -> the kind's failure mode, fail-closed by default).
///
/// Two ways to install it:
/// - [`authorizer`](Self::authorizer) gives the synchronous
///   [`LeaseAuthorizer`] that `MemorySerializer::set_authorizer` takes. It
///   blocks the calling thread for the upcall (under `block_in_place` on a
///   multi-thread runtime) -- one round trip per transition.
/// - [`AuthorizedSerializer`] wraps any serializer and asks asynchronously
///   before each transition; prefer it where the serializer is yours to
///   wrap.
#[derive(Clone, Debug)]
pub struct ForeignLeaseAuthorizer {
    upcalls: Upcalls,
}

impl ForeignLeaseAuthorizer {
    pub fn new(upcalls: Upcalls) -> Self {
        Self { upcalls }
    }

    /// Ask asynchronously whether `op` may be attempted.
    pub async fn authorize(&self, op: &LeaseOp) -> bool {
        let answer = self
            .upcalls
            .call(
                UpcallKind::LeaseAuthorize,
                codec::encode_lease_authorize_request(op),
            )
            .await;
        self.upcalls
            .decide(UpcallKind::LeaseAuthorize, answer, codec::decode_decision)
    }

    /// Ask, blocking the calling thread.
    pub fn authorize_blocking(&self, op: &LeaseOp) -> bool {
        let answer = self.upcalls.call_blocking(
            UpcallKind::LeaseAuthorize,
            codec::encode_lease_authorize_request(op),
        );
        self.upcalls
            .decide(UpcallKind::LeaseAuthorize, answer, codec::decode_decision)
    }

    /// The synchronous hook for `MemorySerializer::set_authorizer`.
    pub fn authorizer(&self) -> LeaseAuthorizer {
        let this = self.clone();
        Arc::new(move |op: &LeaseOp| this.authorize_blocking(op))
    }
}

/// A serializer that asks a [`ForeignLeaseAuthorizer`] before every
/// transition and answers `Denied(Unauthorized, snapshot)` on a refusal --
/// the same outcome `MemorySerializer`'s own hook produces.
pub struct AuthorizedSerializer<S> {
    inner: S,
    authorizer: ForeignLeaseAuthorizer,
}

impl<S: LeaseSerializer> AuthorizedSerializer<S> {
    pub fn new(inner: S, authorizer: ForeignLeaseAuthorizer) -> Self {
        Self { inner, authorizer }
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }
}

#[async_trait]
impl<S: LeaseSerializer> LeaseSerializer for AuthorizedSerializer<S> {
    async fn transition(&self, resource: &str, op: LeaseOp) -> anyhow::Result<TransitionOutcome> {
        if !self.authorizer.authorize(&op).await {
            let snap = self.inner.snapshot(resource).await?;
            return Ok(TransitionOutcome::Denied(DenyReason::Unauthorized, snap));
        }
        self.inner.transition(resource, op).await
    }

    async fn snapshot(&self, resource: &str) -> anyhow::Result<LeaseSnapshot> {
        self.inner.snapshot(resource).await
    }
}
