//go:build cgo

package aster

import (
	"bytes"
	"crypto/ed25519"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"reflect"
	"testing"
)

type provisioningVector struct {
	TestSecret   string `json:"test_only_authority_secret"`
	AuthorityKey string `json:"authority_public_key"`
	Genesis      struct {
		Request   CoordinationGenesisDocument `json:"request"`
		Document  string                      `json:"document_bytes"`
		Signing   string                      `json:"signing_bytes"`
		Signature string                      `json:"signature"`
		Envelope  string                      `json:"signed_document"`
		Hash      string                      `json:"genesis_hash"`
		Verified  json.RawMessage             `json:"verified"`
	} `json:"genesis"`
	Join struct {
		Request   CoordinationJoinPermitDocument `json:"request"`
		Ticket    string                         `json:"candidate_ticket"`
		Document  string                         `json:"document_bytes"`
		Signing   string                         `json:"signing_bytes"`
		Signature string                         `json:"signature"`
		Envelope  string                         `json:"signed_document"`
		Verified  json.RawMessage                `json:"verified"`
	} `json:"join"`
}

func loadProvisioningVector(t *testing.T) provisioningVector {
	t.Helper()
	data, err := os.ReadFile("../../tests/fixtures/coordination_provisioning_v1.json")
	if err != nil {
		t.Fatal(err)
	}
	var v provisioningVector
	if err := json.Unmarshal(data, &v); err != nil {
		t.Fatal(err)
	}
	return v
}
func provisioningHex(t *testing.T, value string) []byte {
	t.Helper()
	data, err := hex.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	return data
}
func checkProvisioningBytes(t *testing.T, got []byte, err error, expected string) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, provisioningHex(t, expected)) {
		t.Fatal("native result differs from shared vector")
	}
}
func checkProvisioningCode(t *testing.T, err error, code string) {
	t.Helper()
	var native *CoordinationProvisioningError
	if !errors.As(err, &native) || native.Code != code || native.Detail == "" {
		t.Fatalf("want native %s: %v", code, err)
	}
}

func checkProvisioningSummary(t *testing.T, value any, document []byte, expected json.RawMessage) {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var got, want map[string]any
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(expected, &want); err != nil {
		t.Fatal(err)
	}
	got["document_bytes"] = hex.EncodeToString(document)
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("verified summary mismatch: got %v want %v", got, want)
	}
}

func TestCoordinationProvisioning_SharedVectors(t *testing.T) {
	v := loadProvisioningVector(t)
	key := provisioningHex(t, v.AuthorityKey)
	got, err := CoordinationCanonicalGenesis(v.Genesis.Request)
	checkProvisioningBytes(t, got, err, v.Genesis.Document)
	got, err = CoordinationGenesisSigningBytes(got)
	checkProvisioningBytes(t, got, err, v.Genesis.Signing)
	got, err = CoordinationGenesisHash(provisioningHex(t, v.Genesis.Document))
	checkProvisioningBytes(t, got, err, v.Genesis.Hash)
	got, err = CoordinationSignedDocumentFromParts(provisioningHex(t, v.Genesis.Document), key, provisioningHex(t, v.Genesis.Signature))
	checkProvisioningBytes(t, got, err, v.Genesis.Envelope)
	genesis, err := CoordinationVerifyGenesis(key, v.Genesis.Request.AuthorityDomain, got)
	if err != nil {
		t.Fatal(err)
	}
	checkProvisioningBytes(t, genesis.DocumentBytes, nil, v.Genesis.Document)
	checkProvisioningSummary(t, genesis, genesis.DocumentBytes, v.Genesis.Verified)
	if genesis.GenesisHash != v.Genesis.Hash || genesis.GroupID != v.Genesis.Request.GroupID || genesis.AuthorityPublicKey != v.AuthorityKey || len(genesis.InitialVoters) != len(v.Genesis.Request.InitialVoters) {
		t.Fatal("verified genesis summary mismatch")
	}
	got, err = CoordinationCanonicalJoinPermit(v.Join.Request)
	checkProvisioningBytes(t, got, err, v.Join.Document)
	got, err = CoordinationJoinSigningBytes(got)
	checkProvisioningBytes(t, got, err, v.Join.Signing)
	candidate := provisioningHex(t, v.Join.Request.CandidateNodeID)
	ticket, err := CoordinationCanonicalCandidateTicket(candidate)
	if err != nil || ticket != v.Join.Ticket {
		t.Fatalf("ticket: %v", err)
	}
	got, err = CoordinationSignedDocumentFromParts(provisioningHex(t, v.Join.Document), key, provisioningHex(t, v.Join.Signature))
	checkProvisioningBytes(t, got, err, v.Join.Envelope)
	join, err := CoordinationVerifyJoinPermitForStart(key, v.Join.Request.AuthorityDomain, candidate, got)
	if err != nil {
		t.Fatal(err)
	}
	checkProvisioningBytes(t, join.DocumentBytes, nil, v.Join.Document)
	checkProvisioningSummary(t, join, join.DocumentBytes, v.Join.Verified)
	if join.GroupID != v.Join.Request.GroupID || join.GenesisHash != v.Join.Request.GenesisHash || join.CandidateNodeID != v.Join.Request.CandidateNodeID || len(join.BootstrapSenders) != len(v.Join.Request.BootstrapSenders) {
		t.Fatal("verified join summary mismatch")
	}
}
func TestCoordinationProvisioning_TrustPinsAndErrors(t *testing.T) {
	v := loadProvisioningVector(t)
	key := provisioningHex(t, v.AuthorityKey)
	env := provisioningHex(t, v.Genesis.Envelope)
	wrong := append([]byte(nil), key...)
	wrong[0] ^= 1
	_, err := CoordinationVerifyGenesis(wrong, v.Genesis.Request.AuthorityDomain, env)
	checkProvisioningCode(t, err, "wrong_authority_key")
	if !errors.Is(err, ErrVerificationFailed) {
		t.Fatal("native status lost")
	}
	_, err = CoordinationVerifyGenesis(key, "wrong-domain", env)
	checkProvisioningCode(t, err, "wrong_domain")
	sig := provisioningHex(t, v.Genesis.Signature)
	sig[0] ^= 1
	tampered, err := CoordinationSignedDocumentFromParts(provisioningHex(t, v.Genesis.Document), key, sig)
	if err != nil {
		t.Fatal(err)
	}
	_, err = CoordinationVerifyGenesis(key, v.Genesis.Request.AuthorityDomain, tampered)
	checkProvisioningCode(t, err, "bad_signature")
	_, err = CoordinationSignedDocumentFromParts(nil, key, []byte{1})
	checkProvisioningCode(t, err, "invalid_input")
	_, err = CoordinationCanonicalCandidateTicket([]byte{1})
	checkProvisioningCode(t, err, "invalid_input")
	malformed := v.Genesis.Request
	malformed.GroupID = "00"
	_, err = CoordinationCanonicalGenesis(malformed)
	checkProvisioningCode(t, err, "invalid_input")
	candidate := provisioningHex(t, v.Join.Request.CandidateNodeID)
	candidate[0] ^= 1
	_, err = CoordinationVerifyJoinPermitForStart(key, v.Join.Request.AuthorityDomain, candidate, provisioningHex(t, v.Join.Envelope))
	checkProvisioningCode(t, err, "wrong_node")
}
func TestCoordinationProvisioning_EncodingDoesNotInferPolicy(t *testing.T) {
	v := loadProvisioningVector(t)
	doc := v.Genesis.Request
	doc.DeploymentProfileCode = 99
	encoded, err := CoordinationCanonicalGenesis(doc)
	if err != nil {
		t.Fatal(err)
	}
	preimage, err := CoordinationGenesisSigningBytes(encoded)
	if err != nil {
		t.Fatal(err)
	}
	secret := ed25519.NewKeyFromSeed(provisioningHex(t, v.TestSecret))
	key := provisioningHex(t, v.AuthorityKey)
	envelope, err := CoordinationSignedDocumentFromParts(encoded, key, ed25519.Sign(secret, preimage))
	if err != nil {
		t.Fatal(err)
	}
	_, err = CoordinationVerifyGenesis(key, doc.AuthorityDomain, envelope)
	checkProvisioningCode(t, err, "unknown_profile")
	// Returned storage is detached from native owners and other calls.
	encoded[0] ^= 1
	fresh, err := CoordinationCanonicalGenesis(v.Genesis.Request)
	checkProvisioningBytes(t, fresh, err, v.Genesis.Document)
}
