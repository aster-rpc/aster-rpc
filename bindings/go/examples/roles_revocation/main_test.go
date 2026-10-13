//go:build cgo

package main

import (
	"context"
	"strings"
	"testing"
	"time"
)

// TestRolesRestrictAndRevoke runs the example end to end: credential roles
// gate calls, a mid-session restriction refuses the session node's next
// write and cannot grant the viewer a role, and the root's revocation closes
// the session node out.
func TestRolesRestrictAndRevoke(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	var out strings.Builder
	if err := Run(ctx, &out); err != nil {
		t.Fatalf("%v\n%s", err, out.String())
	}
	t.Log("\n" + out.String())
}
