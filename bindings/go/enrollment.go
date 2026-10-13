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
	"errors"
	"fmt"
	"os"
	"strings"
	"sync"
	"time"
	"unsafe"
)

// Enrollment (C ABI 5.1): admission on delegation chains, for enrolled nodes
// and delegated issuers. Root operations (namespace create/bind, promote,
// revoke, reauthorize, rotate, delegate) stay in the Rust CLI. Design:
// docs/_internal/working_ideas/aster-enrollment.md.

// EnrollmentALPN is the ALPN of the reciprocal enrollment handshake.
const EnrollmentALPN = "aster.enrollment/1"

// ASTER_EVENT_ENROLLMENT_RESULT completes an Enrollment operation (JSON payload).
const ASTER_EVENT_ENROLLMENT_RESULT = C.ASTER_EVENT_ENROLLMENT_RESULT

// EnrollmentConfig installs Enrollment on a node (NodeOptions.Enrollment,
// ServerOptions.Enrollment). Enrollment then decides the node's admission
// (Gate 0): only peers that complete the enrollment handshake under Policy
// reach its other ALPNs. Admission never grants operation access; map a
// peer's verified claims (Enrollment.PeerClaims) to roles for that.
type EnrollmentConfig struct {
	// Root is the trusted root public key (32 bytes). Empty: the node
	// config's RootPubkey / RootPubkeyFile.
	Root []byte
	// Audience is the fleet audience credentials must carry.
	Audience string
	// FleetNamespace is the fleet policy namespace id (64 hex) pinned to Root.
	FleetNamespace string
	// StatePath is the durable policy state file (denials and revision
	// watermarks survive restarts). Required.
	StatePath string
	// Credential is this node's credential chain (aster-chain-v1~...). Empty:
	// the contents of the node config's EnrollmentCredentialFile, if set;
	// otherwise none (a pure verifier).
	Credential string
	// Policy is the admission policy (required): compose the predicates
	// below, e.g. PolicyAllOf(PolicyScope("session"), PolicyTolerateStale()).
	Policy *Policy
	// Rotation accepts root rotations continuing the pinned root.
	Rotation *RotationPin
	// AppliedRotations are rotation statements that produced Root, oldest first.
	AppliedRotations []string
	// RequireSynchronizedClock refuses to install while the kernel reports
	// the clock unsynchronized (Linux).
	RequireSynchronizedClock bool
	// ActivationLeeway tolerates an issuer clock this far ahead (0 = 60 s).
	ActivationLeeway time.Duration
	// Optional bounds (0 = the Rust defaults).
	HandshakeTimeout       time.Duration
	StepTimeout            time.Duration
	PolicyTimeout          time.Duration
	ReconciliationInterval time.Duration
	MaxAdmittedPeers       int
}

// RotationPin is the commitment to the next root key, pinned beside Root.
type RotationPin struct {
	NextCommitment string `json:"next_commitment"` // 64 hex
	Seq            uint64 `json:"seq"`
}

// Policy is a declarative admission policy, evaluated in Rust
// (PolicyExpressionV1). Build one with the Policy* constructors.
type Policy struct{ v any }

// MarshalJSON encodes the policy as the C ABI's policy JSON.
func (p *Policy) MarshalJSON() ([]byte, error) {
	if p == nil {
		return nil, fmt.Errorf("nil policy: %w", ErrInvalidArgument)
	}
	return json.Marshal(p.v)
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
// revocations still deny).
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
// its connections are already closed. Lagged > 0 instead means that many
// revocations were missed: recheck every peer relied on.
type Revocation struct {
	Peer   string       `json:"peer"`
	Reason DenialReason `json:"reason"`
	Lagged uint64       `json:"lagged"`
}

// EnrollmentStatus is this node's Enrollment state.
type EnrollmentStatus struct {
	TrustedRoot    string        `json:"trusted_root"`
	FleetNamespace string        `json:"fleet_namespace"`
	Audience       string        `json:"audience"`
	Credential     *PeerEvidence `json:"credential"`
	RootState      *struct {
		Root           string `json:"root"`
		NextCommitment string `json:"next_commitment"`
		Seq            uint64 `json:"seq"`
	} `json:"root_state"`
	RootRotations []string `json:"root_rotations"`
}

// UnreachedPeer is a peer a replaced credential could not be presented to.
type UnreachedPeer struct {
	Peer  string `json:"peer"`
	Error string `json:"error"`
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

// Enrollment is Enrollment installed on a node. The node owns it: Node.Close
// releases it (and its issuers and renewals).
type Enrollment struct {
	node     *Node
	rt       *Runtime
	handle   uint64
	mu       sync.Mutex
	closed   bool
	issuers  []*EnrollmentIssuer
	renewals []*Renewal
}

func (e *Enrollment) live() (uint64, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.closed {
		return 0, fmt.Errorf("enrollment closed: %w", ErrAlreadyClosed)
	}
	return e.handle, nil
}

// installConfigJSON resolves defaults from the node config and encodes the
// C ABI install configuration.
func (c *EnrollmentConfig) installJSON(node AsterConfig) ([]byte, error) {
	root := c.Root
	if len(root) == 0 {
		var err error
		if root, err = node.RootKey(); err != nil {
			return nil, fmt.Errorf("enrollment root: %w", err)
		}
	}
	if len(root) != 32 {
		return nil, fmt.Errorf("enrollment root: set EnrollmentConfig.Root or the node's RootPubkey / RootPubkeyFile: %w", ErrInvalidArgument)
	}
	credential := strings.TrimSpace(c.Credential)
	if credential == "" && node.EnrollmentCredentialFile != "" {
		data, err := os.ReadFile(node.EnrollmentCredentialFile)
		if err != nil {
			return nil, fmt.Errorf("enrollment credential: %w", err)
		}
		credential = strings.TrimSpace(string(data))
	}
	if c.Policy == nil {
		return nil, fmt.Errorf("enrollment policy: required: %w", ErrInvalidArgument)
	}
	ms := func(d time.Duration) any {
		if d <= 0 {
			return nil
		}
		return uint64(max(d.Milliseconds(), 1))
	}
	limits := map[string]any{}
	for k, v := range map[string]any{
		"handshake_timeout_ms":       ms(c.HandshakeTimeout),
		"step_timeout_ms":            ms(c.StepTimeout),
		"policy_timeout_ms":          ms(c.PolicyTimeout),
		"reconciliation_interval_ms": ms(c.ReconciliationInterval),
	} {
		if v != nil {
			limits[k] = v
		}
	}
	if c.MaxAdmittedPeers > 0 {
		limits["max_admitted_peers"] = c.MaxAdmittedPeers
	}
	cfg := map[string]any{
		"root":                       hex.EncodeToString(root),
		"audience":                   c.Audience,
		"fleet_namespace":            c.FleetNamespace,
		"state_path":                 c.StatePath,
		"policy":                     c.Policy,
		"applied_rotations":          append([]string{}, c.AppliedRotations...),
		"require_synchronized_clock": c.RequireSynchronizedClock,
		"limits":                     limits,
	}
	if credential != "" {
		cfg["credential"] = credential
	}
	if c.Rotation != nil {
		cfg["rotation"] = c.Rotation
	}
	if c.ActivationLeeway > 0 {
		cfg["activation_leeway_secs"] = uint64(c.ActivationLeeway / time.Second)
	}
	return json.Marshal(cfg)
}

// installEnrollment installs Enrollment on a node started for it.
func installEnrollment(n *Node, cfg *EnrollmentConfig, nodeCfg AsterConfig) (*Enrollment, error) {
	body, err := cfg.installJSON(nodeCfg)
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	var h C.aster_enrollment_t
	if err := wrapStatus("aster_enrollment_install", C.aster_enrollment_install(
		C.uint64_t(n.runtime.handle), C.uint64_t(n.handle), a.bytes(body), &h)); err != nil {
		return nil, err
	}
	return &Enrollment{node: n, rt: n.runtime, handle: uint64(h)}, nil
}

// Close releases the enrollment with its issuers and renewals. The node then
// rejects ordinary inbound connections (fail closed); Node.Close calls it.
func (e *Enrollment) Close() error {
	e.mu.Lock()
	if e.closed {
		e.mu.Unlock()
		return nil
	}
	e.closed = true
	issuers, renewals := e.issuers, e.renewals
	e.issuers, e.renewals = nil, nil
	e.mu.Unlock()
	var errs []error
	for _, r := range renewals {
		errs = append(errs, r.Close())
	}
	for _, i := range issuers {
		errs = append(errs, i.Close())
	}
	errs = append(errs, statusError(C.aster_enrollment_free(C.uint64_t(e.rt.handle), C.aster_enrollment_t(e.handle))))
	return errors.Join(errs...)
}

// result runs an async Enrollment operation and decodes its JSON result.
func (e *Enrollment) result(ctx context.Context, what string, out any,
	call func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t) error {
	h, err := e.live()
	if err != nil {
		return err
	}
	var op C.aster_operation_t
	if err := wrapStatus(what, call(C.aster_enrollment_t(h), &op)); err != nil {
		return err
	}
	return awaitEnrollmentResult(ctx, e.rt, uint64(op), what, out)
}

func awaitEnrollmentResult(ctx context.Context, rt *Runtime, op uint64, what string, out any) error {
	ev, err := rt.await(ctx, op)
	if err != nil {
		return fmt.Errorf("%s: %w", what, err)
	}
	if ev.Kind != ASTER_EVENT_ENROLLMENT_RESULT {
		kind := AdmissionErrorKind(ev.Flags)
		cause := rt.eventError(ev, what)
		if kind != AdmissionErrorOther {
			return &AdmissionError{Kind: kind, Message: cause.Error(), err: cause}
		}
		return cause
	}
	payload := rt.takePayload(ev)
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

func toAddrJSON(a NodeAddr) addrJSON {
	return addrJSON{NodeID: a.EndpointID, RelayURL: a.RelayURL, DirectAddresses: append([]string{}, a.DirectAddresses...)}
}

// Connect enrolls reciprocally with the first of replicas (tried in order, at
// most 16) that completes the handshake. It returns the replica's readiness
// and, when the replica offers one, a read ticket for the fleet namespace
// (pass both to Synchronize).
func (e *Enrollment) Connect(ctx context.Context, replicas ...NodeAddr) (Readiness, string, error) {
	addrs := make([]addrJSON, len(replicas))
	for i, r := range replicas {
		addrs[i] = toAddrJSON(r)
	}
	body, err := json.Marshal(addrs)
	if err != nil {
		return Readiness{}, "", err
	}
	var out struct {
		Readiness    Readiness `json:"readiness"`
		PolicyTicket *string   `json:"policy_ticket"`
	}
	var a cArena
	defer a.free()
	err = e.result(ctx, "aster_enrollment_connect", &out, func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t {
		return C.aster_enrollment_connect(C.uint64_t(e.rt.handle), h, a.bytes(body), 0, op)
	})
	if err != nil {
		return Readiness{}, "", err
	}
	ticket := ""
	if out.PolicyTicket != nil {
		ticket = *out.PolicyTicket
	}
	return out.Readiness, ticket, nil
}

// Synchronize syncs the fleet policy namespace from an admitted peer with a
// read-only ticket (Connect's, or one provisioned out of band), then every
// bound namespace the peer offered, waiting up to timeout (0 = 15 s). It
// returns the fleet namespace id.
func (e *Enrollment) Synchronize(ctx context.Context, peer, ticket string, timeout time.Duration) (string, error) {
	var out struct {
		Namespace string `json:"namespace"`
	}
	var a cArena
	defer a.free()
	ms := uint64(0)
	if timeout > 0 {
		ms = uint64(max(timeout.Milliseconds(), 1))
	}
	err := e.result(ctx, "aster_enrollment_synchronize", &out, func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t {
		return C.aster_enrollment_synchronize(C.uint64_t(e.rt.handle), h, a.str(peer), a.str(ticket), C.uint64_t(ms), 0, op)
	})
	return out.Namespace, err
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

// PeerClaims returns the verified evidence peer's admission session rests on
// (subject, issuer path, scopes, tags, attributes, validity), to map to RPC
// roles (AsterServer.SetPeerAttributes). It reflects the last admission or
// check (CheckPeer re-evaluates). ErrNotFound: the peer has no session.
func (e *Enrollment) PeerClaims(peer string) (*PeerEvidence, error) {
	h, err := e.live()
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	var out C.aster_bytes_t
	if err := wrapStatus("aster_enrollment_peer_claims", C.aster_enrollment_peer_claims(
		C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.str(peer), &out)); err != nil {
		return nil, err
	}
	var ev PeerEvidence
	return &ev, takeJSON(out, &ev)
}

// VerifyCredential verifies chain as subject's credential under this node's
// trust (root, audience, validity, the root's delegate revocations) without
// consulting root records. ErrVerificationFailed carries the reason.
func (e *Enrollment) VerifyCredential(subject, chain string) (*PeerEvidence, error) {
	h, err := e.live()
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	var out C.aster_bytes_t
	if err := wrapStatus("aster_enrollment_verify_credential", C.aster_enrollment_verify_credential(
		C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.str(subject), a.str(chain), &out)); err != nil {
		return nil, err
	}
	var ev PeerEvidence
	return &ev, takeJSON(out, &ev)
}

// Status returns this node's Enrollment state (trusted root, which moves on
// after an accepted root rotation, and its own credential).
func (e *Enrollment) Status() (EnrollmentStatus, error) {
	var st EnrollmentStatus
	h, err := e.live()
	if err != nil {
		return st, err
	}
	var out C.aster_bytes_t
	if err := wrapStatus("aster_enrollment_status", C.aster_enrollment_status(
		C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), &out)); err != nil {
		return st, err
	}
	return st, takeJSON(out, &st)
}

// ReplaceCredential installs a new credential (it must verify for this node)
// and presents it to every peer this node has a session with.
func (e *Enrollment) ReplaceCredential(ctx context.Context, chain string) ([]UnreachedPeer, error) {
	var out struct {
		Unreached []UnreachedPeer `json:"unreached"`
	}
	var a cArena
	defer a.free()
	err := e.result(ctx, "aster_enrollment_replace_credential", &out, func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t {
		return C.aster_enrollment_replace_credential(C.uint64_t(e.rt.handle), h, a.str(chain), 0, op)
	})
	return out.Unreached, err
}

// NextRevocation waits for the next revocation of a formerly admitted peer
// (buffered from install). Cancelling ctx consumes nothing; Close ends a
// pending wait with ErrAlreadyClosed. Drop per-peer state (roles, sessions,
// jobs) on each one.
func (e *Enrollment) NextRevocation(ctx context.Context) (Revocation, error) {
	var out Revocation
	err := e.result(ctx, "aster_enrollment_next_revocation", &out, func(h C.aster_enrollment_t, op *C.aster_operation_t) C.int32_t {
		return C.aster_enrollment_next_revocation(C.uint64_t(e.rt.handle), h, 0, op)
	})
	return out, err
}

// ─── Delegated issuer ─────────────────────────────────────────────────────

// IssuerConfig makes a server's node a delegated issuer
// (ServerOptions.EnrollmentIssuer).
type IssuerConfig struct {
	// Delegation is this node's delegation chain from the root; its leaf
	// names this node's key.
	Delegation string
	// Lifetime of issued (and renewed) credentials (0 = 24 h), shortened to
	// the delegation's max_child_lifetime and expiry.
	Lifetime time.Duration
}

// EnrollmentIssuer signs credentials under a delegation from the root and,
// on a server, serves aster.enrollment.IssuerV1 renewals.
type EnrollmentIssuer struct {
	e      *Enrollment
	handle uint64
	mu     sync.Mutex
	closed bool
}

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

// NewIssuer makes this node a delegated issuer (see IssuerConfig).
func (e *Enrollment) NewIssuer(cfg IssuerConfig) (*EnrollmentIssuer, error) {
	h, err := e.live()
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	var out C.aster_enrollment_issuer_t
	secs := uint64(0)
	if cfg.Lifetime > 0 {
		secs = uint64(max(cfg.Lifetime/time.Second, 1))
	}
	if err := wrapStatus("aster_enrollment_issuer_new", C.aster_enrollment_issuer_new(
		C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.str(cfg.Delegation), C.uint64_t(secs), &out)); err != nil {
		return nil, err
	}
	i := &EnrollmentIssuer{e: e, handle: uint64(out)}
	e.mu.Lock()
	e.issuers = append(e.issuers, i)
	e.mu.Unlock()
	return i, nil
}

func (i *EnrollmentIssuer) live() (uint64, error) {
	i.mu.Lock()
	defer i.mu.Unlock()
	if i.closed {
		return 0, fmt.Errorf("issuer closed: %w", ErrAlreadyClosed)
	}
	return i.handle, nil
}

// Info returns the issuer's key and delegation expiry.
func (i *EnrollmentIssuer) Info() (IssuerInfo, error) {
	var info IssuerInfo
	h, err := i.live()
	if err != nil {
		return info, err
	}
	var out C.aster_bytes_t
	if err := wrapStatus("aster_enrollment_issuer_info", C.aster_enrollment_issuer_info(
		C.uint64_t(i.e.rt.handle), C.aster_enrollment_issuer_t(h), &out)); err != nil {
		return info, err
	}
	return info, takeJSON(out, &info)
}

// Issue signs a credential for subject (a node id the orchestrator has
// authenticated out of band, e.g. from a Kubernetes service-account token).
// A refusal is an *IssuerRefusal.
func (i *EnrollmentIssuer) Issue(subject string, claims EnrollmentClaims, registration Registration) (IssuedCredential, error) {
	var issued IssuedCredential
	h, err := i.live()
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
	status := C.aster_enrollment_issue(C.uint64_t(i.e.rt.handle), C.aster_enrollment_issuer_t(h), a.bytes(body), &out)
	if status == C.ASTER_STATUS_PERMISSION_DENIED && out.ptr != nil {
		var refusal IssuerRefusal
		if err := takeJSON(out, &refusal); err != nil {
			return issued, err
		}
		return issued, &refusal
	}
	if err := wrapStatus("aster_enrollment_issue", status); err != nil {
		return issued, err
	}
	return issued, takeJSON(out, &issued)
}

// Close releases the issuer (a server it serves on keeps serving it).
func (i *EnrollmentIssuer) Close() error {
	i.mu.Lock()
	defer i.mu.Unlock()
	if i.closed {
		return nil
	}
	i.closed = true
	return statusError(C.aster_enrollment_issuer_free(C.uint64_t(i.e.rt.handle), C.aster_enrollment_issuer_t(i.handle)))
}

// ─── Renewal ──────────────────────────────────────────────────────────────

// RenewalConfig says where and how a node renews its credential.
type RenewalConfig struct {
	// Issuer is the issuer that signed this node's credential.
	Issuer NodeAddr
	// Retry is the first retry delay (doubling to MaxRetry); CallTimeout
	// bounds each call. 0 = defaults (1 s, 60 s, 30 s).
	Retry, MaxRetry, CallTimeout time.Duration
}

// Renewal renews this node's credential in the background.
type Renewal struct {
	e      *Enrollment
	handle uint64
	mu     sync.Mutex
	closed bool
}

// RenewalOutcome is why renewal stopped: "registered" (the root registered
// the node; the record is its authority now), "refused" (Refusal says why),
// "expired" or "no_credential".
type RenewalOutcome struct {
	Outcome string         `json:"outcome"`
	Refusal *IssuerRefusal `json:"refusal"`
}

// StartRenewal renews this node's credential from cfg.Issuer whenever a
// third of its lifetime remains, presenting each renewal to every admitted
// peer, until the root registers the node, the issuer refuses for good, or
// the credential expires. Close the renewal to stop it.
func (e *Enrollment) StartRenewal(cfg RenewalConfig) (*Renewal, error) {
	h, err := e.live()
	if err != nil {
		return nil, err
	}
	ms := func(d time.Duration) uint64 {
		if d <= 0 {
			return 0
		}
		return uint64(max(d.Milliseconds(), 1))
	}
	body, err := json.Marshal(map[string]any{
		"issuer": toAddrJSON(cfg.Issuer), "retry_ms": ms(cfg.Retry),
		"max_retry_ms": ms(cfg.MaxRetry), "call_timeout_ms": ms(cfg.CallTimeout),
	})
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	var out C.aster_enrollment_renewal_t
	if err := wrapStatus("aster_enrollment_start_renewal", C.aster_enrollment_start_renewal(
		C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.bytes(body), &out)); err != nil {
		return nil, err
	}
	r := &Renewal{e: e, handle: uint64(out)}
	e.mu.Lock()
	e.renewals = append(e.renewals, r)
	e.mu.Unlock()
	return r, nil
}

// Wait blocks until renewal stops (or ctx ends) and returns why.
func (r *Renewal) Wait(ctx context.Context) (RenewalOutcome, error) {
	var out RenewalOutcome
	r.mu.Lock()
	closed, h := r.closed, r.handle
	r.mu.Unlock()
	if closed {
		return out, fmt.Errorf("renewal closed: %w", ErrAlreadyClosed)
	}
	var op C.aster_operation_t
	if err := wrapStatus("aster_enrollment_renewal_wait", C.aster_enrollment_renewal_wait(
		C.uint64_t(r.e.rt.handle), C.aster_enrollment_renewal_t(h), 0, &op)); err != nil {
		return out, err
	}
	return out, awaitEnrollmentResult(ctx, r.e.rt, uint64(op), "aster_enrollment_renewal_wait", &out)
}

// Close stops renewing.
func (r *Renewal) Close() error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.closed {
		return nil
	}
	r.closed = true
	return statusError(C.aster_enrollment_renewal_free(C.uint64_t(r.e.rt.handle), C.aster_enrollment_renewal_t(r.handle)))
}
