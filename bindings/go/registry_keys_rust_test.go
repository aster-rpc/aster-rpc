//go:build cgo

package aster

import (
	"bytes"
	"testing"
)

// The Go helpers agree byte for byte with core's (through the C ABI).
func TestRegistryKeysMatchRust(t *testing.T) {
	type c struct {
		kind       int32
		a1, a2, a3 string
		got        []byte
	}
	for _, tc := range []c{
		{0, "abc", "", "", ContractKey("abc")},
		{1, "a/b", "10", "", VersionKey("a/b", 10)},
		{2, "svc", "st/able", "", ChannelKey("svc", "st/able")},
		{3, "svc", "cid", "e%id", LeaseKey("svc", "cid", "e%id")},
		{4, "svc", "cid", "", LeasePrefix("svc", "cid")},
		{5, "writers", "", "", AclKey("writers")},
	} {
		want, err := RegistryKeyRust(tc.kind, tc.a1, tc.a2, tc.a3)
		if err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(tc.got, want) {
			t.Errorf("kind %d: Go %q, Rust %q", tc.kind, tc.got, want)
		}
	}
}
