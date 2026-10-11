package aster

import (
	"bytes"
	"errors"
	"math"
	"reflect"
	"testing"
)

func TestRPCAuthCodecSignedSessionAndCanonicalMaps(t *testing.T) {
	for _, sid := range []int32{math.MinInt32, -1, 0, 1, math.MaxInt32} {
		v := RPCAuthRequest{
			PeerID: "127.0.0.1:9", Service: "Sync", Method: "read", SessionID: sid,
			Metadata: map[string]string{"z": "last", "authorization": "Bearer secret", "é": "日本語"},
		}
		raw, err := EncodeRPCAuthRequest(v)
		if err != nil {
			t.Fatal(err)
		}
		got, err := DecodeRPCAuthRequest(raw)
		if err != nil || !reflect.DeepEqual(got, v) {
			t.Fatalf("session %d: %+v, %v", sid, got, err)
		}
		for i := 0; i < len(raw); i++ {
			if _, err := DecodeRPCAuthRequest(raw[:i]); !errors.Is(err, ErrUCv1) {
				t.Fatalf("request prefix %d accepted", i)
			}
		}
		again, err := EncodeRPCAuthRequest(v)
		if err != nil || !bytes.Equal(again, raw) {
			t.Fatal("map encoding is not deterministic")
		}
	}
	for _, v := range []RPCAuthResponse{
		{Accepted: false},
		{Accepted: true, Principal: "user/alice", Attributes: map[string]string{}},
		{Accepted: true, Principal: "ユーザー", Attributes: map[string]string{"z": "last", "aster.role": "operator"}},
	} {
		raw, err := EncodeRPCAuthResponse(v)
		if err != nil {
			t.Fatal(err)
		}
		got, err := DecodeRPCAuthResponse(raw)
		if err != nil || !reflect.DeepEqual(got, v) {
			t.Fatalf("response: %+v, %v", got, err)
		}
		for i := 0; i < len(raw); i++ {
			if _, err := DecodeRPCAuthResponse(raw[:i]); !errors.Is(err, ErrUCv1) {
				t.Fatalf("response prefix %d accepted", i)
			}
		}
	}
	if UpcallRpcAuthenticate.IsDecision() {
		t.Fatal("RPC authentication must never use decision failure modes")
	}
}

func TestRPCAuthCodecRefusesAmbiguousOrMalformedIdentity(t *testing.T) {
	request := func() *ucw { return newUCW(TagRPCAuthRequest).str("peer").str("svc").str("method") }
	badRequests := [][]byte{
		request().uint(uint64(math.MaxUint32) + 1).list(0).b,
		request().uint(0).list(2).str("x").str("a").str("x").str("b").b,
		request().uint(0).list(2).str("z").str("a").str("a").str("b").b,
		request().uint(0).list(math.MaxInt32).b,
		request().uint(0).list(1).str("key").str(string([]byte{0xff})).b,
		append(request().uint(0).list(0).b, 0),
	}
	for i, raw := range badRequests {
		if _, err := DecodeRPCAuthRequest(raw); !errors.Is(err, ErrUCv1) {
			t.Errorf("request %d accepted: %v", i, err)
		}
	}
	badResponses := [][]byte{
		{TagRPCAuthResponse, 1, 2},
		newUCW(TagRPCAuthResponse).bool(true).str("").list(0).b,
		newUCW(TagRPCAuthResponse).bool(true).str("alice").list(2).str("x").str("a").str("x").str("b").b,
		newUCW(TagRPCAuthResponse).bool(true).str("alice").list(2).str("z").str("a").str("a").str("b").b,
		newUCW(TagRPCAuthResponse).bool(false).str("alice").b,
		newUCW(TagRPCAuthResponse).bool(true).str(string([]byte{0xff})).list(0).b,
	}
	for i, raw := range badResponses {
		if _, err := DecodeRPCAuthResponse(raw); !errors.Is(err, ErrUCv1) {
			t.Errorf("response %d accepted: %v", i, err)
		}
	}
	for _, v := range []RPCAuthResponse{
		{Accepted: true},
		{Accepted: false, Principal: "alice"},
		{Accepted: true, Principal: string([]byte{0xff})},
		{Accepted: true, Principal: "alice", Attributes: map[string]string{"role": string([]byte{0xff})}},
	} {
		if _, err := EncodeRPCAuthResponse(v); !errors.Is(err, ErrUCv1) {
			t.Errorf("invalid result encoded: %+v, %v", v, err)
		}
	}
	if _, err := EncodeRPCAuthRequest(RPCAuthRequest{PeerID: string([]byte{0xff})}); !errors.Is(err, ErrUCv1) {
		t.Fatal("invalid request UTF-8 encoded")
	}
}
