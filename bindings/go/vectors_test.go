//go:build cgo

package aster

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// Conformance vectors (conformance/vectors/*.json) through the Go wrappers.

func loadVectors(t *testing.T, name string, v any) {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("..", "..", "conformance", "vectors", name))
	if err != nil {
		t.Fatalf("read %s: %v", name, err)
	}
	if err := json.Unmarshal(data, v); err != nil {
		t.Fatalf("parse %s: %v", name, err)
	}
}

func mustHex(t *testing.T, s string) []byte {
	t.Helper()
	b, err := hex.DecodeString(s)
	if err != nil {
		t.Fatalf("hex %q: %v", s, err)
	}
	return b
}

func pairs(keys, values []string) []MetadataEntry {
	if len(keys) == 0 {
		return nil
	}
	out := make([]MetadataEntry, len(keys))
	for i := range keys {
		out[i] = MetadataEntry{keys[i], values[i]}
	}
	return out
}

func TestVectors_Envelope(t *testing.T) {
	var f struct {
		Vectors []struct {
			Name  string `json:"name"`
			Type  string `json:"type"`
			Hex   string `json:"hex"`
			Input struct {
				Service           string   `json:"service"`
				Method            string   `json:"method"`
				Version           int32    `json:"version"`
				CallID            int32    `json:"callId"`
				Deadline          int16    `json:"deadline"`
				SerializationMode int8     `json:"serializationMode"`
				MetadataKeys      []string `json:"metadataKeys"`
				MetadataValues    []string `json:"metadataValues"`
				SessionID         int32    `json:"sessionId"`
				Code              int32    `json:"code"`
				Message           string   `json:"message"`
				DetailKeys        []string `json:"detailKeys"`
				DetailValues      []string `json:"detailValues"`
			} `json:"input"`
		} `json:"vectors"`
		Invalid []struct {
			Name string `json:"name"`
			Type string `json:"type"`
			Hex  string `json:"hex"`
		} `json:"invalid"`
	}
	loadVectors(t, "envelope.json", &f)
	if len(f.Vectors) == 0 || len(f.Invalid) == 0 {
		t.Fatal("no vectors")
	}
	for _, v := range f.Vectors {
		t.Run(v.Name, func(t *testing.T) {
			want := mustHex(t, v.Hex)
			in := v.Input
			var got []byte
			var err error
			switch v.Type {
			case "StreamHeader":
				h := StreamHeader{
					Service: in.Service, Method: in.Method, Version: in.Version, CallID: in.CallID,
					Deadline: in.Deadline, SerializationMode: SerializationMode(in.SerializationMode),
					Metadata: pairs(in.MetadataKeys, in.MetadataValues), SessionID: in.SessionID,
				}
				if got, err = EncodeStreamHeader(h); err != nil {
					t.Fatal(err)
				}
				back, err := DecodeStreamHeader(want)
				if err != nil {
					t.Fatal(err)
				}
				if back.Service != h.Service || back.Method != h.Method || back.Version != h.Version ||
					back.CallID != h.CallID || back.Deadline != h.Deadline || back.SessionID != h.SessionID ||
					back.SerializationMode != h.SerializationMode || !equalMeta(back.Metadata, h.Metadata) {
					t.Fatalf("decode = %+v, want %+v", back, h)
				}
			case "CallHeader":
				h := CallHeader{Method: in.Method, CallID: in.CallID, Deadline: in.Deadline,
					Metadata: pairs(in.MetadataKeys, in.MetadataValues)}
				if got, err = EncodeCallHeader(h); err != nil {
					t.Fatal(err)
				}
				back, err := DecodeCallHeader(want)
				if err != nil {
					t.Fatal(err)
				}
				if back.Method != h.Method || back.CallID != h.CallID || back.Deadline != h.Deadline || !equalMeta(back.Metadata, h.Metadata) {
					t.Fatalf("decode = %+v, want %+v", back, h)
				}
			case "RpcStatus":
				s := RpcStatus{Code: StatusCode(in.Code), Message: in.Message, Details: pairs(in.DetailKeys, in.DetailValues)}
				if got, err = EncodeRpcStatus(s); err != nil {
					t.Fatal(err)
				}
				back, err := DecodeRpcStatus(want)
				if err != nil {
					t.Fatal(err)
				}
				if back.Code != s.Code || back.Message != s.Message || !equalMeta(back.Details, s.Details) {
					t.Fatalf("decode = %+v, want %+v", back, s)
				}
			default:
				t.Fatalf("unknown type %s", v.Type)
			}
			if !bytes.Equal(got, want) {
				t.Fatalf("encode = %x, want %x", got, want)
			}
		})
	}
	for _, v := range f.Invalid {
		t.Run("invalid/"+v.Name, func(t *testing.T) {
			b := mustHex(t, v.Hex)
			var err error
			switch v.Type {
			case "StreamHeader":
				_, err = DecodeStreamHeader(b)
			case "CallHeader":
				_, err = DecodeCallHeader(b)
			case "RpcStatus":
				_, err = DecodeRpcStatus(b)
			}
			if !errors.Is(err, ErrInvalidArgument) {
				t.Fatalf("err = %v, want ErrInvalidArgument", err)
			}
		})
	}
}

func equalMeta(a, b []MetadataEntry) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func payloadByRule(rule string, n int) []byte {
	p := make([]byte, n)
	if strings.Contains(rule, "% 251") {
		for i := range p {
			p[i] = byte(i % 251)
		}
	}
	return p
}

func TestVectors_Compression(t *testing.T) {
	var f struct {
		Parameters struct {
			Threshold int `json:"threshold_bytes"`
			Max       int `json:"max_decompressed_bytes"`
		} `json:"parameters"`
		Encode []struct {
			Name     string `json:"name"`
			Expected string `json:"expected_frame_hex"`
			Flags    byte   `json:"input_flags"`
			Length   int    `json:"payload_length"`
			Rule     string `json:"payload_rule"`
		} `json:"encode_vectors"`
		Decode []struct {
			Name   string `json:"name"`
			Frame  string `json:"frame_hex"`
			Flags  byte   `json:"flags"`
			Length int    `json:"payload_length"`
			Blake3 string `json:"payload_blake3"`
		} `json:"decode_vectors"`
		Reject []struct {
			Name  string `json:"name"`
			Frame string `json:"frame_hex"`
		} `json:"reject_vectors"`
	}
	loadVectors(t, "compression.json", &f)
	if f.Parameters.Threshold != CompressionThreshold || f.Parameters.Max != MaxDecompressedSize {
		t.Fatalf("parameters %+v != threshold %d / max %d", f.Parameters, CompressionThreshold, MaxDecompressedSize)
	}
	for _, v := range f.Encode {
		t.Run(v.Name, func(t *testing.T) {
			frame, err := EncodePayloadFrame(payloadByRule(v.Rule, v.Length), v.Flags)
			if err != nil {
				t.Fatal(err)
			}
			if got := hex.EncodeToString(frame); got != v.Expected {
				t.Fatalf("frame = %.40s..., want %.40s...", got, v.Expected)
			}
		})
	}
	for _, v := range f.Decode {
		t.Run(v.Name, func(t *testing.T) {
			payload, flags, rest, err := DecodeFrame(mustHex(t, v.Frame))
			if err != nil || len(rest) != 0 {
				t.Fatalf("frame: %v (rest %d)", err, len(rest))
			}
			if flags != v.Flags {
				t.Fatalf("flags %#x != %#x", flags, v.Flags)
			}
			out, err := PayloadOf(payload, flags)
			if err != nil {
				t.Fatal(err)
			}
			if len(out) != v.Length {
				t.Fatalf("length %d != %d", len(out), v.Length)
			}
			if h, _ := Blake3Hex(out); h != v.Blake3 {
				t.Fatalf("blake3 %s != %s", h, v.Blake3)
			}
		})
	}
	for _, v := range f.Reject {
		t.Run("reject/"+v.Name, func(t *testing.T) {
			payload, flags, _, err := DecodeFrame(mustHex(t, v.Frame))
			if err != nil {
				t.Fatal(err)
			}
			if _, err := PayloadOf(payload, flags); !errors.Is(err, ErrInvalidArgument) {
				t.Fatalf("err = %v, want ErrInvalidArgument", err)
			}
		})
	}
	// Round trip above the threshold, and nothing compressed at or below it.
	big := payloadByRule("% 251", 20_000)
	frame, err := EncodePayloadFrame(big, FlagEndStream)
	if err != nil {
		t.Fatal(err)
	}
	payload, flags, _, _ := DecodeFrame(frame)
	if flags != FlagEndStream|FlagCompressed {
		t.Fatalf("flags %#x", flags)
	}
	if out, err := PayloadOf(payload, flags); err != nil || !bytes.Equal(out, big) {
		t.Fatalf("round trip: %v", err)
	}
	if c, err := ZstdMaybeCompress(big[:CompressionThreshold], CompressionThreshold); err != nil || c != nil {
		t.Fatalf("at threshold: %v %d", err, len(c))
	}
}

type grantCtxJSON struct {
	App         string `json:"app"`
	Granter     string `json:"granter"`
	Recipient   string `json:"recipient"`
	ResourceHex string `json:"resource_hex"`
	Path        string `json:"path"`
	Role        string `json:"role"`
}

func (g grantCtxJSON) ctx(t *testing.T) GrantContext {
	return GrantContext{App: g.App, Granter: g.Granter, Recipient: g.Recipient,
		Resource: mustHex(t, g.ResourceHex), Path: g.Path, Role: g.Role}
}

func key32(t *testing.T, s string) Key {
	var k Key
	b := mustHex(t, s)
	if len(b) != 32 {
		t.Fatalf("key %q is %d bytes", s, len(b))
	}
	copy(k[:], b)
	return k
}

func TestVectors_Grants(t *testing.T) {
	var f struct {
		AAD []struct {
			Name    string       `json:"name"`
			AADHex  string       `json:"aad_hex"`
			Context grantCtxJSON `json:"context"`
		} `json:"aad"`
		GrantKey []struct {
			Key, Prefix, Recipient string
		} `json:"grant_key"`
		RoleRule []struct {
			CanWrite bool   `json:"can_write"`
			OK       bool   `json:"ok"`
			Role     string `json:"role"`
		} `json:"role_rule"`
		Open []struct {
			Name         string       `json:"name"`
			Context      grantCtxJSON `json:"context"`
			Envelope     string       `json:"envelope_hex"`
			Plaintext    string       `json:"plaintext_hex"`
			RecipientKey string       `json:"recipient_key_hex"`
			RecipientPub string       `json:"recipient_public_hex"`
			Scheme       string       `json:"scheme"`
		} `json:"open"`
		OpenInvalid []struct {
			Name         string       `json:"name"`
			Context      grantCtxJSON `json:"context"`
			Envelope     string       `json:"envelope_hex"`
			RecipientKey string       `json:"recipient_key_hex"`
			Scheme       string       `json:"scheme"`
		} `json:"open_invalid"`
		NamespaceOpen []struct {
			Name       string       `json:"name"`
			CanWrite   bool         `json:"can_write"`
			Capability string       `json:"capability_hex"`
			Context    grantCtxJSON `json:"context"`
			Envelope   string       `json:"envelope_hex"`
			Secret     string       `json:"recipient_secret_hex"`
		} `json:"namespace_open"`
		NamespaceOpenInvalid []struct {
			Name     string       `json:"name"`
			Context  grantCtxJSON `json:"context"`
			Envelope string       `json:"envelope_hex"`
			Secret   string       `json:"recipient_secret_hex"`
		} `json:"namespace_open_invalid"`
		Reconcile []struct {
			Name    string       `json:"name"`
			Context grantCtxJSON `json:"context"`
			Payload string       `json:"payload"`
			Secret  string       `json:"recipient_secret_hex"`
			Record  *string      `json:"record_hex"`
			Outcome struct {
				Kind          string `json:"kind"`
				Reason        string `json:"reason"`
				Role          string `json:"role"`
				Plaintext     string `json:"plaintext_hex"`
				Capability    string `json:"capability_hex"`
				ErrorContains string `json:"error_contains"`
			} `json:"outcome"`
		} `json:"reconcile"`
	}
	loadVectors(t, "grants.json", &f)

	for _, v := range f.AAD {
		got, err := GrantContextAAD(v.Context.ctx(t))
		if err != nil || hex.EncodeToString(got) != v.AADHex {
			t.Errorf("aad %s: %v %x", v.Name, err, got)
		}
	}
	for _, v := range f.GrantKey {
		if got, err := GrantKey(v.Prefix, v.Recipient); err != nil || got != v.Key {
			t.Errorf("grant key %q: %v %q != %q", v.Prefix, err, got, v.Key)
		}
	}
	for _, v := range f.RoleRule {
		err := GrantCheckRole(v.Role, v.CanWrite)
		if v.OK != (err == nil) || (err != nil && !errors.Is(err, ErrVerificationFailed)) {
			t.Errorf("role rule %q can_write=%v: %v", v.Role, v.CanWrite, err)
		}
	}
	for _, v := range f.Open {
		t.Run("open/"+v.Name, func(t *testing.T) {
			ctx := v.Context.ctx(t)
			env := mustHex(t, v.Envelope)
			var got []byte
			var err error
			if v.Scheme == "x25519" {
				got, err = GrantOpenWithKey(key32(t, v.RecipientKey), ctx, env)
				if pub, perr := HPKEPublicKeyFromPrivate(key32(t, v.RecipientKey)); perr != nil || hex.EncodeToString(pub[:]) != v.RecipientPub {
					t.Errorf("x25519 public: %v %x", perr, pub)
				}
			} else {
				got, err = GrantOpen(key32(t, v.RecipientKey), ctx, env)
			}
			if err != nil || hex.EncodeToString(got) != v.Plaintext {
				t.Fatalf("open: %v %x", err, got)
			}
		})
	}
	for _, v := range f.OpenInvalid {
		t.Run("open_invalid/"+v.Name, func(t *testing.T) {
			ctx := v.Context.ctx(t)
			var err error
			if v.Scheme == "x25519" {
				_, err = GrantOpenWithKey(key32(t, v.RecipientKey), ctx, mustHex(t, v.Envelope))
			} else {
				_, err = GrantOpen(key32(t, v.RecipientKey), ctx, mustHex(t, v.Envelope))
			}
			if !errors.Is(err, ErrVerificationFailed) {
				t.Fatalf("err = %v, want ErrVerificationFailed", err)
			}
		})
	}
	for _, v := range f.NamespaceOpen {
		t.Run("namespace_open/"+v.Name, func(t *testing.T) {
			want, err := DecodeNamespaceCapability(mustHex(t, v.Capability))
			if err != nil {
				t.Fatal(err)
			}
			if (want.Access == AccessWrite) != v.CanWrite {
				t.Fatalf("capability access %d vs can_write %v", want.Access, v.CanWrite)
			}
			if enc, err := EncodeNamespaceCapability(want); err != nil || hex.EncodeToString(enc) != v.Capability {
				t.Fatalf("re-encode: %v %x", err, enc)
			}
			got, err := NamespaceGrantOpen(key32(t, v.Secret), v.Context.ctx(t), mustHex(t, v.Envelope))
			if err != nil || got != want {
				t.Fatalf("open: %v %+v != %+v", err, got, want)
			}
		})
	}
	for _, v := range f.NamespaceOpenInvalid {
		if _, err := NamespaceGrantOpen(key32(t, v.Secret), v.Context.ctx(t), mustHex(t, v.Envelope)); !errors.Is(err, ErrVerificationFailed) {
			t.Errorf("namespace_open_invalid %s: %v", v.Name, err)
		}
	}
	for _, v := range f.Reconcile {
		t.Run("reconcile/"+v.Name, func(t *testing.T) {
			var record []byte
			if v.Record != nil {
				record = mustHex(t, *v.Record)
				if record == nil {
					record = []byte{}
				}
			}
			reconcile := ReconcileGrant
			if v.Payload == "namespace" {
				reconcile = ReconcileNamespaceGrant
			}
			out, err := reconcile(key32(t, v.Secret), v.Context.ctx(t), record)
			if err != nil {
				t.Fatal(err)
			}
			kinds := map[string]GrantOutcomeKind{"active": OutcomeActive, "revoked": OutcomeRevoked, "unopenable": OutcomeUnopenable, "absent": OutcomeAbsent}
			if out.Kind != kinds[v.Outcome.Kind] {
				t.Fatalf("kind %d, want %s (reason %q)", out.Kind, v.Outcome.Kind, out.Reason)
			}
			switch v.Outcome.Kind {
			case "revoked":
				if out.Reason != v.Outcome.Reason || out.Role != v.Outcome.Role {
					t.Fatalf("revoked %q/%q", out.Reason, out.Role)
				}
			case "active":
				if v.Outcome.Capability != "" {
					want, _ := DecodeNamespaceCapability(mustHex(t, v.Outcome.Capability))
					if out.Access != want.Access || !bytes.Equal(out.Payload, want.Key[:]) {
						t.Fatalf("namespace payload %d %x", out.Access, out.Payload)
					}
				} else if hex.EncodeToString(out.Payload) != v.Outcome.Plaintext {
					t.Fatalf("payload %x", out.Payload)
				}
			case "unopenable":
				if v.Outcome.ErrorContains != "" && !strings.Contains(out.Reason, v.Outcome.ErrorContains) {
					t.Fatalf("reason %q lacks %q", out.Reason, v.Outcome.ErrorContains)
				}
			}
			if record != nil && len(record) > 0 && v.Outcome.Kind != "unopenable" {
				rec, err := DecodeGrantRecord(record)
				if err != nil {
					t.Fatal(err)
				}
				enc, err := EncodeGrantRecord(rec)
				if err != nil || !bytes.Equal(enc, record) {
					t.Fatalf("record re-encode: %v", err)
				}
			}
		})
	}
}
