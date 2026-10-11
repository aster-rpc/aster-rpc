//go:build cgo

package aster

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"errors"
	"testing"
	"time"
)

func TestTLSCertificate_GenerationAndSignedBinding(t *testing.T) {
	secret, err := GenerateSecretKey()
	if err != nil {
		t.Fatal(err)
	}
	public, err := SecretKeyPublicKey(secret)
	if err != nil {
		t.Fatal(err)
	}
	expectedID := hex.EncodeToString(public[:])
	for _, webTransport := range []bool{false, true} {
		for _, signed := range []bool{false, true} {
			name := "self-signed"
			if webTransport {
				name = "webtransport"
			}
			if signed {
				name += "/signed"
			} else {
				name += "/unbound"
			}
			t.Run(name, func(t *testing.T) {
				var nodeSecret []byte
				if signed {
					nodeSecret = secret[:]
				}
				var generated *TLSCertificate
				var err error
				if webTransport {
					generated, err = GenerateWebTransportCertificate("display-only", []string{"127.0.0.1"}, nodeSecret)
				} else {
					generated, err = GenerateSelfSignedCertificate([]string{"localhost", "127.0.0.1"}, nodeSecret)
				}
				if err != nil {
					t.Fatal(err)
				}
				pair, err := tls.X509KeyPair(generated.CertificatePEM, generated.PrivateKeyPEM)
				if err != nil {
					t.Fatal(err)
				}
				cert, err := x509.ParseCertificate(pair.Certificate[0])
				if err != nil {
					t.Fatal(err)
				}
				if hash := sha256.Sum256(cert.Raw); hash != generated.SHA256 {
					t.Fatal("certificate DER hash mismatch")
				}
				if err := cert.VerifyHostname("localhost"); err != nil {
					t.Fatal(err)
				}
				if err := cert.VerifyHostname("127.0.0.1"); err != nil {
					t.Fatal(err)
				}
				key, ok := cert.PublicKey.(*ecdsa.PublicKey)
				if !ok || key.Curve != elliptic.P256() {
					t.Fatalf("public key: %T", cert.PublicKey)
				}
				if webTransport && (cert.NotAfter.Sub(cert.NotBefore) > 14*24*time.Hour || time.Now().Before(cert.NotBefore) || time.Now().After(cert.NotAfter)) {
					t.Fatal("certificate unsuitable WebTransport validity")
				}
				err = VerifyCertificateBinding(generated.CertificatePEM, expectedID)
				if signed && err != nil {
					t.Fatal(err)
				}
				if !signed && !errors.Is(err, ErrInvalidArgument) {
					t.Fatalf("unbound certificate accepted: %v", err)
				}
				if err := VerifyCertificateBinding(generated.CertificatePEM, hex.EncodeToString(make([]byte, 32))); !errors.Is(err, ErrInvalidArgument) {
					t.Fatalf("wrong expected identity: %v", err)
				}
			})
		}
	}
}

func TestTLSCertificate_InvalidInputsAndDefaultSAN(t *testing.T) {
	if cert, err := GenerateSelfSignedCertificate(nil, []byte{1}); cert != nil || !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("short secret: cert=%v err=%v", cert, err)
	}
	if cert, err := GenerateWebTransportCertificate("subject", nil, make([]byte, 33)); cert != nil || !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("long secret: cert=%v err=%v", cert, err)
	}
	if err := VerifyCertificateBinding([]byte("not PEM"), "not an identity"); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("malformed verification input: %v", err)
	}
	generated, err := GenerateSelfSignedCertificate(nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	pair, err := tls.X509KeyPair(generated.CertificatePEM, generated.PrivateKeyPEM)
	if err != nil {
		t.Fatal(err)
	}
	cert, err := x509.ParseCertificate(pair.Certificate[0])
	if err != nil {
		t.Fatal(err)
	}
	if err := cert.VerifyHostname("localhost"); err != nil {
		t.Fatal(err)
	}
}
