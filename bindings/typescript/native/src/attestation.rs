//! Ownership attestations — wraps [`aster::attestation`] (plan T7).
//!
//! A chain is opaque wire bytes; the TypeScript layer wraps them in an
//! `AttestationChain` class. Verification is fully offline.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::attestation as att;

use crate::config::aster_err;
use crate::keys::{invalid, public_in, secret_in};

/// Policy options for a minted attestation edge (all default 0).
#[napi(object)]
pub struct AttestOptions {
    /// Monotonic replay counter (caller-managed).
    pub epoch: Option<i64>,
    /// 0 = no lower bound; else unix seconds before which the chain is invalid.
    pub not_before: Option<i64>,
    /// 0 = no expiry; else unix seconds after which the chain is invalid.
    pub not_after: Option<i64>,
}

/// A successfully verified chain. Ids are 64-hex Ed25519 public keys
/// (node-level identities, not docs authors).
#[napi(object)]
pub struct VerifiedChain {
    /// The node the chain binds to (== the expected node).
    pub node: String,
    /// The trusted anchor the chain terminates at.
    pub anchor: String,
    /// Intermediates between node and anchor, node-ward first.
    pub intermediates: Vec<String>,
    /// Number of edges.
    pub depth: u32,
}

/// Mint a single-edge root↔node attestation chain (wire bytes). Both
/// secrets are 32-byte Ed25519 seeds (hex or bytes).
#[napi]
pub fn attest_root_node(
    root_secret: Either<String, Uint8Array>,
    node_secret: Either<String, Uint8Array>,
    options: Option<AttestOptions>,
) -> Result<Buffer> {
    let root = secret_in("root secret", &root_secret)?;
    let node = secret_in("node secret", &node_secret)?;
    let opts = options
        .map(|o| att::AttestOptions {
            epoch: o.epoch.unwrap_or(0),
            not_before: o.not_before.unwrap_or(0),
            not_after: o.not_after.unwrap_or(0),
        })
        .unwrap_or_default();
    let chain = att::attest_root_node(&root, &node, &opts).map_err(aster_err)?;
    Ok(Buffer::from(chain.into_bytes()))
}

/// Verify `chain` offline against `trustedAnchors`, bound to `expectedNode`.
/// Throws describing the first failure.
#[napi]
pub fn attestation_verify_chain(
    chain: Uint8Array,
    trusted_anchors: Vec<Either<String, Uint8Array>>,
    expected_node: Either<String, Uint8Array>,
) -> Result<VerifiedChain> {
    let anchors = trusted_anchors
        .iter()
        .map(|a| public_in("trusted anchor", a))
        .collect::<Result<Vec<_>>>()?;
    let expected = public_in("expected node", &expected_node)?;
    let chain = att::Chain::from_bytes(chain.to_vec());
    let v = att::verify_chain(&chain, &anchors, &expected).map_err(aster_err)?;
    Ok(VerifiedChain {
        node: v.node.to_hex(),
        anchor: v.anchor.to_hex(),
        intermediates: v.intermediates.iter().map(|p| p.to_hex()).collect(),
        depth: u32::try_from(v.depth).map_err(|_| invalid("chain depth overflow"))?,
    })
}

/// Encode chain bytes as `aster.attestation.chain.v1:<base64url>` text.
#[napi]
pub fn attestation_chain_to_text(chain: Uint8Array) -> String {
    att::Chain::from_bytes(chain.to_vec()).to_text()
}

/// Decode `aster.attestation.chain.v1:<base64url>` text into chain bytes.
#[napi]
pub fn attestation_chain_from_text(text: String) -> Result<Buffer> {
    let chain = att::Chain::from_text(&text).map_err(aster_err)?;
    Ok(Buffer::from(chain.into_bytes()))
}

/// The Ed25519 public key (64 hex) of a 32-byte secret — the identity an
/// attestation edge signs as.
#[napi]
pub fn attestation_public_key(secret: Either<String, Uint8Array>) -> Result<String> {
    Ok(att::public_key(&secret_in("secret", &secret)?).to_hex())
}
