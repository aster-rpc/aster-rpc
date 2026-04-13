// Package aster registry types, mirrors of bindings/python/aster/registry/models.py and keys.py.
//
// Spec references:
//   - ArtifactRef:    Aster-SPEC.md §11.2.1
//   - EndpointLease:  Aster-SPEC.md §11.6
//   - GossipEvent:    Aster-SPEC.md §11.7
//   - HealthStatus:   Aster-SPEC.md §11.6
//
// These are pure data types; all resolution/publishing logic lives in Rust and is
// reached via the registry FFI (see registry_ffi.go).
package aster

import (
	"encoding/json"
	"fmt"
	"time"
)

// ─── HealthStatus (§11.6) ───────────────────────────────────────────────────

// HealthStatus constants. Modeled as a string type so the wire format passes through
// unchanged.
type HealthStatus string

const (
	HealthStarting HealthStatus = "starting"
	HealthReady    HealthStatus = "ready"
	HealthDegraded HealthStatus = "degraded"
	HealthDraining HealthStatus = "draining"
)

// ValidateHealthStatus returns an error if s is not a valid HealthStatus.
func ValidateHealthStatus(s string) error {
	switch HealthStatus(s) {
	case HealthStarting, HealthReady, HealthDegraded, HealthDraining:
		return nil
	default:
		return fmt.Errorf("invalid HealthStatus: %q", s)
	}
}

// IsRoutable reports whether the status is READY or DEGRADED.
func IsRoutable(s string) bool {
	return s == string(HealthReady) || s == string(HealthDegraded)
}

// ─── GossipEventType (§11.7) ────────────────────────────────────────────────

// GossipEventType is the 6-variant gossip event enum.
type GossipEventType int32

const (
	GossipContractPublished      GossipEventType = 0
	GossipChannelUpdated         GossipEventType = 1
	GossipEndpointLeaseUpserted  GossipEventType = 2
	GossipEndpointDown           GossipEventType = 3
	GossipAclChanged             GossipEventType = 4
	GossipCompatibilityPublished GossipEventType = 5
)

// ─── ServiceSummary (§3.2.2) ────────────────────────────────────────────────

// ServiceSummary is the compact descriptor returned in ConsumerAdmissionResponse.
type ServiceSummary struct {
	Name               string            `json:"name"`
	Version            int32             `json:"version"`
	ContractID         string            `json:"contract_id"`
	Channels           map[string]string `json:"channels"`
	Pattern            string            `json:"pattern"`
	SerializationModes []string          `json:"serialization_modes"`
}

// ─── ArtifactRef (§11.2.1) ──────────────────────────────────────────────────

// ArtifactRef is the docs pointer to an immutable Iroh collection. Stored at
// "contracts/{contract_id}" in the registry doc.
type ArtifactRef struct {
	ContractID          string `json:"contract_id"`
	CollectionHash      string `json:"collection_hash"`
	ProviderEndpointID  string `json:"provider_endpoint_id,omitempty"`
	RelayURL            string `json:"relay_url,omitempty"`
	Ticket              string `json:"ticket,omitempty"`
	PublishedBy         string `json:"published_by"`
	PublishedAtEpochMs  int64  `json:"published_at_epoch_ms"`
	CollectionFormat    string `json:"collection_format"`
}

// ToJSON serializes the ArtifactRef as a compact JSON string.
func (r *ArtifactRef) ToJSON() (string, error) {
	b, err := json.Marshal(r)
	if err != nil {
		return "", err
	}
	return string(b), nil
}

// ArtifactRefFromJSON parses an ArtifactRef from JSON. Missing collection_format
// defaults to "raw" for backward compatibility.
func ArtifactRefFromJSON(s string) (*ArtifactRef, error) {
	var r ArtifactRef
	if err := json.Unmarshal([]byte(s), &r); err != nil {
		return nil, err
	}
	if r.CollectionFormat == "" {
		r.CollectionFormat = "raw"
	}
	return &r, nil
}

// ─── EndpointLease (§11.6) ──────────────────────────────────────────────────

// EndpointLease is a renewable advertisement for a live endpoint. Stored at
// "services/{name}/contracts/{cid}/endpoints/{eid}".
type EndpointLease struct {
	EndpointID          string   `json:"endpoint_id"`
	ContractID          string   `json:"contract_id"`
	Service             string   `json:"service"`
	Version             int32    `json:"version"`
	LeaseExpiresEpochMs int64    `json:"lease_expires_epoch_ms"`
	LeaseSeq            int64    `json:"lease_seq"`
	Alpn                string   `json:"alpn"`
	SerializationModes  []string `json:"serialization_modes"`
	FeatureFlags        []string `json:"feature_flags"`
	RelayURL            *string  `json:"relay_url"`
	DirectAddrs         []string `json:"direct_addrs"`
	Load                *float32 `json:"load"`
	LanguageRuntime     *string  `json:"language_runtime"`
	AsterVersion        string   `json:"aster_version"`
	PolicyRealm         *string  `json:"policy_realm"`
	HealthStatus        string   `json:"health_status"`
	Tags                []string `json:"tags"`
	UpdatedAtEpochMs    int64    `json:"updated_at_epoch_ms"`
}

// ToJSON serializes the EndpointLease as a compact JSON string.
func (l *EndpointLease) ToJSON() (string, error) {
	b, err := json.Marshal(l)
	if err != nil {
		return "", err
	}
	return string(b), nil
}

// EndpointLeaseFromJSON parses an EndpointLease from JSON.
func EndpointLeaseFromJSON(s string) (*EndpointLease, error) {
	var l EndpointLease
	if err := json.Unmarshal([]byte(s), &l); err != nil {
		return nil, err
	}
	return &l, nil
}

// IsFresh reports whether (now - updated_at) is within the lease duration window.
func (l *EndpointLease) IsFresh(leaseDurationSeconds int) bool {
	nowMs := time.Now().UnixMilli()
	return (nowMs - l.UpdatedAtEpochMs) <= int64(leaseDurationSeconds)*1000
}

// IsRoutable reports whether health is READY or DEGRADED.
func (l *EndpointLease) IsRoutable() bool {
	return IsRoutable(l.HealthStatus)
}

// ─── GossipEvent (§11.7) ────────────────────────────────────────────────────

// GossipEvent is a flat change notification broadcast over gossip.
type GossipEvent struct {
	Type        GossipEventType `json:"type"`
	Service     *string         `json:"service"`
	Version     *int32          `json:"version"`
	Channel     *string         `json:"channel"`
	ContractID  *string         `json:"contract_id"`
	EndpointID  *string         `json:"endpoint_id"`
	KeyPrefix   *string         `json:"key_prefix"`
	TimestampMs int64           `json:"timestamp_ms"`
}

// ToJSON serializes the GossipEvent as a compact JSON string.
func (e *GossipEvent) ToJSON() (string, error) {
	b, err := json.Marshal(e)
	if err != nil {
		return "", err
	}
	return string(b), nil
}

// GossipEventFromJSON parses a GossipEvent from JSON.
func GossipEventFromJSON(s string) (*GossipEvent, error) {
	var e GossipEvent
	if err := json.Unmarshal([]byte(s), &e); err != nil {
		return nil, err
	}
	return &e, nil
}

// ─── Key helpers (§11.2, §12.4) ─────────────────────────────────────────────

// RegistryPrefixes lists the key namespaces a registry client should sync (applied
// via set_download_policy "nothing_except").
var RegistryPrefixes = [][]byte{
	[]byte("contracts/"),
	[]byte("services/"),
	[]byte("endpoints/"),
	[]byte("compatibility/"),
	[]byte("_aster/"),
}

// ContractKey returns the doc key for an ArtifactRef: "contracts/{contract_id}".
func ContractKey(contractID string) []byte {
	return []byte("contracts/" + contractID)
}

// VersionKey returns "services/{name}/versions/v{version}".
func VersionKey(name string, version int32) []byte {
	return []byte(fmt.Sprintf("services/%s/versions/v%d", name, version))
}

// ChannelKey returns "services/{name}/channels/{channel}".
func ChannelKey(name, channel string) []byte {
	return []byte("services/" + name + "/channels/" + channel)
}

// TagKey returns "services/{name}/tags/{tag}".
func TagKey(name, tag string) []byte {
	return []byte("services/" + name + "/tags/" + tag)
}

// LeaseKey returns "services/{name}/contracts/{contract_id}/endpoints/{endpoint_id}".
func LeaseKey(name, contractID, endpointID string) []byte {
	return []byte("services/" + name + "/contracts/" + contractID + "/endpoints/" + endpointID)
}

// LeasePrefix returns "services/{name}/contracts/{contract_id}/endpoints/" for range queries.
func LeasePrefix(name, contractID string) []byte {
	return []byte("services/" + name + "/contracts/" + contractID + "/endpoints/")
}

// AclKey returns "_aster/acl/{subkey}".
func AclKey(subkey string) []byte {
	return []byte("_aster/acl/" + subkey)
}

// ConfigKey returns "_aster/config/{subkey}".
func ConfigKey(subkey string) []byte {
	return []byte("_aster/config/" + subkey)
}
