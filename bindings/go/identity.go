//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"strconv"
	"strings"
	"sync"
	"time"
	"unsafe"
)

// Gateway identity (C ABI 7.7; design
// docs/_internal/web_gateway/gateway-identity.md): an edge holding
// GatewayRole calls a service on behalf of a user with one JWT per call,
// either the identity provider's own token (forwarded) or one the edge
// mints (Enrollment.MintUserToken). The server verifies it before Gate 3
// and the handler sees the user (CallInfo.Principal = iss|sub), the edge
// (CallInfo.Actor) and how the user signed in (CallInfo.AuthMethods).
// Everything is configured through AsterConfig.Identity /
// AsterConfig.RemoteConfig, as the Rust AsterServer / AsterClient are.

// Roles, metadata and sign-in methods (Rust aster::rpc::identity).
const (
	// OnBehalfOfMetadata is the call-metadata entry that carries the user's
	// JWT (WithOnBehalfOf).
	OnBehalfOfMetadata = "aster-on-behalf-of"
	// GatewayRole lets an edge forward users' identities.
	GatewayRole = "aster.gateway"
	// ConfigAdminRole is required on every aster.ops.NodeConfig method.
	ConfigAdminRole = "aster.config-admin"
	// IdentityPublisherRole lets a node write issuer keys into the fleet
	// namespace.
	IdentityPublisherRole = "aster.identity-publisher"
	// IssuableRolesAttribute is the credential attribute naming the roles an
	// edge may put in tokens it mints (comma-separated).
	IssuableRolesAttribute = "aster.identity.issuable_roles"
	// AMR values for the sign-in methods an edge records in tokens it mints.
	AMRWebAuthn  = "webauthn"
	AMRPassword  = "pwd"
	AMRAPIKey    = "apikey"
	maxMintBytes = 64 * 1024
)

// ASTER_EVENT_NODE_CONFIG_RESULT completes an aster.ops.NodeConfig call.
const ASTER_EVENT_NODE_CONFIG_RESULT = C.ASTER_EVENT_NODE_CONFIG_RESULT

// ─── Configuration ([identity], [identity.publisher], [remote_config]) ────

// TrustedIssuer is an identity provider whose tokens the server accepts in
// OnBehalfOfMetadata ([[identity.trusted_issuers]]; Rust
// aster::TrustedIssuer). Its keys come from the fleet namespace, published
// by a node holding IdentityPublisherRole; this says whom to trust and how
// to map roles. Needs Enrollment.
type TrustedIssuer struct {
	// Iss is the exact iss claim.
	Iss string `json:"iss"`
	// Audiences are the accepted aud values (required).
	Audiences []string `json:"audiences"`
	// RolesClaim is the claim roles are read from (an array of strings, or
	// one space-separated string); "" = the tokens grant no roles.
	RolesClaim string `json:"roles_claim,omitempty"`
	// RoleMap maps claim values to Aster roles; unmapped values are dropped.
	RoleMap map[string]string `json:"role_map,omitempty"`
	// MaxTokenLifetimeSecs is the longest exp - iat accepted; 0 = 900.
	MaxTokenLifetimeSecs uint64 `json:"max_token_lifetime_secs,omitempty"`
}

// IdentityPublisherConfig is [identity.publisher] (Rust
// aster::IdentityPublisherConfig): on the node that publishes issuer keys,
// normally the gateway. It fetches each issuer's JWK set over validated
// HTTPS and writes it into the fleet namespace. Needs Enrollment, a
// credential granting IdentityPublisherRole and the root's sealed write
// grant (aster enroll identity-publisher).
type IdentityPublisherConfig struct {
	Issuers []string `json:"issuers"`
	// JWKSURL maps each issuer to the https URL of its JWK set.
	JWKSURL map[string]string `json:"jwks_url,omitempty"`
	// RefreshSecs is the scheduled refresh (60..86400); 0 = 3600.
	RefreshSecs uint64 `json:"refresh_secs,omitempty"`
	// MinRefreshSecs bounds unknown-kid refreshes per issuer; 0 = 60.
	MinRefreshSecs uint64 `json:"min_refresh_secs,omitempty"`
	// CACerts are extra PEM trust anchors for the providers' TLS; CAOnly
	// trusts only them.
	CACerts []string `json:"ca_certs,omitempty"`
	CAOnly  bool     `json:"ca_only,omitempty"`
}

// IdentityConfig is the [identity] section (Rust AsterConfig identity_*
// fields; env ASTER_IDENTITY_*). ConfigFromEnv / ConfigFromTOML fill it
// through the Rust config, with the same names, defaults and precedence.
type IdentityConfig struct {
	// AcceptOnBehalfOf accepts calls made on behalf of a user (default
	// false: a call carrying the entry is refused with PERMISSION_DENIED).
	// Env ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF; TOML [identity]
	// accept_on_behalf_of.
	AcceptOnBehalfOf bool
	// TrustedIssuers are the providers whose tokens may be forwarded. Env
	// ASTER_IDENTITY_TRUSTED_ISSUERS (JSON); TOML [[identity.trusted_issuers]].
	TrustedIssuers []TrustedIssuer
	// EdgeTokenMaxLifetimeSecs (1..300), ClockLeewaySecs (0..300, applied to
	// iat/nbf only) and MaxKeySetAgeSecs (60..2592000): nil = the defaults
	// (300, 60, 172800).
	EdgeTokenMaxLifetimeSecs *uint64
	ClockLeewaySecs          *uint64
	MaxKeySetAgeSecs         *uint64
	// Publisher is [identity.publisher] (env ASTER_IDENTITY_PUBLISHER, JSON).
	Publisher *IdentityPublisherConfig

	// Where each overridable setting came from (aster_config_source_t) and
	// its value then, so aster.ops.NodeConfig get reports file / env
	// faithfully; a value changed in code since reports "code".
	sources [5]uint8
	loaded  [5]string
}

// RemoteConfig is [remote_config]: aster.ops.NodeConfig, remote overrides
// of overridable sections ([identity] today). Served by default behind
// ConfigAdminRole; a node without Enrollment refuses every change.
type RemoteConfig struct {
	// Disabled turns the service off ([remote_config] enabled = false); a
	// stored record is then ignored. Env ASTER_REMOTE_CONFIG_ENABLED.
	Disabled bool
	// Lock names sections that may not be overridden, e.g. "identity".
	// Env ASTER_REMOTE_CONFIG_LOCK (comma-separated).
	Lock []string
}

// fingerprints are the JSON of each overridable setting, in
// ASTER_IDENTITY_SETTING_* order.
func (c *IdentityConfig) fingerprints() [5]string {
	j := func(v any) string {
		b, _ := json.Marshal(v)
		return string(b)
	}
	u := func(p *uint64) string {
		if p == nil {
			return "null"
		}
		return strconv.FormatUint(*p, 10)
	}
	issuers := c.TrustedIssuers
	if len(issuers) == 0 {
		issuers = nil
	}
	return [5]string{j(c.AcceptOnBehalfOf), j(issuers), u(c.EdgeTokenMaxLifetimeSecs),
		u(c.ClockLeewaySecs), u(c.MaxKeySetAgeSecs)}
}

func (c *AsterConfig) hasIdentitySettings() bool {
	i := &c.Identity
	return i.AcceptOnBehalfOf || len(i.TrustedIssuers) > 0 || i.EdgeTokenMaxLifetimeSecs != nil ||
		i.ClockLeewaySecs != nil || i.MaxKeySetAgeSecs != nil || i.Publisher != nil ||
		c.RemoteConfig.Disabled || len(c.RemoteConfig.Lock) > 0 || i.sources != [5]uint8{}
}

// setIdentityNative copies the identity settings into n's library storage
// (aster_config_set_identity, C ABI 7.7). The caller releases n.
func (c *AsterConfig) setIdentityNative(a *cArena, n *C.aster_config_t) error {
	if !c.hasIdentitySettings() {
		return nil
	}
	i := &c.Identity
	var id C.aster_identity_config_t
	id.struct_size = C.uint32_t(C.sizeof_aster_identity_config_t)
	id.accept_on_behalf_of = boolToUint32(i.AcceptOnBehalfOf)
	id.remote_config_disabled = boolToUint32(c.RemoteConfig.Disabled)
	set := func(p *uint64, bit C.uint32_t, field *C.uint64_t) {
		if p != nil {
			*field = C.uint64_t(*p)
			id.set |= bit
		}
	}
	set(i.EdgeTokenMaxLifetimeSecs, C.ASTER_IDENTITY_SET_EDGE_TOKEN_MAX_LIFETIME, &id.edge_token_max_lifetime_secs)
	set(i.ClockLeewaySecs, C.ASTER_IDENTITY_SET_CLOCK_LEEWAY, &id.clock_leeway_secs)
	set(i.MaxKeySetAgeSecs, C.ASTER_IDENTITY_SET_MAX_KEY_SET_AGE, &id.max_key_set_age_secs)
	if len(i.TrustedIssuers) > 0 {
		b, err := json.Marshal(i.TrustedIssuers)
		if err != nil {
			return fmt.Errorf("Identity.TrustedIssuers: %w", err)
		}
		id.trusted_issuers_json = a.bytes(b)
	}
	if i.Publisher != nil {
		b, err := json.Marshal(i.Publisher)
		if err != nil {
			return fmt.Errorf("Identity.Publisher: %w", err)
		}
		id.publisher_json = a.bytes(b)
	}
	id.remote_config_lock = a.list(c.RemoteConfig.Lock)
	now := i.fingerprints()
	for k := range i.sources {
		if i.sources[k] != 0 && now[k] == i.loaded[k] {
			id.sources[k] = C.uint8_t(i.sources[k])
		}
	}
	if err := ffiStatus(func() C.int32_t { return C.aster_config_set_identity(n, &id) }); err != nil {
		return fmt.Errorf("aster_config_set_identity: %w", err)
	}
	return nil
}

// identityFromNative reads the identity settings of n (aster_config_identity).
func (c *AsterConfig) identityFromNative(n *C.aster_config_t) {
	var id C.aster_identity_config_t
	id.struct_size = C.uint32_t(C.sizeof_aster_identity_config_t)
	if C.aster_config_identity(n, &id) != C.ASTER_STATUS_OK {
		return
	}
	i := IdentityConfig{AcceptOnBehalfOf: id.accept_on_behalf_of != 0}
	get := func(bit C.uint32_t, v C.uint64_t) *uint64 {
		if id.set&bit == 0 {
			return nil
		}
		x := uint64(v)
		return &x
	}
	i.EdgeTokenMaxLifetimeSecs = get(C.ASTER_IDENTITY_SET_EDGE_TOKEN_MAX_LIFETIME, id.edge_token_max_lifetime_secs)
	i.ClockLeewaySecs = get(C.ASTER_IDENTITY_SET_CLOCK_LEEWAY, id.clock_leeway_secs)
	i.MaxKeySetAgeSecs = get(C.ASTER_IDENTITY_SET_MAX_KEY_SET_AGE, id.max_key_set_age_secs)
	if b := viewBytes(id.trusted_issuers_json); len(b) > 0 {
		_ = json.Unmarshal(b, &i.TrustedIssuers)
	}
	if b := viewBytes(id.publisher_json); len(b) > 0 {
		var p IdentityPublisherConfig
		if json.Unmarshal(b, &p) == nil {
			i.Publisher = &p
		}
	}
	for k := range i.sources {
		i.sources[k] = uint8(id.sources[k])
	}
	i.loaded = i.fingerprints()
	c.Identity = i
	c.RemoteConfig = RemoteConfig{Disabled: id.remote_config_disabled != 0, Lock: readList(id.remote_config_lock)}
}

// ─── Handlers: the forwarded identity and NodeConfig properties ───────────

// ForwardedIdentity is the verified user of a call a gateway made on its
// behalf (Rust aster::rpc::ForwardedIdentity).
type ForwardedIdentity struct {
	// Issuer is the token's iss: a provider's URL, or the gateway's node id
	// for a token it minted.
	Issuer  string
	Subject string
	// JTI is the token id ("" when it has none).
	JTI       string
	ExpiresAt time.Time
	// AuthMethods are the token's amr (how the user signed in).
	AuthMethods []string
}

// RequireAuthMethod returns PERMISSION_DENIED unless the call was made on
// behalf of a user who signed in with method (one of CallInfo.AuthMethods,
// e.g. AMRWebAuthn); a call made by the peer as itself is refused too (Rust
// CallContext::require_auth_method). Requires checks roles only; use this
// in the handler for methods that need a stronger sign-in.
func (ci *CallInfo) RequireAuthMethod(method string) error {
	for _, m := range ci.AuthMethods {
		if m == method {
			return nil
		}
	}
	return &RpcError{Code: StatusPermissionDenied, Message: "this method needs a sign-in with " + method}
}

// NodeAttribute is a typed aster.ops.NodeConfig attribute (Rust
// aster::rpc::AttrValue): Type is "bool", "int", "float" or "string".
// Attributes describe the node and never authorise anything.
type NodeAttribute struct {
	Type  string          `json:"type"`
	Value json.RawMessage `json:"value"`
}

// AttrBool, AttrInt, AttrFloat and AttrString build attribute values.
func AttrBool(v bool) NodeAttribute     { return attr("bool", v) }
func AttrInt(v int64) NodeAttribute     { return attr("int", v) }
func AttrFloat(v float64) NodeAttribute { return attr("float", v) }
func AttrString(v string) NodeAttribute { return attr("string", v) }

func attr(kind string, v any) NodeAttribute {
	b, _ := json.Marshal(v)
	return NodeAttribute{Type: kind, Value: b}
}

// Bool is the value of a "bool" attribute.
func (a NodeAttribute) Bool() (bool, bool) {
	var v bool
	return v, a.Type == "bool" && json.Unmarshal(a.Value, &v) == nil
}

// Int is the value of an "int" attribute.
func (a NodeAttribute) Int() (int64, bool) {
	var v int64
	return v, a.Type == "int" && json.Unmarshal(a.Value, &v) == nil
}

// Float is the value of a "float" attribute.
func (a NodeAttribute) Float() (float64, bool) {
	var v float64
	return v, a.Type == "float" && json.Unmarshal(a.Value, &v) == nil
}

// Text is the value of a "string" attribute.
func (a NodeAttribute) Text() (string, bool) {
	var v string
	return v, a.Type == "string" && json.Unmarshal(a.Value, &v) == nil
}

// NodeProperties are the node's aster.ops.NodeConfig properties as
// handlers see them now (Rust aster::rpc::NodeProperties).
type NodeProperties struct {
	// Generation is the NodeConfig generation in effect (0: never changed).
	Generation uint64                   `json:"generation"`
	Name       string                   `json:"name"`
	Tags       []string                 `json:"tags"`
	Attributes map[string]NodeAttribute `json:"attributes"`
}

// HasTag reports whether the node carries tag.
func (p *NodeProperties) HasTag(tag string) bool {
	for _, t := range p.Tags {
		if t == tag {
			return true
		}
	}
	return false
}

// nodeProps caches a server's properties per NodeConfig generation.
type nodeProps struct {
	mu     sync.Mutex
	cached *NodeProperties
}

func (d *dispatchServer) nodeProperties(cache *nodeProps) (*NodeProperties, error) {
	gen := uint64(C.aster_server_node_config_generation(d.handle))
	cache.mu.Lock()
	defer cache.mu.Unlock()
	if cache.cached != nil && cache.cached.Generation == gen {
		return cache.cached, nil
	}
	var out C.aster_bytes_t
	if err := wrapCall("aster_server_node_properties", func() C.int32_t {
		return C.aster_server_node_properties(d.handle, &out)
	}); err != nil {
		return nil, err
	}
	var p NodeProperties
	if err := takeJSON(out, &p); err != nil {
		return nil, err
	}
	cache.cached = &p
	return &p, nil
}

// NodeProperties are the server's NodeConfig properties now (Rust
// AsterServer::node_config().properties()).
func (s *AsterServer) NodeProperties() (*NodeProperties, error) {
	return s.ds.nodeProperties(&s.props)
}

// NodeProperties are the serving node's NodeConfig properties (Rust
// CallContext::node_properties). Empty when NodeConfig is off.
func (ci *CallInfo) NodeProperties() *NodeProperties {
	if ci.server == nil {
		return &NodeProperties{}
	}
	p, err := ci.server.NodeProperties()
	if err != nil {
		return &NodeProperties{}
	}
	return p
}

// NodeAttribute is one NodeConfig attribute of the serving node (Rust
// CallContext::node_attribute). Gate 3 never reads these.
func (ci *CallInfo) NodeAttribute(key string) (NodeAttribute, bool) {
	a, ok := ci.NodeProperties().Attributes[key]
	return a, ok
}

// onBehalfOf reads a forwarded call's identity (aster_dispatch_on_behalf_of).
func (d *dispatchServer) onBehalfOf(callID uint64) *ForwardedIdentity {
	var ptr *C.uint8_t
	var n C.uint32_t
	var buf C.uint64_t
	if C.aster_dispatch_on_behalf_of(d.rt, d.handle, C.uint64_t(callID), &ptr, &n, &buf) != C.ASTER_STATUS_OK {
		return nil
	}
	var raw []byte
	if n > 0 {
		raw = C.GoBytes(unsafe.Pointer(ptr), C.int(n))
	}
	d.releaseBuffer(buf)
	kv, _ := decodeKV(raw)
	if len(kv) == 0 {
		return nil
	}
	f := &ForwardedIdentity{}
	for _, e := range kv {
		switch e.Key {
		case "iss":
			f.Issuer = e.Value
		case "sub":
			f.Subject = e.Value
		case "jti":
			f.JTI = e.Value
		case "exp":
			if s, err := strconv.ParseInt(e.Value, 10, 64); err == nil {
				f.ExpiresAt = time.Unix(s, 0)
			}
		case "amr":
			f.AuthMethods = append(f.AuthMethods, e.Value)
		}
	}
	return f
}

// ─── Edges: per-call tokens, minting, the issuer-key publisher ───────────

// WithOnBehalfOf makes the call on behalf of a user: jwt (a provider's
// token, forwarded unchanged, or one from MintUserToken) goes in the
// OnBehalfOfMetadata entry, replacing an earlier one (Rust
// CallOptions::on_behalf_of). The server must accept it and this node must
// hold GatewayRole. A client running [identity.publisher] notices the
// token's key id, so a provider's key rotation is published at once.
func WithOnBehalfOf(jwt string) CallOption {
	return func(o *callOptions) {
		kept := o.metadata[:0:0]
		for _, e := range o.metadata {
			if !strings.EqualFold(e.Key, OnBehalfOfMetadata) {
				kept = append(kept, e)
			}
		}
		o.metadata = append(kept, MetadataEntry{OnBehalfOfMetadata, jwt})
		o.onBehalfOf = jwt
	}
}

// BearerToken is the token of an "Authorization: Bearer <token>" header
// value, for forwarding a provider's token unchanged (Rust
// aster::rpc::bearer_token).
func BearerToken(authorization string) (string, bool) {
	scheme, token, ok := strings.Cut(strings.TrimSpace(authorization), " ")
	if !ok || !strings.EqualFold(scheme, "bearer") {
		return "", false
	}
	token = strings.TrimSpace(token)
	return token, token != ""
}

// noticeOnBehalfOf hands a forwarded token to the client's publisher, if it
// runs one (it ignores tokens of issuers it does not publish).
func (rc *RpcConnection) noticeOnBehalfOf(o *callOptions) {
	if o.onBehalfOf == "" || rc.client == nil || rc.client.enrollment == nil {
		return
	}
	_ = rc.client.enrollment.NoticeToken(o.onBehalfOf)
}

// MintUserToken mints a short-lived token for user sub with roles, signed
// in by amr (at least one, e.g. AMRWebAuthn), valid for ttl (at most 5
// minutes), signed with this node's key (Rust AsterClient::user_token_issuer
// + Issuer::mint_user_token). The roles must be within the issuer
// delegation (IssuableRolesAttribute) in the credential held now; anything
// else is ErrInvalidArgument. Attach it with WithOnBehalfOf.
func (e *Enrollment) MintUserToken(sub string, roles, amr []string, ttl time.Duration) (string, error) {
	h, err := e.live()
	if err != nil {
		return "", err
	}
	if roles == nil {
		roles = []string{}
	}
	secs := uint64(math.Ceil(ttl.Seconds()))
	body, err := json.Marshal(map[string]any{"sub": sub, "roles": roles, "amr": amr, "ttl_secs": secs})
	if err != nil {
		return "", err
	}
	if len(body) > maxMintBytes {
		return "", fmt.Errorf("mint request too large: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	var out C.aster_bytes_t
	if err := wrapCall("aster_enrollment_mint_user_token", func() C.int32_t {
		return C.aster_enrollment_mint_user_token(C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.bytes(body), &out)
	}); err != nil {
		return "", err
	}
	token := C.GoStringN((*C.char)(unsafe.Pointer(out.ptr)), C.int(out.len))
	C.aster_string_release(out.ptr, out.len)
	return token, nil
}

// MintUserToken is Enrollment().MintUserToken: the client's own key, bounded
// by the delegation in its credential. Fails without enrollment.
func (c *AsterClient) MintUserToken(sub string, roles, amr []string, ttl time.Duration) (string, error) {
	if c.enrollment == nil {
		return "", fmt.Errorf("minting users' tokens needs [enrollment]: %w", ErrInvalidArgument)
	}
	return c.enrollment.MintUserToken(sub, roles, amr, ttl)
}

// NoticeKid tells the node's issuer-key publisher that a token of iss named
// kid: an unknown kid refreshes iss at once (at most every
// MinRefreshSecs). ErrNotFound when the node runs no publisher.
func (e *Enrollment) NoticeKid(iss, kid string) error {
	h, err := e.live()
	if err != nil {
		return err
	}
	var a cArena
	defer a.free()
	return wrapCall("aster_enrollment_publisher_notice", func() C.int32_t {
		return C.aster_enrollment_publisher_notice(C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.str(iss), a.str(kid))
	})
}

// NoticeToken is NoticeKid with the iss and kid of a compact JWT.
func (e *Enrollment) NoticeToken(token string) error {
	h, err := e.live()
	if err != nil {
		return err
	}
	var a cArena
	defer a.free()
	return wrapCall("aster_enrollment_publisher_notice_token", func() C.int32_t {
		return C.aster_enrollment_publisher_notice_token(C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), a.str(token))
	})
}

// IssuerPublishStatus is what the publisher last did for one issuer.
type IssuerPublishStatus struct {
	Kids         []string `json:"kids"`
	FetchedAt    *uint64  `json:"fetched_at"`
	Publications uint64   `json:"publications"`
	Attempts     uint64   `json:"attempts"`
	LastError    *string  `json:"last_error"`
}

// PublisherStatus is the publisher's state per issuer. ErrNotFound when the
// node runs no publisher.
func (e *Enrollment) PublisherStatus() (map[string]IssuerPublishStatus, error) {
	h, err := e.live()
	if err != nil {
		return nil, err
	}
	var out C.aster_bytes_t
	if err := wrapCall("aster_enrollment_publisher_status", func() C.int32_t {
		return C.aster_enrollment_publisher_status(C.uint64_t(e.rt.handle), C.aster_enrollment_t(h), &out)
	}); err != nil {
		return nil, err
	}
	st := map[string]IssuerPublishStatus{}
	return st, takeJSON(out, &st)
}

// ─── aster.ops.NodeConfig admin client ────────────────────────────────────

// IdentityOverride overrides [identity] fields through NodeConfig; only the
// fields set apply. TrustedIssuers replaces the whole list.
type IdentityOverride struct {
	AcceptOnBehalfOf         *bool            `json:"accept_on_behalf_of,omitempty"`
	TrustedIssuers           *[]TrustedIssuer `json:"trusted_issuers,omitempty"`
	EdgeTokenMaxLifetimeSecs *uint64          `json:"edge_token_max_lifetime_secs,omitempty"`
	ClockLeewaySecs          *uint64          `json:"clock_leeway_secs,omitempty"`
	MaxKeySetAgeSecs         *uint64          `json:"max_key_set_age_secs,omitempty"`
}

// NodeConfigSetRequest replaces a node's whole record (Rust
// NodeConfigSetRequest). ExpectedGeneration 0 is unconditional; otherwise
// the set is ABORTED unless it matches.
type NodeConfigSetRequest struct {
	ExpectedGeneration uint64
	Name               string
	Tags               []string
	// Attributes may not start with "aster.".
	Attributes map[string]NodeAttribute
	// Identity overrides [identity] (nil: not overridden).
	Identity *IdentityOverride
	// Sections are other section overrides by name, as JSON objects.
	Sections map[string]json.RawMessage
}

// EffectiveSetting is one overridable setting's value in force and where
// it came from ("default", "file", "env", "code" or "override").
type EffectiveSetting struct {
	Key    string `json:"key"`
	Value  any    `json:"value"`
	Source string `json:"source"`
}

// NodeConfigView is NodeConfig get: the record and every overridable
// setting's effective value.
type NodeConfigView struct {
	NodeID              string                     `json:"node_id"`
	Generation          int64                      `json:"generation"`
	AppliedGeneration   int64                      `json:"applied_generation"`
	Change              string                     `json:"change"`
	ChangedBy           string                     `json:"changed_by"`
	ChangedAtMs         int64                      `json:"changed_at_ms"`
	Name                string                     `json:"name"`
	Tags                []string                   `json:"tags"`
	Attributes          map[string]NodeAttribute   `json:"attributes"`
	Sections            map[string]json.RawMessage `json:"sections"`
	IgnoredSections     []string                   `json:"ignored_sections"`
	Settings            []EffectiveSetting         `json:"settings"`
	OverridableSections []string                   `json:"overridable_sections"`
	LockedSections      []string                   `json:"locked_sections"`
}

// Setting is one setting by "section.field" key, or nil.
func (v *NodeConfigView) Setting(key string) *EffectiveSetting {
	for i := range v.Settings {
		if v.Settings[i].Key == key {
			return &v.Settings[i]
		}
	}
	return nil
}

// NodeConfigOutstanding is a part of a record not (yet) in effect.
type NodeConfigOutstanding struct {
	Part   string `json:"part"`
	State  string `json:"state"`
	Detail string `json:"detail"`
}

// NodeConfigAck answers set, clear and ack: Complete only once the
// generation asked about is stored and in effect.
type NodeConfigAck struct {
	Complete          bool                    `json:"complete"`
	Generation        int64                   `json:"generation"`
	AppliedGeneration int64                   `json:"applied_generation"`
	Outstanding       []NodeConfigOutstanding `json:"outstanding"`
}

// NodeConfigClient calls aster.ops.NodeConfig on the server at the other end
// of an RPC connection (Rust NodeConfigClient). Set and Clear need
// ConfigAdminRole from this node's own Enrollment credential.
type NodeConfigClient struct{ rc *RpcConnection }

// NewNodeConfigClient is a NodeConfig client over rc.
func NewNodeConfigClient(rc *RpcConnection) *NodeConfigClient { return &NodeConfigClient{rc: rc} }

const (
	nodeConfigGet   = C.ASTER_NODE_CONFIG_GET
	nodeConfigSet   = C.ASTER_NODE_CONFIG_SET
	nodeConfigClear = C.ASTER_NODE_CONFIG_CLEAR
	nodeConfigAck   = C.ASTER_NODE_CONFIG_ACK
)

func (c *NodeConfigClient) call(ctx context.Context, method C.uint32_t, req map[string]any, out any) error {
	if d, ok := ctx.Deadline(); ok {
		left := time.Until(d)
		if left <= 0 {
			return ctxError(ctx, &RpcError{Code: StatusDeadlineExceeded, Message: "deadline exceeded before the call"})
		}
		req["deadline_ms"] = uint64(math.Ceil(float64(left) / float64(time.Millisecond)))
	}
	body, err := json.Marshal(req)
	if err != nil {
		return err
	}
	rt := c.rc.conn.runtime
	var a cArena
	defer a.free()
	var op C.aster_operation_t
	if err := wrapCall("aster_node_config_call", func() C.int32_t {
		return C.aster_node_config_call(C.uint64_t(rt.handle), C.aster_connection_t(c.rc.conn.handle), method, a.bytes(body), 0, &op)
	}); err != nil {
		return err
	}
	ev, err := rt.await(ctx, uint64(op))
	if err != nil {
		return err
	}
	if ev.Kind != ASTER_EVENT_NODE_CONFIG_RESULT {
		return rt.eventError(ev, "aster.ops.NodeConfig")
	}
	var res struct {
		Code    int32           `json:"code"`
		Message string          `json:"message"`
		Result  json.RawMessage `json:"result"`
	}
	if err := json.Unmarshal(rt.takePayload(ev), &res); err != nil {
		return fmt.Errorf("aster.ops.NodeConfig: decode result: %w", err)
	}
	if res.Code != 0 {
		return ctxError(ctx, &RpcError{Code: StatusCode(res.Code), Message: res.Message})
	}
	return json.Unmarshal(res.Result, out)
}

// Get returns the record and every overridable setting's effective value.
func (c *NodeConfigClient) Get(ctx context.Context) (*NodeConfigView, error) {
	var v NodeConfigView
	return &v, c.call(ctx, nodeConfigGet, map[string]any{}, &v)
}

// Set replaces the record; it answers once the new generation is in effect.
func (c *NodeConfigClient) Set(ctx context.Context, req NodeConfigSetRequest) (*NodeConfigAck, error) {
	sections := map[string]json.RawMessage{}
	for k, v := range req.Sections {
		sections[k] = v
	}
	if req.Identity != nil {
		b, err := json.Marshal(req.Identity)
		if err != nil {
			return nil, err
		}
		sections["identity"] = b
	}
	attributes := req.Attributes
	if attributes == nil {
		attributes = map[string]NodeAttribute{}
	}
	tags := req.Tags
	if tags == nil {
		tags = []string{}
	}
	var ack NodeConfigAck
	err := c.call(ctx, nodeConfigSet, map[string]any{
		"expected_generation": req.ExpectedGeneration, "name": req.Name, "tags": tags,
		"attributes": attributes, "sections": sections,
	}, &ack)
	return &ack, err
}

// Clear reverts the node to its file configuration (a tombstone with the
// next generation); it answers once that is in effect.
func (c *NodeConfigClient) Clear(ctx context.Context, expectedGeneration uint64) (*NodeConfigAck, error) {
	var ack NodeConfigAck
	err := c.call(ctx, nodeConfigClear, map[string]any{"expected_generation": expectedGeneration}, &ack)
	return &ack, err
}

// Ack reports whether generation is stored and in effect, waiting up to
// timeout (0 = 10 s, at most 60 s) for it to be applied.
func (c *NodeConfigClient) Ack(ctx context.Context, generation uint64, timeout time.Duration) (*NodeConfigAck, error) {
	var ack NodeConfigAck
	err := c.call(ctx, nodeConfigAck, map[string]any{"generation": generation, "timeout_ms": timeout.Milliseconds()}, &ack)
	return &ack, err
}
