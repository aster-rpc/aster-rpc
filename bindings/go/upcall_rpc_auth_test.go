//go:build cgo

package aster

import (
	"context"
	"errors"
	"reflect"
	"testing"
)

func TestServeRPCAuthenticatorTypedCallback(t *testing.T) {
	u := &Upcalls{handlers: make(map[UpcallKind]UpcallHandler)}
	if err := u.ServeRPCAuthenticator(nil); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("nil authenticator: %v", err)
	}
	want := RPCAuthRequest{PeerID: "remote", Service: "Sync", Method: "read", SessionID: -7, Metadata: map[string]string{"authorization": "Bearer secret"}}
	raw, err := EncodeRPCAuthRequest(want)
	if err != nil {
		t.Fatal(err)
	}
	backendErr := errors.New("identity backend failed")
	for _, result := range []struct {
		outcome RPCAuthResponse
		err     error
	}{
		{RPCAuthResponse{Accepted: true, Principal: "user/alice", Attributes: map[string]string{"aster.role": "operator"}}, nil},
		{RPCAuthResponse{Accepted: false}, nil},
		{RPCAuthResponse{}, backendErr},
	} {
		calls := 0
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		if err := u.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(gotCtx context.Context, got RPCAuthRequest) (RPCAuthResponse, error) {
			calls++
			if gotCtx != ctx || gotCtx.Err() != context.Canceled {
				t.Error("callback did not receive cancellation context")
			}
			if !reflect.DeepEqual(got, want) {
				t.Errorf("request %+v, want %+v", got, want)
			}
			return result.outcome, result.err
		})); err != nil {
			t.Fatal(err)
		}
		handler := u.handler(UpcallRpcAuthenticate)
		out, err := handler(ctx, &UpcallRequest{Kind: UpcallRpcAuthenticate, Payload: raw})
		if !errors.Is(err, result.err) {
			t.Fatalf("callback error: %v", err)
		}
		if err == nil {
			got, err := DecodeRPCAuthResponse(out)
			if err != nil || !reflect.DeepEqual(got, result.outcome) {
				t.Fatalf("response %+v, %v", got, err)
			}
		}
		if calls != 1 {
			t.Fatalf("callback called %d times", calls)
		}
		if _, err := handler(ctx, &UpcallRequest{Payload: []byte{TagRPCAuthRequest, 1}}); !errors.Is(err, ErrUCv1) {
			t.Fatalf("malformed request: %v", err)
		}
		if calls != 1 {
			t.Fatal("malformed request reached authenticator")
		}
	}
}
