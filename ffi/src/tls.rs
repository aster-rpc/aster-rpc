//! Certificate helpers shared with the Rust HTTP/WebTransport transport.
use std::ptr;

use crate::{alloc_owned_bytes, aster_bytes_list_t, aster_bytes_t, aster_status_t, set_last_error};
use aster_transport_salvo::{
    generate_self_signed, generate_webtransport_cert, verify_cert_binding, GeneratedCert,
    NodeBinding,
};

/// Generated server PEM material and SHA-256 of certificate DER (not SPKI).
/// Release each PEM independently with aster_string_release(ptr, len).
/// No runtime is required; callers own and must protect the private key.
#[repr(C)]
pub struct aster_generated_cert_t {
    pub cert_pem: aster_bytes_t,
    pub key_pem: aster_bytes_t,
    pub sha256: [u8; 32],
}

fn invalid(message: impl ToString) -> i32 {
    set_last_error(message);
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

unsafe fn bytes<'a>(value: aster_bytes_t) -> Result<&'a [u8], String> {
    if value.len == 0 {
        return Ok(&[]);
    }
    if value.ptr.is_null() || value.len > isize::MAX as usize {
        return Err("invalid byte span".into());
    }
    Ok(unsafe { std::slice::from_raw_parts(value.ptr, value.len) })
}

unsafe fn string(value: aster_bytes_t) -> Result<String, String> {
    std::str::from_utf8(unsafe { bytes(value)? })
        .map(str::to_owned)
        .map_err(|_| "expected UTF-8".into())
}

unsafe fn generate(
    subject: Option<aster_bytes_t>,
    sans: aster_bytes_list_t,
    node_secret: aster_bytes_t,
    out: *mut aster_generated_cert_t,
) -> i32 {
    if out.is_null() {
        return invalid("NULL certificate output");
    }
    unsafe {
        ptr::write(
            out,
            aster_generated_cert_t {
                cert_pem: aster_bytes_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                key_pem: aster_bytes_t {
                    ptr: ptr::null(),
                    len: 0,
                },
                sha256: [0; 32],
            },
        );
    }
    let result = (|| -> Result<GeneratedCert, String> {
        if sans.len > isize::MAX as usize / std::mem::size_of::<aster_bytes_t>()
            || (sans.len != 0 && sans.items.is_null())
        {
            return Err("invalid SAN list".into());
        }
        let spans = if sans.len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(sans.items, sans.len) }
        };
        let sans = spans
            .iter()
            .map(|s| unsafe { string(*s) })
            .collect::<Result<Vec<_>, _>>()?;
        let secret = unsafe { bytes(node_secret)? };
        let binding = if secret.is_empty() {
            None
        } else {
            Some(NodeBinding::Signed(
                secret
                    .try_into()
                    .map_err(|_| "node secret must be exactly 32 bytes")?,
            ))
        };
        match subject {
            Some(subject) => {
                generate_webtransport_cert(&unsafe { string(subject)? }, binding, &sans)
            }
            None => generate_self_signed(binding, &sans),
        }
    })();
    match result {
        Ok(cert) => {
            unsafe {
                ptr::write(
                    out,
                    aster_generated_cert_t {
                        cert_pem: alloc_owned_bytes(cert.cert_pem),
                        key_pem: alloc_owned_bytes(cert.key_pem),
                        sha256: cert.sha256,
                    },
                );
            }
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(error) => invalid(error),
    }
}

/// Generate a self-signed ECDSA certificate. Empty SANs default to localhost.
/// node_secret is empty (no binding) or exactly 32 bytes (signed node binding).
/// Outputs are cleared on failure. Malformed inputs return INVALID_ARGUMENT.
///
/// # Safety
/// All nonempty input spans/lists must be readable, and out must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_tls_generate_self_signed(
    sans: aster_bytes_list_t,
    node_secret: aster_bytes_t,
    out: *mut aster_generated_cert_t,
) -> i32 {
    unsafe { generate(None, sans, node_secret, out) }
}

/// Generate a WebTransport pin-compatible P-256 certificate valid less than
/// 14 days. Includes localhost plus supplied SANs. subject labels the common
/// name; only node_secret establishes a signed identity binding.
/// The SHA-256 pin must reach clients over a trusted channel.
///
/// # Safety
/// Same input/output contract as aster_tls_generate_self_signed.
#[no_mangle]
pub unsafe extern "C" fn aster_tls_generate_webtransport(
    subject: aster_bytes_t,
    sans: aster_bytes_list_t,
    node_secret: aster_bytes_t,
    out: *mut aster_generated_cert_t,
) -> i32 {
    unsafe { generate(Some(subject), sans, node_secret, out) }
}

/// Verify that a trusted expected node ID authorized the certificate's TLS
/// public key. This does not verify certificate validity, hostname or PKI trust.
/// Unsigned, mismatched or malformed bindings return INVALID_ARGUMENT.
///
/// # Safety
/// Both nonempty byte spans must be readable for this call.
#[no_mangle]
pub unsafe extern "C" fn aster_tls_verify_binding(
    cert_pem: aster_bytes_t,
    expected_node_id: aster_bytes_t,
) -> i32 {
    let result = (|| {
        verify_cert_binding(unsafe { bytes(cert_pem)? }, &unsafe {
            string(expected_node_id)?
        })
    })();
    match result {
        Ok(()) => aster_status_t::ASTER_STATUS_OK as i32,
        Err(error) => invalid(error),
    }
}
