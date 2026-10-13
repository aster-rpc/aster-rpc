// Package testroot plays an Enrollment root in tests and self-contained
// demos: it signs the credentials and delegations `aster enroll node` and
// `aster enroll delegate` produce (compact Ed25519 JWT delegation chains)
// and builds the revocation record `aster enroll revoke` publishes. It is
// not part of the binding's API: root operations belong to the aster CLI.
// Rust verifying links signed here doubles as a cross-implementation check.
package testroot

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"time"
)

var b64 = base64.RawURLEncoding

// Root signs as the root key.
type Root struct {
	key      ed25519.PrivateKey
	audience string
}

// New is a root from a 32-byte seed, for one fleet audience.
func New(seed []byte, audience string) *Root {
	return &Root{key: ed25519.NewKeyFromSeed(seed), audience: audience}
}

// Generate is a root with a fresh random key.
func Generate(audience string) (*Root, error) {
	seed := make([]byte, 32)
	if _, err := rand.Read(seed); err != nil {
		return nil, err
	}
	return New(seed, audience), nil
}

// PublicKey is the root public key, 64 hex ([trust] root_pubkey).
func (r *Root) PublicKey() string {
	return hex.EncodeToString(r.key.Public().(ed25519.PublicKey))
}

// Seed is the root's secret seed (for a node that is the root itself).
func (r *Root) Seed() []byte { return r.key.Seed() }

// Claims are a credential's claims. Nil sets are unknown.
type Claims struct {
	Scopes     []string       `json:"scopes"`
	Tags       []string       `json:"tags"`
	Attributes map[string]any `json:"attributes"`
}

// RoleClaims carries roles in the aster.role attribute (comma-separated),
// what `aster enroll node --attributes roles.json` writes with
// {"aster.role": {"type": "string", "value": "<roles>"}}. Empty roles: no
// attribute.
func RoleClaims(roles string) Claims {
	attributes := map[string]any{}
	if roles != "" {
		attributes["aster.role"] = map[string]any{"type": "string", "value": roles}
	}
	return Claims{Scopes: []string{}, Tags: []string{}, Attributes: attributes}
}

func (r *Root) link(typ, subject string, lifetime time.Duration, extension string, body any) (string, error) {
	sub, err := hex.DecodeString(subject)
	if err != nil || len(sub) != 32 {
		return "", fmt.Errorf("subject %q: want a 64-hex node id", subject)
	}
	jti := make([]byte, 16)
	if _, err := rand.Read(jti); err != nil {
		return "", err
	}
	now := time.Now().Unix()
	payload, err := json.Marshal(map[string]any{
		"iss":     r.PublicKey(),
		"sub":     subject,
		"cnf":     map[string]any{"jwk": map[string]string{"kty": "OKP", "crv": "Ed25519", "x": b64.EncodeToString(sub)}},
		"aud":     r.audience,
		"jti":     b64.EncodeToString(jti),
		"iat":     now,
		"exp":     now + int64(lifetime/time.Second),
		extension: body,
	})
	if err != nil {
		return "", err
	}
	header, _ := json.Marshal(map[string]string{"alg": "EdDSA", "typ": typ})
	input := b64.EncodeToString(header) + "." + b64.EncodeToString(payload)
	return input + "." + b64.EncodeToString(ed25519.Sign(r.key, []byte(input))), nil
}

// Credential enrolls subject (64-hex node id) with claims: a one-link,
// root-issued, ephemeral credential chain (aster enroll node --out).
func (r *Root) Credential(subject string, claims Claims, lifetime time.Duration) (string, error) {
	link, err := r.link("aster-enrollment-v1+jwt", subject, lifetime, "aster_enrollment_v1", map[string]any{
		"scopes": claims.Scopes, "tags": claims.Tags, "attributes": claims.Attributes,
		"mandatory": []string{}, "registration": "ephemeral",
	})
	if err != nil {
		return "", err
	}
	return "aster-chain-v1~" + link, nil
}

// Delegation lets subject (an online issuer) enroll ephemeral nodes carrying
// attributeKeys within scopes (aster enroll delegate --out).
func (r *Root) Delegation(subject string, scopes, attributeKeys []string, lifetime time.Duration) (string, error) {
	link, err := r.link("aster-delegation-v1+jwt", subject, lifetime, "aster_delegation_v1", map[string]any{
		"may_issue": []string{"enrollment"}, "audiences": []string{r.audience}, "scopes": scopes,
		"tags": []string{}, "attribute_keys": attributeKeys, "max_child_lifetime": 3600, "max_depth": 0,
		"subjects": nil, "operations": []string{}, "mandatory": []string{}, "may_register": false,
	})
	if err != nil {
		return "", err
	}
	return "aster-chain-v1~" + link, nil
}

// RegistrationKey is the fleet-namespace key of subject's registration record.
func RegistrationKey(subject string) string { return "aster/enrollment/v1/nodes/" + subject }

// Revocation is the root NodeRegistrationV1 record revoking subject (what
// aster enroll revoke --fleet-only publishes under RegistrationKey).
func (r *Root) Revocation(subject, reason string) []byte {
	record, _ := json.Marshal(map[string]any{
		"version": 1, "issuer": r.PublicKey(), "subject": subject,
		"audience": r.audience, "revision": 1, "issued_at": time.Now().Unix() - 60, "status": "revoked",
		"claims":    map[string]any{"scopes": []string{}, "tags": []string{}, "attributes": map[string]any{}},
		"mandatory": []string{}, "reauthorizes_revision": nil, "reason": reason,
	})
	return record
}

// Namespace is the fleet namespace id (64 hex) of a namespace secret
// (aster enroll namespace create --out-secret).
func Namespace(secret [32]byte) string {
	return hex.EncodeToString(ed25519.NewKeyFromSeed(secret[:]).Public().(ed25519.PublicKey))
}

// NodeID is the node id (64 hex) of a 32-byte secret key.
func NodeID(secret []byte) string {
	return hex.EncodeToString(ed25519.NewKeyFromSeed(secret).Public().(ed25519.PublicKey))
}
