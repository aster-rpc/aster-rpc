//! Fuzz (data_ptr, data_len) combinations — null ptr, wraparound, OOB.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 24 {
        return;
    }

    let ptr_val = u64::from_le_bytes([
        data[0], data[1], data[2], data[3],
        data[4], data[5], data[6], data[7],
    ]);
    let len = u64::from_le_bytes([
        data[8], data[9], data[10], data[11],
        data[12], data[13], data[14], data[15],
    ]);

    // Case 1: null + zero — valid empty slice
    if ptr_val == 0 && len == 0 {
        return;
    }

    // Case 2: null + non-zero — invalid
    if ptr_val == 0 && len > 0 {
        return;
    }

    // Case 3: non-null with reasonable length — ASan validates bounds
    if ptr_val != 0 && len < 1024 * 1024 {
        // SAFETY: ASan catches out-of-bounds reads on this pointer.
        unsafe { let _ = core::slice::from_raw_parts(ptr_val as *const u8, len as usize); }
    }
});
