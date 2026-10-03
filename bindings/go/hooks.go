//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import "fmt"

// HookDecision is the allow/deny decision for a before_connect hook.
// Mirrors aster_hook_decision_t.
type HookDecision uint32

const (
	HookAllow HookDecision = C.ASTER_HOOK_DECISION_ALLOW
	HookDeny  HookDecision = C.ASTER_HOOK_DECISION_DENY
)

// RespondBeforeConnect releases a pending before_connect hook invocation
// with the given decision. The invocation handle comes from the
// ASTER_EVENT_HOOK_BEFORE_CONNECT event's Related field. A second call
// for the same invocation returns NOT_FOUND.
//
// AsterServer is expected to wire the actual subscribe + dispatch loop
// on top of this primitive — this function only owns the FFI release
// path.
func RespondBeforeConnect(runtime *Runtime, invocation uint64, decision HookDecision) error {
	r := C.aster_hook_before_connect_respond(
		C.uint64_t(runtime.handle),
		C.uint64_t(invocation),
		C.uint32_t(decision),
	)
	if r != 0 {
		return fmt.Errorf("aster_hook_before_connect_respond: %w", Error(r))
	}
	return nil
}

// RespondAfterConnect releases a pending after_connect hook invocation
// (always accepts). The invocation handle comes from the
// ASTER_EVENT_HOOK_AFTER_CONNECT event's Related field. A second call
// returns NOT_FOUND.
func RespondAfterConnect(runtime *Runtime, invocation uint64) error {
	r := C.aster_hook_after_connect_respond(
		C.uint64_t(runtime.handle),
		C.uint64_t(invocation),
	)
	if r != 0 {
		return fmt.Errorf("aster_hook_after_connect_respond: %w", Error(r))
	}
	return nil
}
