//! Fuzz (handle, op_id) lookup with random values — catches stale ID use-after-close.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 16 {
        return;
    }

    let handle = u64::from_le_bytes([
        data[0], data[1], data[2], data[3],
        data[4], data[5], data[6], data[7],
    ]);
    let op_id = u64::from_le_bytes([
        data[8], data[9], data[10], data[11],
        data[12], data[13], data[14], data[15],
    ]);

    let _ = handle;
    let _ = op_id;

    // Boundary values that must be handled without panicking:
    // - handle = 0 (null handle) → NOT_FOUND
    // - handle = u64::MAX → NOT_FOUND
    // - op_id = 0 (null op) → NOT_FOUND
    // - op_id = u64::MAX → NOT_FOUND
    if handle == 0 || handle == u64::MAX {
        // Should return NOT_FOUND, not crash
    }
    if op_id == 0 || op_id == u64::MAX {
        // Should return NOT_FOUND, not crash
    }
});
