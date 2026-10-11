package aster

import (
	"context"
	"sort"
	"unicode/utf8"
)

const (
	TagRPCAuthRequest  = 0xC9
	TagRPCAuthResponse = 0xCA
)

// RPCAuthRequest is the kind-80 authentication context. PeerID is the
// transport's identity hint; authenticate credentials in Metadata before
// returning an accepted principal. SessionID preserves the wire's signed i32.
type RPCAuthRequest struct {
	PeerID    string
	Service   string
	Method    string
	SessionID int32
	Metadata  map[string]string
}

// RPCAuthResponse is an explicit authentication result. Accepted requires a
// nonempty Principal; Attributes are per-call capability attributes. A false
// Accepted is a credential rejection, not a callback/backend error.
type RPCAuthResponse struct {
	Accepted   bool
	Principal  string
	Attributes map[string]string
}

// RPCAuthenticator authenticates one RPC call. Context cancellation means the
// native caller stopped waiting. An error is a backend failure (UNAVAILABLE);
// return RPCAuthResponse{Accepted:false} for invalid credentials instead.
type RPCAuthenticator interface {
	Authenticate(context.Context, RPCAuthRequest) (RPCAuthResponse, error)
}

// RPCAuthenticatorFunc adapts a function to RPCAuthenticator.
type RPCAuthenticatorFunc func(context.Context, RPCAuthRequest) (RPCAuthResponse, error)

func (f RPCAuthenticatorFunc) Authenticate(ctx context.Context, req RPCAuthRequest) (RPCAuthResponse, error) {
	return f(ctx, req)
}

func validAuthString(s string) error {
	if !utf8.ValidString(s) {
		return &UCv1Error{Msg: "invalid UTF-8 in authentication string"}
	}
	return nil
}

func (w *ucw) authMap(m map[string]string) error {
	keys := make([]string, 0, len(m))
	for k, v := range m {
		if err := validAuthString(k); err != nil {
			return err
		}
		if err := validAuthString(v); err != nil {
			return err
		}
		keys = append(keys, k)
	}
	// Go string order compares UTF-8 bytes, as required by UCv1.
	sort.Strings(keys)
	w.list(len(keys))
	for _, k := range keys {
		w.str(k).str(m[k])
	}
	return nil
}

func (r *ucr) authMap() map[string]string {
	n := r.list()
	if r.err != nil {
		return nil
	}
	// Even two empty strings require two bytes. Refuse a hostile count before
	// allocating the map; every remaining field stays bounded by the payload.
	if n > r.remaining()/2 {
		r.fail("authentication map count exceeds payload")
		return nil
	}
	m := make(map[string]string, n)
	previous := ""
	for i := 0; i < n && r.err == nil; i++ {
		k, v := r.str(), r.str()
		if i != 0 && k <= previous {
			r.fail("authentication map keys not strictly ascending")
			break
		}
		m[k] = v
		previous = k
	}
	return m
}

// EncodeRPCAuthRequest encodes C9 with canonical, sorted metadata keys.
func EncodeRPCAuthRequest(v RPCAuthRequest) ([]byte, error) {
	for _, s := range []string{v.PeerID, v.Service, v.Method} {
		if err := validAuthString(s); err != nil {
			return nil, err
		}
	}
	zigzag := uint32(v.SessionID)<<1 ^ uint32(v.SessionID>>31)
	w := newUCW(TagRPCAuthRequest).str(v.PeerID).str(v.Service).str(v.Method).uint(uint64(zigzag))
	if err := w.authMap(v.Metadata); err != nil {
		return nil, err
	}
	return w.b, nil
}

// DecodeRPCAuthRequest decodes C9 and rejects noncanonical or duplicate keys.
func DecodeRPCAuthRequest(b []byte) (RPCAuthRequest, error) {
	r := newUCR(b, TagRPCAuthRequest)
	v := RPCAuthRequest{PeerID: r.str(), Service: r.str(), Method: r.str()}
	zigzag := uint32(r.uintMax(0xffff_ffff, "session id"))
	v.SessionID = int32(zigzag>>1) ^ -int32(zigzag&1)
	v.Metadata = r.authMap()
	r.end()
	if r.err != nil {
		return RPCAuthRequest{}, r.err
	}
	return v, nil
}

// EncodeRPCAuthResponse encodes CA. Rejection has no principal or attributes.
func EncodeRPCAuthResponse(v RPCAuthResponse) ([]byte, error) {
	w := newUCW(TagRPCAuthResponse).bool(v.Accepted)
	if !v.Accepted {
		if v.Principal != "" || len(v.Attributes) != 0 {
			return nil, &UCv1Error{Msg: "rejected authentication has principal or attributes"}
		}
		return w.b, nil
	}
	if v.Principal == "" {
		return nil, &UCv1Error{Msg: "accepted authentication requires a principal"}
	}
	if err := validAuthString(v.Principal); err != nil {
		return nil, err
	}
	w.str(v.Principal)
	if err := w.authMap(v.Attributes); err != nil {
		return nil, err
	}
	return w.b, nil
}

// DecodeRPCAuthResponse decodes CA, requiring an accepted nonempty principal.
func DecodeRPCAuthResponse(b []byte) (RPCAuthResponse, error) {
	r := newUCR(b, TagRPCAuthResponse)
	v := RPCAuthResponse{Accepted: r.bool()}
	if v.Accepted {
		v.Principal = r.str()
		if v.Principal == "" {
			r.fail("accepted authentication requires a principal")
		}
		v.Attributes = r.authMap()
	}
	r.end()
	if r.err != nil {
		return RPCAuthResponse{}, r.err
	}
	return v, nil
}
