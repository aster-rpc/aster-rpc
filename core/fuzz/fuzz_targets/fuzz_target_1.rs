#![no_main]
use libfuzzer_sys::fuzz_target;

// Fuzz the binary ticket decoder — most complex parsing in core
fuzz_target!(|data: &[u8]| {
    let _ = aster_transport_core::ticket::AsterTicket::decode(data);
});
