#![no_main]
use libfuzzer_sys::fuzz_target;

// Fuzz the wire frame decoder
fuzz_target!(|data: &[u8]| {
    let _ = aster_transport_core::framing::decode_frame(data);
});
