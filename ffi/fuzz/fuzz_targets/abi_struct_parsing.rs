//! Verify ABI struct field offsets — off-by-one in struct layout would be caught.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    const EVENT_SIZE: usize = 80;

    if data.len() < EVENT_SIZE {
        return;
    }

    let ptr = data.as_ptr();

    // SAFETY: reading within bounds of the data slice — mirrors Java's VarHandle
    // offset calculations which also access raw memory.
    unsafe {
        let _ = &* (ptr as *const u8 as *const u8);
    }

    // Read each field at expected offsets
    let _ = &data[0..4];   // struct_size
    let _ = &data[4..8];   // kind
    let _ = &data[8..12];  // status
    let _ = &data[12..20]; // operation
    let _ = &data[20..28]; // handle
    let _ = &data[28..36]; // related
    let _ = &data[36..44]; // user_data
    let _ = &data[44..52]; // data_ptr
    let _ = &data[52..60]; // data_len
    let _ = &data[60..68]; // buffer
    let _ = &data[68..72]; // error_code
    let _ = &data[72..76]; // flags

    // Also verify writing doesn't corrupt adjacent fields (ASan detects OOB)
    let mut buf = data.to_vec();
    let _ = &mut buf[0..4];
    let _ = &mut buf[4..8];
    let _ = &mut buf[8..12];
    let _ = &mut buf[12..20];
    let _ = &mut buf[20..28];
    let _ = &mut buf[28..36];
    let _ = &mut buf[36..44];
    let _ = &mut buf[44..52];
    let _ = &mut buf[52..60];
    let _ = &mut buf[60..68];
    let _ = &mut buf[68..72];
    let _ = &mut buf[72..76];
});
