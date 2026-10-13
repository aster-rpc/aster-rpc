//go:build cgo

package main

import (
	"context"
	"strings"
	"testing"
	"time"
)

// TestRolesGrantAndRevoke runs the example end to end: grant, self-grant
// refused, revoke applies from the next call, the in-flight call completes,
// and the API node's own grant is protected.
func TestRolesGrantAndRevoke(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	var out strings.Builder
	if err := Run(ctx, &out); err != nil {
		t.Fatalf("%v\n%s", err, out.String())
	}
	t.Log("\n" + out.String())
}
