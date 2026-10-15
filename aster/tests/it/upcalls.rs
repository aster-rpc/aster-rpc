#![cfg(feature = "binding-support")]
//! R6 upcalls (`aster::binding_support::upcall`): every adapter answered by
//! an in-process "foreign" consumer thread that drains an `UpcallQueue` the
//! way a C ABI poll thread would, plus the mechanism's timeout, failure
//! mode, cancellation, ordering and unknown-id rules.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use aster::binding_support::upcall::codec::{self, HandshakeDecision, TopologyViewParts};
use aster::binding_support::upcall::lease::{
    AuthorizedSerializer, ForeignFencedResource, ForeignLeaseAuthorizer, ForeignLeaseSerializer,
};
use aster::binding_support::upcall::topology::{
    AdmittedCacheOptions, ForeignAdmittedFn, ForeignTopologySource,
};
use aster::binding_support::upcall::{
    admission, CancelReason, HookFailureMode, RespondError, UpcallError, UpcallEvent, UpcallKind,
    UpcallPolicy, UpcallQueue, UpcallStatus, Upcalls,
};
use aster::lease::{
    check_fence, transition, AcquireOutcome, DenyReason, Fence, FenceRejection, FencedResource,
    LeaseHandle, LeaseOp, LeaseOptions, LeaseSerializer, LeaseSnapshot, MemorySerializer, RowState,
    Transition, TransitionOutcome,
};
use aster::topology::{ClusterView, SeparationVerdict};
use aster::NodeId;

// ── the "foreign" side ─────────────────────────────────────────────────────

type Answer = Option<(UpcallStatus, Vec<u8>)>;

/// A poll thread draining the queue and answering through `respond`, like a
/// binding's event loop. Records every event it saw (cancels included).
struct Foreign {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<UpcallEvent>>>,
    thread: Option<JoinHandle<()>>,
}

impl Foreign {
    fn start(
        queue: Arc<UpcallQueue>,
        upcalls: Upcalls,
        mut handler: impl FnMut(&UpcallEvent) -> Answer + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s, v) = (stop.clone(), seen.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                if !queue.wait_blocking(Duration::from_millis(10)) {
                    continue;
                }
                queue.drain(64, |ev| {
                    v.lock().unwrap().push(ev.clone());
                    if ev.kind == UpcallKind::Cancel {
                        return;
                    }
                    if let Some((status, payload)) = handler(&ev) {
                        let _ = upcalls.respond(ev.invocation_id, status, payload);
                    }
                });
            }
        });
        Self {
            stop,
            seen,
            thread: Some(thread),
        }
    }

    fn seen(&self) -> Vec<UpcallEvent> {
        self.seen.lock().unwrap().clone()
    }

    fn cancels(&self) -> Vec<(u64, Option<CancelReason>)> {
        self.seen()
            .into_iter()
            .filter(|e| e.kind == UpcallKind::Cancel)
            .map(|e| (e.invocation_id, e.cancel_reason()))
            .collect()
    }
}

impl Drop for Foreign {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn setup(capacity: usize) -> (Arc<UpcallQueue>, Upcalls) {
    let queue = UpcallQueue::new(capacity);
    let upcalls = Upcalls::new(queue.clone());
    (queue, upcalls)
}

fn ok(bytes: Vec<u8>) -> Answer {
    Some((UpcallStatus::Ok, bytes))
}

fn short(upcalls: &Upcalls, kind: UpcallKind, mode: HookFailureMode) {
    upcalls.set_policy(kind, UpcallPolicy::new(Duration::from_millis(100), mode));
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A foreign lease store: rows in a map, the pure `transition` /
/// `check_fence` rules applied under its own lock -- what a Python or JS
/// backend does. No expiry clock (rows never expire).
#[derive(Default)]
struct ForeignStore {
    rows: HashMap<String, RowState>,
}

impl ForeignStore {
    fn snapshot(&self, resource: &str) -> LeaseSnapshot {
        let row = self
            .rows
            .get(resource)
            .cloned()
            .unwrap_or_else(RowState::initial);
        LeaseSnapshot {
            resource: resource.into(),
            epoch: row.epoch,
            holder: row.holder,
            held: row.held,
            remaining: row.held.then_some(row.ttl),
        }
    }

    fn handle(&mut self, ev: &UpcallEvent) -> Answer {
        match ev.kind {
            UpcallKind::LeaseTransition => {
                let (resource, op) = codec::decode_lease_transition_request(&ev.payload).unwrap();
                let row = self
                    .rows
                    .get(&resource)
                    .cloned()
                    .unwrap_or_else(RowState::initial);
                let outcome = match transition(&row, false, &op, &resource) {
                    Transition::Apply { next, outcome, .. } => {
                        self.rows.insert(resource, next);
                        outcome
                    }
                    Transition::Deny(reason) => {
                        TransitionOutcome::Denied(reason, self.snapshot(&resource))
                    }
                };
                ok(codec::encode_transition_outcome(&outcome))
            }
            UpcallKind::LeaseSnapshot => {
                let resource = codec::decode_lease_snapshot_request(&ev.payload).unwrap();
                ok(codec::encode_lease_snapshot(&self.snapshot(&resource)))
            }
            UpcallKind::FencedMutate => {
                let (fence, writer, op) = codec::decode_fenced_mutate_request(&ev.payload).unwrap();
                if op == b"explode" {
                    return Some((UpcallStatus::Error, b"disk on fire".to_vec()));
                }
                let row = self
                    .rows
                    .get(&fence.resource)
                    .cloned()
                    .unwrap_or_else(RowState::initial);
                let res = check_fence(&row, &fence, &writer).map(|()| {
                    let mut out = op.clone();
                    out.reverse();
                    out
                });
                ok(codec::encode_fenced_mutate_response(&res))
            }
            _ => Some((UpcallStatus::Unimplemented, Vec::new())),
        }
    }
}

fn foreign_store(queue: Arc<UpcallQueue>, upcalls: Upcalls) -> Foreign {
    let mut store = ForeignStore::default();
    Foreign::start(queue, upcalls, move |ev| store.handle(ev))
}

// ── leases ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn lease_serializer_runs_on_a_foreign_store() {
    let (queue, upcalls) = setup(64);
    let _foreign = foreign_store(queue, upcalls.clone());
    let serializer = Arc::new(ForeignLeaseSerializer::new(upcalls.clone()));

    let lease = match LeaseHandle::acquire(
        serializer.clone(),
        "tree/42",
        "node-a",
        LeaseOptions::default(),
    )
    .await
    .unwrap()
    {
        AcquireOutcome::Granted(l) => l,
        AcquireOutcome::Denied(r, _) => panic!("denied: {r:?}"),
    };
    assert_eq!(lease.fence().unwrap().epoch, 1);

    // A second candidate is refused by the foreign predicate, with the
    // foreign snapshot.
    match LeaseHandle::acquire(
        serializer.clone(),
        "tree/42",
        "node-b",
        LeaseOptions::default(),
    )
    .await
    .unwrap()
    {
        AcquireOutcome::Denied(DenyReason::HeldByOther, snap) => {
            assert_eq!(snap.holder.as_deref(), Some("node-a"));
            assert_eq!(snap.epoch, 1);
        }
        _ => panic!("expected HeldByOther"),
    }

    lease.release().await.unwrap();
    let snap = serializer.snapshot("tree/42").await.unwrap();
    assert!(!snap.held);
    assert_eq!(snap.epoch, 2, "release advances the fence");
    assert_eq!(upcalls.pending_count(), 0);
}

#[tokio::test]
async fn fenced_resource_applies_and_rejects_in_foreign_code() {
    let (queue, upcalls) = setup(64);
    let _foreign = foreign_store(queue, upcalls.clone());
    let serializer = ForeignLeaseSerializer::new(upcalls.clone());
    let resource = ForeignFencedResource::new(upcalls.clone());

    let fence = match serializer
        .transition(
            "r",
            LeaseOp::Acquire {
                candidate: "node-a".into(),
                ttl: Duration::from_secs(10),
            },
        )
        .await
        .unwrap()
    {
        TransitionOutcome::Granted(f) => f,
        other => panic!("{other:?}"),
    };

    // Current fence from the holder: applied, the foreign output returned.
    let out = resource
        .mutate(&fence, "node-a", b"abc".to_vec())
        .await
        .unwrap();
    assert_eq!(out.unwrap(), b"cba");

    // Not the holder.
    assert_eq!(
        resource.mutate(&fence, "node-z", b"x".to_vec()).await,
        Err(FenceRejection::NotHolder)
    );

    // Stale after release.
    serializer
        .transition(
            "r",
            LeaseOp::Release {
                holder: "node-a".into(),
                epoch: fence.epoch,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        resource.mutate(&fence, "node-a", b"x".to_vec()).await,
        Err(FenceRejection::StaleEpoch {
            carried: 1,
            current: 2
        })
    );

    // A foreign failure is an unknown outcome, not a rejection.
    let fence2 = Fence {
        resource: "r".into(),
        epoch: 2,
        holder: "node-a".into(),
    };
    match resource
        .mutate(&fence2, "node-a", b"explode".to_vec())
        .await
    {
        Ok(Err(UpcallError::Foreign(msg))) => assert_eq!(msg, "disk on fire"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn fenced_mutation_without_an_answer_times_out_as_unknown() {
    let (queue, upcalls) = setup(64);
    short(
        &upcalls,
        UpcallKind::FencedMutate,
        HookFailureMode::FailOpen,
    );
    let foreign = Foreign::start(queue, upcalls.clone(), |_| None);
    let resource = ForeignFencedResource::new(upcalls.clone());
    let fence = Fence {
        resource: "r".into(),
        epoch: 1,
        holder: "a".into(),
    };
    // Failure modes never apply to a write.
    assert_eq!(
        resource.mutate(&fence, "a", vec![1]).await,
        Ok(Err(UpcallError::Timeout))
    );
    eventually("cancel", || {
        foreign.cancels() == vec![(1, Some(CancelReason::Timeout))]
    })
    .await;
}

fn deny_evil(ev: &UpcallEvent) -> Answer {
    assert_eq!(ev.kind, UpcallKind::LeaseAuthorize);
    let op = codec::decode_lease_authorize_request(&ev.payload).unwrap();
    ok(codec::encode_decision(op.actor() != "node-evil"))
}

async fn acquire(s: &dyn LeaseSerializer, who: &str) -> TransitionOutcome {
    s.transition(
        "res",
        LeaseOp::Acquire {
            candidate: who.into(),
            ttl: Duration::from_secs(10),
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_authorizer_as_memory_serializer_hook() {
    let (queue, upcalls) = setup(64);
    let _foreign = Foreign::start(queue, upcalls.clone(), deny_evil);
    let s = MemorySerializer::new();
    s.set_authorizer(ForeignLeaseAuthorizer::new(upcalls.clone()).authorizer());

    assert!(matches!(
        acquire(&s, "node-evil").await,
        TransitionOutcome::Denied(DenyReason::Unauthorized, _)
    ));
    assert!(matches!(
        acquire(&s, "node-a").await,
        TransitionOutcome::Granted(Fence { epoch: 1, .. })
    ));
}

#[test]
fn lease_authorizer_blocking_on_a_current_thread_runtime() {
    // The queue needs no runtime to deliver, so blocking a current-thread
    // runtime for the answer cannot deadlock.
    let (queue, upcalls) = setup(64);
    let _foreign = Foreign::start(queue, upcalls.clone(), deny_evil);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let s = MemorySerializer::new();
        s.set_authorizer(ForeignLeaseAuthorizer::new(upcalls.clone()).authorizer());
        assert!(matches!(
            acquire(&s, "node-evil").await,
            TransitionOutcome::Denied(DenyReason::Unauthorized, _)
        ));
        assert!(matches!(
            acquire(&s, "node-b").await,
            TransitionOutcome::Granted(_)
        ));
    });
}

#[tokio::test]
async fn authorized_serializer_asks_asynchronously() {
    let (queue, upcalls) = setup(64);
    let _foreign = Foreign::start(queue, upcalls.clone(), deny_evil);
    let s = AuthorizedSerializer::new(
        MemorySerializer::new(),
        ForeignLeaseAuthorizer::new(upcalls.clone()),
    );
    match acquire(&s, "node-evil").await {
        TransitionOutcome::Denied(DenyReason::Unauthorized, snap) => {
            assert_eq!(snap.epoch, 0, "a refused attempt burns no epoch")
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        acquire(&s, "node-a").await,
        TransitionOutcome::Granted(Fence { epoch: 1, .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorizer_timeout_applies_the_failure_mode() {
    let (queue, upcalls) = setup(64);
    let foreign = Foreign::start(queue, upcalls.clone(), |_| None); // never answers
    let auth = ForeignLeaseAuthorizer::new(upcalls.clone());
    let op = LeaseOp::Revoke {
        authority: "x".into(),
    };

    short(
        &upcalls,
        UpcallKind::LeaseAuthorize,
        HookFailureMode::FailClosed,
    );
    assert!(!auth.authorize(&op).await);
    assert!(!auth.authorize_blocking(&op));

    short(
        &upcalls,
        UpcallKind::LeaseAuthorize,
        HookFailureMode::FailOpen,
    );
    assert!(auth.authorize(&op).await);
    assert!(auth.authorize_blocking(&op));

    // Every timed-out upcall was cancelled towards the binding.
    eventually("four cancels", || foreign.cancels().len() == 4).await;
    assert!(foreign
        .cancels()
        .iter()
        .all(|(_, r)| *r == Some(CancelReason::Timeout)));
    assert_eq!(upcalls.pending_count(), 0);
}

// ── mechanism ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn late_respond_after_timeout_is_an_unknown_invocation() {
    let (queue, upcalls) = setup(64);
    short(
        &upcalls,
        UpcallKind::LeaseSnapshot,
        HookFailureMode::FailClosed,
    );
    let err = upcalls
        .call(
            UpcallKind::LeaseSnapshot,
            codec::encode_lease_snapshot_request("r"),
        )
        .await
        .unwrap_err();
    assert_eq!(err, UpcallError::Timeout);
    // Never drained: the cancel withdrew the queued event instead of
    // queueing a Cancel.
    assert!(queue.is_empty());
    assert_eq!(
        upcalls.respond(1, UpcallStatus::Ok, vec![]),
        Err(RespondError::UnknownInvocation(1))
    );
}

#[tokio::test]
async fn dropping_the_waiter_cancels_the_upcall() {
    let (queue, upcalls) = setup(64);

    // Still queued: withdrawn silently.
    {
        let fut = upcalls.call(
            UpcallKind::LeaseSnapshot,
            codec::encode_lease_snapshot_request("r"),
        );
        let _ = tokio::time::timeout(Duration::from_millis(20), fut).await;
    }
    assert!(queue.is_empty());
    assert_eq!(upcalls.pending_count(), 0);

    // Already taken by the binding: a Cancel{Abandoned} follows.
    let foreign = Foreign::start(queue.clone(), upcalls.clone(), |_| None);
    let u2 = upcalls.clone();
    let task = tokio::spawn(async move {
        u2.call(
            UpcallKind::LeaseSnapshot,
            codec::encode_lease_snapshot_request("r"),
        )
        .await
    });
    eventually("delivery", || {
        foreign
            .seen()
            .iter()
            .any(|e| e.kind == UpcallKind::LeaseSnapshot)
    })
    .await;
    task.abort();
    let _ = task.await;
    eventually("abandon cancel", || {
        foreign.cancels() == vec![(2, Some(CancelReason::Abandoned))]
    })
    .await;
    assert_eq!(upcalls.pending_count(), 0);
}

#[tokio::test]
async fn responses_route_by_id_in_any_order() {
    let (queue, upcalls) = setup(64);
    let calls: Vec<_> = (0..3)
        .map(|i| {
            let u = upcalls.clone();
            tokio::spawn(async move {
                u.call(
                    UpcallKind::LeaseSnapshot,
                    codec::encode_lease_snapshot_request(&format!("r{i}")),
                )
                .await
            })
        })
        .collect();
    eventually("three queued", || queue.len() == 3).await;
    let mut events = Vec::new();
    queue.drain(16, |e| events.push(e));
    // Answer newest first, echoing the resource so each waiter can check
    // it got its own answer.
    for ev in events.iter().rev() {
        let resource = codec::decode_lease_snapshot_request(&ev.payload).unwrap();
        let snap = LeaseSnapshot {
            resource,
            epoch: ev.invocation_id,
            holder: None,
            held: false,
            remaining: None,
        };
        upcalls
            .respond(
                ev.invocation_id,
                UpcallStatus::Ok,
                codec::encode_lease_snapshot(&snap),
            )
            .unwrap();
    }
    for (i, c) in calls.into_iter().enumerate() {
        let bytes = c.await.unwrap().unwrap();
        let snap = codec::decode_lease_snapshot(&bytes).unwrap();
        assert_eq!(snap.resource, format!("r{i}"));
    }
    // Answering again: already finished.
    assert_eq!(
        upcalls.respond(events[0].invocation_id, UpcallStatus::Ok, vec![]),
        Err(RespondError::UnknownInvocation(events[0].invocation_id))
    );
}

#[tokio::test]
async fn unknown_invocation_ids_are_refused() {
    let (_queue, upcalls) = setup(4);
    for id in [0, 7, u64::MAX] {
        assert_eq!(
            upcalls.respond(id, UpcallStatus::Ok, b"x".to_vec()),
            Err(RespondError::UnknownInvocation(id))
        );
    }
}

#[tokio::test]
async fn a_full_queue_refuses_upcalls_and_decisions_fall_back() {
    let (queue, upcalls) = setup(1);
    let u = upcalls.clone();
    let first = tokio::spawn(async move {
        u.call(
            UpcallKind::LeaseSnapshot,
            codec::encode_lease_snapshot_request("r"),
        )
        .await
    });
    eventually("first queued", || queue.len() == 1).await;
    assert_eq!(
        upcalls
            .call(
                UpcallKind::LeaseSnapshot,
                codec::encode_lease_snapshot_request("r")
            )
            .await,
        Err(UpcallError::Overloaded)
    );
    let auth = ForeignLeaseAuthorizer::new(upcalls.clone());
    let op = LeaseOp::Revoke {
        authority: "x".into(),
    };
    assert!(!auth.authorize(&op).await, "fail-closed default");
    short(
        &upcalls,
        UpcallKind::LeaseAuthorize,
        HookFailureMode::FailOpen,
    );
    assert!(auth.authorize(&op).await);
    first.abort();
}

#[tokio::test]
async fn close_fails_pending_and_later_upcalls() {
    let (queue, upcalls) = setup(8);
    let u = upcalls.clone();
    let pending = tokio::spawn(async move {
        u.call(
            UpcallKind::LeaseSnapshot,
            codec::encode_lease_snapshot_request("r"),
        )
        .await
    });
    eventually("queued", || queue.len() == 1).await;
    upcalls.close();
    assert_eq!(pending.await.unwrap(), Err(UpcallError::Closed));
    assert_eq!(
        upcalls
            .call(
                UpcallKind::LeaseSnapshot,
                codec::encode_lease_snapshot_request("r")
            )
            .await,
        Err(UpcallError::Closed)
    );
    assert!(upcalls.is_closed());
    assert_eq!(upcalls.pending_count(), 0);
}

#[tokio::test]
async fn foreign_errors_unimplemented_and_garbage_answers() {
    let (queue, upcalls) = setup(8);
    let _foreign = Foreign::start(queue, upcalls.clone(), |ev| {
        let op = codec::decode_lease_authorize_request(&ev.payload).unwrap();
        match op.actor() {
            "err" => Some((UpcallStatus::Error, b"boom".to_vec())),
            "unimpl" => Some((UpcallStatus::from_code(2), vec![])),
            "garbage" => ok(vec![0xFF, 0x01]),
            "unknown-status" => Some((UpcallStatus::from_code(99), b"?".to_vec())),
            _ => ok(codec::encode_decision(true)),
        }
    });
    let ask = |who: &str| {
        upcalls.call(
            UpcallKind::LeaseAuthorize,
            codec::encode_lease_authorize_request(&LeaseOp::Revoke {
                authority: who.into(),
            }),
        )
    };
    assert_eq!(ask("err").await, Err(UpcallError::Foreign("boom".into())));
    assert_eq!(
        ask("unimpl").await,
        Err(UpcallError::Unimplemented(UpcallKind::LeaseAuthorize))
    );
    assert_eq!(
        ask("unknown-status").await,
        Err(UpcallError::Foreign("?".into()))
    );

    let auth = ForeignLeaseAuthorizer::new(upcalls.clone());
    let op = |who: &str| LeaseOp::Revoke {
        authority: who.into(),
    };
    for who in ["err", "unimpl", "garbage"] {
        assert!(!auth.authorize(&op(who)).await, "{who}: fail closed");
    }
    assert!(auth.authorize(&op("fine")).await);
    short(
        &upcalls,
        UpcallKind::LeaseAuthorize,
        HookFailureMode::FailOpen,
    );
    assert!(auth.authorize(&op("garbage")).await, "fail open");
}

#[test]
fn kind_and_status_codes_are_stable() {
    for k in UpcallKind::ALL {
        assert_eq!(UpcallKind::from_code(k.code()), Some(k));
    }
    assert_eq!(UpcallKind::from_code(1000), None);
    assert_eq!(UpcallKind::LeaseTransition.code(), 1);
    assert_eq!(UpcallKind::TopologyAdmitted.code(), 16);
    assert_eq!(UpcallKind::PlacementProvision.code(), 33);
    assert_eq!(UpcallKind::AdmissionHandshake.code(), 65);
    assert!(UpcallKind::TopologyAdmitted.is_decision());
    assert!(!UpcallKind::FencedMutate.is_decision());
    assert_eq!(UpcallStatus::from_code(0), UpcallStatus::Ok);
    assert_eq!(UpcallStatus::from_code(7), UpcallStatus::Error);
}

// ── topology ───────────────────────────────────────────────────────────────

fn hex(i: u8) -> String {
    format!("{:02x}", i).repeat(32)
}

#[tokio::test]
async fn admitted_filter_caches_batches_and_falls_back() {
    let (queue, upcalls) = setup(64);
    let batches = Arc::new(Mutex::new(Vec::<usize>::new()));
    let b = batches.clone();
    let _foreign = Foreign::start(queue, upcalls.clone(), move |ev| {
        let ids = codec::decode_topology_admitted_request(&ev.payload).unwrap();
        b.lock().unwrap().push(ids.len());
        // Even first byte -> admitted.
        let v: Vec<bool> = ids
            .iter()
            .map(|id| u8::from_str_radix(&id[..2], 16).unwrap() % 2 == 0)
            .collect();
        ok(codec::encode_topology_admitted_response(&v))
    });
    let filter = ForeignAdmittedFn::new(upcalls.clone(), AdmittedCacheOptions::default());
    let f = filter.admitted_fn();

    // First read: every id misses and gets the fail-closed answer.
    let ids: Vec<String> = (0..40).map(hex).collect();
    assert!(ids.iter().all(|id| !f(id)));
    assert_eq!(filter.stats().misses, 40);

    // The refresher resolves them in few batched upcalls.
    eventually("resolved", || filter.stats().upcalls >= 1 && f(&hex(0))).await;
    eventually("all resolved", || {
        ids.iter().enumerate().all(|(i, id)| f(id) == (i % 2 == 0))
    })
    .await;
    let batches = batches.lock().unwrap().clone();
    assert_eq!(batches.iter().sum::<usize>(), 40);
    assert!(batches.len() <= 3, "batched, got {batches:?}");
    assert!(filter.stats().hits >= 40);

    // Revocation pushed by the binding takes effect at once.
    filter.set(&hex(0), false);
    assert!(!f(&hex(0)));
    filter.invalidate(&hex(2));
    assert!(!f(&hex(2)), "miss again -> fallback");
    eventually("re-resolved", || f(&hex(2))).await;
}

/// Lookups that miss while an id's batch is in flight must not ask the
/// binding about it again once that batch is answered.
#[tokio::test]
async fn admitted_filter_does_not_requery_in_flight_ids() {
    let (queue, upcalls) = setup(64);
    let release = Arc::new(AtomicBool::new(false));
    let batches = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let (r, b) = (release.clone(), batches.clone());
    let _foreign = Foreign::start(queue, upcalls.clone(), move |ev| {
        let ids = codec::decode_topology_admitted_request(&ev.payload).unwrap();
        b.lock().unwrap().push(ids.clone());
        // Hold the answer until the test has missed again meanwhile.
        while !r.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(2));
        }
        ok(codec::encode_topology_admitted_response(&vec![
            true;
            ids.len()
        ]))
    });
    let filter = ForeignAdmittedFn::new(upcalls.clone(), AdmittedCacheOptions::default());
    let f = filter.admitted_fn();

    assert!(!f(&hex(1)));
    eventually("batch sent", || batches.lock().unwrap().len() == 1).await;
    for _ in 0..5 {
        assert!(!f(&hex(1)), "in flight -> fallback");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    release.store(true, Ordering::Relaxed);
    eventually("resolved", || f(&hex(1))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*batches.lock().unwrap(), vec![vec![hex(1)]]);
    assert_eq!(filter.stats().upcalls, 1);

    // `invalidate` still forces a fresh upcall.
    filter.invalidate(&hex(1));
    assert!(!f(&hex(1)));
    eventually("re-resolved", || f(&hex(1))).await;
    assert_eq!(batches.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn admitted_filter_prefetch_staleness_and_failures() {
    let (queue, upcalls) = setup(64);
    let fail = Arc::new(AtomicBool::new(false));
    let fl = fail.clone();
    let _foreign = Foreign::start(queue, upcalls.clone(), move |ev| {
        if fl.load(Ordering::Relaxed) {
            return Some((UpcallStatus::Error, b"directory down".to_vec()));
        }
        let ids = codec::decode_topology_admitted_request(&ev.payload).unwrap();
        ok(codec::encode_topology_admitted_response(&vec![
            true;
            ids.len()
        ]))
    });
    let filter = ForeignAdmittedFn::new(
        upcalls.clone(),
        AdmittedCacheOptions {
            ttl: Duration::from_millis(50),
            failure_retry: Duration::from_millis(50),
            max_batch: 4,
        },
    );
    filter.prefetch(&[hex(1), hex(3)]).await.unwrap();
    assert!(filter.is_admitted(&hex(1)));
    assert_eq!(filter.stats().misses, 0);

    // Stale: the cached answer is kept while the binding is down...
    fail.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(filter.is_admitted(&hex(1)));
    assert!(filter.stats().stale >= 1);
    // ...and an unknown id gets the failure-mode answer.
    assert_eq!(
        filter.prefetch(&[hex(5)]).await,
        Err(UpcallError::Foreign("directory down".into()))
    );
    assert!(!filter.is_admitted(&hex(5)));

    // Fail-open policy admits unknown ids while there is no answer.
    short(
        &upcalls,
        UpcallKind::TopologyAdmitted,
        HookFailureMode::FailOpen,
    );
    assert!(filter.is_admitted(&hex(7)));
}

#[tokio::test]
async fn topology_source_builds_an_immutable_snapshot() {
    let (queue, upcalls) = setup(8);
    let (a, b, c) = (hex(0xa), hex(0xb), hex(0xc));
    let parts = TopologyViewParts {
        clusters: vec![
            ClusterView {
                members: vec![a.clone(), b.clone()],
                bridge: a.clone(),
                witnesses: vec![a.clone()],
            },
            ClusterView {
                members: vec![c.clone()],
                bridge: c.clone(),
                witnesses: vec![c.clone()],
            },
        ],
        far_pairs: vec![(c.clone(), a.clone()), (b.clone(), c.clone())],
        separation_coverage: 2,
    };
    let answer = codec::encode_topology_snapshot(&parts);
    let _foreign = Foreign::start(queue, upcalls.clone(), move |ev| {
        codec::decode_topology_snapshot_request(&ev.payload).unwrap();
        ok(answer.clone())
    });
    let snap = ForeignTopologySource::new(upcalls).fetch().await.unwrap();
    assert_eq!(snap.clusters().len(), 2);
    assert_eq!(snap.cluster_index_of_hex(&b), Some(0));
    assert_eq!(snap.separated_hex(&a, &b), SeparationVerdict::Connected);
    assert_eq!(snap.separated_hex(&a, &c), SeparationVerdict::Separated);
    let nid = |h: &str| NodeId::from_hex(h.to_string());
    assert_eq!(
        snap.separated(&nid(&b), &nid(&c)),
        SeparationVerdict::Separated
    );
}

// ── placement ──────────────────────────────────────────────────────────────

#[cfg(feature = "coordination-client")]
mod placement {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use aster::binding_support::upcall::placement::{
        ForeignPlacementPolicySource, ForeignPlacementProvisioner,
    };
    use aster::coordination::client::manager::{
        PlacementPolicySource, PlacementProvisioner, ProvisionRequest, TopologySource,
    };
    use aster::coordination::client::placement::{PlacementCandidate, PlacementPolicy};
    use aster::coordination::contract::{LogIdV1, MemberSeedV1};
    use aster::coordination::ids::{CoordinationGroupId, CoordinationNodeId};

    fn id(b: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([b; 32])
    }

    fn seed(b: u8) -> MemberSeedV1 {
        MemberSeedV1 {
            node_id: vec![b; 32],
            ticket: format!("ticket-{b}"),
            signed_join_permit: Some(vec![b, b]),
        }
    }

    fn policy() -> PlacementPolicy {
        let mut eligible = BTreeMap::new();
        for b in [1u8, 2, 3] {
            eligible.insert(
                id(b),
                PlacementCandidate {
                    node_id: id(b),
                    member_seed: (b != 2).then(|| seed(b)),
                },
            );
        }
        PlacementPolicy {
            revision: 7,
            digest: [9; 32],
            authority_public_key: [8; 32],
            authority_domain: "example.org".into(),
            target_voters: 3,
            target_retained_learners: 1,
            eligible,
            pinned_voters: BTreeSet::from([id(1)]),
            excluded: BTreeSet::from([id(4)]),
            minimum_separated_voter_pairs: 1,
            replacement_grace: Duration::from_secs(300),
            allow_healthy_rebalance: true,
        }
    }

    fn same(a: &PlacementPolicy, b: &PlacementPolicy) -> bool {
        codec::encode_placement_policy(a) == codec::encode_placement_policy(b)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn placement_providers_upcall() {
        let (queue, upcalls) = setup(8);
        let seen_req = Arc::new(Mutex::new(None::<ProvisionRequest>));
        let sr = seen_req.clone();
        let _foreign = Foreign::start(queue, upcalls.clone(), move |ev| match ev.kind {
            UpcallKind::PlacementPolicy => {
                codec::decode_placement_policy_request(&ev.payload).unwrap();
                ok(codec::encode_placement_policy(&policy()))
            }
            UpcallKind::PlacementProvision => {
                let req = codec::decode_provision_request(&ev.payload).unwrap();
                let s = seed(req.node_id.as_bytes()[0]);
                *sr.lock().unwrap() = Some(req);
                ok(codec::encode_member_seed(&s))
            }
            UpcallKind::TopologySnapshot => ok(codec::encode_topology_snapshot(
                &TopologyViewParts::default(),
            )),
            _ => None,
        });

        // Synchronous trait method from inside the runtime.
        let source = ForeignPlacementPolicySource::new(upcalls.clone());
        assert!(same(&source.policy().unwrap(), &policy()));
        assert!(same(&source.fetch().await.unwrap(), &policy()));

        let request = ProvisionRequest {
            group_id: CoordinationGroupId::from_array([5; 32]),
            policy_digest: [9; 32],
            membership_log_id: Some(LogIdV1 {
                term: 3,
                leader_node_id: vec![1; 32],
                index: 44,
            }),
            node_id: id(3),
        };
        let got = ForeignPlacementProvisioner::new(upcalls.clone())
            .provision(&request)
            .await
            .unwrap();
        assert_eq!(got, seed(3));
        let echoed = seen_req.lock().unwrap().clone().unwrap();
        assert_eq!(
            codec::encode_provision_request(&echoed),
            codec::encode_provision_request(&request)
        );

        let snap = TopologySource::snapshot(&ForeignTopologySource::new(upcalls.clone()))
            .await
            .unwrap();
        assert!(snap.clusters().is_empty());
    }

    #[tokio::test]
    async fn placement_failures_are_errors() {
        let (queue, upcalls) = setup(8);
        short(
            &upcalls,
            UpcallKind::PlacementProvision,
            HookFailureMode::FailOpen,
        );
        let _foreign = Foreign::start(queue, upcalls.clone(), |ev| match ev.kind {
            UpcallKind::PlacementPolicy => Some((UpcallStatus::Error, b"no policy".to_vec())),
            _ => None,
        });
        let err = ForeignPlacementPolicySource::new(upcalls.clone())
            .fetch()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no policy"), "{err}");
        let request = ProvisionRequest {
            group_id: CoordinationGroupId::from_array([5; 32]),
            policy_digest: [9; 32],
            membership_log_id: None,
            node_id: id(3),
        };
        let err = ForeignPlacementProvisioner::new(upcalls)
            .provision(&request)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}

// ── expose ─────────────────────────────────────────────────────────────────

#[cfg(feature = "expose")]
#[tokio::test]
async fn expose_authorize_upcalls_per_service() {
    use aster::binding_support::upcall::expose::foreign_authorize_fn;
    use aster::expose::PeerContext;

    let (queue, upcalls) = setup(8);
    let _foreign = Foreign::start(queue, upcalls.clone(), |ev| {
        let (service, peer, metadata) =
            codec::decode_expose_authorize_request(&ev.payload).unwrap();
        ok(codec::encode_decision(
            service == "signaling" && peer == "good" && metadata == b"token",
        ))
    });
    let authorize = foreign_authorize_fn(upcalls.clone(), "signaling");
    let ctx = |peer: &str, md: &[u8]| PeerContext {
        peer_id: peer.into(),
        metadata: md.to_vec(),
    };
    assert!(authorize(ctx("good", b"token")).await.is_ok());
    assert!(authorize(ctx("bad", b"token")).await.is_err());
    assert!(authorize(ctx("good", b"nope")).await.is_err());
}

// ── Gate 0 admission ───────────────────────────────────────────────────────

#[test]
fn handshake_decisions_and_their_fallbacks() {
    let (_q, upcalls) = setup(1);
    let reject = HandshakeDecision::Reject {
        code: 451,
        reason: b"geo".to_vec(),
    };
    let bytes = codec::encode_admission_handshake_response(&reject);
    assert_eq!(admission::handshake_decision(&upcalls, Ok(bytes)), reject);
    assert_eq!(
        admission::handshake_decision(
            &upcalls,
            Ok(codec::encode_admission_handshake_response(
                &HandshakeDecision::Accept
            ))
        ),
        HandshakeDecision::Accept
    );
    assert_eq!(
        admission::handshake_decision(&upcalls, Err(UpcallError::Timeout)),
        HandshakeDecision::Reject {
            code: admission::UNAVAILABLE_CODE,
            reason: admission::UNAVAILABLE_REASON.to_vec()
        }
    );
    upcalls.set_policy(
        UpcallKind::AdmissionHandshake,
        UpcallPolicy::new(Duration::from_secs(1), HookFailureMode::FailOpen),
    );
    assert_eq!(
        admission::handshake_decision(&upcalls, Err(UpcallError::Overloaded)),
        HandshakeDecision::Accept
    );
}

#[cfg(feature = "blobs")]
#[tokio::test]
async fn gate0_admission_answered_by_upcall() {
    use aster::{AsterConfig, BlobFormat, Node, RelayMode};

    fn cfg(hooks: bool) -> AsterConfig {
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .hooks(hooks)
            .build()
    }
    async fn wait_for_addr(n: &Node) {
        for _ in 0..50 {
            if !n.addr().direct_addresses.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let server = Node::start(cfg(true)).await.unwrap();
    let client = Node::start(cfg(false)).await.unwrap();
    let client_id = client.id().as_str().to_string();

    let (queue, upcalls) = setup(16);
    let allowed = client_id.clone();
    let foreign = Foreign::start(queue, upcalls.clone(), move |ev| match ev.kind {
        UpcallKind::AdmissionHandshake => {
            let (peer, _alpn, _alive) =
                codec::decode_admission_handshake_request(&ev.payload).unwrap();
            let d = if peer == allowed {
                HandshakeDecision::Accept
            } else {
                HandshakeDecision::Reject {
                    code: 403,
                    reason: b"no".to_vec(),
                }
            };
            ok(codec::encode_admission_handshake_response(&d))
        }
        UpcallKind::AdmissionConnect => ok(codec::encode_decision(true)),
        _ => None,
    });
    let bridge = admission::serve_admission(server.take_admission().unwrap(), upcalls.clone());

    let data = b"admitted by a foreign hook".to_vec();
    let hash = server.blobs().add_bytes(data.clone()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();
    let got = tokio::time::timeout(
        Duration::from_secs(20),
        client
            .blobs()
            .download_hash(&hash, &server.id(), BlobFormat::Raw),
    )
    .await
    .expect("download timed out")
    .expect("admitted peer downloads");
    assert_eq!(got, data);
    assert!(foreign.seen().iter().any(|e| {
        e.kind == UpcallKind::AdmissionHandshake
            && codec::decode_admission_handshake_request(&e.payload)
                .unwrap()
                .0
                == client_id
    }));

    client.shutdown().await;
    server.shutdown().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), bridge).await;
}
