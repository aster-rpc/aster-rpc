//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"fmt"
	"unsafe"
)

// TLSCertificate contains copied PEM material and the SHA-256 of the complete
// DER certificate, suitable for WebTransport serverCertificateHashes. Treat
// PrivateKeyPEM as secret. Generation does not establish external trust.
type TLSCertificate struct {
	CertificatePEM []byte
	PrivateKeyPEM  []byte
	SHA256         [32]byte
}

// GenerateSelfSignedCertificate generates an ECDSA certificate. Empty SANs
// select localhost. An empty nodeSecret leaves it unbound; exactly 32 bytes
// adds a signed Aster node identity binding. No unsigned identity claim is made.
func GenerateSelfSignedCertificate(sans []string, nodeSecret []byte) (*TLSCertificate, error) {
	return generateTLSCertificate("", sans, nodeSecret, false)
}

// GenerateWebTransportCertificate generates an ECDSA P-256 certificate valid
// for at most 14 days, including localhost and the supplied SANs. Subject is a
// display name, not proof of identity. A 32-byte nodeSecret signs its Aster
// identity binding; nil leaves it unbound.
func GenerateWebTransportCertificate(subject string, sans []string, nodeSecret []byte) (*TLSCertificate, error) {
	return generateTLSCertificate(subject, sans, nodeSecret, true)
}

func generateTLSCertificate(subject string, sans []string, nodeSecret []byte, webTransport bool) (*TLSCertificate, error) {
	if len(nodeSecret) != 0 && len(nodeSecret) != 32 {
		return nil, fmt.Errorf("TLS node secret must be empty or exactly 32 bytes: %w", ErrInvalidArgument)
	}
	var arena cArena
	defer arena.free()
	var native C.aster_generated_cert_t
	// Release both outputs even if native validation or copying fails.
	defer func() {
		if native.cert_pem.ptr != nil {
			C.aster_string_release(native.cert_pem.ptr, native.cert_pem.len)
		}
		if native.key_pem.ptr != nil {
			C.aster_string_release(native.key_pem.ptr, native.key_pem.len)
		}
	}()
	var status C.int32_t
	var statusMsg string
	operation := "aster_tls_generate_self_signed"
	if webTransport {
		operation = "aster_tls_generate_webtransport"
		status, statusMsg = ffiCall(func() C.int32_t {
			return C.aster_tls_generate_webtransport(arena.str(subject), arena.list(sans), arena.bytes(nodeSecret), &native)
		})
	} else {
		status, statusMsg = ffiCall(func() C.int32_t {
			return C.aster_tls_generate_self_signed(arena.list(sans), arena.bytes(nodeSecret), &native)
		})
	}
	if err := wrapStatusMsg(operation, status, statusMsg); err != nil {
		return nil, err
	}
	result := &TLSCertificate{CertificatePEM: viewBytes(native.cert_pem), PrivateKeyPEM: viewBytes(native.key_pem)}
	copy(result.SHA256[:], unsafe.Slice((*byte)(unsafe.Pointer(&native.sha256[0])), 32))
	return result, nil
}

// VerifyCertificateBinding verifies a signed Aster binding of the TLS public
// key against a node ID learned through a trusted channel. Never use an ID
// taken only from the certificate itself. This does not verify the whole
// certificate, TLS chain, hostname, validity period or trust in the expected ID.
func VerifyCertificateBinding(certificatePEM []byte, trustedExpectedNodeID string) error {
	var arena cArena
	defer arena.free()
	return wrapCall("aster_tls_verify_binding", func() C.int32_t {
		return C.aster_tls_verify_binding(arena.bytes(certificatePEM), arena.str(trustedExpectedNodeID))
	})
}
