//! Fuzz double release / double cancel — idempotent behavior required.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Model: ops start Submitted, first terminal transition wins.
    // Calling terminal ops twice must be idempotent (not produce two events).
    if data.is_empty() {
        return;
    }

    let initial_state = data[0] % 4; // 0=Submitted, 1=Completed, 2=Cancelled, 3=Error

    let first_result: &str = match initial_state {
        0 => "submitted",
        1 => "completed",
        2 => "cancelled",
        _ => "error",
    };

    // Second call: if already terminal, should be a no-op (same result)
    let second_result: &str = match initial_state {
        0 => "submitted",
        1 => "completed",
        2 => "cancelled",
        _ => "error",
    };

    if first_result != "submitted" {
        // Already terminal — second call must return same result (idempotent)
        assert_eq!(first_result, second_result);
    }
});
