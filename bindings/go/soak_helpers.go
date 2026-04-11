//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "iroh_ffi.h"
*/
import "C"

import "time"

// randU8 returns a pseudo-random u8 based on monotonic clock.
func randU8() uint8 {
	return uint8(time.Now().UnixNano())
}
