package main

/*
#define _GNU_SOURCE
#include "aster_ffi.h"
#include <stdio.h>
#ifdef _WIN32
#include <windows.h>
static const char *loaded_image(void) {
    static char path[32768];
    HMODULE module = GetModuleHandleA("aster_transport_ffi.dll");
    if (!module || !GetModuleFileNameA(module, path, sizeof(path))) return "";
    return path;
}
#else
#include <dlfcn.h>
static const char *loaded_image(void) {
    Dl_info info;
    if (!dladdr((void *)aster_version, &info)) return "";
    return info.dli_fname;
}
#endif
*/
import "C"

import (
	"encoding/json"
	"fmt"
	"os"
)

func main() {
	check := func(ok bool, message string) {
		if !ok {
			fmt.Fprintln(os.Stderr, message)
			os.Exit(1)
		}
	}
	abi := uint32(C.aster_abi_version())
	check(abi == uint32(C.ASTER_ABI_VERSION), "archive header/runtime ABI differs")
	var runtime C.aster_runtime_t
	check(C.aster_runtime_new(C.ASTER_ABI_VERSION_MAJOR+1, nil, &runtime) == C.ASTER_STATUS_ABI_MISMATCH && runtime == 0, "ABI mismatch was not rejected")
	check(C.aster_runtime_new(C.ASTER_ABI_VERSION_MAJOR, nil, &runtime) == C.ASTER_STATUS_OK && runtime != 0, "runtime creation failed")
	// Exercise the current ABI 4.16 family through its documented invalid-owner path.
	check(C.aster_topology_snapshot_free(runtime, 0) == C.ASTER_STATUS_NOT_FOUND, "Topology handle validation failed")
	check(C.aster_runtime_close(runtime) == C.ASTER_STATUS_OK, "runtime close failed")
	check(C.GoString(C.loaded_image()) != "", "native image could not be identified")
	check(json.NewEncoder(os.Stdout).Encode(map[string]any{
		"abi": abi, "version": C.GoString(C.aster_version()), "loaded_image": C.GoString(C.loaded_image()),
		"runtime_lifecycle": true, "abi_mismatch_rejected": true, "topology_handle_validation": true,
	}) == nil, "receipt encoding failed")
}
