//go:build cgo

package aster

import (
	"bytes"
	"context"
	"testing"
	"time"
)

// O1: a Go server lists what it serves in its aster.ops.NodeInfo identity
// (the aster.services attribute, the Python ServiceSummary JSON shape). The
// Rust server it runs on writes it; Go has no NodeInfo decoder, so the test
// reads the raw describe reply.
func TestNodeInfo_ListsServedServices(t *testing.T) {
	f := newRPCFixture(t, ServerOptions{Services: []*Service{interopService()}})
	ctx, cancel := context.WithTimeout(t.Context(), 15*time.Second)
	defer cancel()

	opts, err := lowerOptions(ctx, "aster.ops.NodeInfo", "describe", buildOptions(nil))
	if err != nil {
		t.Fatal(err)
	}
	defer opts.free()
	reply, err := f.conn.unary(ctx, opts, nil)
	if err != nil {
		t.Fatalf("describe: %v", err)
	}
	for _, want := range []string{
		"aster.services",
		`"name":"InteropEcho"`,
		`"contract_id":"` + interopEchoContractID + `"`,
		`"pattern":"shared"`,
		`"channels":{"rpc":`,
		`"serialization_modes":[`,
	} {
		if !bytes.Contains(reply, []byte(want)) {
			t.Errorf("describe reply lacks %s", want)
		}
	}
}
