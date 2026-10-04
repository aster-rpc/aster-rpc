//! `CoordinationLease` and the live lease backend (§9.4, S4-E).
//!
//! The design's rule: **no second renewal engine.** The existing
//! `core::lease` holder — its renew loop, conservative self-limit and status
//! watch — drives the coordination group through the Slice 1
//! [`CoordinationSerializer`], whose backend this module finally supplies:
//! every `LeaseOp` becomes one journaled, committed Store mutation through the
//! redirecting client.
//!
//! ## The self-limit is the safety half (§9.2)
//!
//! The server arms a conservative full-TTL timer per committed acquire/renew;
//! the holder's self-limit starts at request **send** and expires without a
//! committed renew. On quorum loss renewals cannot commit, so
//! [`CoordinationLease::fence`] returns `None` *before* the server's timer
//! could transfer ownership — the holder stops presenting a fence while the
//! group still counts it the holder.
//!
//! ## The adapter path auto-acknowledges
//!
//! Lease transitions are periodic and self-correcting: a lost renewal outcome
//! is superseded by the next renew, and the §10 exactly-once machinery still
//! guarantees the *server* effect of every transition. The journal therefore
//! acknowledges each resolved lease transition immediately rather than
//! demanding an application ack per renew tick — a caller that needs the full
//! delivery contract for a mutation uses [`super::CoordinationClient`]
//! directly. The two APIs share one journal, so a lease transition still
//! cannot interleave with an unacknowledged application mutation.
//!
//! ## Ending a lease mid-renewal
//!
//! The core holder stops its renew loop by aborting the task, which can land
//! while a renewal is mid-drive. The lease path therefore runs through
//! [`CoordinationClient::self_acknowledging`](super::CoordinationClient): the
//! acknowledgement happens under the same journal hold as the resolution, so
//! an abort can strand a renewal only `Pending`, and the journal remembers (in
//! memory) that the entry is the lease path's. The next mutation on the client
//! — the release that caused the abort, a later lease, or an application
//! mutation after the lease was dropped — first drives that renewal to its
//! resolution with the exact journaled bytes and acknowledges it, then
//! proceeds. `release`, `release_with_value` and drop never leave the journal
//! blocked; after a process restart the entry is redelivered like any other.

use std::sync::Arc;

use aster_transport_core::lease::{
    AcquireOutcome as CoreAcquireOutcome, Fence as CoreFence, LeaseHandle, LeaseOp, LeaseOptions,
    LeaseSerializer, LeaseStatus,
};

use crate::coordination::contract::{FenceV1, ResourceKeyV1};
use crate::coordination::ids::{CoordinationNodeId, ResourceKey};
use crate::coordination::model::adapter::{
    self, BackendReply, CoordinationBackend, CoordinationSerializer, ResourceObservation,
};
use crate::coordination::reason::{Outcome, Reason};
use crate::coordination::store::wire::MutationReplyWireV1;
use crate::error::{Error, Result};

use super::CoordinationClient;

fn client_error(what: impl Into<String>) -> Error {
    Error::InvalidArgument(format!("coordination lease: {}", what.into()))
}

/// The live backend: one committed Store mutation per lease transition.
pub(crate) struct ClientBackend {
    client: CoordinationClient,
}

impl ClientBackend {
    fn wire_key(resource: &ResourceKey) -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: resource.namespace().to_string(),
            key: resource.key().to_vec(),
        }
    }

    /// A full §5.2 fence for `epoch` on `resource`, held by this client.
    fn wire_fence(&self, resource: &ResourceKey, epoch: u64, holder: [u8; 32]) -> FenceV1 {
        FenceV1 {
            group_id: self.client.group().as_bytes().to_vec(),
            resource: Self::wire_key(resource),
            epoch,
            holder: holder.to_vec(),
        }
    }

    fn observation_of(reply: &MutationReplyWireV1) -> ResourceObservation {
        match &reply.snapshot {
            Some(snapshot) => ResourceObservation {
                epoch: snapshot.epoch,
                held: snapshot.held,
                holder: snapshot
                    .holder
                    .as_ref()
                    .and_then(|h| CoordinationNodeId::from_slice(h).ok()),
            },
            None => ResourceObservation::default(),
        }
    }
}

#[async_trait::async_trait]
impl CoordinationBackend for ClientBackend {
    async fn submit(&self, resource: &ResourceKey, op: &LeaseOp) -> anyhow::Result<BackendReply> {
        let local = self.client.local_id()?;
        let resolved = match op {
            LeaseOp::Acquire { ttl, .. } => {
                self.client
                    .acquire(Self::wire_key(resource), ttl.as_millis() as u64)
                    .await
            }
            LeaseOp::Renew { epoch, .. } => {
                self.client
                    .renew(self.wire_fence(resource, *epoch, local))
                    .await
            }
            LeaseOp::Release { epoch, .. } => {
                self.client
                    .release(self.wire_fence(resource, *epoch, local))
                    .await
            }
            LeaseOp::Revoke { .. } => self.client.revoke(Self::wire_key(resource), None).await,
        }
        .map_err(|err| anyhow::anyhow!("{err}"))?;

        // See the module docs: the self-acknowledging client acknowledged
        // this transition under the journal hold that resolved it — periodic
        // and self-correcting, with the server effect still exactly-once.
        let reply = resolved.into_settled_reply();

        let outcome = Outcome::from_code(reply.outcome_code).unwrap_or(Outcome::Rejected);
        let reason = Reason::from_code(reply.reason_code).unwrap_or(Reason::None);
        let fence = (outcome == Outcome::Granted)
            .then(|| {
                reply.snapshot.as_ref().map(|snapshot| CoreFence {
                    resource: adapter::encode(&self.client.group(), resource),
                    epoch: snapshot.epoch,
                    holder: hex::encode(local),
                })
            })
            .flatten();
        Ok(BackendReply {
            outcome,
            reason,
            fence,
            observed: Self::observation_of(&reply),
        })
    }

    async fn observe(&self, resource: &ResourceKey) -> anyhow::Result<ResourceObservation> {
        let reply = self
            .client
            .reads()
            .read_linearizable(Self::wire_key(resource))
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(ResourceObservation {
            epoch: reply.snapshot.epoch,
            held: reply.snapshot.held,
            holder: reply
                .snapshot
                .holder
                .as_ref()
                .and_then(|h| CoordinationNodeId::from_slice(h).ok()),
        })
    }
}

/// Why an acquire did not grant.
pub enum LeaseAcquireOutcome {
    Granted(CoordinationLease),
    /// The group refused; the §9.4 deny mapping and post-decision snapshot.
    Denied(
        aster_transport_core::lease::DenyReason,
        aster_transport_core::lease::LeaseSnapshot,
    ),
}

/// The public fenced register handle (§9.4).
///
/// Constructible only from the full durable [`CoordinationClient`]: a lease is
/// a mutation stream, and a read-only client has no journal to sequence one.
pub struct CoordinationLease {
    handle: LeaseHandle,
    client: CoordinationClient,
    resource: ResourceKey,
}

impl CoordinationLease {
    /// One committed acquire; on grant, the existing renew loop and self-limit
    /// take over.
    pub async fn acquire(
        client: CoordinationClient,
        resource: ResourceKeyV1,
        options: LeaseOptions,
    ) -> Result<LeaseAcquireOutcome> {
        // Every transition this lease makes — renewals, the release, its
        // register mutations — is acknowledged by the lease path itself.
        let client = client.self_acknowledging();
        let key = ResourceKey::new(&resource.namespace, resource.key.clone())
            .map_err(|_| client_error("the resource key is not a valid §4.2 identity"))?;
        let local = client.local_id()?;
        let serializer: Arc<dyn LeaseSerializer> = Arc::new(CoordinationSerializer::new(
            client.group(),
            CoordinationNodeId::from_array(local),
            Arc::new(ClientBackend {
                client: client.clone(),
            }),
        ));
        let encoded = adapter::encode(&client.group(), &key);
        let outcome = LeaseHandle::acquire(serializer, &encoded, &hex::encode(local), options)
            .await
            .map_err(|err| client_error(format!("{err}")))?;
        Ok(match outcome {
            CoreAcquireOutcome::Granted(handle) => LeaseAcquireOutcome::Granted(Self {
                handle,
                client,
                resource: key,
            }),
            CoreAcquireOutcome::Denied(reason, snapshot) => {
                LeaseAcquireOutcome::Denied(reason, snapshot)
            }
        })
    }

    /// The full §5.2 fence for a protected action, or `None` once this holder
    /// must stop (status not held, or the self-limit passed). Ask immediately
    /// before each protected action; never cache across awaits.
    pub fn fence(&self) -> Option<FenceV1> {
        let core = self.handle.fence()?;
        let holder = hex::decode(&core.holder).ok()?;
        Some(FenceV1 {
            group_id: self.client.group().as_bytes().to_vec(),
            resource: ClientBackend::wire_key(&self.resource),
            epoch: core.epoch,
            holder,
        })
    }

    /// The holder's live status watch.
    pub fn status(&self) -> LeaseStatus {
        self.handle.status()
    }

    /// Mutate the register under the current fence (§5.2 `compare_and_set`),
    /// journaled and acknowledged like any lease transition.
    pub async fn compare_and_set(
        &self,
        expected_value_revision: u64,
        new_value: Vec<u8>,
    ) -> Result<MutationReplyWireV1> {
        let fence = self
            .fence()
            .ok_or_else(|| client_error("the fence is no longer presentable"))?;
        let resolved = self
            .client
            .compare_and_set(fence, expected_value_revision, new_value)
            .await?;
        Ok(resolved.into_settled_reply())
    }

    /// Graceful release through the serializer's own transition.
    pub async fn release(self) -> Result<()> {
        self.handle
            .release()
            .await
            .map_err(|err| client_error(format!("{err}")))
    }

    /// §9.4's atomic final handoff: consume the renewal engine through
    /// `stop_renewing`, then journal **one** committed command that writes the
    /// final value and releases together — or does neither.
    pub async fn release_with_value(
        self,
        expected_value_revision: u64,
        final_value: Vec<u8>,
    ) -> Result<MutationReplyWireV1> {
        let core = self
            .handle
            .stop_renewing()
            .ok_or_else(|| client_error("the fence is no longer presentable"))?;
        let holder = hex::decode(&core.holder)
            .map_err(|_| client_error("the core fence holder is not hex"))?;
        let fence = FenceV1 {
            group_id: self.client.group().as_bytes().to_vec(),
            resource: ClientBackend::wire_key(&self.resource),
            epoch: core.epoch,
            holder,
        };
        let resolved = self
            .client
            .release_with_value(fence, expected_value_revision, final_value)
            .await?;
        Ok(resolved.into_settled_reply())
    }
}
