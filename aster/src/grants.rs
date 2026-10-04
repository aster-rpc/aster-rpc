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
//!
//! ## Where the logic lives
//!
//! The wire-defining parts — AAD layout, seal/open, the role rule, the
//! reconcile decision and the grant key — are implemented once in
//! [`aster_transport_core::grants`] (bytes in/out, shared with the language
//! bindings; vectors in `conformance/vectors/grants.json`). This module is
//! the typed facade over it plus [`PolicyDoc`].

use crate::crypto::{hpke_x25519_public_from_node_id, hpke_x25519_secret_from_identity};
use crate::error::{Error, Result};
use crate::id::{NamespaceCapability, NodeId, SecretKey};
use aster_transport_core::grants as core_grants;

pub use aster_transport_core::grant_record::{GrantRecord, GrantState, GRANT_RECORD_VERSION};
pub use aster_transport_core::namespace::DecryptedCapabilityPayload;

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
    /// The core (hex-id) form of this context.
    fn to_core(&self) -> core_grants::GrantContext<'_> {
        core_grants::GrantContext {
            app: self.app,
            granter: self.granter.as_str(),
            recipient: self.recipient.as_str(),
            resource: self.resource,
            path: self.path,
            role: self.role,
        }
    }

    /// The canonical AAD: the domain label plus every context field,
    /// length-prefixed (u32 BE) so field boundaries can't be confused.
    pub fn aad(&self) -> Vec<u8> {
        self.to_core().aad()
    }
}

// ── Seal / open (identity-derived recipient key) ──────────────────────────────

/// Seal `plaintext` to `ctx.recipient`'s identity-derived HPKE key. Returns
/// the Fory-encoded envelope (`aster.hpke.Envelope`) — the bytes to embed in
/// the grant record.
pub fn seal_grant(ctx: &GrantContext<'_>, plaintext: &[u8]) -> Result<Vec<u8>> {
    let recipient_public = hpke_x25519_public_from_node_id(ctx.recipient)?;
    seal_grant_to_key(&recipient_public, ctx, plaintext)
}

/// Open an envelope produced by [`seal_grant`] with the recipient's identity
/// secret, under the **same** context. Any context mismatch fails.
pub fn open_grant(
    identity_secret: &SecretKey,
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<DecryptedCapabilityPayload> {
    let secret = hpke_x25519_secret_from_identity(identity_secret);
    open_grant_with_key(&secret, ctx, envelope_bytes)
}

// ── Seal / open (published standalone recipient key) ──────────────────────────

/// [`seal_grant`], but to an explicit X25519 recipient public key (the
/// rotation escape hatch — the key must be published and pinned out of band).
pub fn seal_grant_to_key(
    recipient_public: &[u8; 32],
    ctx: &GrantContext<'_>,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    core_grants::seal_grant_to_key(recipient_public, &ctx.to_core(), plaintext)
        .map_err(Error::Transport)
}

/// [`open_grant`], but with an explicit X25519 private key.
pub fn open_grant_with_key(
    recipient_private: &[u8; 32],
    ctx: &GrantContext<'_>,
    envelope_bytes: &[u8],
) -> Result<DecryptedCapabilityPayload> {
    core_grants::open_grant_with_key(recipient_private, &ctx.to_core(), envelope_bytes)
        .map_err(Error::Transport)
}

// ── Typed namespace-capability grants ─────────────────────────────────────────

/// `ctx.role` ↔ capability consistency: `"read"` must carry a read capability,
/// `"write"` a write one. Other role strings carry no capability implication.
fn check_role(ctx: &GrantContext<'_>, capability: &NamespaceCapability) -> Result<()> {
    core_grants::check_grant_role(ctx.role, capability.can_write())
        .map_err(|e| Error::InvalidArgument(e.to_string()))
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
/// grant key: [`Active`](core_grants::GrantOutcome::Active),
/// [`Revoked`](core_grants::GrantOutcome::Revoked),
/// [`Unopenable`](core_grants::GrantOutcome::Unopenable) (fail closed, carries
/// the [`Error`]) or [`Absent`](core_grants::GrantOutcome::Absent) (**not**
/// revocation — keep waiting). Returned instead of a `Result` because every
/// failure mode is a decision. Defined in core so every binding shares it.
pub type GrantOutcome<T> = core_grants::GrantOutcome<T, Error>;

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
    core_grants::reconcile_grant_with(record, ctx.role, open, |fault| match fault {
        core_grants::GrantRecordFault::Malformed(e) => Error::Transport(e),
        role_mismatch => Error::InvalidArgument(role_mismatch.to_string()),
    })
}

/// The conventional **doc entry key** for a grant record:
/// `<prefix>/grants/<node_id>`. This is an *address* (an iroh-docs entry
/// key), **not key material** — it is public by design, like everything in
/// the policy doc. The recipient's own id being the last segment gives it an
/// O(1) self-lookup; the granter enumerates grants with a prefix query over
/// `<prefix>/grants/`. Confidentiality comes solely from the HPKE envelope
/// stored *at* this key, which only the recipient's identity secret opens.
pub fn grant_key(prefix: &str, recipient: &NodeId) -> String {
    core_grants::grant_key(prefix, recipient.as_str())
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
