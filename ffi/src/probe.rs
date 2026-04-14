//! Env-gated timing probes for Stage-1 profiling of the sequential-unary hot path.
//!
//! Enable by setting `ASTER_PROBES=1` in the process environment before the
//! first call. Records four timestamps per unary call (nanoseconds since
//! process start): function entry, after request write, first data frame
//! read, trailer read.
//!
//! Use `aster_probe_dump_unary_csv(path)` to flush the collected records to
//! disk and `aster_probe_reset()` to clear between benchmark stages.

use std::fs;
use std::sync::Mutex;
use std::time::Instant;

use once_cell::sync::Lazy;

static START: Lazy<Instant> = Lazy::new(Instant::now);

pub static ENABLED: Lazy<bool> =
    Lazy::new(|| std::env::var("ASTER_PROBES").ok().as_deref() == Some("1"));

#[derive(Clone, Copy)]
pub struct UnaryProbe {
    pub t_a: u64,
    pub t_b: u64,
    pub t_c: u64,
    pub t_d: u64,
}

static UNARY_RECORDS: Lazy<Mutex<Vec<UnaryProbe>>> =
    Lazy::new(|| Mutex::new(Vec::with_capacity(4096)));

#[inline]
pub fn now_ns() -> u64 {
    let start = *START;
    Instant::now().duration_since(start).as_nanos() as u64
}

#[inline]
pub fn record_unary(p: UnaryProbe) {
    if *ENABLED {
        UNARY_RECORDS.lock().unwrap().push(p);
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_probe_reset() {
    UNARY_RECORDS.lock().unwrap().clear();
}

#[no_mangle]
pub unsafe extern "C" fn aster_probe_dump_unary_csv(path_ptr: *const u8, path_len: u32) -> i32 {
    if path_ptr.is_null() || path_len == 0 {
        return -1;
    }
    let path_bytes = unsafe { std::slice::from_raw_parts(path_ptr, path_len as usize) };
    let path = match std::str::from_utf8(path_bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let records = UNARY_RECORDS.lock().unwrap();
    let mut out = String::from("i,t_a,t_b,t_c,t_d\n");
    for (i, p) in records.iter().enumerate() {
        out.push_str(&format!("{},{},{},{},{}\n", i, p.t_a, p.t_b, p.t_c, p.t_d));
    }
    if fs::write(path, out).is_err() {
        return -1;
    }
    0
}
