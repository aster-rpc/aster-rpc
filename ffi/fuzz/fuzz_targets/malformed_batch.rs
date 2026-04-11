//! Fuzz malformed batch parsing — truncated events, partial structs.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // A batch: [u32 count][event_1][event_2]...
    if data.len() < 4 {
        return;
    }

    let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

    if count > 256 {
        return; // Prevent OOM on huge counts
    }

    let mut offset = 4;
    const FIXED_EVENT_SIZE: usize = 80;

    for _ in 0..count {
        if offset + FIXED_EVENT_SIZE > data.len() {
            break;
        }

        // Read struct_size at this offset
        let struct_size =
            u32::from_le_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
                as usize;

        // Invalid struct_size: stop parsing
        if struct_size < FIXED_EVENT_SIZE || struct_size > 4096 {
            break;
        }

        offset += struct_size;

        if offset > data.len() {
            break;
        }
    }
});
