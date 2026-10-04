//! ABI 3.4 (additive, rest of tranche F2): sealed grants through the C ABI.
//!
//! Thin wrappers over `aster_transport_core::grants` (and
//! `grant_record`), the one implementation the `aster` crate, PyO3 and NAPI
//! also wrap: the grant AAD, HPKE seal / open to a node identity or an
//! explicit X25519 key, the role ↔ capability rule, typed namespace-capability
//! grants, the grant record framing, the reconcile decision and the
//! conventional grant doc key. Golden vectors:
//! `conformance/vectors/grants.json` (checked in `ffi/tests/grant_vectors.rs`).
//!
//! Conventions follow `pure.rs`: raw keys are 32-byte buffers, variable
//! outputs are `aster_bytes_t` released with `aster_string_release`,
//! decoded structs own one `storage` allocation released with their
//! `*_release`. A namespace capability is `(access, key)`: access
//! `ASTER_LOCAL_ACCESS_WRITE` with the 32-byte namespace secret, or
//! `ASTER_LOCAL_ACCESS_READ` with the 32-byte namespace id.

use std::ffi::c_void;

use aster_transport_core::grant_record::{GrantRecord, GrantState};
use aster_transport_core::grants::{self as core_grants, GrantContext, GrantOutcome};
use aster_transport_core::namespace::CoreNamespaceCapability;

use crate::aster_bytes_t;
use crate::data::aster_local_access_t;
use crate::envelope::{check_struct_size, in_string, invalid, EMPTY_BYTES, OK};
use crate::pure::{
    emit, in_bytes, in_key, out_key, prep_out, try_status, verification_failed, OwnedBufs,
};

/// Domain label at the start of every grant AAD (`aster/sealed-grant/v1`).
pub const ASTER_GRANT_AAD_DOMAIN: &str = "aster/sealed-grant/v1";
const _: () = {
    let a = ASTER_GRANT_AAD_DOMAIN.as_bytes();
    let b = core_grants::GRANT_AAD_DOMAIN.as_bytes();
    assert!(a.len() == b.len());
    let mut i = 0;
    while i < a.len() {
        assert!(a[i] == b[i]);
        i += 1;
    }
};

/// Everything a sealed grant is bound to (`GrantContext`). Sealing and
/// opening must use an identical context. Every field is borrowed for the
/// call; strings are UTF-8, not NUL-terminated.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_grant_context_t {
    /// `sizeof(aster_grant_context_t)`.
    pub struct_size: u32,
    /// Set to 0.
    pub reserved: u32,
    /// Versioned application label, e.g. `portal-sync/tree-grant/v1`.
    pub app: aster_bytes_t,
    /// The sealing authority's node id, hex (bound verbatim).
    pub granter: aster_bytes_t,
    /// The recipient's node id, hex (bound verbatim; also the seal key).
    pub recipient: aster_bytes_t,
    /// Opaque bytes naming what the grant unlocks.
    pub resource: aster_bytes_t,
    /// The exact doc key the grant record is written at.
    pub path: aster_bytes_t,
    /// Grant role, e.g. `read` / `write` (free-form for other schemes).
    pub role: aster_bytes_t,
}

/// `state` of an `aster_grant_record_t`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_grant_state_t {
    /// In force; `envelope` carries the sealed payload.
    ASTER_GRANT_STATE_ACTIVE = 0,
    /// Withdrawn; `envelope` is empty and `reason` says why.
    ASTER_GRANT_STATE_REVOKED = 1,
}

/// The plaintext framing published at a grant key (`GrantRecord`, Fory).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_grant_record_t {
    /// `sizeof(aster_grant_record_t)`: set by the caller for encode, written
    /// by the library on decode.
    pub struct_size: u32,
    /// `aster_grant_state_t`.
    pub state: u32,
    /// Which context template the envelope was sealed under. Read it before
    /// opening to choose the context; never trial-decrypt. Ignored on encode
    /// of a revoked record (always 1).
    pub context_version: i32,
    /// Set to 0.
    pub reserved: u32,
    /// The grant role (kept on revoked records for display).
    pub role: aster_bytes_t,
    /// ACTIVE: the sealed envelope (non-empty). REVOKED: empty.
    pub envelope: aster_bytes_t,
    /// REVOKED: the reason (may be empty). ACTIVE: empty.
    pub reason: aster_bytes_t,
    /// Library-owned backing store of a decoded struct; NULL otherwise.
    pub storage: *mut c_void,
}

/// What a reconcile loop should do with a grant key (`GrantOutcome`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_grant_outcome_kind_t {
    /// Active and opened: apply or refresh the capability (`payload`).
    ASTER_GRANT_OUTCOME_ACTIVE = 0,
    /// Explicitly revoked: drop the capability (`reason`, `role`).
    ASTER_GRANT_OUTCOME_REVOKED = 1,
    /// A record is present but cannot be turned into a capability: drop it
    /// and alert (`reason` = the error).
    ASTER_GRANT_OUTCOME_UNOPENABLE = 2,
    /// No record. Not revocation (may not have synced yet): keep waiting and
    /// keep any capability already held.
    ASTER_GRANT_OUTCOME_ABSENT = 3,
}

/// The result of `aster_grant_reconcile` / `aster_namespace_grant_reconcile`.
/// Release with `aster_grant_outcome_release` (zeroes the payload).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_grant_outcome_t {
    /// `sizeof(aster_grant_outcome_t)`, written by the library.
    pub struct_size: u32,
    /// `aster_grant_outcome_kind_t`.
    pub kind: u32,
    /// Namespace reconcile, ACTIVE: `aster_local_access_t` of `payload`
    /// (WRITE = namespace secret, READ = namespace id). Otherwise 0.
    pub access: u32,
    /// 1 for REVOKED and UNOPENABLE (stop using the capability now), else 0.
    pub drops_capability: u32,
    /// ACTIVE: the opened payload (raw reconcile: the plaintext; namespace
    /// reconcile: the 32-byte key). Otherwise empty.
    pub payload: aster_bytes_t,
    /// REVOKED: the granter's reason. UNOPENABLE: the error message.
    pub reason: aster_bytes_t,
    /// REVOKED: the role the revoked grant carried.
    pub role: aster_bytes_t,
    /// Library-owned backing store; NULL when nothing is allocated.
    pub storage: *mut c_void,
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Owned copy of a caller context (GrantContext borrows `&str`).
struct Ctx {
    app: String,
    granter: String,
    recipient: String,
    resource: Vec<u8>,
    path: String,
    role: String,
}

impl Ctx {
    unsafe fn read(ctx: *const aster_grant_context_t) -> Result<Ctx, i32> {
        if ctx.is_null() {
            return Err(invalid("context is NULL"));
        }
        let c = &*ctx;
        check_struct_size::<aster_grant_context_t>(c.struct_size, "aster_grant_context_t")?;
        Ok(Ctx {
            app: in_string(&c.app, "context.app")?,
            granter: in_string(&c.granter, "context.granter")?,
            recipient: in_string(&c.recipient, "context.recipient")?,
            resource: in_bytes(&c.resource, "context.resource")?.to_vec(),
            path: in_string(&c.path, "context.path")?,
            role: in_string(&c.role, "context.role")?,
        })
    }

    fn borrow(&self) -> GrantContext<'_> {
        GrantContext {
            app: &self.app,
            granter: &self.granter,
            recipient: &self.recipient,
            resource: &self.resource,
            path: &self.path,
            role: &self.role,
        }
    }
}

unsafe fn in_capability(access: u32, key: *const u8) -> Result<CoreNamespaceCapability, i32> {
    let key = in_key(key, "key")?;
    match access {
        x if x == aster_local_access_t::ASTER_LOCAL_ACCESS_WRITE as u32 => {
            Ok(CoreNamespaceCapability::Write(key))
        }
        x if x == aster_local_access_t::ASTER_LOCAL_ACCESS_READ as u32 => {
            Ok(CoreNamespaceCapability::Read(key))
        }
        v => Err(invalid(format!("unknown aster_local_access_t {v}"))),
    }
}

fn split_capability(cap: &CoreNamespaceCapability) -> (u32, [u8; 32]) {
    match cap {
        CoreNamespaceCapability::Write(secret) => (
            aster_local_access_t::ASTER_LOCAL_ACCESS_WRITE as u32,
            *secret,
        ),
        CoreNamespaceCapability::Read(id) => {
            (aster_local_access_t::ASTER_LOCAL_ACCESS_READ as u32, *id)
        }
    }
}

unsafe fn out_capability(
    cap: &CoreNamespaceCapability,
    out_access: *mut u32,
    out_key_ptr: *mut u8,
) {
    let (access, key) = split_capability(cap);
    *out_access = access;
    out_key(out_key_ptr, &key);
}

// ── context, key, role rule ─────────────────────────────────────────────────

/// The canonical grant AAD: the domain `aster/sealed-grant/v1` and each context field
/// (app, granter, recipient, resource, path, role), each prefixed with its
/// u32 big-endian byte length. Release `*out` with `aster_string_release`.
///
/// # Safety
/// `context` valid with readable fields; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_context_aad(
    context: *const aster_grant_context_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_grant_context_aad"));
    let ctx = try_status!(Ctx::read(context));
    emit(out, ctx.borrow().aad())
}

/// The conventional doc key for a grant record: `<prefix>/grants/<recipient>`
/// (trailing `/` on `prefix` trimmed). Release `*out` with
/// `aster_string_release`; not NUL-terminated.
///
/// # Safety
/// Inputs readable for their lengths; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_key(
    prefix: aster_bytes_t,
    recipient: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_grant_key"));
    let prefix = try_status!(in_string(&prefix, "prefix"));
    let recipient = try_status!(in_string(&recipient, "recipient"));
    emit(
        out,
        core_grants::grant_key(&prefix, &recipient).into_bytes(),
    )
}

/// The role ↔ capability rule: role `read` requires `can_write == 0`, role
/// `write` requires `can_write != 0`; any other role (case-sensitive) passes.
/// Returns OK or `ASTER_STATUS_VERIFICATION_FAILED`.
///
/// # Safety
/// `role` readable for its length.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_check_role(role: aster_bytes_t, can_write: u32) -> i32 {
    let role = try_status!(in_string(&role, "role"));
    match core_grants::check_grant_role(&role, can_write != 0) {
        Ok(()) => OK,
        Err(e) => verification_failed(e),
    }
}

// ── raw seal / open ─────────────────────────────────────────────────────────

/// Seal `plaintext` to `context.recipient`'s identity-derived X25519 key
/// with the grant AAD. `*out` = the Fory `aster.hpke.Envelope`; release with
/// `aster_string_release`. `ASTER_STATUS_INVALID_ARGUMENT` if the recipient
/// is not a valid node id.
///
/// # Safety
/// `context` valid; `plaintext` readable; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_seal(
    context: *const aster_grant_context_t,
    plaintext: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_grant_seal"));
    let ctx = try_status!(Ctx::read(context));
    let plaintext = try_status!(in_bytes(&plaintext, "plaintext"));
    match core_grants::seal_grant(&ctx.borrow(), plaintext) {
        Ok(env) => emit(out, env),
        Err(e) => invalid(e),
    }
}

/// `aster_grant_seal` to an explicit 32-byte X25519 recipient public key.
///
/// # Safety
/// `recipient_public_key` readable for 32 bytes; else as `aster_grant_seal`.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_seal_to_key(
    recipient_public_key: *const u8,
    context: *const aster_grant_context_t,
    plaintext: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_grant_seal_to_key"));
    let public = try_status!(in_key(recipient_public_key, "recipient_public_key"));
    let ctx = try_status!(Ctx::read(context));
    let plaintext = try_status!(in_bytes(&plaintext, "plaintext"));
    match core_grants::seal_grant_to_key(&public, &ctx.borrow(), plaintext) {
        Ok(env) => emit(out, env),
        Err(e) => invalid(e),
    }
}

/// Open a grant envelope with the recipient's 32-byte Ed25519 identity
/// secret under the same context. `*out` = the plaintext (release with
/// `aster_string_release`). `ASTER_STATUS_VERIFICATION_FAILED` when the
/// envelope is corrupt or any context field, or the key, differs.
///
/// # Safety
/// `identity_secret` readable for 32 bytes; `context` valid; `envelope`
/// readable; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_open(
    identity_secret: *const u8,
    context: *const aster_grant_context_t,
    envelope: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_grant_open"));
    let secret = try_status!(in_key(identity_secret, "identity_secret"));
    let ctx = try_status!(Ctx::read(context));
    let envelope = try_status!(in_bytes(&envelope, "envelope"));
    match core_grants::open_grant(&secret, &ctx.borrow(), envelope) {
        Ok(p) => emit(out, p.into_secret()),
        Err(e) => verification_failed(e),
    }
}

/// `aster_grant_open` with an explicit 32-byte X25519 private key.
///
/// # Safety
/// As `aster_grant_open`.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_open_with_key(
    recipient_private_key: *const u8,
    context: *const aster_grant_context_t,
    envelope: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_grant_open_with_key"));
    let private = try_status!(in_key(recipient_private_key, "recipient_private_key"));
    let ctx = try_status!(Ctx::read(context));
    let envelope = try_status!(in_bytes(&envelope, "envelope"));
    match core_grants::open_grant_with_key(&private, &ctx.borrow(), envelope) {
        Ok(p) => emit(out, p.into_secret()),
        Err(e) => verification_failed(e),
    }
}

// ── namespace capabilities ──────────────────────────────────────────────────

/// Fory-encode a namespace capability (`access` = `aster_local_access_t`,
/// `key` = 32 bytes). Release `*out` with `aster_string_release`.
///
/// # Safety
/// `key` readable for 32 bytes; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_namespace_capability_encode(
    access: u32,
    key: *const u8,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_namespace_capability_encode"));
    let cap = try_status!(in_capability(access, key));
    emit(out, cap.encode_fory())
}

/// Decode Fory namespace-capability bytes into `*out_access` and 32 bytes at
/// `out_key`.
///
/// # Safety
/// `bytes` readable; `out_access` valid; `out_key` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_namespace_capability_decode(
    bytes: aster_bytes_t,
    out_access: *mut u32,
    out_key: *mut u8,
) -> i32 {
    if out_access.is_null() || out_key.is_null() {
        return invalid("aster_namespace_capability_decode: NULL output");
    }
    let bytes = try_status!(in_bytes(&bytes, "bytes"));
    match CoreNamespaceCapability::decode_fory(bytes) {
        Ok(cap) => {
            out_capability(&cap, out_access, out_key);
            OK
        }
        Err(e) => invalid(e),
    }
}

/// Seal a namespace capability after checking the role rule
/// (`ASTER_STATUS_VERIFICATION_FAILED` if `context.role` does not match).
/// `*out` = the envelope; release with `aster_string_release`.
///
/// # Safety
/// `context` valid; `key` readable for 32 bytes; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_namespace_grant_seal(
    context: *const aster_grant_context_t,
    access: u32,
    key: *const u8,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_namespace_grant_seal"));
    let ctx = try_status!(Ctx::read(context));
    let cap = try_status!(in_capability(access, key));
    if let Err(e) = core_grants::check_grant_role(&ctx.role, cap.can_write()) {
        return verification_failed(e);
    }
    match core_grants::seal_namespace_grant(&ctx.borrow(), &cap) {
        Ok(env) => emit(out, env),
        Err(e) => invalid(e),
    }
}

/// Open a namespace-capability grant and re-check the role rule on the
/// decoded capability. Writes `*out_access` and 32 bytes at `out_key`
/// (the namespace secret for WRITE, the id for READ).
/// `ASTER_STATUS_VERIFICATION_FAILED` when it does not open, or the role
/// does not match the capability.
///
/// # Safety
/// `identity_secret` readable for 32 bytes; `context` valid; `envelope`
/// readable; `out_access` valid; `out_key` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_namespace_grant_open(
    identity_secret: *const u8,
    context: *const aster_grant_context_t,
    envelope: aster_bytes_t,
    out_access: *mut u32,
    out_key: *mut u8,
) -> i32 {
    if out_access.is_null() || out_key.is_null() {
        return invalid("aster_namespace_grant_open: NULL output");
    }
    let secret = try_status!(in_key(identity_secret, "identity_secret"));
    let ctx = try_status!(Ctx::read(context));
    let envelope = try_status!(in_bytes(&envelope, "envelope"));
    match core_grants::open_namespace_grant(&secret, &ctx.borrow(), envelope) {
        Ok(cap) => {
            out_capability(&cap, out_access, out_key);
            OK
        }
        Err(e) => verification_failed(e),
    }
}

// ── grant records ───────────────────────────────────────────────────────────

/// Fory-encode a grant record. `record->struct_size` must be
/// `sizeof(aster_grant_record_t)`; `storage` is ignored. ACTIVE needs a
/// non-empty envelope (and no reason); REVOKED takes role + reason and no
/// envelope. Release `*out` with `aster_string_release`.
///
/// # Safety
/// `record` valid with readable fields; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_record_encode(
    record: *const aster_grant_record_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_grant_record_encode"));
    if record.is_null() {
        return invalid("aster_grant_record_encode: record is NULL");
    }
    let r = &*record;
    try_status!(check_struct_size::<aster_grant_record_t>(
        r.struct_size,
        "aster_grant_record_t"
    ));
    let role = try_status!(in_string(&r.role, "role"));
    let envelope = try_status!(in_bytes(&r.envelope, "envelope"));
    let reason = try_status!(in_string(&r.reason, "reason"));
    let rec = match r.state {
        x if x == aster_grant_state_t::ASTER_GRANT_STATE_ACTIVE as u32 => {
            if envelope.is_empty() {
                return invalid("an active grant record must carry an envelope");
            }
            if !reason.is_empty() {
                return invalid("an active grant record carries no reason");
            }
            GrantRecord::active(role, r.context_version, envelope.to_vec())
        }
        x if x == aster_grant_state_t::ASTER_GRANT_STATE_REVOKED as u32 => {
            if !envelope.is_empty() {
                return invalid("a revoked grant record must not carry an envelope");
            }
            GrantRecord::revoked(role, reason)
        }
        v => return invalid(format!("unknown aster_grant_state_t {v}")),
    };
    emit(out, rec.encode_fory())
}

/// Decode Fory grant-record bytes. On success `*out` is filled (including
/// `struct_size`) and must be released with `aster_grant_record_release`;
/// on failure it is zeroed. Malformed records (bad version / state, active
/// without envelope, revoked with one) are `ASTER_STATUS_INVALID_ARGUMENT`.
///
/// # Safety
/// `bytes` readable; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_record_decode(
    bytes: aster_bytes_t,
    out: *mut aster_grant_record_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_grant_record_decode: out is NULL");
    }
    *out = aster_grant_record_t {
        struct_size: std::mem::size_of::<aster_grant_record_t>() as u32,
        state: 0,
        context_version: 0,
        reserved: 0,
        role: EMPTY_BYTES,
        envelope: EMPTY_BYTES,
        reason: EMPTY_BYTES,
        storage: std::ptr::null_mut(),
    };
    let bytes = try_status!(in_bytes(&bytes, "bytes"));
    let rec = match GrantRecord::decode_fory(bytes) {
        Ok(r) => r,
        Err(e) => return invalid(e),
    };
    let o = &mut *out;
    o.state = match rec.state {
        GrantState::Active => aster_grant_state_t::ASTER_GRANT_STATE_ACTIVE as u32,
        GrantState::Revoked => aster_grant_state_t::ASTER_GRANT_STATE_REVOKED as u32,
    };
    o.context_version = rec.context_version;
    let (storage, v) = OwnedBufs::build(vec![
        rec.role.into_bytes(),
        rec.envelope,
        rec.reason.into_bytes(),
    ]);
    o.role = v[0];
    o.envelope = v[1];
    o.reason = v[2];
    o.storage = storage;
    OK
}

/// Free a decoded grant record and zero its pointers. NULL, zeroed or
/// already released structs are a no-op.
///
/// # Safety
/// `record` NULL or filled by `aster_grant_record_decode`.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_record_release(record: *mut aster_grant_record_t) -> i32 {
    if record.is_null() {
        return OK;
    }
    let r = &mut *record;
    OwnedBufs::release(&mut r.storage);
    r.role = EMPTY_BYTES;
    r.envelope = EMPTY_BYTES;
    r.reason = EMPTY_BYTES;
    OK
}

// ── reconcile ───────────────────────────────────────────────────────────────

unsafe fn fill_outcome<T>(
    out: *mut aster_grant_outcome_t,
    outcome: GrantOutcome<T>,
    payload: impl FnOnce(T) -> (u32, Vec<u8>),
) -> i32 {
    use aster_grant_outcome_kind_t::*;
    let o = &mut *out;
    o.drops_capability = outcome.drops_capability() as u32;
    let (kind, access, bufs) = match outcome {
        GrantOutcome::Active(v) => {
            let (access, bytes) = payload(v);
            (
                ASTER_GRANT_OUTCOME_ACTIVE,
                access,
                vec![bytes, Vec::new(), Vec::new()],
            )
        }
        GrantOutcome::Revoked { reason, role } => (
            ASTER_GRANT_OUTCOME_REVOKED,
            0,
            vec![Vec::new(), reason.into_bytes(), role.into_bytes()],
        ),
        GrantOutcome::Unopenable(e) => (
            ASTER_GRANT_OUTCOME_UNOPENABLE,
            0,
            vec![Vec::new(), format!("{e:#}").into_bytes(), Vec::new()],
        ),
        GrantOutcome::Absent => (ASTER_GRANT_OUTCOME_ABSENT, 0, Vec::new()),
    };
    o.kind = kind as u32;
    o.access = access;
    if bufs.iter().any(|b| !b.is_empty()) {
        let (storage, v) = OwnedBufs::build(bufs);
        o.payload = v[0];
        o.reason = v[1];
        o.role = v[2];
        o.storage = storage;
    }
    OK
}

unsafe fn prep_outcome(out: *mut aster_grant_outcome_t, func: &str) -> Result<(), i32> {
    if out.is_null() {
        return Err(invalid(format!("{func}: out is NULL")));
    }
    *out = aster_grant_outcome_t {
        struct_size: std::mem::size_of::<aster_grant_outcome_t>() as u32,
        kind: aster_grant_outcome_kind_t::ASTER_GRANT_OUTCOME_ABSENT as u32,
        access: 0,
        drops_capability: 0,
        payload: EMPTY_BYTES,
        reason: EMPTY_BYTES,
        role: EMPTY_BYTES,
        storage: std::ptr::null_mut(),
    };
    Ok(())
}

unsafe fn in_record<'a>(record: *const aster_bytes_t) -> Result<Option<&'a [u8]>, i32> {
    if record.is_null() {
        Ok(None)
    } else {
        in_bytes(&*record, "record").map(Some)
    }
}

/// Decide what to do with the record at `context.path`, opening the raw
/// payload with the recipient's 32-byte identity secret if it is active.
/// `record` = NULL when the key has no record (an empty `aster_bytes_t` is a
/// present-but-empty, hence unopenable, record).
///
/// Every outcome, including a record that fails to open, returns OK with
/// `out->kind` set; errors are for bad arguments only. Release `*out` with
/// `aster_grant_outcome_release`.
///
/// # Safety
/// `identity_secret` readable for 32 bytes; `context` valid; `record` NULL
/// or valid; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_reconcile(
    identity_secret: *const u8,
    context: *const aster_grant_context_t,
    record: *const aster_bytes_t,
    out: *mut aster_grant_outcome_t,
) -> i32 {
    try_status!(prep_outcome(out, "aster_grant_reconcile"));
    let secret = try_status!(in_key(identity_secret, "identity_secret"));
    let ctx = try_status!(Ctx::read(context));
    let record = try_status!(in_record(record));
    let outcome = core_grants::reconcile_grant(&secret, &ctx.borrow(), record);
    fill_outcome(out, outcome, |p| (0, p.into_secret()))
}

/// `aster_grant_reconcile` for namespace-capability grants: ACTIVE carries
/// `access` and the 32-byte key in `payload`; a role/capability mismatch is
/// UNOPENABLE.
///
/// # Safety
/// As `aster_grant_reconcile`.
#[no_mangle]
pub unsafe extern "C" fn aster_namespace_grant_reconcile(
    identity_secret: *const u8,
    context: *const aster_grant_context_t,
    record: *const aster_bytes_t,
    out: *mut aster_grant_outcome_t,
) -> i32 {
    try_status!(prep_outcome(out, "aster_namespace_grant_reconcile"));
    let secret = try_status!(in_key(identity_secret, "identity_secret"));
    let ctx = try_status!(Ctx::read(context));
    let record = try_status!(in_record(record));
    let outcome = core_grants::reconcile_namespace_grant(&secret, &ctx.borrow(), record);
    fill_outcome(out, outcome, |cap| {
        let (access, key) = split_capability(&cap);
        (access, key.to_vec())
    })
}

/// Free an outcome's storage (zeroing the payload first) and zero its
/// pointers. NULL, zeroed or already released structs are a no-op.
///
/// # Safety
/// `outcome` NULL or filled by a reconcile function.
#[no_mangle]
pub unsafe extern "C" fn aster_grant_outcome_release(outcome: *mut aster_grant_outcome_t) -> i32 {
    if outcome.is_null() {
        return OK;
    }
    let o = &mut *outcome;
    OwnedBufs::release(&mut o.storage);
    o.payload = EMPTY_BYTES;
    o.reason = EMPTY_BYTES;
    o.role = EMPTY_BYTES;
    OK
}
