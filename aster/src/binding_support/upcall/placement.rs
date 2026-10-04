//! Placement-manager providers implemented by upcalling
//! (`coordination-client`). The topology provider is
//! [`ForeignTopologySource`](super::topology::ForeignTopologySource).
//!
//! The placement manager can then run with every provider in the binding:
//! `CoordinationPlacementManager::new(client, ForeignTopologySource::new(u),
//! ForeignPlacementPolicySource::new(u), ForeignPlacementProvisioner::new(u))`.

use super::codec;
use super::{UpcallError, UpcallKind, Upcalls};
use crate::coordination::client::manager::{
    PlacementPolicySource, PlacementProvisioner, ProvisionRequest,
};
use crate::coordination::client::placement::PlacementPolicy;
use crate::coordination::contract::MemberSeedV1;

fn decode_err(e: anyhow::Error) -> crate::Error {
    UpcallError::Decode(e.to_string()).into()
}

/// [`PlacementProvisioner`] by upcall (kind
/// [`UpcallKind::PlacementProvision`]). The provisioning call is keyed by the
/// idempotency tuple in the request, so the binding may see the same request
/// again after a timeout; it must return the same candidate.
#[derive(Clone, Debug)]
pub struct ForeignPlacementProvisioner {
    upcalls: Upcalls,
}

impl ForeignPlacementProvisioner {
    pub fn new(upcalls: Upcalls) -> Self {
        Self { upcalls }
    }
}

#[async_trait::async_trait]
impl PlacementProvisioner for ForeignPlacementProvisioner {
    async fn provision(&self, request: &ProvisionRequest) -> crate::Result<MemberSeedV1> {
        let bytes = self
            .upcalls
            .call(
                UpcallKind::PlacementProvision,
                codec::encode_provision_request(request),
            )
            .await?;
        codec::decode_member_seed(&bytes).map_err(decode_err)
    }
}

/// [`PlacementPolicySource`] by upcall (kind
/// [`UpcallKind::PlacementPolicy`]). The trait is synchronous, so each read
/// blocks the reconciling task's thread for one round trip (under
/// `block_in_place` on a multi-thread runtime); the manager reads the policy
/// a few times per reconciliation pass, never on a hot path.
#[derive(Clone, Debug)]
pub struct ForeignPlacementPolicySource {
    upcalls: Upcalls,
}

impl ForeignPlacementPolicySource {
    pub fn new(upcalls: Upcalls) -> Self {
        Self { upcalls }
    }

    /// Read the policy asynchronously (same upcall).
    pub async fn fetch(&self) -> crate::Result<PlacementPolicy> {
        let bytes = self
            .upcalls
            .call(
                UpcallKind::PlacementPolicy,
                codec::encode_placement_policy_request(),
            )
            .await?;
        codec::decode_placement_policy(&bytes).map_err(decode_err)
    }
}

impl PlacementPolicySource for ForeignPlacementPolicySource {
    fn policy(&self) -> crate::Result<PlacementPolicy> {
        let bytes = self.upcalls.call_blocking(
            UpcallKind::PlacementPolicy,
            codec::encode_placement_policy_request(),
        )?;
        codec::decode_placement_policy(&bytes).map_err(decode_err)
    }
}
