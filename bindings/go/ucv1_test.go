package aster

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"unicode/utf8"
)

// UCv1 against conformance/vectors/upcalls.json: every valid case of a
// supported type decodes to the vector's JSON rendering and re-encodes to
// the same bytes; every invalid input is refused; the kind / status /
// cancel-reason tables match.

type ucvVectors struct {
	Kinds []struct {
		Code     int    `json:"code"`
		Name     string `json:"name"`
		Decision bool   `json:"decision"`
	} `json:"kinds"`
	Statuses []struct {
		Code int    `json:"code"`
		Name string `json:"name"`
	} `json:"statuses"`
	CancelReasons []struct {
		Code int    `json:"code"`
		Name string `json:"name"`
	} `json:"cancel_reasons"`
	Valid []struct {
		Hex   string          `json:"hex"`
		Name  string          `json:"name"`
		Type  string          `json:"type"`
		Value json.RawMessage `json:"value"`
	} `json:"valid"`
	Invalid []struct {
		Hex  string `json:"hex"`
		Name string `json:"name"`
		Type string `json:"type"`
	} `json:"invalid"`
}

func loadUCVectors(t testing.TB) ucvVectors {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("..", "..", "conformance", "vectors", "upcalls.json"))
	if err != nil {
		t.Fatal(err)
	}
	var v ucvVectors
	if err := json.Unmarshal(data, &v); err != nil {
		t.Fatal(err)
	}
	return v
}

type jm = map[string]any

func hdrsJSON(hs []HTTPHeader) any {
	out := []any{}
	for _, h := range hs {
		if utf8.Valid(h.Value) {
			out = append(out, jm{"name": h.Name, "value": string(h.Value)})
		} else {
			out = append(out, jm{"name": h.Name, "value_hex": hex.EncodeToString(h.Value)})
		}
	}
	return out
}

func bodyJSON(b HTTPBody) any {
	if b.Stream {
		return "stream"
	}
	return jm{"inline": string(b.Inline)}
}

func opJSON(o LeaseOp) any {
	switch o.Kind {
	case LeaseAcquire:
		return jm{"acquire": jm{"candidate": o.Actor, "ttl_ms": o.TTLMs}}
	case LeaseRenew:
		return jm{"renew": jm{"epoch": o.Epoch, "holder": o.Actor}}
	case LeaseRelease:
		return jm{"release": jm{"epoch": o.Epoch, "holder": o.Actor}}
	}
	return jm{"revoke": jm{"authority": o.Actor}}
}

func fenceJSON(f Fence) any { return jm{"epoch": f.Epoch, "holder": f.Holder, "resource": f.Resource} }

func snapJSON(s LeaseSnapshot) any {
	var holder, rem any
	if s.Holder != nil {
		holder = *s.Holder
	}
	if s.RemainingMs != nil {
		rem = *s.RemainingMs
	}
	return jm{"epoch": s.Epoch, "held": s.Held, "holder": holder, "remaining_ms": rem, "resource": s.Resource}
}

type ucCodec struct {
	// decode+render, and decode+re-encode.
	render func([]byte) (any, error)
	round  func([]byte) ([]byte, error)
}

func nz(s []string) []string {
	if s == nil {
		return []string{}
	}
	return s
}

func ucCodecFor(typ string) *ucCodec {
	switch typ {
	case "LeaseTransitionRequest":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeLeaseTransitionRequest(b)
				return jm{"op": opJSON(v.Op), "resource": v.Resource}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeLeaseTransitionRequest(b)
				return EncodeLeaseTransitionRequest(v), err
			}}
	case "TransitionOutcome":
		return &ucCodec{
			func(b []byte) (any, error) {
				o, err := DecodeTransitionOutcome(b)
				if err != nil {
					return nil, err
				}
				switch o.Kind {
				case TransitionGranted:
					return jm{"granted": fenceJSON(o.Fence)}, nil
				case TransitionRenewed:
					return "renewed", nil
				case TransitionReleased:
					return "released", nil
				case TransitionRevoked:
					return "revoked", nil
				}
				return jm{"denied": jm{"reason": o.Reason.String(), "snapshot": snapJSON(o.Snapshot)}}, nil
			},
			func(b []byte) ([]byte, error) {
				o, err := DecodeTransitionOutcome(b)
				return EncodeTransitionOutcome(o), err
			}}
	case "LeaseSnapshotRequest":
		return &ucCodec{
			func(b []byte) (any, error) { s, err := DecodeLeaseSnapshotRequest(b); return jm{"resource": s}, err },
			func(b []byte) ([]byte, error) {
				s, err := DecodeLeaseSnapshotRequest(b)
				return EncodeLeaseSnapshotRequest(s), err
			}}
	case "LeaseSnapshot":
		return &ucCodec{
			func(b []byte) (any, error) { s, err := DecodeLeaseSnapshot(b); return snapJSON(s), err },
			func(b []byte) ([]byte, error) { s, err := DecodeLeaseSnapshot(b); return EncodeLeaseSnapshot(s), err }}
	case "LeaseAuthorizeRequest":
		return &ucCodec{
			func(b []byte) (any, error) { o, err := DecodeLeaseAuthorizeRequest(b); return jm{"op": opJSON(o)}, err },
			func(b []byte) ([]byte, error) {
				o, err := DecodeLeaseAuthorizeRequest(b)
				return EncodeLeaseAuthorizeRequest(o), err
			}}
	case "FencedMutateRequest":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeFencedMutateRequest(b)
				return jm{"fence": fenceJSON(v.Fence), "op_hex": hex.EncodeToString(v.Op), "writer": v.Writer}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeFencedMutateRequest(b)
				return EncodeFencedMutateRequest(v), err
			}}
	case "FencedMutateResponse":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeFencedMutateResponse(b)
				if err != nil {
					return nil, err
				}
				if v.Applied() {
					return jm{"applied": jm{"output_hex": hex.EncodeToString(v.Output)}}, nil
				}
				return jm{"rejected": jm{"carried": v.Rejection.Carried, "current": v.Rejection.Current,
					"reason": v.Rejection.Reason.String()}}, nil
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeFencedMutateResponse(b)
				return EncodeFencedMutateResponse(v), err
			}}
	case "Decision":
		return &ucCodec{
			func(b []byte) (any, error) { v, err := DecodeDecision(b); return v, err },
			func(b []byte) ([]byte, error) { v, err := DecodeDecision(b); return EncodeDecision(v), err }}
	case "TopologyAdmittedRequest":
		return &ucCodec{
			func(b []byte) (any, error) { v, err := DecodeTopologyAdmittedRequest(b); return jm{"node_ids": v}, err },
			func(b []byte) ([]byte, error) {
				v, err := DecodeTopologyAdmittedRequest(b)
				return EncodeTopologyAdmittedRequest(v), err
			}}
	case "TopologyAdmittedResponse":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeTopologyAdmittedResponse(b)
				return jm{"admitted": v}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeTopologyAdmittedResponse(b)
				return EncodeTopologyAdmittedResponse(v), err
			}}
	case "TopologySnapshotRequest":
		return &ucCodec{
			func(b []byte) (any, error) { return jm{}, DecodeTopologySnapshotRequest(b) },
			func(b []byte) ([]byte, error) {
				return EncodeTopologySnapshotRequest(), DecodeTopologySnapshotRequest(b)
			}}
	case "TopologySnapshot":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeTopologySnapshot(b)
				clusters, pairs := []any{}, []any{}
				for _, c := range v.Clusters {
					clusters = append(clusters, jm{"bridge": c.Bridge, "members": nz(c.Members), "witnesses": nz(c.Witnesses)})
				}
				for _, p := range v.FarPairs {
					pairs = append(pairs, []string{p.A, p.B})
				}
				return jm{"clusters": clusters, "far_pairs": pairs, "separation_coverage": v.SeparationCoverage}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeTopologySnapshot(b)
				return EncodeTopologySnapshot(v), err
			}}
	case "ExposeAuthorizeRequest":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeExposeAuthorizeRequest(b)
				return jm{"metadata_hex": hex.EncodeToString(v.Metadata), "peer_id": v.PeerID, "service_id": v.ServiceID}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeExposeAuthorizeRequest(b)
				return EncodeExposeAuthorizeRequest(v), err
			}}
	case "ExposeHttpRequest":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeExposeHTTPRequest(b)
				var peer any
				if v.HasPeer {
					peer = v.PeerID
				}
				return jm{"body": bodyJSON(v.Body), "headers": hdrsJSON(v.Headers), "method": v.Method, "peer_id": peer,
					"service_id": v.ServiceID, "stream_id": v.StreamID, "uri": v.URI, "version": v.Version}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeExposeHTTPRequest(b)
				return EncodeExposeHTTPRequest(v), err
			}}
	case "ExposeHttpResponse":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeExposeHTTPResponse(b)
				return jm{"body": bodyJSON(v.Body), "headers": hdrsJSON(v.Headers), "status": v.Status}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeExposeHTTPResponse(b)
				return EncodeExposeHTTPResponse(v), err
			}}
	case "ExposeHttpTrailers":
		return &ucCodec{
			func(b []byte) (any, error) { v, err := DecodeExposeHTTPTrailers(b); return hdrsJSON(v), err },
			func(b []byte) ([]byte, error) {
				v, err := DecodeExposeHTTPTrailers(b)
				return EncodeExposeHTTPTrailers(v), err
			}}
	case "ExposeRoutePolicyRequest":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeExposeRoutePolicyRequest(b)
				specs := []any{}
				for _, s := range v.Specs {
					proto := "http"
					if s.Protocol == RouteProtocolTCP {
						proto = "tcp"
					}
					specs = append(specs, jm{"host": s.Host, "port": s.Port, "protocol": proto, "service_id": s.ServiceID})
				}
				return jm{"metadata_hex": hex.EncodeToString(v.Metadata), "peer_id": v.PeerID, "specs": specs}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeExposeRoutePolicyRequest(b)
				return EncodeExposeRoutePolicyRequest(v), err
			}}
	case "ExposeRouteGrant":
		return &ucCodec{
			func(b []byte) (any, error) { v, err := DecodeExposeRouteGrant(b); return jm{"granted": v}, err },
			func(b []byte) ([]byte, error) {
				v, err := DecodeExposeRouteGrant(b)
				return EncodeExposeRouteGrant(v), err
			}}
	case "AdmissionConnectRequest":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeAdmissionConnectRequest(b)
				return jm{"alpn": string(v.ALPN), "peer": v.Peer}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeAdmissionConnectRequest(b)
				return EncodeAdmissionConnectRequest(v), err
			}}
	case "AdmissionHandshakeRequest":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeAdmissionHandshakeRequest(b)
				return jm{"alpn": string(v.ALPN), "is_alive": v.IsAlive, "peer": v.Peer}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeAdmissionHandshakeRequest(b)
				return EncodeAdmissionHandshakeRequest(v), err
			}}
	case "AdmissionHandshakeResponse":
		return &ucCodec{
			func(b []byte) (any, error) {
				v, err := DecodeAdmissionHandshakeResponse(b)
				return jm{"accept": v.Accept, "code": v.Code, "reason": string(v.Reason)}, err
			},
			func(b []byte) ([]byte, error) {
				v, err := DecodeAdmissionHandshakeResponse(b)
				return EncodeAdmissionHandshakeResponse(v), err
			}}
	}
	return nil
}

// normalize JSON-roundtrips v so numbers and nested types compare equal.
func normalize(t *testing.T, v any) any {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	var out any
	if err := json.Unmarshal(b, &out); err != nil {
		t.Fatal(err)
	}
	return out
}

// Placement messages are not part of the C ABI (no coordination client).
var ucUnsupported = map[string]bool{
	"PlacementPolicyRequest": true, "PlacementPolicy": true, "PlacementProvisionRequest": true, "MemberSeed": true,
}

func TestUCv1_ValidVectors(t *testing.T) {
	checked := 0
	for _, v := range loadUCVectors(t).Valid {
		c := ucCodecFor(v.Type)
		if c == nil {
			if !ucUnsupported[v.Type] {
				t.Errorf("no codec for %s (%s)", v.Type, v.Name)
			}
			continue
		}
		raw, _ := hex.DecodeString(v.Hex)
		got, err := c.render(raw)
		if err != nil {
			t.Errorf("%s: decode: %v", v.Name, err)
			continue
		}
		var want any
		if err := json.Unmarshal(v.Value, &want); err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(normalize(t, got), want) {
			t.Errorf("%s: decoded %v, want %s", v.Name, normalize(t, got), v.Value)
		}
		re, err := c.round(raw)
		if err != nil || !bytes.Equal(re, raw) {
			t.Errorf("%s: re-encode %x (%v), want %x", v.Name, re, err, raw)
		}
		checked++
	}
	if checked != 41 {
		t.Errorf("checked %d valid vectors, want 41", checked)
	}
}

func TestUCv1_InvalidVectors(t *testing.T) {
	checked := 0
	for _, v := range loadUCVectors(t).Invalid {
		c := ucCodecFor(v.Type)
		if c == nil {
			t.Fatalf("no codec for %s", v.Type)
		}
		raw, _ := hex.DecodeString(v.Hex)
		if _, err := c.render(raw); !errors.Is(err, ErrUCv1) {
			t.Errorf("%s (%s): err = %v, want ErrUCv1", v.Name, v.Type, err)
		}
		checked++
	}
	if checked != 21 {
		t.Errorf("checked %d invalid vectors, want 21", checked)
	}
}

func TestUCv1_TablesMatchTheSpec(t *testing.T) {
	d := loadUCVectors(t)
	names := map[UpcallKind]string{
		UpcallCancel: "cancel", UpcallLeaseTransition: "lease_transition", UpcallLeaseSnapshot: "lease_snapshot",
		UpcallLeaseAuthorize: "lease_authorize", UpcallFencedMutate: "fenced_mutate",
		UpcallTopologyAdmitted: "topology_admitted", 17: "topology_snapshot", 32: "placement_policy",
		33: "placement_provision", UpcallExposeAuthorize: "expose_authorize", UpcallExposeHTTP: "expose_http",
		UpcallExposeRoutePolicy: "expose_route_policy", UpcallAdmissionConnect: "admission_connect",
		UpcallAdmissionHandshake: "admission_handshake",
	}
	for _, k := range d.Kinds {
		kind := UpcallKind(k.Code)
		if names[kind] != k.Name {
			t.Errorf("kind %d: vectors say %q, binding %q", k.Code, k.Name, names[kind])
		}
		if kind.IsDecision() != k.Decision {
			t.Errorf("kind %s: decision = %v, want %v", k.Name, kind.IsDecision(), k.Decision)
		}
	}
	st := map[string]UpcallStatus{"ok": UpcallStatusOK, "error": UpcallStatusError, "unimplemented": UpcallStatusUnimplemented}
	for _, s := range d.Statuses {
		if uint32(st[s.Name]) != uint32(s.Code) {
			t.Errorf("status %s = %d, want %d", s.Name, st[s.Name], s.Code)
		}
	}
	for _, r := range d.CancelReasons {
		if CancelReason(r.Code).String() != r.Name {
			t.Errorf("cancel reason %d = %q, want %q", r.Code, CancelReason(r.Code), r.Name)
		}
	}
}

func TestUCv1_BoundariesAndHostileInput(t *testing.T) {
	// Epochs are u64: the full range round-trips.
	op := LeaseOp{Kind: LeaseRenew, Actor: "h", Epoch: ^uint64(0)}
	b := EncodeLeaseAuthorizeRequest(op)
	got, err := DecodeLeaseAuthorizeRequest(b)
	if err != nil || got != op {
		t.Fatalf("u64 epoch: %+v, %v", got, err)
	}
	// Every strict prefix of a valid message is refused, never read past.
	for i := 0; i < len(b); i++ {
		if _, err := DecodeLeaseAuthorizeRequest(b[:i]); !errors.Is(err, ErrUCv1) {
			t.Errorf("prefix %d accepted: %v", i, err)
		}
	}
	// Invalid UTF-8 in a string.
	if _, err := DecodeLeaseSnapshotRequest([]byte{0xB3, 0x01, 0x06, 0xFF}); !errors.Is(err, ErrUCv1) {
		t.Errorf("bad utf-8: %v", err)
	}
	// A varint with 11 continuation bytes, and one overflowing 64 bits.
	long := append([]byte{0xB3, 0x01}, bytes.Repeat([]byte{0x80}, 11)...)
	if _, err := DecodeLeaseSnapshotRequest(long); !errors.Is(err, ErrUCv1) {
		t.Errorf("long varint: %v", err)
	}
	over := append([]byte{0xB8, 0x01}, append(bytes.Repeat([]byte{0xFF}, 9), 0x02)...)
	if _, err := DecodeTopologyAdmittedRequest(over); !errors.Is(err, ErrUCv1) {
		t.Errorf("overflowing varint: %v", err)
	}
	// A list count far beyond the payload must not allocate.
	huge := []byte{0xB8, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x0C}
	if _, err := DecodeTopologyAdmittedRequest(huge); !errors.Is(err, ErrUCv1) {
		t.Errorf("huge list: %v", err)
	}
	// An uppercase header name is refused.
	bad := EncodeExposeHTTPResponse(ExposeHTTPResponse{Status: 200, Headers: []HTTPHeader{{Name: "X-A", Value: nil}}})
	if _, err := DecodeExposeHTTPResponse(bad); !errors.Is(err, ErrUCv1) || !strings.Contains(err.Error(), "lowercase") {
		t.Errorf("uppercase header: %v", err)
	}
	// A payload with another message's tag is refused.
	if _, err := DecodeDecision(EncodeLeaseSnapshotRequest("x")); !errors.Is(err, ErrUCv1) {
		t.Errorf("wrong tag: %v", err)
	}
}

func TestCheckFence(t *testing.T) {
	f := Fence{Resource: "r", Epoch: 7, Holder: "alice"}
	cases := []struct {
		row    uint64
		held   bool
		holder string
		writer string
		want   *FenceRejection
	}{
		{7, true, "alice", "alice", nil},
		{8, true, "alice", "alice", &FenceRejection{RejectStaleEpoch, 7, 8}},
		{6, true, "alice", "alice", &FenceRejection{RejectFutureEpoch, 7, 6}},
		{7, false, "", "alice", &FenceRejection{Reason: RejectNotHeld}},
		{7, true, "alice", "mallory", &FenceRejection{Reason: RejectNotHolder}},
	}
	for i, c := range cases {
		got := CheckFence(c.row, c.held, c.holder, f, c.writer)
		if !reflect.DeepEqual(got, c.want) {
			t.Errorf("case %d: %+v, want %+v", i, got, c.want)
		}
	}
}
