//! Stateless authority provisioning adapters. Canonical encoding and verification
//! live in Rust; private signing keys never cross these APIs.
use crate::{alloc_owned_bytes, aster_bytes_t, aster_status_t, set_last_error};
use aster::binding_support::provisioning as native;
use std::ptr;

/// Owned provisioning result. On success data contains the documented output;
/// on failure error contains UTF-8 JSON {"code": string, "detail": string}.
/// Both spans start empty. Release each nonempty span with aster_string_release.
/// No runtime is required. Verified JSON is metadata from native verified
/// accessors, not a transferable authorization capability.
#[repr(C)]
pub struct aster_provisioning_result_t {
    pub data: aster_bytes_t,
    pub error: aster_bytes_t,
}

type Failure = (aster_status_t, String, String);
type Output = Result<Vec<u8>, Failure>;
fn invalid(detail: impl Into<String>) -> Failure {
    (
        aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
        "invalid_input".into(),
        detail.into(),
    )
}
fn native_error(error: native::ProvisioningError) -> Failure {
    let status = match error.code() {
        "invalid_input" => aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
        "encoding" => aster_status_t::ASTER_STATUS_INTERNAL,
        _ => aster_status_t::ASTER_STATUS_VERIFICATION_FAILED,
    };
    (status, error.code().into(), error.detail().into())
}
unsafe fn bytes<'a>(value: aster_bytes_t) -> Result<&'a [u8], Failure> {
    if value.len == 0 {
        return Ok(&[]);
    }
    if value.ptr.is_null() || value.len > isize::MAX as usize {
        return Err(invalid("invalid input byte span"));
    }
    Ok(unsafe { std::slice::from_raw_parts(value.ptr, value.len) })
}
unsafe fn string<'a>(value: aster_bytes_t) -> Result<&'a str, Failure> {
    std::str::from_utf8(unsafe { bytes(value)? }).map_err(|_| invalid("expected UTF-8 input"))
}
unsafe fn run(out: *mut aster_provisioning_result_t, work: impl FnOnce() -> Output) -> i32 {
    if out.is_null() {
        set_last_error("NULL provisioning result");
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    unsafe {
        ptr::write(
            out,
            aster_provisioning_result_t {
                data: aster_bytes_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                error: aster_bytes_t {
                    ptr: ptr::null(),
                    len: 0,
                },
            },
        );
    }
    match work() {
        Ok(data) => {
            unsafe {
                (*out).data = alloc_owned_bytes(data);
            }
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err((status, code, detail)) => {
            let error = serde_json::json!({"code": code, "detail": detail}).to_string();
            set_last_error(&error);
            unsafe {
                (*out).error = alloc_owned_bytes(error.into_bytes());
            }
            status as i32
        }
    }
}

/// Canonically encode a v1 genesis from strict UTF-8 JSON. Fixed-width fields
/// are hex32; format_version is fixed at 1. The DTO preserves caller ordering,
/// and encoding alone does not establish validity or authorization.
/// Success data is opaque canonical Fory document bytes.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_canonical_genesis(
    json: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe {
        run(out, || {
            native::canonical_genesis_json(string(json)?).map_err(native_error)
        })
    }
}

/// Canonically encode a v1 join permit from strict UTF-8 JSON. The candidate
/// ticket is derived natively and must not be supplied. Success data is opaque
/// canonical Fory bytes. Encoding alone does not grant membership.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_canonical_join_permit(
    json: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe {
        run(out, || {
            native::canonical_join_permit_json(string(json)?).map_err(native_error)
        })
    }
}

/// Exact domain-separated genesis preimage for an external Ed25519 signer.
/// This accepts opaque bytes and does not verify their content.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_genesis_signing_bytes(
    document: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe { run(out, || Ok(native::genesis_signing_bytes(bytes(document)?))) }
}

/// Exact domain-separated join preimage for an external Ed25519 signer.
/// This accepts opaque bytes and does not verify their content.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_join_signing_bytes(
    document: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe { run(out, || Ok(native::join_signing_bytes(bytes(document)?))) }
}

/// Assemble an opaque signed document from exact document bytes, raw 32-byte
/// authority public key and raw 64-byte external signature. Does not verify.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_signed_document_from_parts(
    document: aster_bytes_t,
    authority_key: aster_bytes_t,
    signature: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe {
        run(out, || {
            native::signed_document_from_parts(
                bytes(document)?,
                bytes(authority_key)?,
                bytes(signature)?,
            )
            .map_err(native_error)
        })
    }
}

/// Verify genesis against independently configured raw public key and UTF-8
/// domain pins. Returns UTF-8 JSON exclusively from VerifiedGenesis accessors.
/// Never uses the envelope's key as the trust anchor. No group is created.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_verify_genesis(
    authority_key: aster_bytes_t,
    authority_domain: aster_bytes_t,
    envelope: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe {
        run(out, || {
            native::verify_genesis_json(
                bytes(authority_key)?,
                string(authority_domain)?,
                bytes(envelope)?,
            )
            .map(String::into_bytes)
            .map_err(native_error)
        })
    }
}

/// Verify a join permit against configured raw key, domain and raw candidate
/// node ID. Returns UTF-8 JSON from VerifiedJoinPermit accessors. This extracts
/// the signed group/genesis; it does not compare independent expectations for
/// them and does not commit membership or start a voter.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_verify_join_permit_for_start(
    authority_key: aster_bytes_t,
    authority_domain: aster_bytes_t,
    candidate: aster_bytes_t,
    envelope: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe {
        run(out, || {
            native::verify_join_permit_for_start_json(
                bytes(authority_key)?,
                string(authority_domain)?,
                bytes(candidate)?,
                bytes(envelope)?,
            )
            .map(String::into_bytes)
            .map_err(native_error)
        })
    }
}

/// Canonical addressless open ticket for a raw 32-byte candidate ID. Output is
/// UTF-8 text; caller-supplied route hints cannot enter the signed permit.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_canonical_candidate_ticket(
    node: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe {
        run(out, || {
            native::canonical_candidate_ticket(bytes(node)?)
                .map(String::into_bytes)
                .map_err(native_error)
        })
    }
}

/// Hash supplied genesis document bytes using the native domain separation.
/// Output is raw 32-byte digest. This makes no verification/authority claim;
/// use verify_genesis before acting on an externally supplied genesis.
/// # Safety
/// Input spans must be readable for this call; out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_coordination_genesis_hash(
    document: aster_bytes_t,
    out: *mut aster_provisioning_result_t,
) -> i32 {
    unsafe { run(out, || Ok(native::genesis_hash(bytes(document)?).to_vec())) }
}
