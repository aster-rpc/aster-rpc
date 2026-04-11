//go:build cgo

package aster

import (
	"testing"
)

func TestContractID(t *testing.T) {
	contractID := ContractID("abcd12345678" + string(make([]byte, 52)))
	if string(contractID) != "abcd12345678"+string(make([]byte, 52)) {
		t.Errorf("ContractID does not preserve value")
	}
}

func TestCanonicalJSON(t *testing.T) {
	// Test that CanonicalJSON.Normalize parses and normalizes JSON
	canon := CanonicalJSON{}

	// Input JSON with whitespace and unsorted keys
	input := `{"b":1,"a":2}`
	normalized, err := canon.Normalize(input)
	if err != nil {
		t.Fatalf("Normalize failed: %v", err)
	}

	// Normalized output should be compact
	if normalized != `{"a":2,"b":1}` {
		t.Errorf("Normalize = %s, want {\"a\":2,\"b\":1}", normalized)
	}
}

func TestCanonicalJSONInvalidInput(t *testing.T) {
	canon := CanonicalJSON{}

	// Invalid JSON should return error
	_, err := canon.Normalize(`not json`)
	if err == nil {
		t.Errorf("Expected error for invalid JSON, got nil")
	}
}

func TestParseTicketJSON(t *testing.T) {
	jsonStr := `{"endpoint_id":"abc123","relay_addr":null,"direct_addrs":["1.2.3.4:5678"],"credential_type":"test","credential_data":"somedata"}`

	content, err := parseTicketJSON(jsonStr)
	if err != nil {
		t.Fatalf("parseTicketJSON failed: %v", err)
	}

	if content.EndpointID != "abc123" {
		t.Errorf("EndpointID = %s, want abc123", content.EndpointID)
	}
	if content.RelayAddr != "" {
		t.Errorf("RelayAddr = %s, want empty", content.RelayAddr)
	}
	if len(content.DirectAddrs) != 1 || content.DirectAddrs[0] != "1.2.3.4:5678" {
		t.Errorf("DirectAddrs = %v, want [1.2.3.4:5678]", content.DirectAddrs)
	}
	if content.CredentialType != "test" {
		t.Errorf("CredentialType = %s, want test", content.CredentialType)
	}
	if content.CredentialData != "somedata" {
		t.Errorf("CredentialData = %s, want somedata", content.CredentialData)
	}
}

func TestParseTicketJSONWithRelay(t *testing.T) {
	jsonStr := `{"endpoint_id":"xyz789","relay_addr":"1.2.3.4:9999","direct_addrs":["addr1","addr2"],"credential_type":"type","credential_data":"data"}`

	content, err := parseTicketJSON(jsonStr)
	if err != nil {
		t.Fatalf("parseTicketJSON failed: %v", err)
	}

	if content.EndpointID != "xyz789" {
		t.Errorf("EndpointID = %s, want xyz789", content.EndpointID)
	}
	if content.RelayAddr != "1.2.3.4:9999" {
		t.Errorf("RelayAddr = %s, want 1.2.3.4:9999", content.RelayAddr)
	}
	if len(content.DirectAddrs) != 2 {
		t.Errorf("DirectAddrs len = %d, want 2", len(content.DirectAddrs))
	}
}

func TestTicketContent(t *testing.T) {
	content := &TicketContent{
		EndpointID:      "test123",
		RelayAddr:      "relay.example.com:443",
		DirectAddrs:    []string{"1.2.3.4:5678", "5.6.7.8:9012"},
		CredentialType: "TestCredential",
		CredentialData: "testdata",
	}

	if content.EndpointID != "test123" {
		t.Errorf("EndpointID = %s, want test123", content.EndpointID)
	}
	if content.RelayAddr != "relay.example.com:443" {
		t.Errorf("RelayAddr = %s, want relay.example.com:443", content.RelayAddr)
	}
	if len(content.DirectAddrs) != 2 {
		t.Errorf("DirectAddrs len = %d, want 2", len(content.DirectAddrs))
	}
	if content.CredentialType != "TestCredential" {
		t.Errorf("CredentialType = %s, want TestCredential", content.CredentialType)
	}
}
