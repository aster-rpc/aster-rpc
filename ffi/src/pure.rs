//! ABI 3.4 (additive, rest of tranche F2): pure functions through the C ABI.
//!
//! Typed id parse / format / validate, HPKE (X25519 keypairs, seal / open of
//! Fory `aster.hpke.Envelope` bytes, X25519 keys derived from an Ed25519
//! identity or node id) and Ed25519 ownership attestations (mint a root↔node
//! chain, verify a chain offline, classify an identity's role). Sealed grants
//! live in `grants.rs`.
//!
//! Everything here is synchronous and needs no runtime. Each function wraps
//! one Rust implementation (`aster_transport_core::{hpke_envelope,
//! attestation}`, `aster::NamespaceSecret`, `iroh::PublicKey`) so every
//! binding gets identical bytes.
//!
//! # Conventions
//!
//! - Keys and ids are raw 32-byte buffers (`const uint8_t*` that must point at
//!   32 readable bytes; `uint8_t*` outputs at 32 writable bytes) unless a
//!   parameter is documented as hex text.
//! - Variable-length inputs are `aster_bytes_t` by value: `{NULL, 0}` is
//!   empty, `{NULL, n > 0}` is invalid.
//! - Variable-length outputs are library-owned `aster_bytes_t`, released with
//!   `aster_string_release(ptr, len)`; an empty output is `{NULL, 0}`.
//!   Decrypted plaintext is secret material: release it promptly.
//! - Structured outputs (`aster_verified_chain_t`) point into one library-owned
//!   `storage` allocation, released with the matching `*_release`.
//!
//! # Status codes
//!
//! `ASTER_STATUS_INVALID_ARGUMENT`: NULL pointers, wrong `struct_size`,
//! unknown enum values, malformed hex / keys / text. `ASTER_STATUS_VERIFICATION_FAILED`:
//! the arguments were well-formed but a security check said no (envelope does
//! not open, chain does not verify). Every failure sets
//! `aster_last_error_message`.

use std::ffi::c_void;

use aster_transport_core::attestation as core_att;
use aster_transport_core::hpke_envelope::{
    hpke_generate_keypair, hpke_open, hpke_public_key_from_private, hpke_seal,
    hpke_x25519_public_from_identity, hpke_x25519_secret_from_identity, HpkeEnvelope,
};

use crate::envelope::{check_struct_size, in_slice, in_string, invalid, EMPTY_BYTES, OK};
use crate::{alloc_owned_bytes, aster_bytes_t, aster_status_t, set_last_error};

// ── shared helpers (also used by grants.rs) ─────────────────────────────────

/// Size of every raw key / id this module takes or returns.
pub const ASTER_KEY_SIZE: usize = 32;
/// Length of the hex text form of a 32-byte id (without the NUL).
pub const ASTER_ID_HEX_LEN: usize = 64;

pub(crate) fn verification_failed(msg: impl ToString) -> i32 {
    set_last_error(msg);
    aster_status_t::ASTER_STATUS_VERIFICATION_FAILED as i32
}

macro_rules! try_status {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(s) => return s,
        }
    };
}
pub(crate) use try_status;

/// Copy a caller-provided 32-byte key.
pub(crate) unsafe fn in_key(ptr: *const u8, what: &str) -> Result<[u8; 32], i32> {
    if ptr.is_null() {
        return Err(invalid(format!("{what}: NULL pointer")));
    }
    let mut key = [0u8; 32];
    std::ptr::copy_nonoverlapping(ptr, key.as_mut_ptr(), 32);
    Ok(key)
}

pub(crate) unsafe fn in_bytes<'a>(b: &aster_bytes_t, what: &str) -> Result<&'a [u8], i32> {
    in_slice(b.ptr, b.len, what)
}

pub(crate) unsafe fn out_key(out: *mut u8, key: &[u8; 32]) {
    std::ptr::copy_nonoverlapping(key.as_ptr(), out, 32);
}

/// Null-check an output pointer and clear it, so a failed call leaves `{NULL, 0}`.
pub(crate) unsafe fn prep_out(out: *mut aster_bytes_t, func: &str) -> Result<(), i32> {
    if out.is_null() {
        return Err(invalid(format!("{func}: out is NULL")));
    }
    *out = EMPTY_BYTES;
    Ok(())
}

pub(crate) unsafe fn emit(out: *mut aster_bytes_t, bytes: Vec<u8>) -> i32 {
    *out = alloc_owned_bytes(bytes);
    OK
}

/// One allocation owning the byte buffers a decoded struct points into. A
/// `Vec`'s heap buffer does not move when the `Vec` is moved into the box.
pub(crate) struct OwnedBufs {
    bufs: Vec<Vec<u8>>,
}

impl OwnedBufs {
    /// Box the buffers; returns the storage pointer and a view of each.
    pub(crate) fn build(bufs: Vec<Vec<u8>>) -> (*mut c_void, Vec<aster_bytes_t>) {
        let storage = Box::new(OwnedBufs { bufs });
        let views = storage
            .bufs
            .iter()
            .map(|b| {
                if b.is_empty() {
                    EMPTY_BYTES
                } else {
                    aster_bytes_t {
                        ptr: b.as_ptr(),
                        len: b.len(),
                    }
                }
            })
            .collect();
        (Box::into_raw(storage) as *mut c_void, views)
    }

    /// Free storage made by `OwnedBufs::build`, zeroing the bytes first
    /// (decoded grant payloads are secret). NULL is a no-op.
    pub(crate) unsafe fn release(storage: &mut *mut c_void) {
        if storage.is_null() {
            return;
        }
        let mut owned = Box::from_raw(*storage as *mut OwnedBufs);
        for b in &mut owned.bufs {
            b.iter_mut().for_each(|x| *x = 0);
        }
        drop(owned);
        *storage = std::ptr::null_mut();
    }
}

// ── typed ids ───────────────────────────────────────────────────────────────

/// The kind of a 32-byte id, for `aster_id_parse` / `_format` / `_validate`.
/// Every kind's text form is 64 hex characters.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_id_kind_t {
    /// A node / endpoint id: an Ed25519 public key (must be a valid point).
    ASTER_ID_NODE_ID = 0,
    /// An Ed25519 public key, e.g. an attestation anchor (must be a valid point).
    ASTER_ID_PUBLIC_KEY = 1,
    /// A node secret key (Ed25519 seed). Any 32 bytes.
    ASTER_ID_SECRET_KEY = 2,
    /// A BLAKE3 content hash. Any 32 bytes.
    ASTER_ID_HASH = 3,
    /// A docs namespace id (public). Any 32 bytes.
    ASTER_ID_NAMESPACE_ID = 4,
    /// A docs namespace secret (write capability). Any 32 bytes.
    ASTER_ID_NAMESPACE_SECRET = 5,
    /// A docs author id: an Ed25519 public key (must be a valid point).
    ASTER_ID_AUTHOR_ID = 6,
}

fn id_kind(kind: u32) -> Result<aster_id_kind_t, i32> {
    use aster_id_kind_t::*;
    Ok(match kind {
        0 => ASTER_ID_NODE_ID,
        1 => ASTER_ID_PUBLIC_KEY,
        2 => ASTER_ID_SECRET_KEY,
        3 => ASTER_ID_HASH,
        4 => ASTER_ID_NAMESPACE_ID,
        5 => ASTER_ID_NAMESPACE_SECRET,
        6 => ASTER_ID_AUTHOR_ID,
        v => return Err(invalid(format!("unknown aster_id_kind_t {v}"))),
    })
}

fn id_name(kind: aster_id_kind_t) -> &'static str {
    use aster_id_kind_t::*;
    match kind {
        ASTER_ID_NODE_ID => "node id",
        ASTER_ID_PUBLIC_KEY => "public key",
        ASTER_ID_SECRET_KEY => "secret key",
        ASTER_ID_HASH => "hash",
        ASTER_ID_NAMESPACE_ID => "namespace id",
        ASTER_ID_NAMESPACE_SECRET => "namespace secret",
        ASTER_ID_AUTHOR_ID => "author id",
    }
}

fn check_id_bytes(kind: aster_id_kind_t, bytes: &[u8; 32]) -> Result<(), i32> {
    use aster_id_kind_t::*;
    match kind {
        ASTER_ID_NODE_ID | ASTER_ID_PUBLIC_KEY | ASTER_ID_AUTHOR_ID => {
            iroh::PublicKey::from_bytes(bytes).map(|_| ()).map_err(|e| {
                invalid(format!(
                    "{} is not a valid Ed25519 public key: {e}",
                    id_name(kind)
                ))
            })
        }
        _ => Ok(()),
    }
}

/// Parse a 64-hex-character id (either case, no surrounding whitespace) into
/// its 32 bytes, checking it is valid for `kind` (`aster_id_kind_t`; Ed25519
/// kinds must decode to a curve point). Writes 32 bytes to `out`.
///
/// # Safety
/// `text` must be readable for its length; `out` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_id_parse(kind: u32, text: aster_bytes_t, out: *mut u8) -> i32 {
    if out.is_null() {
        return invalid("aster_id_parse: out is NULL");
    }
    let kind = try_status!(id_kind(kind));
    let text = try_status!(in_bytes(&text, "text"));
    if text.len() != ASTER_ID_HEX_LEN {
        return invalid(format!(
            "{} must be {ASTER_ID_HEX_LEN} hex characters, got {}",
            id_name(kind),
            text.len()
        ));
    }
    let mut bytes = [0u8; 32];
    if let Err(e) = hex::decode_to_slice(text, &mut bytes) {
        return invalid(format!("{} is not hex: {e}", id_name(kind)));
    }
    try_status!(check_id_bytes(kind, &bytes));
    out_key(out, &bytes);
    OK
}

/// Format a 32-byte id as 64 lowercase hex characters plus a NUL terminator
/// into `out` (65 writable bytes). Total for every valid `kind`; does not
/// validate the bytes (use `aster_id_validate`).
///
/// # Safety
/// `id` must be readable for 32 bytes; `out` writable for 65 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_id_format(kind: u32, id: *const u8, out: *mut u8) -> i32 {
    if out.is_null() {
        return invalid("aster_id_format: out is NULL");
    }
    try_status!(id_kind(kind));
    let bytes = try_status!(in_key(id, "id"));
    let mut text = [0u8; ASTER_ID_HEX_LEN + 1];
    hex::encode_to_slice(bytes, &mut text[..ASTER_ID_HEX_LEN]).expect("64-byte buffer");
    std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len());
    OK
}

/// Check raw id bytes for `kind`: OK, or `ASTER_STATUS_INVALID_ARGUMENT` (with
/// the reason in `aster_last_error_message`) if an Ed25519 kind is not a
/// valid curve point. Every 32-byte value is valid for the other kinds.
///
/// # Safety
/// `id` must be readable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_id_validate(kind: u32, id: *const u8) -> i32 {
    let kind = try_status!(id_kind(kind));
    let bytes = try_status!(in_key(id, "id"));
    try_status!(check_id_bytes(kind, &bytes));
    OK
}

/// The Ed25519 public key (= node id) of a 32-byte node secret key. Total.
///
/// # Safety
/// `secret_key` readable and `out` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_secret_key_public_key(secret_key: *const u8, out: *mut u8) -> i32 {
    if out.is_null() {
        return invalid("aster_secret_key_public_key: out is NULL");
    }
    let secret = try_status!(in_key(secret_key, "secret_key"));
    out_key(out, &core_att::public_key(&secret));
    OK
}

/// The public namespace id of a 32-byte namespace secret
/// (`aster::NamespaceSecret::id`). Total.
///
/// # Safety
/// `namespace_secret` readable and `out` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_namespace_secret_id(
    namespace_secret: *const u8,
    out: *mut u8,
) -> i32 {
    if out.is_null() {
        return invalid("aster_namespace_secret_id: out is NULL");
    }
    let secret = try_status!(in_key(namespace_secret, "namespace_secret"));
    out_key(
        out,
        &aster::NamespaceSecret::from_bytes(secret).id().to_bytes(),
    );
    OK
}

// ── HPKE ────────────────────────────────────────────────────────────────────

/// Generate a fresh X25519 HPKE keypair (OS CSPRNG). Writes 32 bytes to each
/// output.
///
/// # Safety
/// Both outputs must be writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_hpke_generate_keypair(
    out_private_key: *mut u8,
    out_public_key: *mut u8,
) -> i32 {
    if out_private_key.is_null() || out_public_key.is_null() {
        return invalid("aster_hpke_generate_keypair: NULL output");
    }
    let pair = hpke_generate_keypair();
    out_key(out_private_key, &pair.private_key());
    out_key(out_public_key, &pair.public_key());
    OK
}

/// The X25519 HPKE public key of a 32-byte private key.
///
/// # Safety
/// `private_key` readable and `out` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_hpke_public_key_from_private(
    private_key: *const u8,
    out: *mut u8,
) -> i32 {
    if out.is_null() {
        return invalid("aster_hpke_public_key_from_private: out is NULL");
    }
    let private = try_status!(in_key(private_key, "private_key"));
    match hpke_public_key_from_private(&private) {
        Ok(public) => {
            out_key(out, &public);
            OK
        }
        Err(e) => invalid(e),
    }
}

/// The X25519 HPKE private key derived from a 32-byte Ed25519 identity
/// secret (clamped `SHA-512(seed)[..32]`, libsodium
/// `crypto_sign_ed25519_sk_to_curve25519`). Total.
///
/// # Safety
/// `identity_secret` readable and `out` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_hpke_x25519_secret_from_identity(
    identity_secret: *const u8,
    out: *mut u8,
) -> i32 {
    if out.is_null() {
        return invalid("aster_hpke_x25519_secret_from_identity: out is NULL");
    }
    let secret = try_status!(in_key(identity_secret, "identity_secret"));
    out_key(out, &hpke_x25519_secret_from_identity(&secret));
    OK
}

/// The X25519 HPKE public key derived from a 32-byte Ed25519 identity public
/// key (Edwards→Montgomery, libsodium `crypto_sign_ed25519_pk_to_curve25519`).
/// `ASTER_STATUS_INVALID_ARGUMENT` for an off-curve, non-canonical or
/// small-order key.
///
/// # Safety
/// `identity_public` readable and `out` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_hpke_x25519_public_from_identity(
    identity_public: *const u8,
    out: *mut u8,
) -> i32 {
    if out.is_null() {
        return invalid("aster_hpke_x25519_public_from_identity: out is NULL");
    }
    let public = try_status!(in_key(identity_public, "identity_public"));
    match hpke_x25519_public_from_identity(&public) {
        Ok(x) => {
            out_key(out, &x);
            OK
        }
        Err(e) => invalid(e),
    }
}

/// `aster_hpke_x25519_public_from_identity` for a node id given as 64 hex
/// characters (the grant recipient key of that node).
///
/// # Safety
/// `node_id` readable for its length; `out` writable for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_hpke_x25519_public_from_node_id(
    node_id: aster_bytes_t,
    out: *mut u8,
) -> i32 {
    if out.is_null() {
        return invalid("aster_hpke_x25519_public_from_node_id: out is NULL");
    }
    let node_id = try_status!(in_string(&node_id, "node_id"));
    match aster::crypto::hpke_x25519_public_from_node_id(&aster::NodeId::from_hex(node_id)) {
        Ok(x) => {
            out_key(out, &x);
            OK
        }
        Err(e) => invalid(e),
    }
}

/// Seal `plaintext` to an X25519 HPKE public key with `aad` as associated
/// data (HPKE Base, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256,
/// ChaCha20-Poly1305). `*out` = the Fory `aster.hpke.Envelope` bytes;
/// release with `aster_string_release`. Randomized.
///
/// # Safety
/// `recipient_public_key` readable for 32 bytes; byte inputs readable for
/// their lengths; `out` a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn aster_hpke_seal(
    recipient_public_key: *const u8,
    aad: aster_bytes_t,
    plaintext: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_hpke_seal"));
    let public = try_status!(in_key(recipient_public_key, "recipient_public_key"));
    let aad = try_status!(in_bytes(&aad, "aad"));
    let plaintext = try_status!(in_bytes(&plaintext, "plaintext"));
    match hpke_seal(&public, aad, plaintext) {
        Ok(env) => emit(out, env.encode_fory()),
        Err(e) => invalid(e),
    }
}

/// Open a Fory `aster.hpke.Envelope` with the recipient's X25519 private key
/// and the same `aad`. `*out` = the plaintext (release with
/// `aster_string_release`). `ASTER_STATUS_VERIFICATION_FAILED` when the
/// envelope is corrupt or does not open under this key and AAD.
///
/// # Safety
/// `recipient_private_key` readable for 32 bytes; byte inputs readable for
/// their lengths; `out` a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn aster_hpke_open(
    recipient_private_key: *const u8,
    aad: aster_bytes_t,
    envelope: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_hpke_open"));
    let private = try_status!(in_key(recipient_private_key, "recipient_private_key"));
    let aad = try_status!(in_bytes(&aad, "aad"));
    let envelope = try_status!(in_bytes(&envelope, "envelope"));
    let opened = HpkeEnvelope::decode_fory(envelope).and_then(|env| hpke_open(&private, aad, &env));
    match opened {
        Ok(payload) => emit(out, payload.into_secret()),
        Err(e) => verification_failed(e),
    }
}

// ── attestation ─────────────────────────────────────────────────────────────

/// Policy of a minted attestation edge (`aster::attestation::AttestOptions`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_attest_options_t {
    /// `sizeof(aster_attest_options_t)`.
    pub struct_size: u32,
    /// Set to 0.
    pub reserved: u32,
    /// Monotonic replay counter (caller-managed; 0 for a first issue).
    pub epoch: i64,
    /// 0 = no lower bound; else unix seconds before which the chain is invalid.
    pub not_before: i64,
    /// 0 = no expiry; else unix seconds after which the chain is invalid.
    pub not_after: i64,
}

/// An identity's role within a verified chain (`aster::attestation::Role`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_attestation_role_t {
    /// Not a participant of the chain.
    ASTER_ATTESTATION_ROLE_NONE = 0,
    /// The leaf node the chain binds (takes precedence if node == anchor).
    ASTER_ATTESTATION_ROLE_NODE = 1,
    /// The trusted anchor the chain terminates at.
    ASTER_ATTESTATION_ROLE_ANCHOR = 2,
    /// An intermediate delegating identity.
    ASTER_ATTESTATION_ROLE_INTERMEDIATE = 3,
}

/// The result of a successful `aster_attestation_verify_chain`. Release with
/// `aster_verified_chain_release`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_verified_chain_t {
    /// `sizeof(aster_verified_chain_t)`, written by the library.
    pub struct_size: u32,
    /// Number of edges.
    pub depth: u32,
    /// The node the chain binds (= `expected_node`).
    pub node: [u8; 32],
    /// The trusted anchor the chain terminates at.
    pub anchor: [u8; 32],
    /// `intermediates_count` consecutive 32-byte public keys, node-ward →
    /// anchor-ward; NULL when there are none (single-edge chain).
    pub intermediates: *const u8,
    /// Number of 32-byte keys at `intermediates`.
    pub intermediates_count: usize,
    /// Library-owned backing store; NULL when there are no intermediates.
    pub storage: *mut c_void,
}

/// Mint a single-edge root↔node attestation chain: the root attests it owns
/// the node and the node co-signs. Both secrets are 32-byte Ed25519 seeds.
/// `options` may be NULL (epoch 0, no time bounds). `*out` = the chain's
/// wire bytes; release with `aster_string_release`.
///
/// # Safety
/// Secrets readable for 32 bytes; `options` NULL or valid; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_attest_root_node(
    root_secret: *const u8,
    node_secret: *const u8,
    options: *const aster_attest_options_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_attest_root_node"));
    let root = try_status!(in_key(root_secret, "root_secret"));
    let node = try_status!(in_key(node_secret, "node_secret"));
    let opts = if options.is_null() {
        core_att::AttestOptions::default()
    } else {
        let o = &*options;
        try_status!(check_struct_size::<aster_attest_options_t>(
            o.struct_size,
            "aster_attest_options_t"
        ));
        core_att::AttestOptions {
            epoch: o.epoch,
            not_before: o.not_before,
            not_after: o.not_after,
        }
    };
    match core_att::attest_root_node(&root, &node, &opts) {
        Ok(bytes) => emit(out, bytes),
        Err(e) => invalid(e),
    }
}

/// Encode chain wire bytes as `aster.attestation.chain.v1:<base64url>` text.
/// Release `*out` with `aster_string_release`. Not NUL-terminated.
///
/// # Safety
/// `chain` readable for its length; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_attestation_chain_to_text(
    chain: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_attestation_chain_to_text"));
    let chain = try_status!(in_bytes(&chain, "chain"));
    emit(out, core_att::encode_text(chain).into_bytes())
}

/// Decode `aster.attestation.chain.v1:<base64url>` text to chain wire bytes
/// (not verified). Release `*out` with `aster_string_release`.
///
/// # Safety
/// `text` readable for its length; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_attestation_chain_from_text(
    text: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    try_status!(prep_out(out, "aster_attestation_chain_from_text"));
    let text = try_status!(in_string(&text, "text"));
    match core_att::decode_text(&text) {
        Ok(bytes) => emit(out, bytes),
        Err(e) => invalid(e),
    }
}

/// Verify a chain end to end, offline: size bounds, structure, the leaf
/// binding to `expected_node`, bridging, the top anchor being one of the
/// `anchor_count` 32-byte keys at `trusted_anchors`, and every signature.
///
/// On success fills `*out` (release with `aster_verified_chain_release`).
/// A chain that fails any check returns `ASTER_STATUS_VERIFICATION_FAILED`
/// with the failed check in `aster_last_error_message`; `*out` is then
/// zeroed (except `struct_size`).
///
/// # Safety
/// `chain` readable for its length; `trusted_anchors` readable for
/// `anchor_count * 32` bytes (may be NULL when 0); `expected_node` readable
/// for 32 bytes; `out` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_attestation_verify_chain(
    chain: aster_bytes_t,
    trusted_anchors: *const u8,
    anchor_count: usize,
    expected_node: *const u8,
    out: *mut aster_verified_chain_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_attestation_verify_chain: out is NULL");
    }
    *out = aster_verified_chain_t {
        struct_size: std::mem::size_of::<aster_verified_chain_t>() as u32,
        depth: 0,
        node: [0; 32],
        anchor: [0; 32],
        intermediates: std::ptr::null(),
        intermediates_count: 0,
        storage: std::ptr::null_mut(),
    };
    let chain = try_status!(in_bytes(&chain, "chain"));
    let Some(anchor_bytes) = anchor_count.checked_mul(32) else {
        return invalid("anchor_count overflows");
    };
    let anchors_raw = try_status!(in_slice(trusted_anchors, anchor_bytes, "trusted_anchors"));
    let anchors: Vec<[u8; 32]> = anchors_raw
        .chunks_exact(32)
        .map(|c| c.try_into().expect("32-byte chunk"))
        .collect();
    let expected = try_status!(in_key(expected_node, "expected_node"));
    let v = match core_att::verify_chain(chain, &anchors, &expected) {
        Ok(v) => v,
        Err(e) => return verification_failed(e),
    };
    let o = &mut *out;
    o.depth = v.depth as u32;
    o.node = v.node;
    o.anchor = v.anchor;
    if !v.intermediates.is_empty() {
        let flat: Vec<u8> = v.intermediates.iter().flatten().copied().collect();
        let (storage, views) = OwnedBufs::build(vec![flat]);
        o.intermediates = views[0].ptr;
        o.intermediates_count = v.intermediates.len();
        o.storage = storage;
    }
    OK
}

/// Free the storage of a verified chain and zero its pointers. A zeroed or
/// already released struct is a no-op; NULL is a no-op.
///
/// # Safety
/// `chain` NULL or filled by `aster_attestation_verify_chain`.
#[no_mangle]
pub unsafe extern "C" fn aster_verified_chain_release(chain: *mut aster_verified_chain_t) -> i32 {
    if chain.is_null() {
        return OK;
    }
    let c = &mut *chain;
    OwnedBufs::release(&mut c.storage);
    c.intermediates = std::ptr::null();
    c.intermediates_count = 0;
    OK
}

/// The role (`aster_attestation_role_t`) of the 32-byte public key `id` in a
/// verified chain; `ASTER_ATTESTATION_ROLE_NONE` if it is not a participant.
///
/// # Safety
/// `chain` filled by `aster_attestation_verify_chain`; `id` readable for 32
/// bytes; `out_role` valid.
#[no_mangle]
pub unsafe extern "C" fn aster_verified_chain_role_of(
    chain: *const aster_verified_chain_t,
    id: *const u8,
    out_role: *mut u32,
) -> i32 {
    if chain.is_null() || out_role.is_null() {
        return invalid("aster_verified_chain_role_of: NULL argument");
    }
    let c = &*chain;
    try_status!(check_struct_size::<aster_verified_chain_t>(
        c.struct_size,
        "aster_verified_chain_t"
    ));
    let id = try_status!(in_key(id, "id"));
    let Some(n) = c.intermediates_count.checked_mul(32) else {
        return invalid("intermediates_count overflows");
    };
    let mids = try_status!(in_slice(c.intermediates, n, "intermediates"));
    use aster_attestation_role_t::*;
    let role = if c.node == id {
        ASTER_ATTESTATION_ROLE_NODE
    } else if c.anchor == id {
        ASTER_ATTESTATION_ROLE_ANCHOR
    } else if mids.chunks_exact(32).any(|m| m == id) {
        ASTER_ATTESTATION_ROLE_INTERMEDIATE
    } else {
        ASTER_ATTESTATION_ROLE_NONE
    };
    *out_role = role as u32;
    OK
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aster_string_release;
    use std::slice;

    fn b(s: &[u8]) -> aster_bytes_t {
        aster_bytes_t {
            ptr: if s.is_empty() {
                std::ptr::null()
            } else {
                s.as_ptr()
            },
            len: s.len(),
        }
    }

    unsafe fn take(out: aster_bytes_t) -> Vec<u8> {
        let v = if out.len == 0 {
            Vec::new()
        } else {
            slice::from_raw_parts(out.ptr, out.len).to_vec()
        };
        aster_string_release(out.ptr, out.len);
        v
    }

    const OKS: i32 = aster_status_t::ASTER_STATUS_OK as i32;
    const INV: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    const VF: i32 = aster_status_t::ASTER_STATUS_VERIFICATION_FAILED as i32;

    #[test]
    fn id_parse_format_validate() {
        unsafe {
            let secret = [7u8; 32];
            let mut public = [0u8; 32];
            assert_eq!(
                aster_secret_key_public_key(secret.as_ptr(), public.as_mut_ptr()),
                OKS
            );
            let mut text = [0u8; 65];
            assert_eq!(aster_id_format(0, public.as_ptr(), text.as_mut_ptr()), OKS);
            assert_eq!(text[64], 0);
            assert_eq!(&text[..64], hex::encode(public).as_bytes());
            let upper = hex::encode_upper(public);
            let mut parsed = [0u8; 32];
            assert_eq!(
                aster_id_parse(0, b(upper.as_bytes()), parsed.as_mut_ptr()),
                OKS
            );
            assert_eq!(parsed, public);
            assert_eq!(aster_id_validate(0, public.as_ptr()), OKS);
            // Not a point on the curve (y = 2 has no x): invalid for Ed25519 kinds only.
            let mut bad = [0u8; 32];
            bad[0] = 2;
            assert_eq!(aster_id_validate(0, bad.as_ptr()), INV);
            assert_eq!(aster_id_validate(6, bad.as_ptr()), INV);
            assert_eq!(aster_id_validate(3, bad.as_ptr()), OKS);
            assert_eq!(
                aster_id_parse(1, b(hex::encode(bad).as_bytes()), parsed.as_mut_ptr()),
                INV
            );
            assert_eq!(aster_id_parse(3, b(b"abcd"), parsed.as_mut_ptr()), INV);
            assert_eq!(aster_id_parse(3, b(&[b'z'; 64]), parsed.as_mut_ptr()), INV);
            assert_eq!(
                aster_id_parse(99, b(upper.as_bytes()), parsed.as_mut_ptr()),
                INV
            );
            assert_eq!(aster_id_format(7, public.as_ptr(), text.as_mut_ptr()), INV);
            // Namespace secret -> id agrees with the aster crate.
            let mut ns = [0u8; 32];
            assert_eq!(
                aster_namespace_secret_id(secret.as_ptr(), ns.as_mut_ptr()),
                OKS
            );
            assert_eq!(
                ns,
                aster::NamespaceSecret::from_bytes(secret).id().to_bytes()
            );
        }
    }

    #[test]
    fn hpke_roundtrip_and_identity_derivation() {
        unsafe {
            let (mut sk, mut pk) = ([0u8; 32], [0u8; 32]);
            assert_eq!(
                aster_hpke_generate_keypair(sk.as_mut_ptr(), pk.as_mut_ptr()),
                OKS
            );
            let mut pk2 = [0u8; 32];
            assert_eq!(
                aster_hpke_public_key_from_private(sk.as_ptr(), pk2.as_mut_ptr()),
                OKS
            );
            assert_eq!(pk, pk2);
            let mut env = EMPTY_BYTES;
            assert_eq!(
                aster_hpke_seal(pk.as_ptr(), b(b"aad"), b(b"hello"), &mut env),
                OKS
            );
            let env = take(env);
            let mut pt = EMPTY_BYTES;
            assert_eq!(
                aster_hpke_open(sk.as_ptr(), b(b"aad"), b(&env), &mut pt),
                OKS
            );
            assert_eq!(take(pt), b"hello");
            assert_eq!(
                aster_hpke_open(sk.as_ptr(), b(b"other"), b(&env), &mut pt),
                VF
            );
            assert!(pt.ptr.is_null());
            assert_eq!(
                aster_hpke_open(sk.as_ptr(), b(b"aad"), b(&env[..10]), &mut pt),
                VF
            );

            // Identity-derived keys agree, and node-id hex gives the same key.
            let id_secret = [9u8; 32];
            let mut id_public = [0u8; 32];
            aster_secret_key_public_key(id_secret.as_ptr(), id_public.as_mut_ptr());
            let (mut xs, mut xp, mut xp2, mut xp3) = ([0u8; 32], [0u8; 32], [0u8; 32], [0u8; 32]);
            assert_eq!(
                aster_hpke_x25519_secret_from_identity(id_secret.as_ptr(), xs.as_mut_ptr()),
                OKS
            );
            assert_eq!(
                aster_hpke_x25519_public_from_identity(id_public.as_ptr(), xp.as_mut_ptr()),
                OKS
            );
            assert_eq!(
                aster_hpke_public_key_from_private(xs.as_ptr(), xp2.as_mut_ptr()),
                OKS
            );
            assert_eq!(xp, xp2);
            let hex_id = hex::encode(id_public);
            assert_eq!(
                aster_hpke_x25519_public_from_node_id(b(hex_id.as_bytes()), xp3.as_mut_ptr()),
                OKS
            );
            assert_eq!(xp, xp3);
            assert_eq!(
                aster_hpke_x25519_public_from_node_id(b(b"nothex"), xp3.as_mut_ptr()),
                INV
            );
            let small_order = [0u8; 32]; // not a valid point encoding for the map
            let mut x = [0u8; 32];
            assert_eq!(
                aster_hpke_x25519_public_from_identity(small_order.as_ptr(), x.as_mut_ptr()),
                INV
            );
        }
    }

    #[test]
    fn attestation_mint_verify_role() {
        unsafe {
            let (root, node) = ([1u8; 32], [2u8; 32]);
            let (mut root_pk, mut node_pk) = ([0u8; 32], [0u8; 32]);
            aster_secret_key_public_key(root.as_ptr(), root_pk.as_mut_ptr());
            aster_secret_key_public_key(node.as_ptr(), node_pk.as_mut_ptr());
            let mut chain = EMPTY_BYTES;
            assert_eq!(
                aster_attest_root_node(root.as_ptr(), node.as_ptr(), std::ptr::null(), &mut chain),
                OKS
            );
            let chain = take(chain);
            // Same bytes as the aster crate.
            let via_aster = aster::attestation::attest_root_node(
                &aster::SecretKey::from_bytes(root),
                &aster::SecretKey::from_bytes(node),
                &Default::default(),
            )
            .unwrap();
            assert_eq!(chain, via_aster.as_bytes());

            let mut text = EMPTY_BYTES;
            assert_eq!(aster_attestation_chain_to_text(b(&chain), &mut text), OKS);
            let text = take(text);
            assert_eq!(text, via_aster.to_text().as_bytes());
            let mut back = EMPTY_BYTES;
            assert_eq!(aster_attestation_chain_from_text(b(&text), &mut back), OKS);
            assert_eq!(take(back), chain);
            assert_eq!(
                aster_attestation_chain_from_text(b(b"nope"), &mut back),
                INV
            );

            let mut v: aster_verified_chain_t = std::mem::zeroed();
            assert_eq!(
                aster_attestation_verify_chain(
                    b(&chain),
                    root_pk.as_ptr(),
                    1,
                    node_pk.as_ptr(),
                    &mut v
                ),
                OKS
            );
            assert_eq!(v.depth, 1);
            assert_eq!(v.node, node_pk);
            assert_eq!(v.anchor, root_pk);
            assert_eq!(v.intermediates_count, 0);
            let mut role = 99u32;
            assert_eq!(
                aster_verified_chain_role_of(&v, node_pk.as_ptr(), &mut role),
                OKS
            );
            assert_eq!(
                role,
                aster_attestation_role_t::ASTER_ATTESTATION_ROLE_NODE as u32
            );
            assert_eq!(
                aster_verified_chain_role_of(&v, root_pk.as_ptr(), &mut role),
                OKS
            );
            assert_eq!(
                role,
                aster_attestation_role_t::ASTER_ATTESTATION_ROLE_ANCHOR as u32
            );
            let stranger = [5u8; 32];
            assert_eq!(
                aster_verified_chain_role_of(&v, stranger.as_ptr(), &mut role),
                OKS
            );
            assert_eq!(
                role,
                aster_attestation_role_t::ASTER_ATTESTATION_ROLE_NONE as u32
            );
            assert_eq!(aster_verified_chain_release(&mut v), OKS);
            assert_eq!(aster_verified_chain_release(&mut v), OKS);

            // Untrusted anchor, wrong node, garbage -> VERIFICATION_FAILED.
            assert_eq!(
                aster_attestation_verify_chain(
                    b(&chain),
                    node_pk.as_ptr(),
                    1,
                    node_pk.as_ptr(),
                    &mut v
                ),
                VF
            );
            assert_eq!(
                aster_attestation_verify_chain(
                    b(&chain),
                    root_pk.as_ptr(),
                    1,
                    root_pk.as_ptr(),
                    &mut v
                ),
                VF
            );
            assert_eq!(
                aster_attestation_verify_chain(
                    b(b"junk"),
                    root_pk.as_ptr(),
                    1,
                    node_pk.as_ptr(),
                    &mut v
                ),
                VF
            );
            assert!(v.storage.is_null());
            // Expired.
            let opts = aster_attest_options_t {
                struct_size: std::mem::size_of::<aster_attest_options_t>() as u32,
                reserved: 0,
                epoch: 3,
                not_before: 0,
                not_after: 1,
            };
            let mut expired = EMPTY_BYTES;
            assert_eq!(
                aster_attest_root_node(root.as_ptr(), node.as_ptr(), &opts, &mut expired),
                OKS
            );
            let expired = take(expired);
            assert_eq!(
                aster_attestation_verify_chain(
                    b(&expired),
                    root_pk.as_ptr(),
                    1,
                    node_pk.as_ptr(),
                    &mut v
                ),
                VF
            );
            let bad = aster_attest_options_t {
                struct_size: 8,
                ..opts
            };
            let mut none = EMPTY_BYTES;
            assert_eq!(
                aster_attest_root_node(root.as_ptr(), node.as_ptr(), &bad, &mut none),
                INV
            );
            assert!(none.ptr.is_null());
        }
    }
}
