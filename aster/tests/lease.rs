//! The lease design-doc test matrix
//! (`docs/_internal/aster-leases.md`), rows 1–6, 8, 10, plus
//! holder-engine behaviour. Deferred with their backends: row 7 (directory
//! divergence — needs the advisory-row integration) and row 9 (fast clock on
//! the dumb-CAS/S3 path).
//!
//! All tests run under a paused tokio clock: `advance` drives serializer
//! expiry deterministically in the raw-transition tests, and plain `sleep`
//! (auto-advance) interleaves the background renew loop in the engine tests.

use std::sync::Arc;
use std::time::Duration;

use aster::lease::{
    check_fence, revoke, AcquireOutcome, DenyReason, Fence, FenceRejection, LeaseHandle, LeaseOp,
    LeaseOptions, LeaseSerializer, LeaseStatus, MemorySerializer, TransitionOutcome,
};

const TTL: Duration = Duration::from_secs(30);

fn opts() -> LeaseOptions {
    LeaseOptions {
        ttl: TTL,
        renew_interval: Duration::from_secs(10),
        holder_margin: Duration::from_secs(1),
    }
}

async fn acquire_raw(s: &MemorySerializer, who: &str) -> TransitionOutcome {
    s.transition(
        "r",
        LeaseOp::Acquire {
            candidate: who.into(),
            ttl: TTL,
        },
    )
    .await
    .unwrap()
}

fn granted(outcome: TransitionOutcome) -> Fence {
    match outcome {
        TransitionOutcome::Granted(fence) => fence,
        other => panic!("expected grant, got {other:?}"),
    }
}

// Matrix 1 — same-epoch collision: two candidates race ACQUIRE; exactly one
// CAS wins, the loser sees the new row.
#[tokio::test(start_paused = true)]
async fn same_epoch_collision() {
    let s = Arc::new(MemorySerializer::new());
    let (a, b) = tokio::join!(
        LeaseHandle::acquire(s.clone(), "r", "node-a", opts()),
        LeaseHandle::acquire(s.clone(), "r", "node-b", opts()),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    let wins = outcomes
        .iter()
        .filter(|o| matches!(o, AcquireOutcome::Granted(_)))
        .count();
    assert_eq!(wins, 1, "exactly one candidate must win the CAS");
    for o in &outcomes {
        if let AcquireOutcome::Denied(reason, snap) = o {
            assert_eq!(*reason, DenyReason::HeldByOther);
            assert_eq!(snap.epoch, 1);
            assert!(snap.held);
        }
    }
}

// Matrix 2 — equal-epoch after release: release advances the fence, so the
// releaser's own in-flight epoch-N write is rejected afterwards.
#[tokio::test(start_paused = true)]
async fn equal_epoch_after_release_is_fenced() {
    let s = MemorySerializer::new();
    let fence = granted(acquire_raw(&s, "node-a").await);
    assert_eq!(fence.epoch, 1);
    assert!(s.mutate_fenced(&fence, "node-a", || ()).is_ok());

    let released = s
        .transition(
            "r",
            LeaseOp::Release {
                holder: "node-a".into(),
                epoch: fence.epoch,
            },
        )
        .await
        .unwrap();
    assert!(matches!(released, TransitionOutcome::Released));

    // The "in-flight" write lands after the release: epoch 1 < 2.
    assert_eq!(
        s.mutate_fenced(&fence, "node-a", || ()).unwrap_err(),
        FenceRejection::StaleEpoch {
            carried: 1,
            current: 2
        }
    );
}

// Matrix 3 — equal-epoch replay: the current epoch stamped by an admitted
// non-holder is rejected; the fence is not a bearer token.
#[tokio::test(start_paused = true)]
async fn equal_epoch_replay_by_non_holder() {
    let s = MemorySerializer::new();
    let fence = granted(acquire_raw(&s, "node-a").await);
    // node-b saw the epoch (directory is readable) and stamps it — but the
    // writer identity is authenticated by the transport, not the request.
    assert_eq!(
        s.mutate_fenced(&fence, "node-b", || ()).unwrap_err(),
        FenceRejection::NotHolder
    );
}

// Matrix 4 — paused-holder resurrection: holder pauses past ttl, a successor
// takes over (ACQUIRE under the expired predicate), the old holder's writes
// and renewals are rejected.
#[tokio::test(start_paused = true)]
async fn paused_holder_is_fenced_after_takeover() {
    let s = MemorySerializer::new();
    let old = granted(acquire_raw(&s, "node-a").await);

    tokio::time::advance(TTL + Duration::from_secs(1)).await;
    let new = granted(acquire_raw(&s, "node-b").await);
    assert_eq!(new.epoch, old.epoch + 1);

    assert_eq!(
        s.mutate_fenced(&old, "node-a", || ()).unwrap_err(),
        FenceRejection::StaleEpoch {
            carried: 1,
            current: 2
        }
    );
    let renew = s
        .transition(
            "r",
            LeaseOp::Renew {
                holder: "node-a".into(),
                epoch: old.epoch,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        renew,
        TransitionOutcome::Denied(DenyReason::EpochMismatch, _)
    ));
}

// Matrix 5 — renew/takeover race at the expiry boundary: both CAS at the
// serializer; exactly one succeeds, and the row stays consistent.
#[tokio::test(start_paused = true)]
async fn renew_takeover_race_at_expiry() {
    let s = MemorySerializer::new();
    let fence = granted(acquire_raw(&s, "node-a").await);
    tokio::time::advance(TTL).await; // exactly expired

    let (renew, steal) = tokio::join!(
        s.transition(
            "r",
            LeaseOp::Renew {
                holder: "node-a".into(),
                epoch: fence.epoch,
            },
        ),
        acquire_raw(&s, "node-b"),
    );
    let renew_ok = matches!(renew.unwrap(), TransitionOutcome::Renewed);
    let steal_ok = matches!(steal, TransitionOutcome::Granted(_));
    assert!(
        renew_ok != steal_ok,
        "exactly one of renew/takeover may win at the boundary"
    );
    let snap = s.snapshot("r").await.unwrap();
    assert!(snap.held);
    let expected = if steal_ok {
        ("node-b", 2)
    } else {
        ("node-a", 1)
    };
    assert_eq!(snap.holder.as_deref(), Some(expected.0));
    assert_eq!(snap.epoch, expected.1);
}

// Matrix 6 — serializer restart: durable fence fields survive, expiry
// re-arms to a full ttl (never advances), stale old-epoch writes stay
// rejected, and the current holder's same-epoch write is still accepted.
#[tokio::test(start_paused = true)]
async fn restart_rearms_ttl_without_advancing_fence() {
    let s = MemorySerializer::new();
    let stale = granted(acquire_raw(&s, "node-a").await);
    s.transition(
        "r",
        LeaseOp::Release {
            holder: "node-a".into(),
            epoch: stale.epoch,
        },
    )
    .await
    .unwrap();
    let current = granted(acquire_raw(&s, "node-b").await);
    assert_eq!(current.epoch, 3);

    // Crash just before the lease would have expired, then restart.
    tokio::time::advance(TTL - Duration::from_secs(1)).await;
    s.restart();

    // Fence not advanced: the current holder's same-epoch write still passes;
    // the pre-crash stale epoch is still rejected.
    assert!(s.mutate_fenced(&current, "node-b", || ()).is_ok());
    assert!(matches!(
        s.mutate_fenced(&stale, "node-a", || ()).unwrap_err(),
        FenceRejection::StaleEpoch { .. }
    ));

    // Expiry re-armed to a FULL ttl: without the re-arm this advance would
    // cross the original deadline and permit a takeover.
    tokio::time::advance(TTL - Duration::from_secs(1)).await;
    let steal = acquire_raw(&s, "node-c").await;
    assert!(
        matches!(steal, TransitionOutcome::Denied(DenyReason::HeldByOther, _)),
        "takeover before the re-armed deadline must be denied, got {steal:?}"
    );
    // …and after the re-armed deadline the takeover proceeds (the dead
    // holder's replacement is delayed by at most one extra ttl).
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        acquire_raw(&s, "node-c").await,
        TransitionOutcome::Granted(_)
    ));
}

// Matrix 8 — no live-steal / ping-pong bound: an eager candidate hammering
// ACQUIRE against a live, renewing holder never wins.
#[tokio::test(start_paused = true)]
async fn live_holder_cannot_be_stolen() {
    let s = Arc::new(MemorySerializer::new());
    let lease = match LeaseHandle::acquire(s.clone(), "r", "node-a", opts())
        .await
        .unwrap()
    {
        AcquireOutcome::Granted(l) => l,
        _ => panic!("grant expected"),
    };

    // Several ttl-lengths of eager stealing while the renew loop runs
    // (paused-clock sleep auto-advances through the renew deadlines).
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let attempt = acquire_raw(&s, "node-b").await;
        assert!(
            matches!(
                attempt,
                TransitionOutcome::Denied(DenyReason::HeldByOther, _)
            ),
            "live holder stolen: {attempt:?}"
        );
    }
    assert_eq!(lease.status(), LeaseStatus::Held);
    assert!(lease.fence().is_some());
    lease.release().await.unwrap();
}

// Matrix 10 — unauthorized ACQUIRE is refused at the serializer.
#[tokio::test(start_paused = true)]
async fn unauthorized_acquire_refused() {
    let s = MemorySerializer::new();
    s.set_authorizer(Arc::new(|op| op.actor() != "node-evil"));

    let denied = acquire_raw(&s, "node-evil").await;
    assert!(matches!(
        denied,
        TransitionOutcome::Denied(DenyReason::Unauthorized, _)
    ));
    // …and the refused attempt burned no epoch.
    let fence = granted(acquire_raw(&s, "node-a").await);
    assert_eq!(fence.epoch, 1);
}

// Engine — the renew loop keeps the lease alive far past several ttls, and
// a graceful release leaves the resource immediately acquirable.
#[tokio::test(start_paused = true)]
async fn renew_loop_keeps_lease_alive() {
    let s = Arc::new(MemorySerializer::new());
    let lease = match LeaseHandle::acquire(s.clone(), "r", "node-a", opts())
        .await
        .unwrap()
    {
        AcquireOutcome::Granted(l) => l,
        _ => panic!("grant expected"),
    };

    tokio::time::sleep(10 * TTL).await;
    assert_eq!(lease.status(), LeaseStatus::Held);
    assert!(lease.fence().is_some());

    lease.release().await.unwrap();
    assert!(matches!(
        acquire_raw(&s, "node-b").await,
        TransitionOutcome::Granted(_)
    ));
}

// Engine — revoke fences the running holder: its next renewal is refused,
// the handle flips to Fenced, and fence() stops handing out the token.
#[tokio::test(start_paused = true)]
async fn revoke_fences_running_holder() {
    let s = Arc::new(MemorySerializer::new());
    let lease = match LeaseHandle::acquire(s.clone(), "r", "node-a", opts())
        .await
        .unwrap()
    {
        AcquireOutcome::Granted(l) => l,
        _ => panic!("grant expected"),
    };
    let fence = lease.fence().unwrap();

    revoke(s.as_ref(), "r", "authority").await.unwrap();
    // Fence advanced immediately — enforcement rejects before the holder
    // even notices.
    assert!(matches!(
        s.mutate_fenced(&fence, "node-a", || ()).unwrap_err(),
        FenceRejection::StaleEpoch { .. }
    ));

    let mut watch = lease.watch();
    tokio::time::advance(opts().renew_interval).await;
    watch.changed().await.unwrap();
    assert_eq!(lease.status(), LeaseStatus::Fenced);
    assert!(lease.fence().is_none());
}

// Engine — the self-limit: when the serializer becomes unreachable the
// holder stops acting at ttl - margin, before any candidate could have
// taken over serializer-side.
#[tokio::test(start_paused = true)]
async fn self_limit_stops_holder_before_takeover_window() {
    // A serializer whose renewals start failing (backend error, not deny).
    struct FlakySerializer {
        inner: MemorySerializer,
        fail: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl LeaseSerializer for FlakySerializer {
        async fn transition(
            &self,
            resource: &str,
            op: LeaseOp,
        ) -> anyhow::Result<TransitionOutcome> {
            if matches!(op, LeaseOp::Renew { .. })
                && self.fail.load(std::sync::atomic::Ordering::SeqCst)
            {
                anyhow::bail!("serializer unreachable");
            }
            self.inner.transition(resource, op).await
        }
        async fn snapshot(&self, resource: &str) -> anyhow::Result<aster::lease::LeaseSnapshot> {
            self.inner.snapshot(resource).await
        }
    }

    let s = Arc::new(FlakySerializer {
        inner: MemorySerializer::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let lease = match LeaseHandle::acquire(s.clone(), "r", "node-a", opts())
        .await
        .unwrap()
    {
        AcquireOutcome::Granted(l) => l,
        _ => panic!("grant expected"),
    };

    s.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut watch = lease.watch();

    // Holder must stop handing out the fence at ttl - margin (t=29)…
    tokio::time::sleep(TTL - Duration::from_secs(1)).await;
    assert!(lease.fence().is_none());
    // …which is before the serializer-side ttl: at this instant no
    // candidate can take over yet, so there is never a window where a
    // successor holds a grant while the old holder still acts.
    assert!(matches!(
        acquire_raw(&s.inner, "node-b").await,
        TransitionOutcome::Denied(DenyReason::HeldByOther, _)
    ));
    let snap = s.snapshot("r").await.unwrap();
    assert!(snap.remaining.unwrap() > Duration::ZERO);

    // The loop notices on its next (failing) renew attempt and flips Lost.
    watch.changed().await.unwrap();
    assert_eq!(lease.status(), LeaseStatus::Lost);
}

// Fence sanity — a fence forged with a future epoch trips the integrity
// alarm rather than being accepted or reported as merely stale.
#[tokio::test(start_paused = true)]
async fn future_epoch_is_integrity_alarm() {
    let s = MemorySerializer::new();
    let fence = granted(acquire_raw(&s, "node-a").await);
    let forged = Fence {
        epoch: fence.epoch + 7,
        ..fence
    };
    assert_eq!(
        s.mutate_fenced(&forged, "node-a", || ()).unwrap_err(),
        FenceRejection::FutureEpoch {
            carried: 8,
            current: 1
        }
    );
    // check_fence agrees when evaluated against the same row state.
    let snap = s.snapshot("r").await.unwrap();
    let row = aster::lease::RowState {
        epoch: snap.epoch,
        holder: snap.holder,
        held: snap.held,
        ttl: TTL,
    };
    assert!(matches!(
        check_fence(&row, &forged, "node-a"),
        Err(FenceRejection::FutureEpoch { .. })
    ));
}
