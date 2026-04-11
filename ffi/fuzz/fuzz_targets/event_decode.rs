//! Fuzz `iroh_event_t` struct decoding — random bytes parsed as an event.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Minimum valid event size: struct_size(4) + kind(4) + status(4) +
    // operation(8) + handle(8) + related(8) + user_data(8) + data_ptr(8) +
    // data_len(8) + buffer(8) + error_code(4) + flags(4) = 80
    const MIN_EVENT_SIZE: usize = 80;

    if data.len() < MIN_EVENT_SIZE {
        return;
    }

    // struct_size at offset 0
    let struct_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);

    // Don't try to read beyond available data
    if struct_size as usize > data.len() || struct_size < MIN_EVENT_SIZE as u32 {
        return;
    }

    // Read key fields to exercise struct field offsets
    let _kind = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let _status = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    let _operation = u64::from_le_bytes([
        data[12], data[13], data[14], data[15],
        data[16], data[17], data[18], data[19],
    ]);
    let _handle = u64::from_le_bytes([
        data[20], data[21], data[22], data[23],
        data[24], data[25], data[26], data[27],
    ]);

    // data_ptr at offset 44
    let data_ptr_val = u64::from_le_bytes([
        data[44], data[45], data[46], data[47],
        data[48], data[49], data[50], data[51],
    ]);
    // data_len at offset 52
    let data_len = usize::from_le_bytes([
        data[52], data[53], data[54], data[55],
        data[56], data[57], data[58], data[59],
    ]);

    // If data_ptr is non-null with reasonable length, ASan validates bounds.
    if data_ptr_val != 0 && data_len > 0 && data_len < 65536 {
        // SAFETY: this mimics what the FFI boundary does when reading event data_ptr.
        // ASan will catch out-of-bounds reads.
        unsafe { let _ = core::slice::from_raw_parts(data_ptr_val as *const u8, data_len); }
    }

    // error_code at offset 68, flags at offset 72
    let _error_code = i32::from_le_bytes([data[68], data[69], data[70], data[71]]);
    let _flags = u32::from_le_bytes([data[72], data[73], data[74], data[75]]);
});
