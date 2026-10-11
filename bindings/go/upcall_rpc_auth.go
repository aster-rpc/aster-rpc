//go:build cgo

package aster

import (
	"context"
	"fmt"
)

// ServeRPCAuthenticator handles typed RPC authentication upcalls (kind 80).
// It does not attach a listener or alter an Iroh server's authentication.
// Authentication is a typed, non-decision upcall: failure policy cannot grant
// access. Return Accepted:false for credential denial; callback errors are
// backend failures. Rust enforces admission, payload limits and the deadline.
func (u *Upcalls) ServeRPCAuthenticator(auth RPCAuthenticator) error {
	if auth == nil {
		return fmt.Errorf("RPC authenticator is nil: %w", ErrInvalidArgument)
	}
	return u.HandleKind(UpcallRpcAuthenticate, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		v, err := DecodeRPCAuthRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		outcome, err := auth.Authenticate(ctx, v)
		if err != nil {
			return nil, err
		}
		return EncodeRPCAuthResponse(outcome)
	})
}
