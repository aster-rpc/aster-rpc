#![no_main]
use aster_transport_core::delegation::{verify_chain, LeafKind, NoRevocations, TrustContext};
use libfuzzer_sys::fuzz_target;

// Fuzz chain framing and link decoding (gates G0-G1) through the only entry
// point. Arbitrary input must be rejected without panicking.
fuzz_target!(|data: &[u8]| {
    let anchors = [[1; 32]];
    let trust = TrustContext::new(&anchors, "fleet", 1_000_000, &NoRevocations);
    let _ = verify_chain(data, LeafKind::Enrollment, &[2; 32], &trust);
});
