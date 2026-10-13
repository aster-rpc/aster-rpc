//! Root pre-rotation (KERI-style). A verifier pins the active root and a
//! commitment to the next root key. Only that committed key can sign the
//! statement that replaces the root, so a compromised active root cannot
//! rotate. These are pure functions; profile storage is the caller's.

use serde::{Deserialize, Serialize};

use super::jws::{self, JwsError};
use super::links::parse_key;
use super::ChainError;

pub const ROOT_ROTATION_JWT_TYPE: &str = "aster-root-rotation-v1+jwt";
const COMMITMENT_CONTEXT: &[u8] = b"aster.root.next.v1\0";
pub const MAX_ROTATION_BYTES: usize = 8 * 1024;
const MAX_ROTATION_NAMESPACES: usize = 16;

/// `BLAKE3("aster.root.next.v1\0" || next_root_public_key)`.
pub fn next_root_commitment(next_root: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(COMMITMENT_CONTEXT);
    hasher.update(next_root);
    *hasher.finalize().as_bytes()
}

/// What a verifier pins about its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootState {
    pub root: [u8; 32],
    pub next_commitment: [u8; 32],
    /// 0 until the first rotation.
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationReason {
    Routine,
    Compromise,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RotationClaims {
    iss: String,
    prev: String,
    seq: u64,
    next: String,
    namespaces: Vec<String>,
    iat: u64,
    reason: RotationReason,
}

/// A verified rotation statement. Apply it with [`RootRotation::apply`] and
/// persist the result atomically with any namespace re-targeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRotation {
    new_root: [u8; 32],
    previous_root: [u8; 32],
    seq: u64,
    next_commitment: [u8; 32],
    namespaces: Vec<[u8; 32]>,
    issued_at: u64,
    reason: RotationReason,
    statement: String,
}

impl RootRotation {
    pub fn new_root(&self) -> &[u8; 32] {
        &self.new_root
    }
    pub fn previous_root(&self) -> &[u8; 32] {
        &self.previous_root
    }
    pub fn seq(&self) -> u64 {
        self.seq
    }
    pub fn next_commitment(&self) -> &[u8; 32] {
        &self.next_commitment
    }
    /// The new root's fleet namespace ids.
    pub fn namespaces(&self) -> &[[u8; 32]] {
        &self.namespaces
    }
    pub fn issued_at(&self) -> u64 {
        self.issued_at
    }
    pub fn reason(&self) -> RotationReason {
        self.reason
    }
    /// The statement as received, for onward delivery.
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// The pinned state after this rotation: the anchor set becomes the new
    /// root alone, and its commitment the new pin.
    pub fn apply(&self) -> RootState {
        RootState {
            root: self.new_root,
            next_commitment: self.next_commitment,
            seq: self.seq,
        }
    }
}

/// Sign a rotation with the committed next root's secret key.
pub fn issue_root_rotation(
    next_root_secret: &[u8; 32],
    previous_root: &[u8; 32],
    seq: u64,
    following_commitment: &[u8; 32],
    namespaces: &[[u8; 32]],
    issued_at: u64,
    reason: RotationReason,
) -> Result<String, ChainError> {
    if seq == 0 || namespaces.is_empty() || namespaces.len() > MAX_ROTATION_NAMESPACES {
        return Err(ChainError::Malformed);
    }
    let issuer = jws::public_key(next_root_secret).map_err(|_| ChainError::Issuance)?;
    let claims = RotationClaims {
        iss: hex::encode(issuer),
        prev: hex::encode(previous_root),
        seq,
        next: hex::encode(following_commitment),
        namespaces: namespaces.iter().map(hex::encode).collect(),
        iat: issued_at,
        reason,
    };
    let payload = serde_json::to_vec(&claims).map_err(|_| ChainError::Malformed)?;
    jws::sign(ROOT_ROTATION_JWT_TYPE, &payload, next_root_secret).map_err(|_| ChainError::Issuance)
}

/// The sequence number a statement claims, read without verifying it. Use
/// only to order or skip statements; [`verify_root_rotation`] decides
/// whether one is accepted.
pub fn claimed_rotation_seq(statement: &str) -> Option<u64> {
    if statement.len() > MAX_ROTATION_BYTES {
        return None;
    }
    let compact = jws::decode(statement).ok()?;
    if compact.typ != ROOT_ROTATION_JWT_TYPE {
        return None;
    }
    serde_json::from_slice::<RotationClaims>(&compact.payload)
        .ok()
        .map(|claims| claims.seq)
}

/// Accept a rotation only if it is signed by the key matching the pinned
/// commitment, names the current root as `prev`, and is exactly the next
/// sequence number. A statement signed by the current root never matches.
pub fn verify_root_rotation(
    statement: &str,
    current: &RootState,
    now: u64,
    leeway: u64,
) -> Result<RootRotation, ChainError> {
    if statement.len() > MAX_ROTATION_BYTES {
        return Err(ChainError::Size);
    }
    let compact = jws::decode(statement).map_err(|error| match error {
        JwsError::Header => ChainError::Header,
        _ => ChainError::Malformed,
    })?;
    if compact.typ != ROOT_ROTATION_JWT_TYPE {
        return Err(ChainError::Header);
    }
    let claims: RotationClaims =
        serde_json::from_slice(&compact.payload).map_err(|_| ChainError::Malformed)?;
    let new_root = parse_key(&claims.iss).ok_or(ChainError::Malformed)?;
    let previous_root = parse_key(&claims.prev).ok_or(ChainError::Malformed)?;
    let next_commitment: [u8; 32] = hex::decode(&claims.next)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .filter(|bytes: &[u8; 32]| hex::encode(bytes) == claims.next)
        .ok_or(ChainError::Malformed)?;
    let namespaces = claims
        .namespaces
        .iter()
        .map(|value| {
            hex::decode(value)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .filter(|bytes: &[u8; 32]| hex::encode(bytes) == *value)
        })
        .collect::<Option<Vec<[u8; 32]>>>()
        .filter(|namespaces| !namespaces.is_empty() && namespaces.len() <= MAX_ROTATION_NAMESPACES)
        .ok_or(ChainError::Malformed)?;
    if next_root_commitment(&new_root) != current.next_commitment {
        return Err(ChainError::UntrustedAnchor);
    }
    if previous_root != current.root {
        return Err(ChainError::Bridge);
    }
    if Some(claims.seq) != current.seq.checked_add(1) {
        return Err(ChainError::Sequence);
    }
    if claims.iat > now.saturating_add(leeway) {
        return Err(ChainError::NotYetValid);
    }
    compact
        .verify(&new_root)
        .map_err(|_| ChainError::Signature)?;
    Ok(RootRotation {
        new_root,
        previous_root,
        seq: claims.seq,
        next_commitment,
        namespaces,
        issued_at: claims.iat,
        reason: claims.reason,
        statement: statement.to_owned(),
    })
}
