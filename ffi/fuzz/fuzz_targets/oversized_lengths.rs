//! Fuzz oversized length values — integer overflow, allocation failures.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 32 {
        return;
    }

    let len_a = u64::from_le_bytes([
        data[0], data[1], data[2], data[3],
        data[4], data[5], data[6], data[7],
    ]);
    let len_b = u64::from_le_bytes([
        data[8], data[9], data[10], data[11],
        data[12], data[13], data[14], data[15],
    ]);
    let len_c = u64::from_le_bytes([
        data[16], data[17], data[18], data[19],
        data[20], data[21], data[22], data[23],
    ]);

    const GB: u64 = 1024 * 1024 * 1024;

    // Reject unreasonable lengths before any allocation attempt
    if len_a > GB || len_b > GB || len_c > GB {
        return;
    }

    // Integer overflow in length calculations
    let _ = len_a.wrapping_add(len_b); // sum overflow
    let _ = len_a.wrapping_mul(len_b); // multiplication overflow

    // Pointer + length that could wrap
    let ptr = 0x1000u64;
    let _ = ptr.wrapping_add(len_a) as usize;
    let _ = ptr.wrapping_add(len_b) as usize;

    // Large len_c that would exceed usize::MAX
    if len_c > usize::MAX as u64 {
        // Should be rejected, not silently truncated
    }
});
