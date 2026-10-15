//! ABI 6.0: a caller built for 5.x is refused cleanly and nothing writes past
//! its 280-byte `aster_config_t`.
//!
//! One test in its own binary (its own process): the ABI handshake is
//! process-wide, so the "not yet accepted" state must not be shared with
//! tests that create runtimes.

use std::ptr;

use aster_transport_ffi::node::*;
use aster_transport_ffi::*;

/// `sizeof(aster_config_t)` in ABI 5.x.
const V5_CONFIG_SIZE: usize = 280;
const GUARD: u8 = 0xa5;

const MISMATCH: i32 = aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;

fn last_error() -> String {
    let mut msg = [0u8; 512];
    let n = unsafe { aster_last_error_message(msg.as_mut_ptr(), msg.len()) };
    String::from_utf8_lossy(&msg[..n.min(msg.len())]).into_owned()
}

/// A 5.x caller's config buffer: 280 bytes, followed by guard bytes that
/// stand in for whatever the caller keeps after its struct. Aligned like the
/// struct.
#[repr(C, align(8))]
struct V5Buffer {
    bytes: [u8; V5_CONFIG_SIZE + 64],
}

impl V5Buffer {
    fn new() -> Self {
        Self {
            bytes: [GUARD; V5_CONFIG_SIZE + 64],
        }
    }
    fn as_config(&mut self) -> *mut aster_config_t {
        self.bytes.as_mut_ptr().cast()
    }
    fn untouched(&self) -> bool {
        self.bytes.iter().all(|&b| b == GUARD)
    }
}

#[test]
fn a_5x_caller_is_refused_without_writing_past_its_struct() {
    assert_eq!(ASTER_ABI_VERSION_MAJOR, 6);
    assert_eq!(aster_abi_version() >> 16, 6);
    assert_eq!(std::mem::size_of::<aster_config_t>(), 288);

    unsafe {
        // A 5.x caller fills a config before (or without) a runtime: refused,
        // and not one byte of its buffer is written.
        let mut buf = V5Buffer::new();
        assert_eq!(aster_config_init(buf.as_config()), MISMATCH);
        assert!(
            last_error().contains("aster_abi_accept(6)"),
            "{}",
            last_error()
        );
        assert_eq!(aster_config_from_env(buf.as_config()), MISMATCH);
        let path = b"/nonexistent/aster.toml";
        let path = aster_bytes_t {
            ptr: path.as_ptr(),
            len: path.len(),
        };
        assert_eq!(aster_config_from_toml(path, buf.as_config()), MISMATCH);
        assert!(
            buf.untouched(),
            "a refused fill wrote into the caller's memory"
        );

        // Its version handshake is refused cleanly, and a refusal does not
        // unlock the fill functions.
        let mut rt: aster_runtime_t = 0;
        assert_eq!(aster_runtime_new(5, ptr::null(), &mut rt), MISMATCH);
        assert_eq!(rt, 0);
        assert!(
            last_error().contains("caller expects 5"),
            "{}",
            last_error()
        );
        assert_eq!(aster_abi_accept(5), MISMATCH);
        assert_eq!(aster_config_init(buf.as_config()), MISMATCH);
        assert!(buf.untouched());

        // A 5.x struct handed to a reader is refused on struct_size before
        // anything past 280 bytes is read.
        let mut v5 = V5Buffer::new();
        v5.bytes[..4].copy_from_slice(&(V5_CONFIG_SIZE as u32).to_ne_bytes());
        let mut op = 0;
        assert_eq!(aster_node_start(0, v5.as_config(), 0, 0, &mut op), INVALID);
        assert!(
            last_error().contains("struct_size is 280"),
            "{}",
            last_error()
        );
        assert_eq!(aster_config_release(v5.as_config()), INVALID);

        // A 6.x caller passes the handshake and the fill works.
        assert_eq!(aster_abi_accept(ASTER_ABI_VERSION_MAJOR), 0);
        let mut cfg: aster_config_t = std::mem::zeroed();
        assert_eq!(aster_config_init(&mut cfg), 0);
        assert_eq!(
            cfg.struct_size as usize,
            std::mem::size_of::<aster_config_t>()
        );
        assert_eq!(cfg.blob_zstd_level, 3);
        assert_eq!(aster_config_release(&mut cfg), 0);
    }
}
