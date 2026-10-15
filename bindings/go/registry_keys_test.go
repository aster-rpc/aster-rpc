package aster

import (
	"bytes"
	"testing"
)

// The pure-Go key helpers produce the documented, prefix-free format.
func TestRegistryKeyFormat(t *testing.T) {
	cases := map[string][2]string{
		"contract":      {string(ContractKey("abc")), "contracts/abc/"},
		"manifest":      {string(ManifestKey("abc")), "manifests/abc/"},
		"version":       {string(VersionKey("svc", 7)), "services/svc/versions/v7/"},
		"versionPrefix": {string(VersionPrefix("svc")), "services/svc/versions/"},
		"channel":       {string(ChannelKey("svc", "stable")), "services/svc/channels/stable/"},
		"tag":           {string(TagKey("svc", "latest")), "services/svc/tags/latest/"},
		"lease":         {string(LeaseKey("svc", "cid", "eid")), "services/svc/contracts/cid/endpoints/eid/"},
		"leasePrefix":   {string(LeasePrefix("svc", "cid")), "services/svc/contracts/cid/endpoints/"},
		"acl":           {string(AclKey("writers")), "_aster/acl/writers/"},
		"config":        {string(ConfigKey("version")), "_aster/config/version/"},
		"escaped":       {string(ChannelKey("x", "a/b%c")), "services/x/channels/a%2Fb%25c/"},
	}
	for name, c := range cases {
		if c[0] != c[1] {
			t.Errorf("%s: got %q, want %q", name, c[0], c[1])
		}
	}
}

// iroh-docs lets a write at a key supersede the same author's older entries
// whose keys extend it, so no registry key may be a byte prefix of another.
func TestRegistryKeysArePrefixFree(t *testing.T) {
	keys := familyKeys()
	for _, a := range keys {
		for _, b := range keys {
			if !bytes.Equal(a, b) && bytes.HasPrefix(b, a) {
				t.Fatalf("%q is a prefix of %q", a, b)
			}
		}
	}
}

// familyKeys returns keys from every helper, over segments chosen to provoke
// prefix collisions. A new key family belongs here.
func familyKeys() [][]byte {
	leaves := []string{"1", "10", "stable", "stable2", "x/y", "x", "", "%2F"}
	var keys [][]byte
	for _, name := range []string{"svc", "svc2", "a/b", "a", "50%"} {
		for _, leaf := range leaves {
			keys = append(keys, ContractKey(leaf), ManifestKey(leaf), AclKey(leaf), ConfigKey(leaf),
				ChannelKey(name, leaf), TagKey(name, leaf))
			for _, eid := range leaves {
				keys = append(keys, LeaseKey(name, leaf, eid))
			}
		}
		for _, v := range []int32{1, 10, 100, -1} {
			keys = append(keys, VersionKey(name, v))
		}
	}
	return keys
}

// A client that syncs only RegistryPrefixes receives every family the helpers write.
func TestRegistryPrefixesCoverEveryFamily(t *testing.T) {
	for _, key := range familyKeys() {
		covered := false
		for _, p := range RegistryPrefixes {
			covered = covered || bytes.HasPrefix(key, p)
		}
		if !covered {
			t.Fatalf("no RegistryPrefixes entry covers %q", key)
		}
	}
}
