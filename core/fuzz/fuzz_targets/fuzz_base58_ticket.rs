#![no_main]
use libfuzzer_sys::fuzz_target;

// Fuzz the base58 ticket decoder
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = aster_transport_core::ticket::AsterTicket::from_base58_str(s);
    }
});
