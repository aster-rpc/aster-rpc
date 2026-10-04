//! Sealed grants — the wire-defining half: AAD layout, HPKE seal/open, the
//! role/capability rule, the reconcile decision, and the grant doc key.
//!
//! This is the single implementation every binding wraps (the `aster` crate's
//! `aster::grants` re-exposes it with typed ids; PyO3 / NAPI / the C ABI call
//! these bytes-in/bytes-out functions directly). Everything here is pure: no
//! node, no docs, no I/O. Golden vectors: `conformance/vectors/grants.json`.
//!
//! A grant is a secret HPKE-sealed to exactly one recipient, with a
//! [`GrantContext`] bound into the AEAD associated data so the sealed bytes
//! cannot be replayed at another node, resource, role, or doc path. The
//! envelope is published inside a [`crate::grant_record::GrantRecord`]
//! and decided on with [`reconcile_grant`]. See the `aster::grants` module
//! docs for the full pattern (pinned-author policy docs, revocation).

use anyhow::{anyhow, bail, Result};

use crate::grant_record::GrantRecord;
use crate::hpke_envelope::{
    hpke_open, hpke_seal, hpke_x25519_public_from_identity, hpke_x25519_secret_from_identity,
    HpkeEnvelope,
};
use crate::namespace::{CoreNamespaceCapability, DecryptedCapabilityPayload};

/// Domain-separation label baked into every grant AAD. Versioned: changing
/// the AAD layout must bump it.
pub const GRANT_AAD_DOMAIN: &str = "aster/sealed-grant/v1";

/// Everything a sealed grant is bound to. Sealing and opening must use an
/// identical context — any mismatch makes `open` fail.
///
/// `granter` / `recipient` are node ids as **hex strings**, bound into the
/// AAD verbatim (Aster node ids are lowercase hex Ed25519 public keys).
#[derive(Clone, Debug)]
pub struct GrantContext<'a> {
    /// Versioned application label, e.g. `"portal-sync/tree-grant/v1"`.
    pub app: &'a str,
    /// The sealing authority (root) node id, hex.
    pub granter: &'a str,
    /// The one node this grant is addressed to, hex.
    pub recipient: &'a str,
    /// What the sealed capability unlocks — caller-chosen opaque bytes.
    pub resource: &'a [u8],
    /// The exact doc key the grant record is written at.
    pub path: &'a str,
    /// Grant role, e.g. `"read"` / `"write"` (free-form for other schemes).
    pub role: &'a str,
}

impl GrantContext<'_> {
    /// The canonical AAD: [`GRANT_AAD_DOMAIN`] plus every context field, in
    /// the order domain, app, granter, recipient, resource, path, role, each
    /// length-prefixed (u32 BE) so field boundaries can't be confused.
    pub fn aad(&self) -> Vec<u8> {
        let parts: [&[u8]; 7] = [
            GRANT_AAD_DOMAIN.as_bytes(),
            self.app.as_bytes(),
            self.granter.as_bytes(),
            self.recipient.as_bytes(),
            self.resource,
            self.path.as_bytes(),
            self.role.as_bytes(),
        ];
        let mut buf = Vec::with_capacity(parts.iter().map(|p| p.len() + 4).sum());
        for part in parts {
            buf.extend_from_slice(&(part.len() as u32).to_be_bytes());
            buf.extend_from_slice(part);
        }
        buf
    }
}

/// The X25519 HPKE public key a grant to `node_id` (hex Ed25519 public key)
/// is sealed to. Errors on bad hex, wrong length, or an invalid Ed25519 key.
pub fn grant_recipient_public_key(node_id: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(node_id).map_err(|e| anyhow!("invalid node id hex: {e}"))?;
    let public: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("node id must be 32 bytes"))?;
    hpke_x25519_public_from_identity(&public)
}

// ── Seal / open ──────────────────────────────────────────────────────────────

/// Seal `plaintext` to `ctx.recipient`'s identity-derived HPKE key. Returns
/// the Fory-encoded envelope (`aster.hpke.Envelope`).
pub fn seal_grant(ctx: &GrantContext<'_>, plaintext: &[u8]) -> Result<Vec<u8>> {
    let recipient_public = grant_recipient_public_key(ctx.recipient)?;
    seal_grant_to_key(&recipient_public, ctx, plaintext)
}

/// [`seal_grant`], but to an explicit X25519 recipient public key.
pub fn seal_grant_to_key(
    recipient_public: &[u8],
    ctx: &GrantContext<'_>,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    Ok(hpke_seal(recipient_public, &ctx.aad(), plaintext)?.encode_fory())
}

/// Open an envelope produced by [`seal_grant`] with the recipient's 32-byte
/// Ed25519 identity secret, under the **same** context.
pub fn open_grant(
    identity_secret: &[u8; 32],
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<DecryptedCapabilityPayload> {
    let secret = hpke_x25519_secret_from_identity(identity_secret);
    open_grant_with_key(&secret, ctx, envelope_bytes)
}

/// [`open_grant`], but with an explicit X25519 private key.
pub fn open_grant_with_key(
    recipient_private: &[u8],
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<DecryptedCapabilityPayload> {
    let envelope = HpkeEnvelope::decode_fory(envelope_bytes)?;
    hpke_open(recipient_private, &ctx.aad(), &envelope)
}

// ── Typed namespace-capability grants ────────────────────────────────────────

/// The role ↔ capability rule: `"read"` must carry a read capability,
/// `"write"` a write one; other roles (case-sensitive) carry no implication.
pub fn check_grant_role(role: &str, can_write: bool) -> Result<()> {
    let ok = match role {
        "read" => !can_write,
        "write" => can_write,
        _ => true,
    };
    if !ok {
        bail!("grant role '{role}' does not match the capability (can_write = {can_write})");
    }
    Ok(())
}

/// Seal a namespace capability (Fory-encoded) after checking the role rule.
pub fn seal_namespace_grant(
    ctx: &GrantContext<'_>,
    capability: &CoreNamespaceCapability,
) -> Result<Vec<u8>> {
    check_grant_role(ctx.role, capability.can_write())?;
    seal_grant(ctx, &capability.encode_fory())
}

/// Open a namespace-capability grant, re-checking the role rule on the
/// decoded payload.
pub fn open_namespace_grant(
    identity_secret: &[u8; 32],
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<CoreNamespaceCapability> {
    let payload = open_grant(identity_secret, ctx, envelope_bytes)?;
    let capability = CoreNamespaceCapability::decode_fory(payload.expose_secret())?;
    check_grant_role(ctx.role, capability.can_write())?;
    Ok(capability)
}

// ── Reconcile ────────────────────────────────────────────────────────────────

/// What a reconcile loop should do with the record (or absence of one) at a
/// grant key. Every failure mode is a decision, so this is not a `Result`.
#[derive(Debug)]
pub enum GrantOutcome<T, E = anyhow::Error> {
    /// Active and opened — apply or refresh the capability.
    Active(T),
    /// Explicitly revoked by the granter. Drop the cached capability and tear
    /// down sessions using it.
    Revoked {
        /// The granter's stated reason (may be empty).
        reason: String,
        /// The role the revoked grant carried, for display.
        role: String,
    },
    /// A record is present but could not be turned into a capability — bad
    /// framing, a context mismatch, or a failed decrypt. Fail closed: drop the
    /// capability exactly as for [`GrantOutcome::Revoked`], and surface the
    /// error, because unlike revocation this is a fault worth alerting on.
    Unopenable(E),
    /// No record at the key. **Not** revocation — under eventual replication
    /// this is indistinguishable from "hasn't synced yet". Keep waiting, and
    /// keep any capability already held.
    Absent,
}

impl<T, E> GrantOutcome<T, E> {
    /// Whether this outcome means "stop using the capability now" — true for
    /// [`Revoked`](GrantOutcome::Revoked) and [`Unopenable`](GrantOutcome::Unopenable),
    /// false for [`Active`](GrantOutcome::Active) and [`Absent`](GrantOutcome::Absent).
    pub fn drops_capability(&self) -> bool {
        matches!(
            self,
            GrantOutcome::Revoked { .. } | GrantOutcome::Unopenable(_)
        )
    }

    /// The opened capability, if the grant is active.
    pub fn active(self) -> Option<T> {
        match self {
            GrantOutcome::Active(value) => Some(value),
            _ => None,
        }
    }
}

/// Why [`reconcile_grant_with`] refused a record before trying to open it.
#[derive(Debug)]
pub enum GrantRecordFault {
    /// The bytes are not a valid `GrantRecord`.
    Malformed(anyhow::Error),
    /// The record's plaintext role differs from the context's role.
    RoleMismatch {
        /// The role on the record.
        record_role: String,
        /// The role the context expects.
        context_role: String,
    },
}

impl std::fmt::Display for GrantRecordFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrantRecordFault::Malformed(e) => write!(f, "{e}"),
            GrantRecordFault::RoleMismatch {
                record_role,
                context_role,
            } => write!(
                f,
                "grant record role '{record_role}' does not match the context role '{context_role}'"
            ),
        }
    }
}

impl std::error::Error for GrantRecordFault {}

/// The reconcile rule, generic over the payload opener and error type (the
/// form `aster` builds its typed API on):
///
/// 1. no record → [`Absent`](GrantOutcome::Absent);
/// 2. undecodable record → `Unopenable(fault(Malformed))`;
/// 3. revoked → [`Revoked`](GrantOutcome::Revoked) (before any role check);
/// 4. record role ≠ `context_role` → `Unopenable(fault(RoleMismatch))`;
/// 5. `open(envelope)` fails → `Unopenable(err)`; else `Active`.
///
/// The record's `context_version` is not consulted: select the context from
/// it *before* calling this, never trial-decrypt.
pub fn reconcile_grant_with<T, E>(
    record: Option<&[u8]>,
    context_role: &str,
    open: impl FnOnce(&[u8]) -> std::result::Result<T, E>,
    fault: impl FnOnce(GrantRecordFault) -> E,
) -> GrantOutcome<T, E> {
    let Some(bytes) = record else {
        return GrantOutcome::Absent;
    };
    let record = match GrantRecord::decode_fory(bytes) {
        Ok(record) => record,
        Err(e) => return GrantOutcome::Unopenable(fault(GrantRecordFault::Malformed(e))),
    };
    if !record.is_active() {
        return GrantOutcome::Revoked {
            reason: record.reason,
            role: record.role,
        };
    }
    // The role is also in the AAD, so a mismatch would fail the decrypt — but
    // as a plaintext field it can say so precisely instead of "decrypt failed".
    if record.role != context_role {
        return GrantOutcome::Unopenable(fault(GrantRecordFault::RoleMismatch {
            record_role: record.role,
            context_role: context_role.to_string(),
        }));
    }
    match open(&record.envelope) {
        Ok(value) => GrantOutcome::Active(value),
        Err(e) => GrantOutcome::Unopenable(e),
    }
}

/// Decide what to do with the record at `ctx.path`, opening the raw payload
/// with the recipient's identity secret if the grant is active. Pass `None`
/// when the key has no record.
pub fn reconcile_grant(
    identity_secret: &[u8; 32],
    ctx: &GrantContext<'_>,
    record: Option<&[u8]>,
) -> GrantOutcome<DecryptedCapabilityPayload> {
    reconcile_grant_with(
        record,
        ctx.role,
        |envelope| open_grant(identity_secret, ctx, envelope),
        anyhow::Error::new,
    )
}

/// [`reconcile_grant`] for the typed namespace payload.
pub fn reconcile_namespace_grant(
    identity_secret: &[u8; 32],
    ctx: &GrantContext<'_>,
    record: Option<&[u8]>,
) -> GrantOutcome<CoreNamespaceCapability> {
    reconcile_grant_with(
        record,
        ctx.role,
        |envelope| open_namespace_grant(identity_secret, ctx, envelope),
        anyhow::Error::new,
    )
}

/// The conventional doc entry key for a grant record:
/// `<prefix>/grants/<recipient>` (trailing `/` on `prefix` trimmed). An
/// address, not key material.
pub fn grant_key(prefix: &str, recipient: &str) -> String {
    format!("{}/grants/{}", prefix.trim_end_matches('/'), recipient)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aad_layout_is_length_prefixed_fields() {
        let ctx = GrantContext {
            app: "a",
            granter: "g",
            recipient: "r",
            resource: &[0xff],
            path: "",
            role: "read",
        };
        let mut want = Vec::new();
        for part in [
            GRANT_AAD_DOMAIN.as_bytes(),
            b"a",
            b"g",
            b"r",
            &[0xff],
            b"",
            b"read",
        ] {
            want.extend_from_slice(&(part.len() as u32).to_be_bytes());
            want.extend_from_slice(part);
        }
        assert_eq!(ctx.aad(), want);
    }

    #[test]
    fn bad_recipient_node_id_is_refused() {
        assert!(grant_recipient_public_key("zz").is_err());
        assert!(grant_recipient_public_key("00").is_err());
    }
}
