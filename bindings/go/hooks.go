//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "iroh_ffi.h"
*/
import "C"

import "fmt"

// HookDecision is the allow/deny decision for a before_connect hook.
// Mirrors iroh_hook_decision_t.
type HookDecision int32

const (
	HookAllow HookDecision = 0
	HookDeny  HookDecision = 1
)

// RespondBeforeConnect releases a pending before_connect hook invocation
// with the given decision. The invocation handle comes from the
// IROH_EVENT_HOOK_BEFORE_CONNECT event's Related field. A second call
// for the same invocation returns NOT_FOUND.
//
// AsterServer is expected to wire the actual subscribe + dispatch loop
// on top of this primitive — this function only owns the FFI release
// path.
func RespondBeforeConnect(runtime *Runtime, invocation uint64, decision HookDecision) error {
	r := C.iroh_hook_before_connect_respond(
		C.uint64_t(runtime.handle),
		C.uint64_t(invocation),
		C.enum_iroh_hook_decision_t(decision),
	)
	if r != 0 {
		return fmt.Errorf("iroh_hook_before_connect_respond: %w", Error(r))
	}
	return nil
}

// RespondAfterConnect releases a pending after_connect hook invocation
// (always accepts). The invocation handle comes from the
// IROH_EVENT_HOOK_AFTER_CONNECT event's Related field. A second call
// returns NOT_FOUND.
func RespondAfterConnect(runtime *Runtime, invocation uint64) error {
	r := C.iroh_hook_after_connect_respond(
		C.uint64_t(runtime.handle),
		C.uint64_t(invocation),
	)
	if r != 0 {
		return fmt.Errorf("iroh_hook_after_connect_respond: %w", Error(r))
	}
	return nil
}
