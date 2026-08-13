//! Sealed grants — per-recipient capability distribution over public docs.
//!
//! The pattern (generalized from portal-sync): a **root-authored,
//! publicly-replicated policy doc** carries records anyone admitted can sync,
//! and among them per-recipient **grants**: a secret (typically an iroh-docs
//! [`NamespaceCapability`]) HPKE-sealed to exactly one node, written at a
//! deterministic key (`<prefix>/grants/<node_id>`). The recipient finds its
//! grant by direct key lookup and opens it with a key derived from its
//! identity secret — no pairwise RPC, no requirement that granter and
//! recipient are online together.
//!
//! Four composable pieces:
//!
//! - [`GrantContext`] + [`seal_grant`] / [`open_grant`] — the crypto with the
//!   AAD discipline built in. The context binds *app label, granter,
//!   recipient, resource, doc path, and role* into the AEAD's associated
//!   data, so a sealed grant cannot be replayed at another node, resource,
//!   role, or path. Never hand-roll the AAD.
//! - [`seal_namespace_grant`] / [`open_namespace_grant`] — the typed
//!   convenience for the common payload, with role/capability consistency
//!   checked at both ends.
//! - [`GrantRecord`] + [`reconcile_grant`] / [`reconcile_namespace_grant`] —
//!   the record framing an envelope is published in (explicit state, plaintext
//!   context version) and the reconcile decision over it. See *Revocation*.
//! - [`PolicyDoc`] — pinned-author reads over a replicated doc. HPKE Base
//!   gives confidentiality only, **not sender authenticity**; authenticity
//!   comes from reading records via `get_exact(<root author>, key)` and
//!   ignoring every other author. (Aster's docs author *is* the node
//!   identity, so pinning an author pins a node key.)
//!
//! ## Recipient keys
//!
//! Grants are addressed to a node's **identity-derived** HPKE key by default
//! ([`crate::crypto`]): the granter needs only the recipient's node id (from
//! any `aster1` ticket), and the recipient opens with its identity secret —
//! nothing extra to publish. The trade-off: rotating the recipient key means
//! rotating the node identity. For rotation-without-identity-rotation, seal
//! to a published standalone X25519 key with [`seal_grant_to_key`] /
//! [`open_grant_with_key`] — and keep the two schemes explicit; a grant
//! sealed to a key the recipient doesn't use fails only at open time.
//!
//! ## Revocation
//!
//! Revoke by writing [`GrantRecord::revoked`] at the grant key, not by
//! deleting the key. Under eventual replication an absent key is ambiguous —
//! it reads the same as "not synced yet", so a recipient that treats absence
//! as revocation tears itself down during initial sync, and a delete relies
//! on tombstone propagation to beat any replica still holding the old entry.
//! An explicit state is unambiguous on arrival, carries a reason, and is
//! auditable.
//!
//! [`reconcile_grant`] returns that rule as a [`GrantOutcome`] you have to
//! match on, so it can't be got wrong by omission: *revoked* and *unopenable*
//! both drop the capability, *absent* is not a decision.
//!
//! Either way this only stops an **honest** recipient — a malicious node that
//! already learned a namespace secret keeps it. True revocation of a leaked
//! capability requires rotating the underlying namespace and re-granting.

use crate::crypto::{hpke_x25519_public_from_node_id, hpke_x25519_secret_from_identity};
use crate::error::{Error, Result};
use crate::id::{NamespaceCapability, NodeId, SecretKey};
use crate::{hpke_open, hpke_seal, HpkeEnvelope};

pub use aster_transport_core::grant_record::{GrantRecord, GrantState, GRANT_RECORD_VERSION};
pub use aster_transport_core::namespace::DecryptedCapabilityPayload;

/// Domain-separation label baked into every grant AAD. Versioned: changing
/// the AAD layout must bump it.
const AAD_DOMAIN: &str = "aster/sealed-grant/v1";

/// Everything a sealed grant is bound to. Sealing and opening must use an
/// identical context — any mismatch (different recipient, resource, role, or
/// doc path) makes `open` fail, which is what prevents replaying a grant
/// record somewhere it wasn't issued.
#[derive(Clone, Debug)]
pub struct GrantContext<'a> {
    /// Versioned application label, e.g. `"portal-sync/tree-grant/v1"`.
    pub app: &'a str,
    /// The sealing authority (root) node id.
    pub granter: &'a NodeId,
    /// The one node this grant is addressed to.
    pub recipient: &'a NodeId,
    /// What the sealed capability unlocks — **caller-chosen opaque bytes**,
    /// not interpreted by Aster and not tied to the payload type. The
    /// namespace id is the obvious choice when the namespace *is* the durable
    /// thing; when it can rotate under a longer-lived object, bind that
    /// object's stable id instead, so re-sealing after a rotation keeps the
    /// same context. The typed helpers below impose no constraint here.
    pub resource: &'a [u8],
    /// The exact doc key the grant record is written at.
    pub path: &'a str,
    /// Grant role, e.g. `"read"` / `"write"` (free-form for other schemes).
    pub role: &'a str,
}

impl GrantContext<'_> {
    /// The canonical AAD: the domain label plus every context field,
    /// length-prefixed (u32 BE) so field boundaries can't be confused.
    pub fn aad(&self) -> Vec<u8> {
        let parts: [&[u8]; 7] = [
            AAD_DOMAIN.as_bytes(),
            self.app.as_bytes(),
            self.granter.as_str().as_bytes(),
            self.recipient.as_str().as_bytes(),
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

// ── Seal / open (identity-derived recipient key) ──────────────────────────────

/// Seal `plaintext` to `ctx.recipient`'s identity-derived HPKE key. Returns
/// the Fory-encoded envelope (`aster.hpke.Envelope`) — the bytes to embed in
/// the grant record.
pub fn seal_grant(ctx: &GrantContext<'_>, plaintext: &[u8]) -> Result<Vec<u8>> {
    let recipient_public = hpke_x25519_public_from_node_id(ctx.recipient)?;
    let envelope = hpke_seal(&recipient_public, &ctx.aad(), plaintext).map_err(Error::Transport)?;
    Ok(envelope.encode_fory())
}

/// Open an envelope produced by [`seal_grant`] with the recipient's identity
/// secret, under the **same** context. Any context mismatch fails.
pub fn open_grant(
    identity_secret: &SecretKey,
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<DecryptedCapabilityPayload> {
    let envelope = HpkeEnvelope::decode_fory(envelope_bytes).map_err(Error::Transport)?;
    let secret = hpke_x25519_secret_from_identity(identity_secret);
    hpke_open(&secret, &ctx.aad(), &envelope).map_err(Error::Transport)
}

// ── Seal / open (published standalone recipient key) ──────────────────────────

/// [`seal_grant`], but to an explicit X25519 recipient public key (the
/// rotation escape hatch — the key must be published and pinned out of band).
pub fn seal_grant_to_key(
    recipient_public: &[u8; 32],
    ctx: &GrantContext<'_>,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let envelope = hpke_seal(recipient_public, &ctx.aad(), plaintext).map_err(Error::Transport)?;
    Ok(envelope.encode_fory())
}

/// [`open_grant`], but with an explicit X25519 private key.
pub fn open_grant_with_key(
    recipient_private: &[u8; 32],
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<DecryptedCapabilityPayload> {
    let envelope = HpkeEnvelope::decode_fory(envelope_bytes).map_err(Error::Transport)?;
    hpke_open(recipient_private, &ctx.aad(), &envelope).map_err(Error::Transport)
}

// ── Typed namespace-capability grants ─────────────────────────────────────────

/// `ctx.role` ↔ capability consistency: `"read"` must carry a read capability,
/// `"write"` a write one. Other role strings carry no capability implication.
fn check_role(ctx: &GrantContext<'_>, capability: &NamespaceCapability) -> Result<()> {
    let ok = match ctx.role {
        "read" => !capability.can_write(),
        "write" => capability.can_write(),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "grant role '{}' does not match the capability (can_write = {})",
            ctx.role,
            capability.can_write(),
        )))
    }
}

/// Seal a [`NamespaceCapability`] (the usual grant payload) to
/// `ctx.recipient`. Checks role/capability consistency first.
pub fn seal_namespace_grant(
    ctx: &GrantContext<'_>,
    capability: &NamespaceCapability,
) -> Result<Vec<u8>> {
    check_role(ctx, capability)?;
    seal_grant(ctx, &capability.encode_fory())
}

/// Open a namespace-capability grant sealed by [`seal_namespace_grant`],
/// re-checking role/capability consistency on the decoded payload.
pub fn open_namespace_grant(
    identity_secret: &SecretKey,
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<NamespaceCapability> {
    let payload = open_grant(identity_secret, ctx, envelope_bytes)?;
    let capability = NamespaceCapability::decode_fory(payload.expose_secret())?;
    check_role(ctx, &capability)?;
    Ok(capability)
}

// ── Reconcile: the revocation rule as a type ──────────────────────────────────

/// What a reconcile loop should do with the record (or absence of one) at a
/// grant key. Returned instead of a `Result` because every failure mode is a
/// decision: there is no case where ignoring the outcome is correct.
#[derive(Debug)]
pub enum GrantOutcome<T> {
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
    Unopenable(Error),
    /// No record at the key. **Not** revocation — under eventual replication
    /// this is indistinguishable from "hasn't synced yet". Keep waiting, and
    /// keep any capability already held.
    Absent,
}

impl<T> GrantOutcome<T> {
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

/// Decide what to do with the record at `ctx.path`, opening the raw payload if
/// the grant is active. Pass `None` when the key has no record.
///
/// Apps with more than one AAD version should decode the record first
/// ([`GrantRecord::decode_fory`]), read `context_version`, build the matching
/// `ctx`, then pass the same bytes here — never trial-decrypt across contexts.
pub fn reconcile_grant(
    identity_secret: &SecretKey,
    ctx: &GrantContext<'_>,
    record: Option<&[u8]>,
) -> GrantOutcome<DecryptedCapabilityPayload> {
    reconcile(record, ctx, |envelope| {
        open_grant(identity_secret, ctx, envelope)
    })
}

/// [`reconcile_grant`] for the typed namespace payload, re-checking
/// role/capability consistency as [`open_namespace_grant`] does.
pub fn reconcile_namespace_grant(
    identity_secret: &SecretKey,
    ctx: &GrantContext<'_>,
    record: Option<&[u8]>,
) -> GrantOutcome<NamespaceCapability> {
    reconcile(record, ctx, |envelope| {
        open_namespace_grant(identity_secret, ctx, envelope)
    })
}

fn reconcile<T>(
    record: Option<&[u8]>,
    ctx: &GrantContext<'_>,
    open: impl FnOnce(&[u8]) -> Result<T>,
) -> GrantOutcome<T> {
    let Some(bytes) = record else {
        return GrantOutcome::Absent;
    };
    let record = match GrantRecord::decode_fory(bytes) {
        Ok(record) => record,
        Err(e) => return GrantOutcome::Unopenable(Error::Transport(e)),
    };
    if !record.is_active() {
        return GrantOutcome::Revoked {
            reason: record.reason,
            role: record.role,
        };
    }
    // The role is also in the AAD, so a mismatch would fail the decrypt — but
    // as a plaintext field it can say so precisely instead of "decrypt failed".
    if record.role != ctx.role {
        return GrantOutcome::Unopenable(Error::InvalidArgument(format!(
            "grant record role '{}' does not match the context role '{}'",
            record.role, ctx.role,
        )));
    }
    match open(&record.envelope) {
        Ok(value) => GrantOutcome::Active(value),
        Err(e) => GrantOutcome::Unopenable(e),
    }
}

/// The conventional **doc entry key** for a grant record:
/// `<prefix>/grants/<node_id>`. This is an *address* (an iroh-docs entry
/// key), **not key material** — it is public by design, like everything in
/// the policy doc. The recipient's own id being the last segment gives it an
/// O(1) self-lookup; the granter enumerates grants with a prefix query over
/// `<prefix>/grants/`. Confidentiality comes solely from the HPKE envelope
/// stored *at* this key, which only the recipient's identity secret opens.
pub fn grant_key(prefix: &str, recipient: &NodeId) -> String {
    format!(
        "{}/grants/{}",
        prefix.trim_end_matches('/'),
        recipient.as_str()
    )
}

// ── PolicyDoc: pinned-author reads ────────────────────────────────────────────

#[cfg(feature = "docs")]
mod policy {
    use super::*;
    use crate::docs::{Doc, DocEntry, DocEvent, DocEvents, Docs};
    use crate::id::{AuthorId, NamespaceId};

    /// A replicated doc read through a **pinned author**: only records written
    /// by that author are visible. This is what makes sealed grants
    /// trustworthy — HPKE Base has no sender authenticity, so a grant (or any
    /// policy record) counts only when the root wrote it. Since Aster's docs
    /// author is the node identity key, the pinned [`AuthorId`] *is* the
    /// root's node id.
    #[derive(Clone)]
    pub struct PolicyDoc {
        doc: Doc,
        author: AuthorId,
    }

    impl PolicyDoc {
        /// Import `namespace` read-only (idempotent) and pin `author`.
        pub async fn import_read_only(
            docs: &Docs,
            namespace: NamespaceId,
            author: AuthorId,
        ) -> Result<Self> {
            let doc = docs.open_or_import_read_namespace(namespace).await?;
            Ok(Self { doc, author })
        }

        /// Wrap an already-opened doc (e.g. from a ticket join) with a pinned
        /// author.
        pub fn wrap(doc: Doc, author: AuthorId) -> Self {
            Self { doc, author }
        }

        /// The underlying doc (e.g. for `start_sync` / `share`).
        pub fn doc(&self) -> &Doc {
            &self.doc
        }

        /// The pinned author all reads are scoped to.
        pub fn author(&self) -> &AuthorId {
            &self.author
        }

        /// The pinned author's record at `key` (other authors are invisible).
        pub async fn get(&self, key: impl Into<Vec<u8>>) -> Result<Option<Vec<u8>>> {
            self.doc.get_exact(&self.author, key).await
        }

        /// The pinned author's entries under `prefix` (other authors filtered).
        pub async fn list_prefix(&self, prefix: impl Into<Vec<u8>>) -> Result<Vec<DocEntry>> {
            Ok(self
                .doc
                .query_key_prefix(prefix, None)
                .await?
                .into_iter()
                .filter(|e| e.author == self.author)
                .collect())
        }

        /// Subscribe to live events. Use [`is_policy_write`](Self::is_policy_write)
        /// to keep only the pinned author's inserts.
        pub async fn subscribe(&self) -> Result<DocEvents> {
            self.doc.subscribe().await
        }

        /// Whether `event` is an insert authored by the pinned author.
        pub fn is_policy_write(&self, event: &DocEvent) -> bool {
            match event {
                DocEvent::InsertLocal { entry } | DocEvent::InsertRemote { entry, .. } => {
                    entry.author == self.author
                }
                _ => false,
            }
        }

        /// Fetch the grant record addressed to `recipient` under the
        /// conventional key scheme (`<prefix>/grants/<recipient>`). Returns
        /// the raw record bytes (application framing + envelope).
        pub async fn fetch_grant(
            &self,
            prefix: &str,
            recipient: &NodeId,
        ) -> Result<Option<Vec<u8>>> {
            self.get(grant_key(prefix, recipient).into_bytes()).await
        }
    }
}

#[cfg(feature = "docs")]
pub use policy::PolicyDoc;
