//! Temporary custody and verified retention through the actual asynchronous C ABI.
use aster_transport_ffi::{custody::*, data::*, *};
use std::{ptr, slice, time::Duration};
mod common;
fn span(b: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: b.as_ptr(),
        len: b.len(),
    }
}
fn hash(b: &[u8]) -> String {
    aster::native::iroh_blobs::Hash::new(b).to_hex().to_string()
}
struct Fixture {
    rt: u64,
    node: u64,
}
impl Fixture {
    fn new() -> Self {
        unsafe {
            let mut rt = 0;
            assert_eq!(
                aster_runtime_new(
                    aster_transport_ffi::ASTER_ABI_VERSION_MAJOR,
                    &common::default_runtime_config(),
                    &mut rt
                ),
                0
            );
            let node = common::start_node(rt, &[]);
            Self { rt, node }
        }
    }
    fn scope(&self) -> u64 {
        unsafe {
            let mut s = 0;
            assert_eq!(aster_blobs_custody_new(self.rt, self.node, &mut s), 0);
            s
        }
    }
    fn finish(&self, op: u64) -> (aster_event_t, Vec<u8>) {
        unsafe {
            let ev = common::wait_for_operation(self.rt, op);
            let data = if ev.data_len == 0 {
                vec![]
            } else {
                slice::from_raw_parts(ev.data_ptr, ev.data_len).to_vec()
            };
            if ev.buffer != 0 {
                assert_eq!(aster_buffer_release(self.rt, ev.buffer), 0);
            }
            (ev, data)
        }
    }
    fn run(&self, kind: aster_event_kind_t, f: impl FnOnce(*mut u64) -> i32) -> Vec<u8> {
        let mut op = 0;
        assert_eq!(f(&mut op), 0);
        let (ev, data) = self.finish(op);
        assert_eq!(
            (ev.kind, ev.status),
            (kind as u32, 0),
            "{}",
            String::from_utf8_lossy(&data)
        );
        data
    }
    fn gc(&self) {
        self.run(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, |op| unsafe {
            aster_blobs_gc_run_once(self.rt, self.node, 0, op)
        });
    }
    fn read(&self, h: &str) -> Vec<u8> {
        self.run(aster_event_kind_t::ASTER_EVENT_BLOB_READ, |op| unsafe {
            aster_blobs_read(self.rt, self.node, span(h.as_bytes()), 0, op)
        })
    }
    fn missing(&self, h: &str) {
        let mut op = 0;
        assert_eq!(
            unsafe { aster_blobs_read(self.rt, self.node, span(h.as_bytes()), 0, &mut op) },
            0
        );
        assert_ne!(self.finish(op).0.status, 0);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert_eq!(
            aster_runtime_close_and_wait(self.rt, Duration::from_secs(10)),
            0
        );
    }
}
#[test]
fn temporary_scope_owns_multiple_typed_roots_until_free() {
    let f = Fixture::new();
    let scope = f.scope();
    let child = b"custody child";
    let h = hash(child);
    f.run(aster_event_kind_t::ASTER_EVENT_BLOB_ADDED, |op| unsafe {
        aster_blob_custody_add_bytes(f.rt, scope, span(child), 0, 0, op)
    });
    let seq = hex::decode(&h).unwrap();
    let root = hash(&seq);
    f.run(aster_event_kind_t::ASTER_EVENT_BLOB_ADDED, |op| unsafe {
        aster_blob_custody_add_bytes(f.rt, scope, span(&seq), 1, 0, op)
    });
    f.gc();
    assert_eq!(f.read(&h), child);
    assert_eq!(f.read(&root), seq);
    f.run(
        aster_event_kind_t::ASTER_EVENT_BLOB_CUSTODY_DONE,
        |op| unsafe {
            aster_blob_custody_retain(f.rt, scope, span(root.as_bytes()), 1, span(b"tree"), 0, op)
        },
    );
    assert_eq!(aster_blob_custody_free(f.rt, scope), 0);
    f.gc();
    assert_eq!(f.read(&h), child);
    f.run(aster_event_kind_t::ASTER_EVENT_TAG_DELETED, |op| unsafe {
        aster_tags_delete(f.rt, f.node, b"tree".as_ptr(), 4, 0, op)
    });
    f.gc();
    f.missing(&h);
    f.missing(&root);
}
#[test]
fn verified_import_preserves_named_root_on_mismatch_and_has_no_anonymous_root() {
    let f = Fixture::new();
    let data = b"verified import";
    let h = hash(data);
    assert_eq!(
        f.run(aster_event_kind_t::ASTER_EVENT_BLOB_ADDED, |op| unsafe {
            aster_blobs_add_bytes_verified(
                f.rt,
                f.node,
                span(data),
                span(h.as_bytes()),
                0,
                span(b"keep"),
                0,
                op,
            )
        }),
        h.as_bytes()
    );
    let mut op = 0;
    assert_eq!(
        unsafe {
            aster_blobs_add_bytes_verified(
                f.rt,
                f.node,
                span(b"wrong"),
                span(h.as_bytes()),
                0,
                span(b"keep"),
                0,
                &mut op,
            )
        },
        0
    );
    assert_eq!(
        f.finish(op).0.status,
        aster_status_t::ASTER_STATUS_VERIFICATION_FAILED as u32
    );
    f.gc();
    assert_eq!(f.read(&h), data);
    f.missing(&hash(b"wrong"));
    f.run(aster_event_kind_t::ASTER_EVENT_TAG_DELETED, |op| unsafe {
        aster_tags_delete(f.rt, f.node, b"keep".as_ptr(), 4, 0, op)
    });
    f.gc();
    f.missing(&h);
}
#[test]
fn copied_path_is_independent_and_closed_scope_rejects_new_work() {
    let f = Fixture::new();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"copied").unwrap();
    let h = hash(b"copied");
    let path = file.path().to_str().unwrap();
    f.run(aster_event_kind_t::ASTER_EVENT_BLOB_ADDED, |op| unsafe {
        aster_blobs_add_path_verified(
            f.rt,
            f.node,
            span(path.as_bytes()),
            span(h.as_bytes()),
            0,
            span(b"file"),
            0,
            op,
        )
    });
    std::fs::write(file.path(), b"changed").unwrap();
    assert_eq!(f.read(&h), b"copied");
    let scope = f.scope();
    assert_eq!(aster_blob_custody_free(f.rt, scope), 0);
    let mut op = 0;
    assert_eq!(
        unsafe { aster_blob_custody_protect(f.rt, scope, span(h.as_bytes()), 0, 0, &mut op) },
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    );
    assert_eq!(
        aster_blob_custody_free(f.rt, scope),
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    );
}
#[test]
fn invalid_inputs_and_wrong_runtime_do_not_create_operations() {
    let f = Fixture::new();
    let other = Fixture::new();
    let scope = f.scope();
    let mut op = 0;
    assert_eq!(
        unsafe {
            aster_blob_custody_protect(other.rt, scope, span(hash(b"x").as_bytes()), 0, 0, &mut op)
        },
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    );
    assert_eq!(
        unsafe { aster_blob_custody_protect(f.rt, scope, span(b"bad"), 0, 0, &mut op) },
        aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
    );
    assert_eq!(
        unsafe { aster_blob_custody_add_bytes(f.rt, scope, span(b"x"), 9, 0, &mut op) },
        aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
    );
    assert_eq!(
        unsafe { aster_blobs_custody_new(f.rt, f.node, ptr::null_mut()) },
        aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
    );
    assert_eq!(aster_blob_custody_free(f.rt, scope), 0);
}
