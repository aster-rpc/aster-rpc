//! Sealed grants — wraps [`aster::grants`] (plan T7).
//!
//! The AAD layout, seal/open, the role rule, the reconcile decision and the
//! grant key all run in Rust (`aster_transport_core::grants`, vectors in
//! `conformance/vectors/grants.json`); nothing wire-defining is reimplemented
//! in JavaScript. [`PolicyDoc`] gives pinned-author reads over a replicated
//! doc — the authenticity half HPKE Base does not provide.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::grants::{self as g, GrantRecord, GrantState};
use aster::{AuthorId, NamespaceCapability, NamespaceId, NodeId};

use crate::config::aster_err;
use crate::docs::{DocEntry, DocEvent, DocEventReceiver, DocHandle, DocsClient};
use crate::error::to_napi_err;
use crate::keys::{bytes32, invalid, key32, secret_in};

/// Everything a sealed grant is bound to. Sealing and opening must use an
/// identical context; `granter` / `recipient` are node ids (64 hex), bound
/// into the AAD verbatim.
#[napi(object)]
pub struct GrantContext {
    /// Versioned application label, e.g. `"portal-sync/tree-grant/v1"`.
    pub app: String,
    /// The sealing authority (root) node id.
    pub granter: String,
    /// The one node this grant is addressed to.
    pub recipient: String,
    /// What the sealed capability unlocks — caller-chosen opaque bytes.
    pub resource: Uint8Array,
    /// The exact doc key the grant record is written at.
    pub path: String,
    /// Grant role, e.g. `"read"` / `"write"` (free-form for other schemes).
    pub role: String,
}

fn with_ctx<R>(c: &GrantContext, f: impl FnOnce(&g::GrantContext<'_>) -> R) -> R {
    let granter = NodeId::from_hex(c.granter.clone());
    let recipient = NodeId::from_hex(c.recipient.clone());
    let ctx = g::GrantContext {
        app: &c.app,
        granter: &granter,
        recipient: &recipient,
        resource: &c.resource,
        path: &c.path,
        role: &c.role,
    };
    f(&ctx)
}

/// The canonical grant AAD for `ctx`.
#[napi]
pub fn grant_aad(ctx: GrantContext) -> Buffer {
    Buffer::from(with_ctx(&ctx, |c| c.aad()))
}

/// Seal `plaintext` to `ctx.recipient`'s identity-derived HPKE key; returns
/// the Fory-encoded envelope.
#[napi]
pub fn seal_grant(ctx: GrantContext, plaintext: Uint8Array) -> Result<Buffer> {
    let sealed = with_ctx(&ctx, |c| g::seal_grant(c, &plaintext)).map_err(aster_err)?;
    Ok(Buffer::from(sealed))
}

/// Seal to an explicit 32-byte X25519 recipient public key.
#[napi]
pub fn seal_grant_to_key(
    recipient_public_key: Uint8Array,
    ctx: GrantContext,
    plaintext: Uint8Array,
) -> Result<Buffer> {
    let key = bytes32("recipient public key", &recipient_public_key)?;
    let sealed =
        with_ctx(&ctx, |c| g::seal_grant_to_key(&key, c, &plaintext)).map_err(aster_err)?;
    Ok(Buffer::from(sealed))
}

/// Open an envelope with the recipient's identity secret (hex or 32 bytes)
/// under the same context.
#[napi]
pub fn open_grant(
    identity_secret: Either<String, Uint8Array>,
    ctx: GrantContext,
    envelope: Uint8Array,
) -> Result<Buffer> {
    let secret = secret_in("identity secret", &identity_secret)?;
    let payload = with_ctx(&ctx, |c| g::open_grant(&secret, c, &envelope)).map_err(aster_err)?;
    Ok(Buffer::from(payload.into_secret()))
}

/// Open with an explicit 32-byte X25519 private key.
#[napi]
pub fn open_grant_with_key(
    recipient_private_key: Uint8Array,
    ctx: GrantContext,
    envelope: Uint8Array,
) -> Result<Buffer> {
    let key = bytes32("recipient private key", &recipient_private_key)?;
    let payload =
        with_ctx(&ctx, |c| g::open_grant_with_key(&key, c, &envelope)).map_err(aster_err)?;
    Ok(Buffer::from(payload.into_secret()))
}

/// Seal a namespace capability (its Fory bytes) after the role check.
#[napi]
pub fn seal_namespace_grant(ctx: GrantContext, capability: Uint8Array) -> Result<Buffer> {
    let capability = NamespaceCapability::decode_fory(&capability).map_err(aster_err)?;
    let sealed = with_ctx(&ctx, |c| g::seal_namespace_grant(c, &capability)).map_err(aster_err)?;
    Ok(Buffer::from(sealed))
}

/// Open a namespace-capability grant; returns the capability's Fory bytes.
#[napi]
pub fn open_namespace_grant(
    identity_secret: Either<String, Uint8Array>,
    ctx: GrantContext,
    envelope: Uint8Array,
) -> Result<Buffer> {
    let secret = secret_in("identity secret", &identity_secret)?;
    let capability =
        with_ctx(&ctx, |c| g::open_namespace_grant(&secret, c, &envelope)).map_err(aster_err)?;
    Ok(Buffer::from(capability.encode_fory()))
}

/// The role ↔ capability rule; throws when `role` does not fit.
#[napi]
pub fn check_grant_role(role: String, can_write: bool) -> Result<()> {
    aster_transport_core::grants::check_grant_role(&role, can_write)
        .map_err(|e| invalid(e.to_string()))
}

/// The conventional grant doc key, `<prefix>/grants/<recipient>`.
#[napi]
pub fn grant_key(prefix: String, recipient: String) -> String {
    g::grant_key(&prefix, &NodeId::from_hex(recipient))
}

/// A reconcile decision. `kind`: `'active'` (apply; `plaintext` or
/// `capability` set), `'revoked'` (drop; `reason`, `role`), `'unopenable'`
/// (drop and alert; `error`), `'absent'` (keep waiting — not revocation).
#[napi(object)]
pub struct GrantOutcome {
    #[napi(ts_type = "'active' | 'revoked' | 'unopenable' | 'absent'")]
    pub kind: String,
    /// Raw payload (`payload: 'raw'`, active only).
    pub plaintext: Option<Buffer>,
    /// Namespace capability Fory bytes (`payload: 'namespace'`, active only).
    pub capability: Option<Buffer>,
    pub reason: Option<String>,
    pub role: Option<String>,
    pub error: Option<String>,
    /// True for `revoked` / `unopenable`.
    pub drops_capability: bool,
}

fn outcome_out<T>(
    o: g::GrantOutcome<T>,
    payload: impl FnOnce(T, &mut GrantOutcome),
) -> GrantOutcome {
    let drops_capability = o.drops_capability();
    let mut out = GrantOutcome {
        kind: String::new(),
        plaintext: None,
        capability: None,
        reason: None,
        role: None,
        error: None,
        drops_capability,
    };
    match o {
        g::GrantOutcome::Active(v) => {
            out.kind = "active".into();
            payload(v, &mut out);
        }
        g::GrantOutcome::Revoked { reason, role } => {
            out.kind = "revoked".into();
            out.reason = Some(reason);
            out.role = Some(role);
        }
        g::GrantOutcome::Unopenable(e) => {
            out.kind = "unopenable".into();
            out.error = Some(e.to_string());
        }
        g::GrantOutcome::Absent => out.kind = "absent".into(),
    }
    out
}

/// Decide what to do with the record at `ctx.path` (`null` = no record),
/// opening it with the identity secret when active. `payload` selects a raw
/// (`'raw'`, default) or namespace-capability (`'namespace'`) payload.
#[napi]
pub fn reconcile_grant(
    identity_secret: Either<String, Uint8Array>,
    ctx: GrantContext,
    record: Option<Uint8Array>,
    #[napi(ts_arg_type = "'raw' | 'namespace'")] payload: Option<String>,
) -> Result<GrantOutcome> {
    let secret = secret_in("identity secret", &identity_secret)?;
    let record = record.as_deref();
    match payload.as_deref().unwrap_or("raw") {
        "raw" => Ok(with_ctx(&ctx, |c| {
            outcome_out(g::reconcile_grant(&secret, c, record), |p, out| {
                out.plaintext = Some(Buffer::from(p.into_secret()))
            })
        })),
        "namespace" => Ok(with_ctx(&ctx, |c| {
            outcome_out(
                g::reconcile_namespace_grant(&secret, c, record),
                |cap, out| out.capability = Some(Buffer::from(cap.encode_fory())),
            )
        })),
        other => Err(invalid(format!(
            "payload must be 'raw' or 'namespace', got {other:?}"
        ))),
    }
}

// ── Grant records ────────────────────────────────────────────────────────────

/// The plaintext framing a sealed grant is published in.
#[napi(object)]
pub struct GrantRecordObject {
    #[napi(ts_type = "'active' | 'revoked'")]
    pub state: String,
    /// Which context template the envelope was sealed under.
    pub context_version: i32,
    pub role: String,
    /// The Fory-encoded HPKE envelope; empty when revoked.
    pub envelope: Uint8Array,
    /// Revocation reason; empty on active records.
    pub reason: String,
}

/// Encode a grant record (Fory XLANG). An active record needs an envelope;
/// a revoked one must have none.
#[napi]
pub fn grant_record_encode(record: GrantRecordObject) -> Result<Buffer> {
    let rec = match record.state.as_str() {
        "active" => {
            if record.envelope.is_empty() {
                return Err(invalid("active grant record must carry an envelope"));
            }
            if !record.reason.is_empty() {
                return Err(invalid("active grant record must not carry a reason"));
            }
            GrantRecord::active(
                record.role,
                record.context_version,
                record.envelope.to_vec(),
            )
        }
        "revoked" => {
            if !record.envelope.is_empty() {
                return Err(invalid("revoked grant record must not carry an envelope"));
            }
            let mut rec = GrantRecord::revoked(record.role, record.reason);
            rec.context_version = record.context_version;
            rec
        }
        other => {
            return Err(invalid(format!(
                "grant record state must be 'active' or 'revoked', got {other:?}"
            )))
        }
    };
    Ok(Buffer::from(rec.encode_fory()))
}

/// Decode grant record bytes (validates version and state/envelope rules).
#[napi]
pub fn grant_record_decode(bytes: Uint8Array) -> Result<GrantRecordObject> {
    let rec = GrantRecord::decode_fory(&bytes).map_err(to_napi_err)?;
    Ok(GrantRecordObject {
        state: match rec.state {
            GrantState::Active => "active".into(),
            GrantState::Revoked => "revoked".into(),
        },
        context_version: rec.context_version,
        role: rec.role,
        envelope: Uint8Array::from(rec.envelope),
        reason: rec.reason,
    })
}

// ── PolicyDoc ────────────────────────────────────────────────────────────────

/// A replicated doc read through a **pinned author**: only records the
/// pinned author (the root's node id) wrote are visible. This is what makes
/// sealed grants trustworthy — HPKE Base has no sender authenticity.
#[napi]
pub struct PolicyDoc {
    inner: g::PolicyDoc,
}

#[napi]
impl PolicyDoc {
    /// Import `namespace` read-only (idempotent) and pin `author`.
    #[napi(ts_return_type = "Promise<PolicyDoc>")]
    pub fn import_read_only<'env>(
        env: &'env Env,
        docs: &DocsClient,
        namespace: Either<String, Uint8Array>,
        author: String,
    ) -> Result<PromiseRaw<'env, PolicyDoc>> {
        let docs = docs.inner.clone();
        let namespace = NamespaceId::from_bytes(key32("namespace id", &namespace)?);
        env.spawn_future(async move {
            let inner =
                g::PolicyDoc::import_read_only(&docs, namespace, AuthorId::from_hex(author))
                    .await
                    .map_err(aster_err)?;
            Ok(PolicyDoc { inner })
        })
    }

    /// Wrap an already-opened doc with a pinned author.
    #[napi(factory)]
    pub fn wrap(doc: &DocHandle, author: String) -> PolicyDoc {
        PolicyDoc {
            inner: g::PolicyDoc::wrap(doc.inner.clone(), AuthorId::from_hex(author)),
        }
    }

    /// The pinned author (hex) all reads are scoped to.
    #[napi(getter)]
    pub fn author(&self) -> String {
        self.inner.author().to_string()
    }

    /// The underlying doc (e.g. to start sync or share).
    #[napi]
    pub fn doc(&self) -> DocHandle {
        DocHandle::from(self.inner.doc().clone())
    }

    /// The pinned author's record at `key`, or `null`.
    #[napi]
    pub async fn get(&self, key: Either<String, Uint8Array>) -> Result<Option<Buffer>> {
        let key = crate::docs::bytes_in(key);
        Ok(self
            .inner
            .get(key)
            .await
            .map_err(aster_err)?
            .map(Buffer::from))
    }

    /// The pinned author's entries under `prefix`.
    #[napi]
    pub async fn list_prefix(&self, prefix: Either<String, Uint8Array>) -> Result<Vec<DocEntry>> {
        let prefix = crate::docs::bytes_in(prefix);
        Ok(self
            .inner
            .list_prefix(prefix)
            .await
            .map_err(aster_err)?
            .into_iter()
            .map(DocEntry::from)
            .collect())
    }

    /// Live doc events; keep only `isPolicyWrite(ev)` ones.
    #[napi]
    pub async fn subscribe(&self) -> Result<DocEventReceiver> {
        Ok(DocEventReceiver::from(
            self.inner.subscribe().await.map_err(aster_err)?,
        ))
    }

    /// Whether `event` is an insert authored by the pinned author.
    #[napi]
    pub fn is_policy_write(&self, event: DocEvent) -> bool {
        matches!(event.kind.as_str(), "insert_local" | "insert_remote")
            && event
                .entry
                .is_some_and(|e| e.author == self.inner.author().as_str())
    }

    /// The raw grant record addressed to `recipient` under
    /// `<prefix>/grants/<recipient>`, or `null`.
    #[napi]
    pub async fn fetch_grant(&self, prefix: String, recipient: String) -> Result<Option<Buffer>> {
        Ok(self
            .inner
            .fetch_grant(&prefix, &NodeId::from_hex(recipient))
            .await
            .map_err(aster_err)?
            .map(Buffer::from))
    }
}
