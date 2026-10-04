//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"errors"
	"fmt"
	"strings"
	"unsafe"
)

// IrohError is the base error type returned by all FFI operations.
// It wraps a status code and supports errors.Is / errors.As.
type IrohError struct {
	code    int32  // status code
	message string // human-readable message
}

func (e *IrohError) Error() string {
	if e.message != "" {
		return fmt.Sprintf("%s (status=%d)", e.message, e.code)
	}
	return fmt.Sprintf("iroh error %d", e.code)
}

func (e *IrohError) Unwrap() error {
	return errors.New(e.message)
}

// Code returns the iroh status code that caused this error.
func (e *IrohError) Code() int32 {
	return e.code
}

// StatusCode implements the status.Error interface (if we used golang.org/x/net/status).
// For now, the code is accessible via Code().
func (e *IrohError) StatusCode() int32 {
	return e.code
}

// errorf creates an IrohError with a formatted message.
func errorf(code int32, format string, args ...any) *IrohError {
	return &IrohError{
		code:    code,
		message: fmt.Sprintf(format, args...),
	}
}

// errorFromStatus returns an IrohError for the given status code.
func errorFromStatus(code C.int) *IrohError {
	if code == 0 {
		return nil
	}
	c := int32(code)
	switch code {
	case ASTER_STATUS_INVALID_ARGUMENT:
		return errorf(c, "invalid argument")
	case ASTER_STATUS_NOT_FOUND:
		return errorf(c, "not found")
	case ASTER_STATUS_ALREADY_CLOSED:
		return errorf(c, "already closed")
	case ASTER_STATUS_QUEUE_FULL:
		return errorf(c, "queue full")
	case ASTER_STATUS_BUFFER_TOO_SMALL:
		return errorf(c, "buffer too small")
	case ASTER_STATUS_UNSUPPORTED:
		return errorf(c, "unsupported")
	case ASTER_STATUS_INTERNAL:
		return errorf(c, "internal error")
	case ASTER_STATUS_TIMEOUT:
		return errorf(c, "timeout")
	case ASTER_STATUS_CANCELLED:
		return errorf(c, "cancelled")
	case ASTER_STATUS_CONNECTION_REFUSED:
		return errorf(c, "connection refused")
	case ASTER_STATUS_STREAM_RESET:
		return errorf(c, "stream reset")
	case ASTER_STATUS_ABI_MISMATCH:
		return errorf(c, "ABI version mismatch")
	case ASTER_STATUS_UNAUTHENTICATED:
		return errorf(c, "unauthenticated")
	case ASTER_STATUS_PERMISSION_DENIED:
		return errorf(c, "permission denied")
	case ASTER_STATUS_CREDENTIAL_EXPIRED:
		return errorf(c, "credential expired")
	case ASTER_STATUS_VERIFICATION_FAILED:
		return errorf(c, "verification failed")
	default:
		return errorf(c, "unknown status %d", c)
	}
}

// errors for sentinel checks via errors.Is
var (
	ErrInvalidArgument   = errors.New("invalid argument")
	ErrNotFound          = errors.New("not found")
	ErrAlreadyClosed     = errors.New("already closed")
	ErrQueueFull         = errors.New("queue full")
	ErrBufferTooSmall    = errors.New("buffer too small")
	ErrUnsupported       = errors.New("unsupported")
	ErrInternal          = errors.New("internal error")
	ErrTimeout           = errors.New("timeout")
	ErrCancelled         = errors.New("cancelled")
	ErrConnectionRefused = errors.New("connection refused")
	ErrStreamReset       = errors.New("stream reset")
	ErrABIMismatch       = errors.New("ABI version mismatch")
	// ErrVerificationFailed: well-formed input failed a security check (an
	// envelope does not open, a chain does not verify, role != capability).
	ErrVerificationFailed = errors.New("verification failed")
	// ErrUnauthenticated, ErrPermissionDenied and ErrCredentialExpired are
	// the auth statuses (ABI 4.3). Only a denied expose relay produces one
	// today (ErrPermissionDenied); the others are reserved for the R4 auth
	// model.
	ErrUnauthenticated   = errors.New("unauthenticated")
	ErrPermissionDenied  = errors.New("permission denied")
	ErrCredentialExpired = errors.New("credential expired")
)

// statusToError maps status codes to sentinel errors for errors.Is matching.
var statusToError = map[int32]error{
	ASTER_STATUS_INVALID_ARGUMENT:    ErrInvalidArgument,
	ASTER_STATUS_NOT_FOUND:           ErrNotFound,
	ASTER_STATUS_ALREADY_CLOSED:      ErrAlreadyClosed,
	ASTER_STATUS_QUEUE_FULL:          ErrQueueFull,
	ASTER_STATUS_BUFFER_TOO_SMALL:    ErrBufferTooSmall,
	ASTER_STATUS_UNSUPPORTED:         ErrUnsupported,
	ASTER_STATUS_INTERNAL:            ErrInternal,
	ASTER_STATUS_TIMEOUT:             ErrTimeout,
	ASTER_STATUS_CANCELLED:           ErrCancelled,
	ASTER_STATUS_CONNECTION_REFUSED:  ErrConnectionRefused,
	ASTER_STATUS_STREAM_RESET:        ErrStreamReset,
	ASTER_STATUS_ABI_MISMATCH:        ErrABIMismatch,
	ASTER_STATUS_VERIFICATION_FAILED: ErrVerificationFailed,
	ASTER_STATUS_UNAUTHENTICATED:     ErrUnauthenticated,
	ASTER_STATUS_PERMISSION_DENIED:   ErrPermissionDenied,
	ASTER_STATUS_CREDENTIAL_EXPIRED:  ErrCredentialExpired,
}

// Is returns true if target is a matching sentinel error.
// This implements errors.Is for IrohError chains.
func (e *IrohError) Is(target error) bool {
	return statusToError[e.code] == target
}

// As returns true if the error chain contains a *IrohError and sets *err to it.
// This implements errors.As for IrohError chains.
func (e *IrohError) As(target any) bool {
	if _, ok := target.(*IrohError); ok {
		return true
	}
	return false
}

// wrapError wraps a C status code into a Go error.
// Returns nil for ASTER_STATUS_OK.
func wrapError(code C.int) error {
	if code == 0 {
		return nil
	}
	err := errorFromStatus(code)
	// Also store the sentinel error in the chain via Unwrap.
	// We do this by making Unwrap return the sentinel.
	return &irohErrorWithSentinel{err, statusToError[int32(code)]}
}

// irohErrorWithSentinel wraps an IrohError and carries a sentinel error
// so that errors.Is() can match the sentinel while errors.As() can
// unwrap to the full IrohError.
type irohErrorWithSentinel struct {
	err      *IrohError
	sentinel error
}

func (e *irohErrorWithSentinel) Error() string { return e.err.Error() }
func (e *irohErrorWithSentinel) Unwrap() error { return e.err }
func (e *irohErrorWithSentinel) Is(target error) bool {
	return e.sentinel == target
}
func (e *irohErrorWithSentinel) As(target any) bool {
	if _, ok := target.(*IrohError); ok {
		*target.(*IrohError) = *e.err
		return true
	}
	return false
}

// statusError converts a non-OK status from a synchronous FFI call into an
// error carrying the library's last-error message (if any). Returns nil for OK.
func statusError(code C.int32_t) error {
	if code == 0 {
		return nil
	}
	base := errorFromStatus(C.int(code))
	if msg := lastErrorMessage(); msg != "" {
		base = errorf(base.code, "%s: %s", base.message, msg)
	}
	return &irohErrorWithSentinel{base, statusToError[int32(code)]}
}

// lastErrorMessage reads the calling thread's last-error message.
func lastErrorMessage() string {
	var buf [1024]byte
	n := C.aster_last_error_message((*C.uint8_t)(unsafe.Pointer(&buf[0])), C.uintptr_t(len(buf)))
	if n == 0 {
		return ""
	}
	if int(n) > len(buf) {
		n = C.uintptr_t(len(buf))
	}
	return strings.TrimRight(string(buf[:n]), "\x00")
}
