//! ABI 4.1 (F5 upcalls) through the C functions: a "foreign" thread polls
//! `aster_upcall_poll` and answers with `aster_upcall_respond`, decoding and
//! encoding UCv1 with the Rust codec (as a binding would by hand), while the
//! test drives leases, fenced mutations and the topology admission filter
//! through the completion queue.

use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aster::binding_support::upcall::codec;
use aster::lease::{
    DenyReason, FenceRejection, LeaseOp, LeaseSerializer, MemorySerializer, TransitionOutcome,
};
use aster_transport_ffi::upcall::*;
use aster_transport_ffi::*;

mod common;

const OK: i32 = 0;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const NOT_FOUND: i32 = aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
const ALREADY_CLOSED: i32 = aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;

const K_CANCEL: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_CANCEL as u16;
const K_TRANSITION: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_LEASE_TRANSITION as u16;
const K_SNAPSHOT: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_LEASE_SNAPSHOT as u16;
const K_AUTHORIZE: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_LEASE_AUTHORIZE as u16;
const K_FENCED: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_FENCED_MUTATE as u16;
const K_ADMITTED: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_TOPOLOGY_ADMITTED as u16;

const S_OK: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_OK as u32;
const S_ERROR: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_ERROR as u32;
const S_UNIMPLEMENTED: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_UNIMPLEMENTED as u32;

const FAIL_OPEN: u32 = aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_OPEN as u32;
const FAIL_CLOSED: u32 = aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED as u32;

fn bytes(b: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: if b.is_empty() {
            ptr::null()
        } else {
            b.as_ptr()
        },
        len: b.len(),
    }
}

fn list(items: &[aster_bytes_t]) -> aster_bytes_list_t {
    aster_bytes_list_t {
        items: if items.is_empty() {
            ptr::null()
        } else {
            items.as_ptr()
        },
        len: items.len(),
    }
}

fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    let cfg = aster_runtime_config_t {
        struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
        worker_threads: 2,
        event_queue_capacity: 256,
        reserved: 0,
    };
    assert_eq!(
        unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &cfg, &mut rt) },
        OK
    );
    rt
}

fn new_upcalls(rt: aster_runtime_t) -> aster_upcalls_t {
    let mut u = 0;
    assert_eq!(unsafe { aster_upcalls_new(rt, 0, &mut u) }, OK);
    assert_ne!(u, 0);
    u
}

/// A finished operation: the event plus a copy of its payload (buffer freed).
struct Ev {
    ev: aster_event_t,
    data: Vec<u8>,
}

impl Ev {
    fn is_ok(&self, kind: aster_event_kind_t) -> bool {
        self.ev.kind == kind as u32 && self.ev.status == 0
    }
    fn is_error(&self) -> bool {
        self.ev.kind == aster_event_kind_t::ASTER_EVENT_ERROR as u32
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

unsafe fn finish(rt: aster_runtime_t, op: aster_operation_t) -> Ev {
    let ev = common::wait_for_operation(rt, op);
    let data = if ev.data_len == 0 || ev.data_ptr.is_null() {
        Vec::new()
    } else {
        slice::from_raw_parts(ev.data_ptr, ev.data_len).to_vec()
    };
    if ev.buffer != 0 {
        aster_buffer_release(rt, ev.buffer);
    }
    Ev { ev, data }
}

/// What the foreign handler does with one upcall.
enum Reply {
    Answer(u32, Vec<u8>),
    /// Never answer (let Rust time out / cancel).
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    id: u64,
    kind: u16,
    cancel_reason: u8,
    payload: Vec<u8>,
}

/// The "binding": one thread draining the queue and answering inline.
struct Foreign {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<Seen>>>,
    late: Arc<Mutex<Vec<i32>>>,
    thread: Option<JoinHandle<()>>,
}

impl Foreign {
    fn start(
        rt: aster_runtime_t,
        upcalls: aster_upcalls_t,
        handler: impl Fn(u16, &[u8]) -> Reply + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let late = Arc::new(Mutex::new(Vec::new()));
        let (s, sn, lt) = (stop.clone(), seen.clone(), late.clone());
        let thread = std::thread::spawn(move || {
            let mut events: [aster_upcall_event_t; 8] = unsafe { std::mem::zeroed() };
            while !s.load(Ordering::Acquire) {
                let mut n = 0u32;
                let _ =
                    unsafe { aster_upcall_poll(rt, upcalls, events.as_mut_ptr(), 8, 50, &mut n) };
                for ev in events.iter().take(n as usize) {
                    let payload = if ev.payload_len == 0 {
                        Vec::new()
                    } else {
                        unsafe { slice::from_raw_parts(ev.payload_ptr, ev.payload_len as usize) }
                            .to_vec()
                    };
                    assert_eq!(
                        unsafe { aster_upcall_buffer_release(rt, upcalls, ev.buffer) },
                        OK
                    );
                    sn.lock().unwrap().push(Seen {
                        id: ev.invocation_id,
                        kind: ev.kind,
                        cancel_reason: ev.cancel_reason,
                        payload: payload.clone(),
                    });
                    if ev.kind == K_CANCEL {
                        assert_eq!(ev.buffer, 0);
                        assert!(ev.payload_ptr.is_null());
                        continue;
                    }
                    assert!(ev.deadline_unix_ms > 0);
                    if let Reply::Answer(status, body) = handler(ev.kind, &payload) {
                        let st = unsafe {
                            aster_upcall_respond(
                                rt,
                                upcalls,
                                ev.invocation_id,
                                status,
                                if body.is_empty() {
                                    ptr::null()
                                } else {
                                    body.as_ptr()
                                },
                                body.len() as u32,
                            )
                        };
                        if st != OK {
                            lt.lock().unwrap().push(st);
                        }
                    }
                }
            }
        });
        Foreign {
            stop,
            seen,
            late,
            thread: Some(thread),
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn wait_for(&self, what: &str, f: impl Fn(&[Seen]) -> bool) {
        let start = Instant::now();
        while !f(&self.seen()) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out waiting for {what}: {:?}",
                self.seen()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Foreign {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A foreign lease serializer backed by a `MemorySerializer` living on the
/// foreign side (the binding's own serialization point).
fn memory_backed_serializer(
    mem: MemorySerializer,
) -> impl Fn(u16, &[u8]) -> Reply + Send + 'static {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    move |kind, payload| match kind {
        K_TRANSITION => {
            let (resource, op) = codec::decode_lease_transition_request(payload).unwrap();
            let out = rt.block_on(mem.transition(&resource, op)).unwrap();
            Reply::Answer(S_OK, codec::encode_transition_outcome(&out))
        }
        K_SNAPSHOT => {
            let resource = codec::decode_lease_snapshot_request(payload).unwrap();
            let snap = rt.block_on(mem.snapshot(&resource)).unwrap();
            Reply::Answer(S_OK, codec::encode_lease_snapshot(&snap))
        }
        _ => Reply::Answer(S_UNIMPLEMENTED, Vec::new()),
    }
}

unsafe fn acquire(
    rt: aster_runtime_t,
    ser: aster_lease_serializer_t,
    resource: &str,
    holder: &str,
    ttl_ms: u64,
) -> Ev {
    let mut op = 0;
    assert_eq!(
        aster_lease_acquire(
            rt,
            ser,
            bytes(resource.as_bytes()),
            bytes(holder.as_bytes()),
            ttl_ms,
            0,
            0,
            7,
            &mut op
        ),
        OK
    );
    finish(rt, op)
}

#[test]
fn foreign_serializer_acquire_renew_fence_release() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        let foreign = Foreign::start(rt, u, memory_backed_serializer(MemorySerializer::new()));
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_foreign_new(rt, u, &mut ser), OK);

        // Granted: the payload is a UCv1 TransitionOutcome with the fence.
        let got = acquire(rt, ser, "tree/42", "alice", 600);
        assert!(
            got.is_ok(aster_event_kind_t::ASTER_EVENT_LEASE_ACQUIRED),
            "{}",
            got.text()
        );
        assert_eq!(got.ev.user_data, 7);
        let lease = got.ev.handle;
        assert_ne!(lease, 0);
        match codec::decode_transition_outcome(&got.data).unwrap() {
            TransitionOutcome::Granted(f) => {
                assert_eq!((f.resource.as_str(), f.epoch), ("tree/42", 1));
                assert_eq!(f.holder, "alice");
            }
            other => panic!("expected granted, got {other:?}"),
        }
        let mut epoch = 0;
        assert_eq!(aster_lease_fence(rt, lease, &mut epoch), OK);
        assert_eq!(epoch, 1);
        let mut status = 99;
        assert_eq!(aster_lease_status(rt, lease, &mut status), OK);
        assert_eq!(status, aster_lease_status_t::ASTER_LEASE_STATUS_HELD as u32);

        // Another holder is denied (HeldByOther) with a snapshot.
        let denied = acquire(rt, ser, "tree/42", "bob", 600);
        assert!(denied.is_ok(aster_event_kind_t::ASTER_EVENT_LEASE_ACQUIRED));
        assert_eq!(denied.ev.handle, 0);
        match codec::decode_transition_outcome(&denied.data).unwrap() {
            TransitionOutcome::Denied(DenyReason::HeldByOther, snap) => {
                assert_eq!(snap.holder.as_deref(), Some("alice"));
                assert!(snap.held);
            }
            other => panic!("expected denied, got {other:?}"),
        }

        // Renewals go through the foreign serializer (ttl 600 → every 200 ms).
        foreign.wait_for("two renewals", |seen| {
            seen.iter()
                .filter(|s| {
                    s.kind == K_TRANSITION
                        && matches!(
                            codec::decode_lease_transition_request(&s.payload),
                            Ok((_, LeaseOp::Renew { .. }))
                        )
                })
                .count()
                >= 2
        });
        assert_eq!(aster_lease_fence(rt, lease, &mut epoch), OK);

        // Snapshot through the serializer.
        let mut op = 0;
        assert_eq!(
            aster_lease_snapshot(rt, ser, bytes(b"tree/42"), 0, &mut op),
            OK
        );
        let snap = finish(rt, op);
        assert!(snap.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT));
        let snap = codec::decode_lease_snapshot(&snap.data).unwrap();
        assert_eq!((snap.epoch, snap.held), (1, true));

        // Watch, then release: the watch completes with RELEASED.
        let mut watch_op = 0;
        assert_eq!(aster_lease_watch(rt, lease, 0, &mut watch_op), OK);
        assert_eq!(aster_lease_release(rt, lease, 0, &mut op), OK);
        // The watch fires before the RELEASE round trip ends; wait for it
        // first (the helper drops other operations' events).
        let w = finish(rt, watch_op);
        assert!(w.is_ok(aster_event_kind_t::ASTER_EVENT_LEASE_STATUS));
        assert_eq!(
            w.ev.flags,
            aster_lease_status_t::ASTER_LEASE_STATUS_RELEASED as u32
        );
        let rel = finish(rt, op);
        assert!(
            rel.is_ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT),
            "{}",
            rel.text()
        );
        assert_eq!(aster_lease_fence(rt, lease, &mut epoch), NOT_FOUND);
        assert_eq!(aster_lease_release(rt, lease, 0, &mut op), NOT_FOUND);
        assert!(foreign.seen().iter().any(|s| s.kind == K_TRANSITION
            && matches!(
                codec::decode_lease_transition_request(&s.payload),
                Ok((_, LeaseOp::Release { epoch: 1, .. }))
            )));

        // Released: bob can take it at the next epoch; revoke advances it.
        let got = acquire(rt, ser, "tree/42", "bob", 600);
        let bob = got.ev.handle;
        assert_ne!(bob, 0);
        assert_eq!(
            aster_lease_revoke(rt, ser, bytes(b"tree/42"), bytes(b"root"), 0, &mut op),
            OK
        );
        let rev = finish(rt, op);
        assert!(matches!(
            codec::decode_transition_outcome(&rev.data).unwrap(),
            TransitionOutcome::Revoked
        ));
        // Bob's next renewal is refused: fenced.
        let mut watch_op = 0;
        assert_eq!(aster_lease_watch(rt, bob, 0, &mut watch_op), OK);
        let w = finish(rt, watch_op);
        assert_eq!(
            w.ev.flags,
            aster_lease_status_t::ASTER_LEASE_STATUS_FENCED as u32
        );
        assert_eq!(aster_lease_fence(rt, bob, &mut epoch), ALREADY_CLOSED);
        assert_eq!(aster_lease_free(rt, bob), OK);
        assert_eq!(aster_lease_free(rt, bob), NOT_FOUND);
        assert!(foreign.late.lock().unwrap().is_empty());

        assert_eq!(aster_lease_serializer_free(rt, ser), OK);
        assert_eq!(aster_lease_serializer_free(rt, ser), NOT_FOUND);
        drop(foreign);
        assert_eq!(aster_upcalls_free(rt, u), OK);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn memory_serializer_with_foreign_authorizer() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        let _foreign = Foreign::start(rt, u, |kind, payload| {
            assert_eq!(kind, K_AUTHORIZE);
            let op = codec::decode_lease_authorize_request(payload).unwrap();
            Reply::Answer(S_OK, codec::encode_decision(op.actor() != "mallory"))
        });
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_memory_new(rt, u, &mut ser), OK);

        let m = acquire(rt, ser, "db", "mallory", 0);
        match codec::decode_transition_outcome(&m.data).unwrap() {
            TransitionOutcome::Denied(DenyReason::Unauthorized, _) => {}
            other => panic!("expected unauthorized, got {other:?}"),
        }
        let a = acquire(rt, ser, "db", "alice", 0);
        assert_ne!(
            a.ev.handle,
            0,
            "{:?}",
            codec::decode_transition_outcome(&a.data)
        );
        assert_eq!(aster_lease_free(rt, a.ev.handle), OK);

        // Without an authorizer table everyone is admitted.
        let mut open = 0;
        assert_eq!(aster_lease_serializer_memory_new(rt, 0, &mut open), OK);
        let m = acquire(rt, open, "db", "mallory", 0);
        assert_ne!(m.ev.handle, 0);
        assert_eq!(aster_lease_free(rt, m.ev.handle), OK);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn authorizer_failure_mode_applies_without_an_answer() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        // Nobody polls: each authorize upcall times out and is withdrawn.
        assert_eq!(
            aster_upcall_set_policy(rt, u, K_AUTHORIZE as u32, 100, FAIL_CLOSED),
            OK
        );
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_memory_new(rt, u, &mut ser), OK);
        let started = Instant::now();
        let d = acquire(rt, ser, "r", "alice", 0);
        assert!(started.elapsed() >= Duration::from_millis(90));
        assert!(matches!(
            codec::decode_transition_outcome(&d.data).unwrap(),
            TransitionOutcome::Denied(DenyReason::Unauthorized, _)
        ));

        assert_eq!(
            aster_upcall_set_policy(rt, u, K_AUTHORIZE as u32, 100, FAIL_OPEN),
            OK
        );
        let g = acquire(rt, ser, "r", "alice", 0);
        assert_ne!(g.ev.handle, 0);
        assert_eq!(aster_lease_free(rt, g.ev.handle), OK);

        // Withdrawn events never reach a late poller.
        let mut events: [aster_upcall_event_t; 4] = std::mem::zeroed();
        let mut n = 7u32;
        assert_eq!(
            aster_upcall_poll(rt, u, events.as_mut_ptr(), 4, 0, &mut n),
            OK
        );
        assert_eq!(n, 0);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn timeout_sends_cancel_and_late_respond_is_not_found() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        assert_eq!(aster_upcall_set_default_policy(rt, u, 150, FAIL_CLOSED), OK);
        let foreign = Foreign::start(rt, u, |_, _| Reply::Ignore);
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_foreign_new(rt, u, &mut ser), OK);
        let e = acquire(rt, ser, "r", "alice", 0);
        assert!(e.is_error(), "{}", e.text());
        assert!(e.text().contains("timed out"), "{}", e.text());

        foreign.wait_for("cancel", |seen| seen.iter().any(|s| s.kind == K_CANCEL));
        let seen = foreign.seen();
        let call = seen.iter().find(|s| s.kind == K_TRANSITION).unwrap();
        let cancel = seen.iter().find(|s| s.kind == K_CANCEL).unwrap();
        assert_eq!(cancel.id, call.id);
        assert_eq!(
            cancel.cancel_reason,
            aster_upcall_cancel_reason_t::ASTER_UPCALL_CANCEL_TIMEOUT as u8
        );
        assert!(cancel.payload.is_empty());
        let body = codec::encode_transition_outcome(&TransitionOutcome::Renewed);
        assert_eq!(
            aster_upcall_respond(rt, u, call.id, S_OK, body.as_ptr(), body.len() as u32),
            NOT_FOUND
        );
        assert_eq!(
            aster_upcall_respond(rt, u, 999_999, S_OK, ptr::null(), 0),
            NOT_FOUND
        );
        drop(foreign);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn error_and_unimplemented_statuses_surface_as_errors() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        let flip = Arc::new(AtomicBool::new(false));
        let f2 = flip.clone();
        let _foreign = Foreign::start(rt, u, move |_, _| {
            if f2.swap(true, Ordering::AcqRel) {
                Reply::Answer(S_UNIMPLEMENTED, Vec::new())
            } else {
                Reply::Answer(S_ERROR, b"disk on fire".to_vec())
            }
        });
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_foreign_new(rt, u, &mut ser), OK);
        let e = acquire(rt, ser, "r", "alice", 0);
        assert!(
            e.is_error() && e.text().contains("disk on fire"),
            "{}",
            e.text()
        );
        let e = acquire(rt, ser, "r", "alice", 0);
        assert!(
            e.is_error() && e.text().contains("not implemented"),
            "{}",
            e.text()
        );
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn fenced_mutate_round_trip() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        let _foreign = Foreign::start(rt, u, |kind, payload| {
            assert_eq!(kind, K_FENCED);
            let (fence, writer, op) = codec::decode_fenced_mutate_request(payload).unwrap();
            let res = if fence.epoch == 7 && writer == fence.holder {
                Ok(op.iter().rev().copied().collect())
            } else if writer != fence.holder {
                Err(FenceRejection::NotHolder)
            } else {
                Err(FenceRejection::StaleEpoch {
                    carried: fence.epoch,
                    current: 7,
                })
            };
            Reply::Answer(S_OK, codec::encode_fenced_mutate_response(&res))
        });
        let mut res = 0;
        assert_eq!(aster_fenced_resource_foreign_new(rt, u, &mut res), OK);
        let mutate = |epoch: u64, writer: &str| {
            let mut op = 0;
            assert_eq!(
                aster_fenced_mutate(
                    rt,
                    res,
                    bytes(b"tree/42"),
                    epoch,
                    bytes(b"alice"),
                    bytes(writer.as_bytes()),
                    bytes(b"abc"),
                    0,
                    &mut op
                ),
                OK
            );
            let ev = finish(rt, op);
            assert!(ev.is_ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT));
            codec::decode_fenced_mutate_response(&ev.data).unwrap()
        };
        assert_eq!(mutate(7, "alice"), Ok(b"cba".to_vec()));
        assert_eq!(
            mutate(6, "alice"),
            Err(FenceRejection::StaleEpoch {
                carried: 6,
                current: 7
            })
        );
        assert_eq!(mutate(7, "mallory"), Err(FenceRejection::NotHolder));
        assert_eq!(aster_fenced_resource_free(rt, res), OK);
        assert_eq!(aster_fenced_resource_free(rt, res), NOT_FOUND);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn fenced_mutate_without_answer_is_outcome_unknown() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        assert_eq!(
            aster_upcall_set_policy(rt, u, K_FENCED as u32, 100, FAIL_OPEN),
            OK
        );
        let mut res = 0;
        assert_eq!(aster_fenced_resource_foreign_new(rt, u, &mut res), OK);
        let mut op = 0;
        assert_eq!(
            aster_fenced_mutate(
                rt,
                res,
                bytes(b"r"),
                1,
                bytes(b"a"),
                bytes(b"a"),
                bytes(b""),
                0,
                &mut op
            ),
            OK
        );
        let ev = finish(rt, op);
        assert!(ev.is_error());
        assert!(ev.text().contains("outcome unknown"), "{}", ev.text());
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

fn hex_id(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

#[test]
fn topology_admitted_filter_batches_and_caches() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        let foreign = Foreign::start(rt, u, |kind, payload| {
            assert_eq!(kind, K_ADMITTED);
            let ids = codec::decode_topology_admitted_request(payload).unwrap();
            let answers: Vec<bool> = ids.iter().map(|id| id.starts_with('a')).collect();
            Reply::Answer(S_OK, codec::encode_topology_admitted_response(&answers))
        });
        let mut f = 0;
        assert_eq!(aster_topology_admitted_new(rt, u, 0, 0, 0, &mut f), OK);
        let (a, b, c) = (hex_id('a'), hex_id('b'), hex_id('c'));
        let ids = [bytes(a.as_bytes()), bytes(b.as_bytes())];
        let mut op = 0;
        assert_eq!(
            aster_topology_admitted_prefetch(rt, f, list(&ids), 0, &mut op),
            OK
        );
        let ev = finish(rt, op);
        assert!(ev.is_ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT));
        // One batched upcall for both ids.
        let batches: Vec<_> = foreign
            .seen()
            .into_iter()
            .filter(|s| s.kind == K_ADMITTED)
            .collect();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            codec::decode_topology_admitted_request(&batches[0].payload).unwrap(),
            vec![a.clone(), b.clone()]
        );

        let check = |id: &str| {
            let mut out = 9;
            assert_eq!(
                aster_topology_admitted_check(rt, f, bytes(id.as_bytes()), &mut out),
                OK
            );
            out
        };
        assert_eq!(check(&a), 1);
        assert_eq!(check(&b), 0);
        // Upper-case hex is the same id.
        assert_eq!(check(&a.to_uppercase()), 1);
        // A miss answers the failure mode (closed: 0) and resolves in the
        // background.
        assert_eq!(check(&c), 0);
        foreign.wait_for("background batch", |seen| {
            seen.iter().filter(|s| s.kind == K_ADMITTED).count() >= 2
        });

        assert_eq!(
            aster_topology_admitted_set(rt, f, bytes(b.as_bytes()), 1),
            OK
        );
        assert_eq!(check(&b), 1);
        assert_eq!(
            aster_topology_admitted_invalidate(rt, f, bytes(a.as_bytes())),
            OK
        );
        assert_eq!(aster_topology_admitted_invalidate(rt, f, bytes(b"")), OK);
        let mut stats = aster_topology_admitted_stats_t::default();
        assert_eq!(aster_topology_admitted_stats(rt, f, &mut stats), OK);
        assert_eq!(stats.hits, 4);
        assert_eq!(stats.misses, 1);
        assert!(stats.upcalls >= 2);

        // Bad ids are refused up front.
        let mut out = 0;
        assert_eq!(
            aster_topology_admitted_check(rt, f, bytes(b"zz"), &mut out),
            INVALID
        );
        assert_eq!(aster_topology_admitted_free(rt, f), OK);
        assert_eq!(aster_topology_admitted_free(rt, f), NOT_FOUND);
        drop(foreign);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn close_fails_pending_upcalls_with_shutdown_cancel() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        let foreign = Foreign::start(rt, u, |_, _| Reply::Ignore);
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_foreign_new(rt, u, &mut ser), OK);
        let mut op = 0;
        assert_eq!(
            aster_lease_acquire(rt, ser, bytes(b"r"), bytes(b"alice"), 0, 0, 0, 0, &mut op),
            OK
        );
        foreign.wait_for("transition", |seen| {
            seen.iter().any(|s| s.kind == K_TRANSITION)
        });
        assert_eq!(aster_upcalls_close(rt, u), OK);
        let ev = finish(rt, op);
        assert!(ev.is_error(), "{}", ev.text());
        foreign.wait_for("shutdown cancel", |seen| {
            seen.iter().any(|s| {
                s.kind == K_CANCEL
                    && s.cancel_reason
                        == aster_upcall_cancel_reason_t::ASTER_UPCALL_CANCEL_SHUTDOWN as u8
            })
        });
        // New upcalls fail at once.
        let e = acquire(rt, ser, "r", "alice", 0);
        assert!(e.is_error());
        assert_eq!(aster_upcalls_close(rt, u), OK);
        drop(foreign);
        assert_eq!(aster_upcalls_free(rt, u), OK);
        assert_eq!(aster_upcalls_free(rt, u), NOT_FOUND);
        assert_eq!(aster_upcalls_close(rt, u), NOT_FOUND);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn queue_capacity_refuses_overflow() {
    unsafe {
        let rt = runtime();
        let mut u = 0;
        assert_eq!(aster_upcalls_new(rt, 1, &mut u), OK);
        assert_eq!(aster_upcall_set_default_policy(rt, u, 300, FAIL_CLOSED), OK);
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_foreign_new(rt, u, &mut ser), OK);
        let submit = |res: &[u8]| {
            let mut op = 0;
            assert_eq!(
                aster_lease_acquire(rt, ser, bytes(res), bytes(b"a"), 0, 0, 0, 0, &mut op),
                OK
            );
            op
        };
        let first = submit(b"r1");
        // Let the first one occupy the single slot before the second arrives.
        std::thread::sleep(Duration::from_millis(50));
        let second = finish(rt, submit(b"r2"));
        assert!(second.is_error());
        assert!(second.text().contains("refused"), "{}", second.text());
        let first = finish(rt, first);
        assert!(first.text().contains("timed out"), "{}", first.text());
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn argument_validation() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        assert_eq!(aster_upcalls_new(rt, 0, ptr::null_mut()), INVALID);
        let mut x = 0;
        assert_eq!(aster_upcalls_new(424242, 0, &mut x), NOT_FOUND);
        // Policies.
        assert_eq!(aster_upcall_set_policy(rt, u, 0, 100, FAIL_OPEN), INVALID);
        assert_eq!(aster_upcall_set_policy(rt, u, 5, 100, FAIL_OPEN), INVALID);
        assert_eq!(
            aster_upcall_set_policy(rt, u, 70000, 100, FAIL_OPEN),
            INVALID
        );
        assert_eq!(aster_upcall_set_policy(rt, u, 3, 0, FAIL_OPEN), INVALID);
        assert_eq!(aster_upcall_set_policy(rt, u, 3, 100, 7), INVALID);
        assert_eq!(
            aster_upcall_set_policy(rt, 424242 + u, 3, 100, 0),
            NOT_FOUND
        );
        assert_eq!(aster_upcall_set_default_policy(rt, u, 0, 0), INVALID);
        // Poll / respond / buffers.
        let mut ev: aster_upcall_event_t = std::mem::zeroed();
        let mut n = 7u32;
        assert_eq!(
            aster_upcall_poll(rt, u, ptr::null_mut(), 1, 0, &mut n),
            INVALID
        );
        assert_eq!(aster_upcall_poll(rt, u, &mut ev, 0, 0, &mut n), INVALID);
        assert_eq!(
            aster_upcall_poll(rt, u, &mut ev, 1, 0, ptr::null_mut()),
            INVALID
        );
        assert_eq!(
            aster_upcall_poll(rt, 424242, &mut ev, 1, 0, &mut n),
            NOT_FOUND
        );
        assert_eq!(n, 0);
        let started = Instant::now();
        assert_eq!(aster_upcall_poll(rt, u, &mut ev, 1, 50, &mut n), OK);
        assert_eq!(n, 0);
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert_eq!(
            aster_upcall_respond(rt, u, 1, S_OK, ptr::null(), 3),
            INVALID
        );
        assert_eq!(aster_upcall_buffer_release(rt, u, 0), OK);
        assert_eq!(aster_upcall_buffer_release(rt, u, 77), NOT_FOUND);
        // Lease arguments.
        let mut ser = 0;
        assert_eq!(aster_lease_serializer_memory_new(rt, 0, &mut ser), OK);
        assert_eq!(
            aster_lease_serializer_foreign_new(rt, 424242, &mut x),
            NOT_FOUND
        );
        assert_eq!(
            aster_lease_serializer_memory_new(rt, 424242, &mut x),
            NOT_FOUND
        );
        let mut op = 0;
        let acq = |ttl, renew, margin, op: &mut u64| {
            aster_lease_acquire(rt, ser, bytes(b"r"), bytes(b"a"), ttl, renew, margin, 0, op)
        };
        assert_eq!(acq(0, 5, 0, &mut op), INVALID);
        assert_eq!(acq(1000, 0, 1000, &mut op), INVALID);
        assert_eq!(acq(1000, 900, 200, &mut op), INVALID);
        assert_eq!(acq(1000, 0, 0, &mut op), OK);
        let g = finish(rt, op);
        assert_ne!(g.ev.handle, 0);
        assert_eq!(
            aster_lease_acquire(rt, ser, bytes(b""), bytes(b"a"), 0, 0, 0, 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_lease_acquire(rt, 424242, bytes(b"r"), bytes(b"a"), 0, 0, 0, 0, &mut op),
            NOT_FOUND
        );
        let mut epoch = 0;
        assert_eq!(aster_lease_fence(rt, g.ev.handle, ptr::null_mut()), INVALID);
        assert_eq!(aster_lease_fence(rt, 424242, &mut epoch), NOT_FOUND);
        assert_eq!(aster_lease_free(rt, g.ev.handle), OK);
        // Fenced / topology.
        assert_eq!(
            aster_fenced_resource_foreign_new(rt, 424242, &mut x),
            NOT_FOUND
        );
        assert_eq!(
            aster_topology_admitted_new(rt, 424242, 0, 0, 0, &mut x),
            NOT_FOUND
        );
        let mut st = aster_topology_admitted_stats_t::default();
        assert_eq!(
            aster_topology_admitted_stats(rt, 424242, &mut st),
            NOT_FOUND
        );
        let secret = [7u8; 32];
        assert_eq!(
            aster_topology_enable_shared(rt, 424242, secret.as_ptr(), 0, list(&[]), 0, 0, &mut op),
            NOT_FOUND
        );
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

unsafe fn start_node(rt: aster_runtime_t, monitoring: bool) -> aster_node_t {
    use aster_transport_ffi::node::{aster_config_init, aster_config_t, aster_node_start};
    let bind = b"127.0.0.1:0";
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(bind);
    cfg.enable_monitoring = u32::from(monitoring);
    let mut op = 0;
    assert_eq!(aster_node_start(rt, &cfg, 0, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_NODE_CREATED)
        .handle
}

#[test]
fn topology_enable_shared_installs_the_foreign_filter() {
    unsafe {
        let rt = runtime();
        let u = new_upcalls(rt);
        let _foreign = Foreign::start(rt, u, |_, payload| {
            let ids = codec::decode_topology_admitted_request(payload).unwrap();
            Reply::Answer(
                S_OK,
                codec::encode_topology_admitted_response(&vec![true; ids.len()]),
            )
        });
        let mut f = 0;
        assert_eq!(aster_topology_admitted_new(rt, u, 0, 0, 0, &mut f), OK);
        let secret = [0x42u8; 32];

        // Without monitoring the swarm cannot start.
        let plain = start_node(rt, false);
        let mut op = 0;
        assert_eq!(
            aster_topology_enable_shared(rt, plain, secret.as_ptr(), f, list(&[]), 0, 0, &mut op),
            OK
        );
        let ev = finish(rt, op);
        assert!(
            ev.is_error() && ev.text().contains("monitoring"),
            "{}",
            ev.text()
        );

        let node = start_node(rt, true);
        assert_eq!(
            aster_topology_enable_shared(rt, node, ptr::null(), f, list(&[]), 0, 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_topology_enable_shared(rt, node, secret.as_ptr(), f, list(&[]), 1000, 5, &mut op),
            OK
        );
        let ev = finish(rt, op);
        assert!(
            ev.is_ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT),
            "{}",
            ev.text()
        );
        assert_eq!((ev.ev.handle, ev.ev.user_data), (node, 5));
        assert_eq!(aster_node_close(rt, node, 0, &mut op), OK);
        assert_eq!(aster_node_close(rt, plain, 0, &mut op), OK);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}
