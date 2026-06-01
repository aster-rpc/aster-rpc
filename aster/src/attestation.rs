//! Ownership attestations.
//!
//! A [`Chain`] is a portable, signed artifact proving that a *root* identity
//! owns a *node* identity (and, in multi-tier deployments, that the root is
//! itself authorised up to a trusted anchor). Verification is fully offline:
//! given the chain bytes, the set of trusted anchors, and the node you expect,
//! [`verify_chain`] confirms the binding and every signature with no network
//! access.
//!
//! Backed by the `aster_transport_core` attestation primitives (Apache Fory
//! wire format, Ed25519). This is portal-sync's Gate-0 trust primitive.

use crate::error::{Error, Result};
use crate::id::{PublicKey, SecretKey};
use aster_transport_core::attestation as core_att;

fn map_err(e: core_att::AttestationError) -> Error {
    Error::Attestation(e.to_string())
}

/// Policy options for a minted attestation edge.
#[derive(Debug, Clone, Default)]
pub struct AttestOptions {
    /// Monotonic replay counter (caller-managed; 0 is fine for a first issue).
    pub epoch: i64,
    /// 0 = no lower bound; else unix seconds before which the chain is invalid.
    pub not_before: i64,
    /// 0 = no expiry; else unix seconds after which the chain is invalid.
    pub not_after: i64,
}

impl AttestOptions {
    fn to_core(&self) -> core_att::AttestOptions {
        core_att::AttestOptions {
            epoch: self.epoch,
            not_before: self.not_before,
            not_after: self.not_after,
        }
    }
}

/// An ownership-attestation chain — the opaque, portable wire artifact.
#[derive(Clone, PartialEq, Eq)]
pub struct Chain {
    bytes: Vec<u8>,
}

impl Chain {
    /// Wrap raw chain wire bytes (e.g. received from a peer or storage).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// The chain's wire bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume into the owned wire bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Encode as `aster.attestation.chain.v1:<base64url>` text.
    pub fn to_text(&self) -> String {
        core_att::encode_text(&self.bytes)
    }

    /// Decode from `aster.attestation.chain.v1:<base64url>` text.
    pub fn from_text(s: &str) -> Result<Self> {
        Ok(Self {
            bytes: core_att::decode_text(s).map_err(map_err)?,
        })
    }
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chain({} bytes)", self.bytes.len())
    }
}

/// The public key for a secret key (the attestation identity it signs as).
pub fn public_key(secret: &SecretKey) -> PublicKey {
    PublicKey::from_bytes(core_att::public_key(&secret.to_bytes()))
}

/// Mint a single-edge root↔node attestation: the root attests it owns the
/// node, and the node co-signs. Both secrets are 32-byte Ed25519 seeds.
pub fn attest_root_node(root: &SecretKey, node: &SecretKey, opts: &AttestOptions) -> Result<Chain> {
    let bytes = core_att::attest_root_node(&root.to_bytes(), &node.to_bytes(), &opts.to_core())
        .map_err(map_err)?;
    Ok(Chain { bytes })
}

/// An identity's role within a verified chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The leaf — the node the chain binds to.
    Node,
    /// The trusted anchor the chain terminates at (the deployment root, or an
    /// anchor above it).
    Anchor,
    /// An intermediate delegating identity between the node and the anchor.
    Intermediate,
}

/// The outcome of a successful [`verify_chain`].
///
/// Note: these are **node-level** identities (Ed25519 transport keys), the
/// participants in the attestation chain — *not* docs authors. Use the role
/// predicates to classify a given identity (e.g. an admission peer's `NodeId`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The node the chain binds to (== the `expected_node` you passed).
    pub node: PublicKey,
    /// The trusted anchor the chain terminates at.
    pub anchor: PublicKey,
    /// Intermediate identities between node and anchor, node-ward → anchor-ward.
    /// Empty for a single-edge (root↔node) chain.
    pub intermediates: Vec<PublicKey>,
    /// Chain depth (number of edges).
    pub depth: usize,
}

impl Verified {
    /// Whether `id` is the leaf node this chain binds.
    pub fn is_node(&self, id: &PublicKey) -> bool {
        &self.node == id
    }

    /// Whether `id` is the trusted anchor (top of the chain).
    pub fn is_anchor(&self, id: &PublicKey) -> bool {
        &self.anchor == id
    }

    /// Whether `id` is an intermediate delegating identity in this chain.
    pub fn is_intermediate(&self, id: &PublicKey) -> bool {
        self.intermediates.contains(id)
    }

    /// The role of `id` within this chain, or `None` if it is not a participant.
    /// (If a chain ever had `node == anchor`, [`Role::Node`] takes precedence.)
    pub fn role_of(&self, id: &PublicKey) -> Option<Role> {
        if self.is_node(id) {
            Some(Role::Node)
        } else if self.is_anchor(id) {
            Some(Role::Anchor)
        } else if self.is_intermediate(id) {
            Some(Role::Intermediate)
        } else {
            None
        }
    }
}

/// Verify a chain end-to-end, entirely offline: structural well-formedness, the
/// leaf binding to `expected_node`, bridging up each edge, the top anchor being
/// in `trusted_anchors`, and every signature. Returns the matched anchor on
/// success, or an [`Error::Attestation`] describing the first failure.
pub fn verify_chain(
    chain: &Chain,
    trusted_anchors: &[PublicKey],
    expected_node: &PublicKey,
) -> Result<Verified> {
    let anchors: Vec<[u8; 32]> = trusted_anchors.iter().map(|a| a.to_bytes()).collect();
    let v = core_att::verify_chain(&chain.bytes, &anchors, &expected_node.to_bytes())
        .map_err(map_err)?;
    Ok(Verified {
        node: PublicKey::from_bytes(v.node),
        anchor: PublicKey::from_bytes(v.anchor),
        intermediates: v
            .intermediates
            .into_iter()
            .map(PublicKey::from_bytes)
            .collect(),
        depth: v.depth,
    })
}
