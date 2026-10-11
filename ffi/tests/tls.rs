//! Certificate ownership and signed-key verification across the actual C ABI.
use aster_transport_ffi::tls::*;
use aster_transport_ffi::*;
use std::{mem, ptr};
fn bytes(b: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: b.as_ptr(),
        len: b.len(),
    }
}
fn empty() -> aster_bytes_list_t {
    aster_bytes_list_t {
        items: ptr::null(),
        len: 0,
    }
}
unsafe fn release(cert: aster_generated_cert_t) {
    assert_eq!(
        unsafe { aster_string_release(cert.cert_pem.ptr, cert.cert_pem.len) },
        0
    );
    assert_eq!(
        unsafe { aster_string_release(cert.key_pem.ptr, cert.key_pem.len) },
        0
    );
}
#[test]
fn signed_generation_and_verification() {
    unsafe {
        let secret = [42u8; 32];
        let expected = iroh::SecretKey::from_bytes(&secret).public().to_string();
        let other = iroh::SecretKey::from_bytes(&[43u8; 32])
            .public()
            .to_string();
        let sans = [bytes(b"127.0.0.1"), bytes(b"example.test")];
        for wt in [false, true] {
            let mut out = mem::zeroed();
            let list = aster_bytes_list_t {
                items: sans.as_ptr(),
                len: sans.len(),
            };
            let status = if wt {
                aster_tls_generate_webtransport(bytes(b"test"), list, bytes(&secret), &mut out)
            } else {
                aster_tls_generate_self_signed(list, bytes(&secret), &mut out)
            };
            assert_eq!(status, 0);
            assert!(
                std::slice::from_raw_parts(out.cert_pem.ptr, out.cert_pem.len)
                    .starts_with(b"-----BEGIN CERTIFICATE-----")
            );
            assert!(out.key_pem.len > 100);
            assert_ne!(out.sha256, [0; 32]);
            assert_eq!(
                aster_tls_verify_binding(out.cert_pem, bytes(expected.as_bytes())),
                0
            );
            assert_eq!(
                aster_tls_verify_binding(out.cert_pem, bytes(other.as_bytes())),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            release(out);
        }
    }
}
#[test]
fn unsigned_and_invalid_inputs_fail_closed() {
    unsafe {
        let mut out = mem::zeroed();
        assert_eq!(
            aster_tls_generate_self_signed(empty(), bytes(&[]), &mut out),
            0
        );
        assert_ne!(
            aster_tls_verify_binding(out.cert_pem, bytes(b"untrusted")),
            0
        );
        release(out);
        let mut out = mem::zeroed();
        assert_ne!(
            aster_tls_generate_self_signed(empty(), bytes(&[0; 31]), &mut out),
            0
        );
        assert!(out.cert_pem.ptr.is_null());
        assert!(out.key_pem.ptr.is_null());
        assert_eq!(out.sha256, [0; 32]);
        assert_ne!(
            aster_tls_generate_self_signed(
                aster_bytes_list_t {
                    items: ptr::null(),
                    len: 1
                },
                bytes(&[]),
                &mut out
            ),
            0
        );
        assert_ne!(
            aster_tls_generate_webtransport(bytes(&[255]), empty(), bytes(&[]), &mut out),
            0
        );
        assert_ne!(aster_tls_verify_binding(bytes(b"bad PEM"), bytes(b"id")), 0);
        assert_ne!(
            aster_tls_generate_self_signed(empty(), bytes(&[]), ptr::null_mut()),
            0
        );
    }
}
#[test]
fn layout_is_stable() {
    if mem::size_of::<usize>() == 8 {
        assert_eq!(mem::size_of::<aster_generated_cert_t>(), 64);
        assert_eq!(mem::offset_of!(aster_generated_cert_t, cert_pem), 0);
        assert_eq!(mem::offset_of!(aster_generated_cert_t, key_pem), 16);
        assert_eq!(mem::offset_of!(aster_generated_cert_t, sha256), 32);
    }
}
