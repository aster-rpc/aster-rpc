//go:build cgo

package aster

/*
#include <stdlib.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import "unsafe"

// cArena owns C allocations made while building an FFI argument (config
// structs, aster_bytes_t / aster_bytes_list_t payloads). Struct layouts come
// from the cgo types generated from aster_ffi.h, never from hand-computed
// offsets. Call free() once the FFI call has returned (every config struct is
// copied by Rust before the call returns).
type cArena struct {
	ptrs []unsafe.Pointer
}

func (a *cArena) alloc(n uintptr) unsafe.Pointer {
	if n == 0 {
		n = 1
	}
	p := C.calloc(1, C.size_t(n))
	if p == nil {
		panic("aster: calloc failed")
	}
	a.ptrs = append(a.ptrs, p)
	return p
}

// bytes copies b into C memory. Empty input yields {NULL, 0}.
func (a *cArena) bytes(b []byte) C.struct_aster_bytes_t {
	if len(b) == 0 {
		return C.struct_aster_bytes_t{}
	}
	p := a.alloc(uintptr(len(b)))
	C.memcpy(p, unsafe.Pointer(&b[0]), C.size_t(len(b)))
	return C.struct_aster_bytes_t{ptr: (*C.uint8_t)(p), len: C.uintptr_t(len(b))}
}

func (a *cArena) str(s string) C.struct_aster_bytes_t {
	return a.bytes([]byte(s))
}

// list builds an aster_bytes_list_t whose items array lives in C memory.
func (a *cArena) list(ss []string) C.struct_aster_bytes_list_t {
	if len(ss) == 0 {
		return C.struct_aster_bytes_list_t{}
	}
	itemSize := unsafe.Sizeof(C.struct_aster_bytes_t{})
	base := a.alloc(itemSize * uintptr(len(ss)))
	items := unsafe.Slice((*C.struct_aster_bytes_t)(base), len(ss))
	for i, s := range ss {
		items[i] = a.str(s)
	}
	return C.struct_aster_bytes_list_t{
		items: (*C.struct_aster_bytes_t)(base),
		len:   C.uintptr_t(len(ss)),
	}
}

func (a *cArena) free() {
	for _, p := range a.ptrs {
		C.free(p)
	}
	a.ptrs = nil
}

// viewBytes copies a borrowed aster_bytes_t into Go memory without freeing it.
func viewBytes(b C.struct_aster_bytes_t) []byte {
	if b.ptr == nil || b.len == 0 {
		return nil
	}
	return C.GoBytes(unsafe.Pointer(b.ptr), C.int(b.len))
}

// takeRustBytes copies a Rust-owned aster_bytes_t into Go memory and releases
// it with aster_string_release(ptr, len). Use for out-struct fields documented
// as "caller must free with aster_string_release".
func takeRustBytes(b C.struct_aster_bytes_t) []byte {
	out := viewBytes(b)
	if b.ptr != nil {
		C.aster_string_release(b.ptr, b.len)
	}
	return out
}
