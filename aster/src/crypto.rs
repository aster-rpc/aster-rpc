//! HPKE recipient-key derivation from Aster (Ed25519) node identities.
//!
//! These let a node's HPKE recipient keypair be **derived from its Aster
//! identity** rather than separately minted and published: the node id (an
//! Ed25519 public key, carried by every `aster1` ticket) is sufficient to
//! compute the recipient public key, and the node opens envelopes with its
//! identity secret. The derived keys interoperate with [`crate::hpke_seal`] /
//! [`crate::hpke_open`], which take raw 32-byte X25519 keys.
//!
//! The two derivations agree by construction:
//!
//! ```
//! # use aster::{SecretKey, PublicKey};
//! # use aster::crypto::{hpke_x25519_secret_from_identity, hpke_x25519_public_from_identity};
//! # use aster::hpke_public_key_from_private;
//! # fn check(secret: &SecretKey, public: &PublicKey) -> aster::Result<()> {
//! let from_public = hpke_x25519_public_from_identity(public)?;
//! let from_private = hpke_public_key_from_private(&hpke_x25519_secret_from_identity(secret))?;
//! assert_eq!(from_public, from_private);
//! # Ok(())
//! # }
//! ```
//!
//! ## Security note
//!
//! This binds a node's encryption key to its identity key: the identity secret
//! also unlocks anything sealed to the node. Rotating the recipient key means
//! rotating the node identity unless a separate published encryption key is
//! introduced.

use crate::error::{Error, Result};
use crate::id::{NodeId, PublicKey, SecretKey};

/// Derive the X25519 HPKE *private* key from an Ed25519 node identity secret.
///
/// Standard Ed25519→X25519 scalar derivation (clamped `SHA-512(seed)[..32]`,
/// matching libsodium `crypto_sign_ed25519_sk_to_curve25519`). Total over any
/// identity secret.
pub fn hpke_x25519_secret_from_identity(secret: &SecretKey) -> [u8; 32] {
    aster_transport_core::hpke_envelope::hpke_x25519_secret_from_identity(&secret.to_bytes())
}

/// Derive the matching X25519 HPKE *public* key from an Ed25519 node identity
/// public key (the Edwards→Montgomery map, libsodium
/// `crypto_sign_ed25519_pk_to_curve25519`).
///
/// Returns [`Error::InvalidArgument`] for an invalid Ed25519 public key:
/// off-curve, non-canonical encoding, or a small-order / identity point.
pub fn hpke_x25519_public_from_identity(public: &PublicKey) -> Result<[u8; 32]> {
    aster_transport_core::hpke_envelope::hpke_x25519_public_from_identity(&public.to_bytes())
        .map_err(|e| Error::InvalidArgument(e.to_string()))
}

/// Convenience wrapper over [`hpke_x25519_public_from_identity`] that accepts a
/// [`NodeId`] (a hex-encoded Ed25519 public key) directly.
///
/// Returns [`Error::InvalidArgument`] if the node id is not valid hex or not a
/// valid Ed25519 public key (see [`hpke_x25519_public_from_identity`]).
pub fn hpke_x25519_public_from_node_id(node_id: &NodeId) -> Result<[u8; 32]> {
    let public = PublicKey::from_hex(node_id.as_str())?;
    hpke_x25519_public_from_identity(&public)
}
