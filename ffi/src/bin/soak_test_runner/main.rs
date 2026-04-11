//! Standalone runner for the soak test — calls the public run_soak_test in lib.rs.
//!
//! Usage:
//!     cargo run -p aster_transport_ffi --bin soak_test_runner -- 1800

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let duration_secs = if args.len() > 1 {
        args[1].parse().unwrap_or(14400)
    } else {
        14400
    };
    aster_transport_ffi::run_soak_test(duration_secs);
}
