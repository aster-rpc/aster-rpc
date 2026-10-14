//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"sync"
	"time"
	"unsafe"
)

// Enrollment (C ABI 5.1), driven by AsterConfig exactly as the Rust
// AsterServer / AsterClient are (design
// docs/_internal/working_ideas/aster-enrollment.md §15): StartServer with a
// config that enables enrollment admits only nodes holding a credential from
// the configured root and turns each peer's verified claims into the roles
// Requires checks, per call; StartClient presents its credential, enrolls
// with each server it connects to, and renews. Root operations (namespace
// create/bind, promote, revoke, reauthorize, rotate, delegate) stay in the
// aster CLI.

// EnrollmentALPN is the ALPN of the reciprocal enrollment handshake.
const EnrollmentALPN = "aster.enrollment/1"

// ASTER_EVENT_ENROLLMENT_RESULT completes an Enrollment operation (JSON payload).
const ASTER_EVENT_ENROLLMENT_RESULT = C.ASTER_EVENT_ENROLLMENT_RESULT

const (
	enrollmentSideServer = C.ASTER_ENROLLMENT_SIDE_SERVER
	enrollmentSideClient = C.ASTER_ENROLLMENT_SIDE_CLIENT
)

// EnrollmentConfig is the [enrollment] section (Rust aster::EnrollmentConfig;
// env ASTER_ENROLLMENT_*). The root and this node's credential live in
// AsterConfig's trust fields. Once enabled every value is validated when the
// server or client starts, and any error stops the start: enrollment never
// degrades to an open node.
type EnrollmentConfig struct {
	// Enabled forces enrollment on or off; nil enables it exactly when
	// Audience, FleetNamespace, IssuerDelegationFile or a credential is set.
	// TOML enabled; env ASTER_ENROLLMENT_ENABLED.
	Enabled *bool `json:"enabled"`
	// Audience is the fleet audience credentials must name. Required.
	// TOML audience; env ASTER_ENROLLMENT_AUDIENCE.
	Audience string `json:"audience,omitempty"`
	// FleetNamespace is the fleet policy namespace (64 hex, aster enroll
	// namespace create). Required. TOML fleet_namespace; env
	// ASTER_ENROLLMENT_FLEET_NAMESPACE.
	FleetNamespace string `json:"fleet_namespace,omitempty"`
	// StatePath keeps denials and watermarks durably. Default
	// <StoragePath>/enrollment.json; a server with neither refuses to
	// start, a client uses a temporary directory. TOML state_path; env
	// ASTER_ENROLLMENT_STATE_PATH.
	StatePath string `json:"state_path,omitempty"`
	// Replicas are policy replicas (aster1 tickets or 64-hex node ids) a
	// server enrolls with and synchronizes the fleet namespace from at
	// start. Empty: the server holds the fleet namespace and the root
	// publishes into it (aster enroll ... --peer <server>). TOML replicas;
	// env ASTER_ENROLLMENT_REPLICAS (comma-separated).
	Replicas []string `json:"replicas"`
	// Policy is the admission policy; nil = PolicyTolerateStale(): any
	// valid credential from the root for the audience, minus every known
	// revocation. TOML policy (inline table or string); env
	// ASTER_ENROLLMENT_POLICY (JSON).
	Policy *Policy `json:"policy"`
	// RequireSynchronizedClock refuses to start on an unsynchronized clock
	// (Linux). TOML require_synchronized_clock.
	RequireSynchronizedClock bool `json:"require_synchronized_clock"`
	// ActivationLeewaySecs is the iat/nbf leeway, 0-300; nil = 60. TOML
	// activation_leeway_secs.
	ActivationLeewaySecs *uint64 `json:"activation_leeway_secs"`
	// Limits overrides Enrollment's limits ([enrollment.limits]).
	Limits EnrollmentLimits `json:"limits"`
	// Roles maps verified claims to Gate-3 roles ([enrollment.roles]).
	Roles RoleMapping `json:"roles"`
	// IssuerDelegationFile makes a server a delegated issuer serving
	// aster.enrollment.IssuerV1 (aster enroll delegate --out). A client
	// with it set is refused. TOML [enrollment.issuer] delegation_file; env
	// ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE.
	IssuerDelegationFile string `json:"issuer_delegation_file,omitempty"`
	// IssuerLifetimeSecs is the lifetime of issued credentials, shortened
	// to the delegation; 0 = 24 h. TOML [enrollment.issuer] lifetime
	// (seconds or "24h"); env ASTER_ENROLLMENT_ISSUER_LIFETIME.
	IssuerLifetimeSecs uint64 `json:"issuer_lifetime_secs,omitempty"`
	// RenewFrom is the issuer this node renews from (ticket or node id).
	// TOML renew_from; env ASTER_ENROLLMENT_RENEW_FROM.
	RenewFrom string `json:"renew_from,omitempty"`
	// Renew = false disables renewal; nil renews from RenewFrom, or a
	// client from the server that signed its credential. TOML renew.
	Renew *bool `json:"renew"`
	// Synchronize (client; nil = true) synchronizes the fleet namespace the
	// server offers, so the client learns revocations of the server too.
	Synchronize *bool `json:"synchronize"`
}

// EnrollmentLimits are [enrollment.limits] overrides; 0 keeps the default.
// Env ASTER_ENROLLMENT_<FIELD> (e.g. ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS).
type EnrollmentLimits struct {
	HandshakeTimeoutMs       uint64 `json:"handshake_timeout_ms,omitempty"`
	StepTimeoutMs            uint64 `json:"step_timeout_ms,omitempty"`
	PolicyTimeoutMs          uint64 `json:"policy_timeout_ms,omitempty"`
	ReconciliationIntervalMs uint64 `json:"reconciliation_interval_ms,omitempty"`
	MaxAdmittedPeers         uint64 `json:"max_admitted_peers,omitempty"`
	// SyncTimeoutMs bounds enrolling with and synchronizing from a replica
	// or server (default 15000).
	SyncTimeoutMs uint64 `json:"sync_timeout_ms,omitempty"`
}

// RoleMapping turns a peer's verified claims into the attributes Gate 3
// checks Requires against ([enrollment.roles]; Rust RoleMapping). Roles =
// the comma-separated values of the string claim attribute Attribute,
// plus every verified scope when ScopesAsRoles, plus the roles Scopes and
// Tags map each verified scope or tag to; joined, sorted, into aster.role.
// It is evaluated per call from the peer's live session: a revocation,
// promotion or renewal applies on the next call.
type RoleMapping struct {
	// Attribute is the claim attribute listing roles; nil = "aster.role",
	// "" disables it. TOML attribute; env ASTER_ENROLLMENT_ROLE_ATTRIBUTE.
	Attribute *string `json:"attribute"`
	// ScopesAsRoles makes every verified scope a role.
	ScopesAsRoles bool `json:"scopes_as_roles"`
	// Scopes and Tags map a verified scope or tag to roles.
	Scopes map[string][]string `json:"scopes,omitempty"`
	Tags   map[string][]string `json:"tags,omitempty"`
	// IncludeAttributes passes the other claim attributes to handlers as
	// strings (CallInfo.Attributes); nil = true.
	IncludeAttributes *bool `json:"include_attributes"`
}

// IsEnrollmentEnabled reports whether StartServer / StartClient install
// Enrollment from this config: Enrollment.Enabled when set, else whether an
// audience, fleet namespace, issuer delegation or credential is configured.
// The root alone does not enable it. (Rust AsterConfig::is_enrollment_enabled.)
func (c *AsterConfig) IsEnrollmentEnabled() bool {
	if c.Enrollment.Enabled != nil {
		return *c.Enrollment.Enabled
	}
	e := &c.Enrollment
	return strings.TrimSpace(e.Audience) != "" || strings.TrimSpace(e.FleetNamespace) != "" ||
		strings.TrimSpace(e.IssuerDelegationFile) != "" || strings.TrimSpace(c.EnrollmentCredentialFile) != "" ||
		strings.TrimSpace(c.EnrollmentCredentialChain) != ""
}

// trustJSON is the [trust] + [enrollment] JSON of the C ABI
// (aster_config_enrollment_from_*, aster_node_start_enrolled).
type trustJSON struct {
	RootPubkey                string           `json:"root_pubkey,omitempty"`
	RootPubkeyFile            string           `json:"root_pubkey_file,omitempty"`
	NextRootCommitment        string           `json:"next_root_commitment,omitempty"`
	RootSeq                   uint64           `json:"root_seq"`
	RootRotations             []string         `json:"root_rotations"`
	EnrollmentCredentialFile  string           `json:"enrollment_credential_file,omitempty"`
	EnrollmentCredentialChain string           `json:"enrollment_credential_chain,omitempty"`
	Enrollment                EnrollmentConfig `json:"enrollment"`
}

func (c *AsterConfig) trustJSON() ([]byte, error) {
	t := trustJSON{
		RootPubkeyFile:            c.RootPubkeyFile,
		NextRootCommitment:        c.NextRootCommitment,
		RootSeq:                   c.RootSeq,
		RootRotations:             append([]string{}, c.RootRotations...),
		EnrollmentCredentialFile:  c.EnrollmentCredentialFile,
		EnrollmentCredentialChain: c.EnrollmentCredentialChain,
		Enrollment:                c.Enrollment,
	}
	if len(c.RootPubkey) > 0 {
		if len(c.RootPubkey) != 32 {
			return nil, fmt.Errorf("RootPubkey: %d bytes, want 32: %w", len(c.RootPubkey), ErrInvalidArgument)
		}
		t.RootPubkey = hex.EncodeToString(c.RootPubkey)
	}
	if t.Enrollment.Replicas == nil {
		t.Enrollment.Replicas = []string{}
	}
	return json.Marshal(t)
}

// applyTrustJSON takes the owned JSON Rust resolved and sets the trust and
// enrollment fields from it.
func (c *AsterConfig) applyTrustJSON(b C.aster_bytes_t) error {
	var t trustJSON
	if err := takeJSON(b, &t); err != nil {
		return fmt.Errorf("enrollment config: %w", err)
	}
	c.RootPubkey = nil
	if t.RootPubkey != "" {
		key, err := hex.DecodeString(t.RootPubkey)
		if err != nil || len(key) != 32 {
			return fmt.Errorf("root_pubkey: want 64 hex characters: %w", ErrInvalidArgument)
		}
		c.RootPubkey = key
	}
	c.RootPubkeyFile = t.RootPubkeyFile
	c.NextRootCommitment = t.NextRootCommitment
	c.RootSeq = t.RootSeq
	c.RootRotations = t.RootRotations
	c.EnrollmentCredentialFile = t.EnrollmentCredentialFile
	c.EnrollmentCredentialChain = t.EnrollmentCredentialChain
	c.Enrollment = t.Enrollment
	return nil
}

// ─── Policy ───────────────────────────────────────────────────────────────

// Policy is a declarative admission policy, evaluated in Rust (PolicyConfig
// / PolicyExpressionV1). Build one with the Policy* constructors, or set it
// in TOML ([enrollment] policy) or ASTER_ENROLLMENT_POLICY.
type Policy struct{ v any }

// MarshalJSON encodes the policy as the PolicyConfig JSON.
func (p *Policy) MarshalJSON() ([]byte, error) {
	if p == nil || p.v == nil {
		return []byte("null"), nil
	}
	return json.Marshal(p.v)
}

// UnmarshalJSON keeps a PolicyConfig JSON tree as is (Rust validates it).
func (p *Policy) UnmarshalJSON(data []byte) error {
	p.v = json.RawMessage(append([]byte(nil), data...))
	return nil
}

func policyList(ps []*Policy) []any {
	out := make([]any, len(ps))
	for i, p := range ps {
		out[i] = p
	}
	return out
}

// PolicyAllOf matches when every child matches (unknown stays unknown).
func PolicyAllOf(ps ...*Policy) *Policy { return &Policy{map[string]any{"all_of": policyList(ps)}} }

// PolicyAnyOf matches when any child matches.
func PolicyAnyOf(ps ...*Policy) *Policy { return &Policy{map[string]any{"any_of": policyList(ps)}} }

// PolicyNoneOf matches when no child matches.
func PolicyNoneOf(ps ...*Policy) *Policy {
	return &Policy{map[string]any{"none_of": policyList(ps)}}
}

// PolicyScope requires the scope among the evidence's scopes.
func PolicyScope(scope string) *Policy { return &Policy{map[string]any{"scope": scope}} }

// PolicyTag requires the tag among the evidence's tags.
func PolicyTag(tag string) *Policy { return &Policy{map[string]any{"tag": tag}} }

// PolicyAudience requires the audience among the evidence's audiences.
func PolicyAudience(audience string) *Policy {
	return &Policy{map[string]any{"audience": audience}}
}

// PolicySourceCIDR requires a direct path from inside the network (e.g. "10.0.0.0/8").
func PolicySourceCIDR(cidr string) *Policy { return &Policy{map[string]any{"source_cidr": cidr}} }

// PolicyAttributeEquals requires a typed attribute value.
func PolicyAttributeEquals(key string, value Attribute) *Policy {
	return &Policy{map[string]any{"attribute_equals": map[string]any{"key": key, "value": value}}}
}

// PolicyTolerateStale explicitly accepts cached/offline policy (known
// revocations still deny). The default policy.
func PolicyTolerateStale() *Policy { return &Policy{"tolerate_stale_policy"} }

// PolicySynchronizedWithin requires a policy synchronization at most maxAge
// old (from the root itself when requireRoot).
func PolicySynchronizedWithin(maxAge time.Duration, requireRoot bool) *Policy {
	return &Policy{map[string]any{"policy_synchronized_within": map[string]any{
		"max_age_secs": uint64(maxAge / time.Second), "require_root": requireRoot}}}
}

// PolicyRequireRegistration admits only peers the root registered (a
// presented chain alone does not match).
func PolicyRequireRegistration() *Policy { return &Policy{"require_registration"} }

// PolicyIssuedBy requires the key (64 hex) on the presented chain's issuer path.
func PolicyIssuedBy(key string) *Policy { return &Policy{map[string]any{"issued_by": key}} }

// PolicyMaxCredentialAge requires evidence issued at most maxAge ago.
func PolicyMaxCredentialAge(maxAge time.Duration) *Policy {
	return &Policy{map[string]any{"max_credential_age_secs": uint64(maxAge / time.Second)}}
}

// ─── Claims and evidence ──────────────────────────────────────────────────

// Attribute is a typed claim value: Type is "bool", "int", "float" or "string".
type Attribute struct {
	Type  string `json:"type"`
	Value any    `json:"value"`
}

// StringAttribute, IntAttribute, BoolAttribute and FloatAttribute build typed values.
func StringAttribute(v string) Attribute { return Attribute{"string", v} }
func IntAttribute(v int64) Attribute     { return Attribute{"int", v} }
func BoolAttribute(v bool) Attribute     { return Attribute{"bool", v} }
func FloatAttribute(v float64) Attribute { return Attribute{"float", v} }
func (a Attribute) String() string       { return fmt.Sprint(a.Value) }

// EnrollmentClaims are the claims a credential carries. A nil set is
// unknown; a non-nil empty set is complete and empty.
type EnrollmentClaims struct {
	Scopes     []string             `json:"scopes"`
	Tags       []string             `json:"tags"`
	Attributes map[string]Attribute `json:"attributes"`
}

// Registration is what an issued credential asks for.
type Registration string

const (
	// RegistrationEphemeral: the credential is the node's whole authority (default).
	RegistrationEphemeral Registration = "ephemeral"
	// RegistrationPersistent: admits the node until the root promotes it.
	RegistrationPersistent Registration = "persistent"
)

// PeerEvidence is the verified evidence a peer's admission rests on.
type PeerEvidence struct {
	Subject string `json:"subject"`
	// Root is the trusted root the evidence is anchored at.
	Root string `json:"root"`
	// Issuer signed the leaf (a delegated issuer, or the root); for a
	// registration record, the root.
	Issuer string `json:"issuer"`
	// Issuers is the chain's issuer path, leaf signer first (empty for a
	// registration).
	Issuers      []string             `json:"issuers"`
	Source       string               `json:"source"` // "chain" or "registration"
	ChainDigest  string               `json:"chain_digest"`
	DocumentID   string               `json:"document_id"`
	Revision     *uint64              `json:"revision"`
	CredentialID string               `json:"credential_id"`
	Audience     []string             `json:"audience"`
	Scopes       []string             `json:"scopes"`
	Tags         []string             `json:"tags"`
	Attributes   map[string]Attribute `json:"attributes"`
	IssuedAt     uint64               `json:"issued_at"`
	NotBefore    *uint64              `json:"not_before"`
	ExpiresAt    *uint64              `json:"expires_at"`
	// Registration is what a presented credential asks for; "" for a
	// registration record (the long-lived authority itself).
	Registration Registration `json:"registration"`
}

// Registered reports whether a root-authored registration admits the peer.
func (e *PeerEvidence) Registered() bool { return e.Source == "registration" }

// HasScope reports whether scope is among the (known) scopes.
func (e *PeerEvidence) HasScope(scope string) bool {
	for _, s := range e.Scopes {
		if s == scope {
			return true
		}
	}
	return false
}

// Readiness describes a peer's admission (membership only, never operation
// permission).
type Readiness struct {
	Peer           string      `json:"peer"`
	Source         string      `json:"source"` // "chain" or "registration"
	ObservedSource string      `json:"observed_source"`
	PolicySync     *PolicySync `json:"policy_sync"`
	LocalClock     string      `json:"local_clock"` // synchronized | unsynchronized | not_checked
}

// PolicySync is the last completed policy synchronization.
type PolicySync struct {
	CompletedAt      uint64  `json:"completed_at"`
	Source           string  `json:"source"` // "root" or the replica's node id
	ObservedRevision *uint64 `json:"observed_revision"`
}

// DenialReason says why a peer was denied. Definite reasons revoke sessions.
type DenialReason struct {
	Code     string          `json:"code"` // e.g. node_revoked, credential_revoked, issuer_revoked
	Definite bool            `json:"definite"`
	Detail   json.RawMessage `json:"detail"`
}

// Revocation reports a formerly admitted peer revoked by a definite denial;
// its connections are already closed and its roles are gone from the next
// call. Lagged > 0 instead means that many revocations were missed.
type Revocation struct {
	Peer   string       `json:"peer"`
	Reason DenialReason `json:"reason"`
	Lagged uint64       `json:"lagged"`
}

// EnrollmentStatus is this node's Enrollment state.
type EnrollmentStatus struct {
	Side           string        `json:"side"` // "server" or "client"
	TrustedRoot    string        `json:"trusted_root"`
	FleetNamespace string        `json:"fleet_namespace"`
	Audience       string        `json:"audience"`
	Credential     *PeerEvidence `json:"credential"` // follows renewals
	RootState      *struct {
		Root           string `json:"root"`
		NextCommitment string `json:"next_commitment"`
		Seq            uint64 `json:"seq"`
	} `json:"root_state"`
	RootRotations []string `json:"root_rotations"`
	// Renewing reports whether this node renews its credential.
	Renewing bool `json:"renewing"`
	// Issuer is the configured delegated issuer's key ("" when none).
	Issuer string `json:"issuer"`
}

// AdmissionErrorKind classifies an AdmissionError (aster_enrollment_error_t).
type AdmissionErrorKind uint32

const (
	AdmissionErrorOther       AdmissionErrorKind = C.ASTER_ENROLLMENT_ERROR_OTHER
	AdmissionErrorNotAdmitted AdmissionErrorKind = C.ASTER_ENROLLMENT_ERROR_NOT_ADMITTED
	// AdmissionErrorDenied is a definite denial; only this kind revokes.
	AdmissionErrorDenied      AdmissionErrorKind = C.ASTER_ENROLLMENT_ERROR_DENIED
	AdmissionErrorClockSkew   AdmissionErrorKind = C.ASTER_ENROLLMENT_ERROR_CLOCK_SKEW
	AdmissionErrorUnavailable AdmissionErrorKind = C.ASTER_ENROLLMENT_ERROR_UNAVAILABLE
)

// AdmissionError is why an Enrollment operation did not admit a peer.
type AdmissionError struct {
	Kind    AdmissionErrorKind
	Message string
	err     error
}

func (e *AdmissionError) Error() string { return e.Message }
func (e *AdmissionError) Unwrap() error { return e.err }

// ─── Enrollment ───────────────────────────────────────────────────────────

// Enrollment is the Enrollment a server or client installed from its
// config (AsterServer.Enrollment, AsterClient.Enrollment; Rust
// aster::enrollment::Enrollment). Nothing here is needed for roles or
// revocation, which apply automatically; these are escape hatches for
// audit, diagnostics and checks before a sensitive operation. The node owns
// it: closing the server or client releases it.
type Enrollment struct {
	rt     *Runtime
	handle uint64
	mu     sync.Mutex
	closed bool
	// enrolled: servers a client has enrolled with (node id → done).
	enrolled map[string]bool
}

func newEnrollment(rt *Runtime, handle uint64) *Enrollment {
	return &Enrollment{rt: rt, handle: handle, enrolled: map[string]bool{}}
}

func (e *Enrollment) live() (uint64, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.closed {
		return 0, fmt.Errorf("enrollment closed: %w", ErrAlreadyClosed)
	}
	return e.handle, nil
}

// Close releases the enrollment early (closing the node does it too):
// pending NextRevocation calls end with ErrAlreadyClosed and a client stops
// renewing.
func (e *Enrollment) Close() error {
	e.mu.Lock()
	if e.closed {
		e.mu.Unlock()
		return nil
	}
	e.closed = true
	e.mu.Unlock()
	return ffiStatus(func() C.int32_t {
		return C.aster_enrollment_free(C.uint64_t(e.rt.handle), C.aster_enrollment_t(e.handle))
	})
}

// result runs an async Enrollment operation and decodes its JSON result.
func (e *Enrollment) result(ctx context.Context, what string, out any,
	call func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t) error {
	h, err := e.live()
	if err != nil {
		return err
	}
	var op C.aster_operation_t
	if err := wrapCall(what, func() C.int32_t { return call(C.aster_enrollment_t(h), &op) }); err != nil {
		return err
	}
	ev, err := e.rt.await(ctx, uint64(op))
	if err != nil {
		return fmt.Errorf("%s: %w", what, err)
	}
	if ev.Kind != ASTER_EVENT_ENROLLMENT_RESULT {
		kind := AdmissionErrorKind(ev.Flags)
		cause := e.rt.eventError(ev, what)
		if kind != AdmissionErrorOther {
			return &AdmissionError{Kind: kind, Message: cause.Error(), err: cause}
		}
		return cause
	}
	payload := e.rt.takePayload(ev)
	if out == nil {
		return nil
	}
	if err := json.Unmarshal(payload, out); err != nil {
		return fmt.Errorf("%s: decode result: %w", what, err)
	}
	return nil
}

type addrJSON struct {
	NodeID          string   `json:"node_id"`
	RelayURL        string   `json:"relay_url,omitempty"`
	DirectAddresses []string `json:"direct_addresses"`
}

// enroll runs the client sequence against server once (design §15.7):
// reciprocal handshake, fleet namespace sync, renewal when the server signed
// this node's credential (or RenewFrom is set).
func (e *Enrollment) enroll(ctx context.Context, server *NodeAddr) error {
	e.mu.Lock()
	done := e.enrolled[server.EndpointID]
	e.mu.Unlock()
	if done {
		return nil
	}
	body, err := json.Marshal(addrJSON{NodeID: server.EndpointID, RelayURL: server.RelayURL,
		DirectAddresses: append([]string{}, server.DirectAddresses...)})
	if err != nil {
		return err
	}
	var a cArena
	defer a.free()
	err = e.result(ctx, "aster_enrollment_enroll", nil, func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t {
		return C.aster_enrollment_enroll(C.uint64_t(e.rt.handle), h, a.bytes(body), 0, op)
	})
	if err != nil {
		return err
	}
	e.mu.Lock()
	e.enrolled[server.EndpointID] = true
	e.mu.Unlock()
	return nil
}

// CheckPeer re-evaluates an admitted peer against current policy (call it
// before a sensitive operation). A *AdmissionError says why not; only
// AdmissionErrorDenied revokes the session.
func (e *Enrollment) CheckPeer(ctx context.Context, peer string) (Readiness, error) {
	var out Readiness
	var a cArena
	defer a.free()
	err := e.result(ctx, "aster_enrollment_check_peer", &out, func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t {
		return C.aster_enrollment_check_peer(C.uint64_t(e.rt.handle), h, a.str(peer), 0, op)
	})
	return out, err
}

// takeJSON decodes and releases an owned JSON output.
func takeJSON(b C.aster_bytes_t, out any) error {
	if b.ptr == nil || b.len == 0 {
		return fmt.Errorf("empty native result: %w", ErrInternal)
	}
	data := C.GoBytes(unsafe.Pointer(b.ptr), C.int(b.len))
	C.aster_string_release(b.ptr, b.len)
	return json.Unmarshal(data, out)
}

// PeerClaims returns the verified evidence peer's admission session rests
// on (subject, issuer path, scopes, tags, attributes, validity), for audit.
// A server already maps it to roles on every call ([enrollment.roles]).
// ErrNotFound: the peer has no session (never enrolled, revoked, evicted).
func (e *Enrollment) PeerClaims(peer string) (*PeerEvidence, error) {
	h, err := e.live()
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	var out C.aster_bytes_t
	if err := wrapCall("aster_enrollment_peer_claims", func() C.int32_t {
		return C.aster_enrollment_peer_claims(
			C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.str(peer), &out)
	}); err != nil {
		return nil, err
	}
	var ev PeerEvidence
	return &ev, takeJSON(out, &ev)
}

// Status returns this node's Enrollment state.
func (e *Enrollment) Status() (EnrollmentStatus, error) {
	var st EnrollmentStatus
	h, err := e.live()
	if err != nil {
		return st, err
	}
	var out C.aster_bytes_t
	if err := wrapCall("aster_enrollment_status", func() C.int32_t {
		return C.aster_enrollment_status(
			C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), &out)
	}); err != nil {
		return st, err
	}
	return st, takeJSON(out, &st)
}

// Credential returns this node's current credential chain evidence (it
// changes as renewal replaces it).
func (e *Enrollment) Credential() (*PeerEvidence, error) {
	st, err := e.Status()
	if err != nil {
		return nil, err
	}
	return st.Credential, nil
}

// NextRevocation waits for the next revocation of a formerly admitted peer
// (buffered from start). Cancelling ctx consumes nothing; Close ends a
// pending wait with ErrAlreadyClosed. Roles and connections need no action:
// both are already gone. Use it for application state keyed by peer.
func (e *Enrollment) NextRevocation(ctx context.Context) (Revocation, error) {
	var out Revocation
	err := e.result(ctx, "aster_enrollment_next_revocation", &out, func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t {
		return C.aster_enrollment_next_revocation(C.uint64_t(e.rt.handle), h, 0, op)
	})
	return out, err
}

// ─── Configured issuer ────────────────────────────────────────────────────

// EnrollmentIssuer is the delegated issuer a server runs when
// Enrollment.IssuerDelegationFile is configured (AsterServer.Issuer; Rust
// aster::enrollment::Issuer). It already serves aster.enrollment.IssuerV1
// renewals; Issue enrolls new nodes.
type EnrollmentIssuer struct{ e *Enrollment }

// IssuerInfo identifies an issuer.
type IssuerInfo struct {
	PublicKey           string `json:"public_key"`
	DelegationExpiresAt uint64 `json:"delegation_expires_at"`
}

// IssuedCredential is a credential an issuer signed; hand Chain to the
// subject (it is public: possession without the subject's key is useless).
type IssuedCredential struct {
	Chain        string `json:"chain"`
	CredentialID string `json:"credential_id"`
	ExpiresAt    uint64 `json:"expires_at"`
}

// IssuerRefusal is why an issuer refused (codes as aster.enrollment.IssuerV1
// reports them: subject_denied, registered, policy_unavailable,
// foreign_credential, issuer_revoked, invalid_credential, invalid_argument).
type IssuerRefusal struct {
	Code      string `json:"code"`
	Message   string `json:"message"`
	Retryable bool   `json:"retryable"`
}

func (r *IssuerRefusal) Error() string {
	return "enrollment issuer refused: " + r.Code + ": " + r.Message
}

// Is matches ErrPermissionDenied.
func (r *IssuerRefusal) Is(target error) bool { return target == ErrPermissionDenied }

// Info returns the issuer's key and delegation expiry.
func (i *EnrollmentIssuer) Info() (IssuerInfo, error) {
	var info IssuerInfo
	h, err := i.e.live()
	if err != nil {
		return info, err
	}
	var out C.aster_bytes_t
	if err := wrapCall("aster_enrollment_issuer_info", func() C.int32_t {
		return C.aster_enrollment_issuer_info(
			C.uint64_t(i.e.rt.handle), C.aster_enrollment_t(h), &out)
	}); err != nil {
		return info, err
	}
	return info, takeJSON(out, &info)
}

// Issue signs a credential for subject (a node id the orchestrator has
// authenticated out of band). A refusal is an *IssuerRefusal.
func (i *EnrollmentIssuer) Issue(subject string, claims EnrollmentClaims, registration Registration) (IssuedCredential, error) {
	var issued IssuedCredential
	h, err := i.e.live()
	if err != nil {
		return issued, err
	}
	if registration == "" {
		registration = RegistrationEphemeral
	}
	body, err := json.Marshal(map[string]any{"subject": subject, "claims": claims, "registration": registration})
	if err != nil {
		return issued, err
	}
	var a cArena
	defer a.free()
	var out C.aster_bytes_t
	status, statusMsg := ffiCall(func() C.int32_t {
		return C.aster_enrollment_issue(C.uint64_t(i.e.rt.handle), C.aster_enrollment_t(h), a.bytes(body), &out)
	})
	if status == C.ASTER_STATUS_PERMISSION_DENIED && out.ptr != nil {
		var refusal IssuerRefusal
		if err := takeJSON(out, &refusal); err != nil {
			return issued, err
		}
		return issued, &refusal
	}
	if err := wrapStatusMsg("aster_enrollment_issue", status, statusMsg); err != nil {
		return issued, err
	}
	return issued, takeJSON(out, &issued)
}
