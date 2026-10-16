use super::*;
use std::cell::Cell;
thread_local! {
    pub(crate) static COMMIT_FAILURE: Cell<bool> = const { Cell::new(false) };
    pub(crate) static COMMIT_AFTER_FAILURE: Cell<bool> = const { Cell::new(false) };
    pub(crate) static WRITE_FAILURE: Cell<bool> = const { Cell::new(false) };
    static QUARANTINE_PAUSE: Cell<u8> = const { Cell::new(0) };
    static ACK_PAUSE: Cell<u8> = const { Cell::new(0) };
}
/// Child-process seam: park forever just before (1) or after (2) the
/// acknowledgement transaction commits, so the parent can kill it there.
pub(crate) fn ack_crash_point(stage: u8) {
    if ACK_PAUSE.with(|value| value.get()) != stage {
        return;
    }
    let marker = std::env::var_os("ASTER_AUDIT_CHILD_MARKER").unwrap();
    write_marker(std::path::Path::new(&marker), &[stage]);
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
pub(crate) fn quarantine_crash_point(stage: u8) {
    if QUARANTINE_PAUSE.with(|value| value.get()) != stage {
        return;
    }
    let marker = std::env::var_os("ASTER_AUDIT_CHILD_MARKER").unwrap();
    write_marker(std::path::Path::new(&marker), &[stage]);
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
#[derive(Default)]
pub(crate) struct Hooks {
    pub storage_pause: std::sync::atomic::AtomicBool,
    pub storage_paused: std::sync::atomic::AtomicBool,
    pub storage_exit: std::sync::atomic::AtomicBool,
    pub delivery_exit: std::sync::atomic::AtomicBool,
    pub hold_completions: std::sync::atomic::AtomicBool,
    pub write_failure: std::sync::atomic::AtomicBool,
}

// Every store fixture owns an explicit private root, independent of Docker
// exec's ambient umask. Do not change the shared process umask for this suite.
pub(super) fn private_directory() -> tempfile::TempDir {
    let mut builder = tempfile::Builder::new();
    builder.prefix("aster-audit-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir().expect("private audit test directory")
}

fn event(n: u64) -> AuditEvent {
    AuditEvent {
        timestamp_ms: n,
        kind: "intent".into(),
        payload: serde_json::json!({"n":n}),
    }
}
fn config(path: &std::path::Path) -> OutboxConfig {
    OutboxConfig::new(
        path,
        Destination {
            endpoint: "http://127.0.0.1:9/audit".into(),
            receiver_identity: Some("fixture".into()),
            authorization: None,
        },
    )
}

#[test]
fn opaque_wire_and_restore_safe_ids() {
    let envelope = wire::EnvelopeV1 {
        version: 1,
        outbox_id: "ab".repeat(16),
        sequence: u64::MAX,
        timestamp_ms: 7,
        kind: "intent".into(),
        payload: serde_json::json!({"a":1,"b":2}),
    };
    let bytes = serde_json::to_vec(&envelope).unwrap();
    let id = wire::id_for(&envelope.outbox_id, envelope.sequence, &bytes);
    assert_eq!(id.as_str().len(), 133);
    use base64::Engine;
    let mut event = wire::WireEvent {
        event_id: id.to_string(),
        envelope_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    };
    assert_eq!(
        wire::verify_wire_event(&event, MAX_EVENT_BYTES).unwrap().1,
        bytes
    );
    let mut changed = bytes.clone();
    changed.push(b' ');
    event.envelope_base64 = base64::engine::general_purpose::STANDARD.encode(&changed);
    assert_eq!(
        wire::verify_wire_event(&event, MAX_EVENT_BYTES)
            .unwrap_err()
            .kind(),
        AuditErrorKind::IntegrityConflict
    );
    assert_ne!(
        wire::id_for(&envelope.outbox_id, envelope.sequence, &changed),
        id
    );
    event.envelope_base64 = "?".into();
    assert!(wire::verify_wire_event(&event, MAX_EVENT_BYTES).is_err());
}

#[test]
fn durable_groups_capacity_quarantine_replay_and_receipt_budget() {
    let directory = private_directory();
    let mut config = config(directory.path());
    config.max_retained_bytes = 4096;
    let mut store = store::Store::open(config.clone()).unwrap();
    let ids = store
        .submit_group((1..=4).map(event).collect())
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(store.commits, 1);
    assert_eq!(store.last_group_size, 4);
    drop(store);
    let mut store = store::Store::open(config.clone()).unwrap();
    assert_eq!(store.batch().unwrap().unwrap().events[0].id, ids[0]);
    reject(&mut store, 1);
    store
        .quarantine(ids[0].to_string(), "rejection".into(), "ticket-1".into())
        .unwrap();
    assert_eq!(
        store.flush_result(4).unwrap_err().kind(),
        AuditErrorKind::Disposition
    );
    assert!(!store.export(ids[0].to_string()).unwrap().pending);
    store
        .quarantine(ids[0].to_string(), "again".into(), "ticket-1".into())
        .unwrap();
    assert_eq!(store.export(ids[0].to_string()).unwrap().receipts.len(), 1);
    store.replay(ids[0].to_string()).unwrap();
    assert_eq!(store.batch().unwrap().unwrap().events[0].id, ids[0]);
    assert_eq!(store.export(ids[0].to_string()).unwrap().receipts.len(), 2);
    let generation = store.generation;
    store.acknowledge(generation, ids).unwrap();
    assert!(store.state.receipt_bytes > 0);
    config.max_retained_bytes = store.state.receipt_bytes;
    drop(store);
    let mut store = store::Store::open(config).unwrap();
    assert_eq!(
        store
            .submit_group(vec![event(8)])
            .remove(0)
            .unwrap_err()
            .kind(),
        AuditErrorKind::StorageFull
    );
}
fn reject(store: &mut store::Store, sequence: u64) {
    store
        .delivery_error(
            store.generation,
            DeliveryError {
                kind: DeliveryErrorKind::Rejected,
                http_status: Some(422),
                sequence,
                immediate_flush_failure: true,
                message: "fixture singleton rejected".into(),
            },
            Some(sequence),
            true,
        )
        .unwrap();
}

#[test]
fn commit_failure_fails_entire_group_without_acknowledging() {
    let directory = private_directory();
    let mut store = store::Store::open(config(directory.path())).unwrap();
    COMMIT_FAILURE.with(|f| f.set(true));
    let outcomes = store.submit_group(vec![event(1), event(2)]);
    COMMIT_FAILURE.with(|f| f.set(false));
    assert!(outcomes
        .iter()
        .all(|r| r.as_ref().unwrap_err().kind() == AuditErrorKind::CommitUncertain));
    assert_eq!(store.state.pending_records, 0);
    drop(store);
    let store = store::Store::open(config(directory.path())).unwrap();
    assert_eq!(store.state.pending_records, 0);
}

#[test]
fn delivery_classification_complete_table() {
    use types::DeliveryErrorKind::*;
    for (status, integrity, singleton, expected, isolate, stall, immediate) in [
        (200, false, false, Protocol, false, false, false),
        (401, false, false, Credentials, false, false, false),
        (403, false, false, Credentials, false, false, false),
        (408, false, false, Transport, false, false, false),
        (425, false, false, Transport, false, false, false),
        (429, false, false, Transport, false, false, false),
        (500, false, false, Transport, false, false, false),
        (503, false, false, Transport, false, false, false),
        (413, false, false, Rejected, true, false, false),
        (422, false, true, Rejected, false, true, true),
        (409, true, false, Integrity, true, false, true),
        (409, true, true, Integrity, false, true, true),
        (409, false, false, Configuration, false, true, true),
        (404, false, false, Configuration, false, true, true),
        (302, false, false, Configuration, false, true, true),
    ] {
        let result = worker::classify(status, false, integrity, singleton).unwrap();
        assert_eq!(
            (result.kind, result.isolate, result.stall, result.immediate),
            (expected, isolate, stall, immediate)
        );
    }
    assert!(worker::classify(200, true, false, false).is_none());
}

#[tokio::test]
async fn cloned_handles_outage_and_nonblocking_close_reopen() {
    let directory = private_directory();
    let config = config(directory.path());
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    let clone = outbox.clone();
    let id = clone.submit(event(1)).await.unwrap();
    assert_eq!(outbox.status().await.unwrap().pending_records, 1);
    assert_eq!(outbox.export(id.to_string()).await.unwrap().event_id, id);
    assert_eq!(
        outbox
            .flush(Duration::from_millis(30))
            .await
            .unwrap_err()
            .kind(),
        AuditErrorKind::Timeout
    );
    clone.close().await.unwrap();
    assert_eq!(
        outbox.submit(event(2)).await.unwrap_err().kind(),
        AuditErrorKind::Closed
    );
    let reopened = AuditOutbox::open(config).await.unwrap();
    assert_eq!(reopened.export(id.to_string()).await.unwrap().event_id, id);
    reopened.close().await.unwrap();
}

#[test]
fn only_immediate_storage_constructor_and_no_maintenance_operation() {
    for source in [include_str!("store.rs"), include_str!("worker.rs")] {
        assert!(!source.contains(".begin_write("));
        assert!(!source.contains("Durability::None"));
    }
    assert!(include_str!("../durable_storage.rs")
        .contains("write.set_durability(redb::Durability::Immediate)"));
}

async fn wait_status(
    outbox: &AuditOutbox,
    predicate: impl Fn(&OutboxStatus) -> bool,
) -> OutboxStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = outbox.status().await.unwrap();
        if predicate(&status) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "status condition timed out: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn persistent_receiver_lost_ack_deduplicates_exact_bytes() {
    let directory = private_directory();
    let receiver = fixture::Receiver::start(&directory.path().join("receiver.redb"), 1);
    let outbox = AuditOutbox::open(OutboxConfig::new(
        directory.path().join("outbox"),
        receiver.destination(),
    ))
    .await
    .unwrap();
    let id = outbox.submit(event(1)).await.unwrap();
    outbox.flush(Duration::from_secs(5)).await.unwrap();
    assert_eq!(receiver.count(), 1);
    assert!(receiver.contains(id.as_str()));
    assert!(
        receiver
            .attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|batch| batch.contains(&id.to_string()))
            .count()
            >= 2
    );
    assert_eq!(outbox.status().await.unwrap().pending_records, 0);
    outbox.close().await.unwrap();
}

#[tokio::test]
async fn rejected_batch_isolates_in_order_and_survives_restart_with_remedy() {
    let directory = private_directory();
    let receiver = fixture::Receiver::start(&directory.path().join("receiver.redb"), 2);
    let config = OutboxConfig::new(directory.path().join("outbox"), receiver.destination());
    let mut store = store::Store::open(config.clone()).unwrap();
    let ids = store
        .submit_group(vec![event(1), event(2), event(3)])
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    store.close().unwrap();
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    let status = wait_status(&outbox, |s| s.delivery_state == "stalled").await;
    assert_eq!(status.pending_records, 2);
    assert_eq!(receiver.count(), 1);
    assert!(receiver.contains(ids[0].as_str()));
    assert!(!receiver.contains(ids[2].as_str()));
    assert_eq!(
        outbox
            .flush(Duration::from_secs(5))
            .await
            .unwrap_err()
            .kind(),
        AuditErrorKind::DeliveryStalled
    );
    outbox.close().await.unwrap();
    let outbox = AuditOutbox::open(config).await.unwrap();
    assert_eq!(outbox.status().await.unwrap().delivery_state, "stalled");
    outbox
        .quarantine(
            ids[1].to_string(),
            "receiver schema rejected".into(),
            "incident-42".into(),
        )
        .await
        .unwrap();
    wait_status(&outbox, |s| s.pending_records == 0).await;
    assert!(receiver.contains(ids[2].as_str()));
    assert_eq!(
        outbox
            .flush(Duration::from_secs(1))
            .await
            .unwrap_err()
            .kind(),
        AuditErrorKind::Disposition
    );
    let exported = outbox.export(ids[1].to_string()).await.unwrap();
    assert!(!exported.pending);
    assert_eq!(exported.receipts.len(), 1);
    receiver.mode.store(0, std::sync::atomic::Ordering::Release);
    outbox.replay(ids[1].to_string()).await.unwrap();
    wait_status(&outbox, |s| s.pending_records == 0).await;
    assert!(receiver.contains(ids[1].as_str()));
    assert_eq!(receiver.count(), 3);
    assert_eq!(outbox.status().await.unwrap().disposition_receipts, 1);
    outbox.close().await.unwrap();
}

#[tokio::test]
async fn credential_rotation_unblocks_waiting_flush_and_protocol_errors_retry() {
    let directory = private_directory();
    let receiver = fixture::Receiver::start(&directory.path().join("receiver.redb"), 3);
    let outbox = AuditOutbox::open(OutboxConfig::new(
        directory.path().join("outbox"),
        receiver.destination(),
    ))
    .await
    .unwrap();
    outbox.submit(event(1)).await.unwrap();
    wait_status(&outbox, |s| {
        s.last_delivery_error
            .as_ref()
            .is_some_and(|e| e.kind == DeliveryErrorKind::Credentials)
    })
    .await;
    let waiter = {
        let clone = outbox.clone();
        tokio::spawn(async move { clone.flush(Duration::from_secs(5)).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!waiter.is_finished());
    let mut destination = receiver.destination();
    destination.authorization = Some("Bearer rotated".into());
    outbox.reconfigure(destination).await.unwrap();
    waiter.await.unwrap().unwrap();
    receiver.mode.store(4, std::sync::atomic::Ordering::Release);
    outbox.submit(event(2)).await.unwrap();
    wait_status(&outbox, |s| {
        s.last_delivery_error
            .as_ref()
            .is_some_and(|e| e.kind == DeliveryErrorKind::Protocol)
    })
    .await;
    assert_eq!(
        outbox
            .flush(Duration::from_millis(50))
            .await
            .unwrap_err()
            .kind(),
        AuditErrorKind::Timeout
    );
    assert_eq!(outbox.status().await.unwrap().pending_records, 1);
    receiver.mode.store(0, std::sync::atomic::Ordering::Release);
    outbox.flush(Duration::from_secs(5)).await.unwrap();
    outbox.close().await.unwrap();
}

#[test]
fn stale_ack_destination_and_quarantine_classes_are_fenced() {
    let directory = private_directory();
    let config = config(directory.path());
    let mut store = store::Store::open(config.clone()).unwrap();
    let id = store.submit_group(vec![event(1)]).remove(0).unwrap();
    assert_eq!(
        store
            .quarantine(id.to_string(), "unattempted".into(), "op".into())
            .unwrap_err()
            .kind(),
        AuditErrorKind::InvalidEvent
    );
    for kind in [
        DeliveryErrorKind::Credentials,
        DeliveryErrorKind::Transport,
        DeliveryErrorKind::Configuration,
    ] {
        store
            .delivery_error(
                store.generation,
                DeliveryError {
                    kind,
                    http_status: Some(401),
                    sequence: 1,
                    immediate_flush_failure: false,
                    message: "fixture".into(),
                },
                None,
                true,
            )
            .unwrap();
        assert_eq!(
            store
                .quarantine(id.to_string(), "wrong class".into(), "op".into())
                .unwrap_err()
                .kind(),
            AuditErrorKind::InvalidEvent
        );
    }
    let generation = store.generation;
    let mut destination = config.destination.clone();
    destination.authorization = Some("Bearer rotated".into());
    store.reconfigure(destination.clone()).unwrap();
    assert_eq!(
        store
            .acknowledge(generation, vec![id.clone()])
            .unwrap_err()
            .kind(),
        AuditErrorKind::StaleDelivery
    );
    let mut changed = config.clone();
    changed.destination.endpoint.push_str("?different");
    store.close().unwrap();
    assert_eq!(
        store::Store::open(changed).err().unwrap().kind(),
        AuditErrorKind::DestinationMismatch
    );
    let mut config = config;
    config.destination = destination;
    let mut store = store::Store::open(config).unwrap();
    assert_eq!(store.batch().unwrap().unwrap().events[0].id, id);
    reject(&mut store, 1);
    store
        .quarantine(id.to_string(), "rejected".into(), "op".into())
        .unwrap();
    let mut wrong = id.to_string();
    let last = wrong.pop().unwrap();
    wrong.push(if last == '0' { '1' } else { '0' });
    assert_eq!(
        store.export(wrong.clone()).unwrap_err().kind(),
        AuditErrorKind::NotFound
    );
    assert_eq!(
        store.replay(wrong).unwrap_err().kind(),
        AuditErrorKind::NotFound
    );
}

#[test]
fn restore_reuses_sequence_with_content_bound_distinct_ids() {
    let directory = private_directory();
    let config = config(directory.path());
    let mut store = store::Store::open(config.clone()).unwrap();
    let first = store.submit_group(vec![event(1)]).remove(0).unwrap();
    store.close().unwrap();
    std::fs::copy(
        directory.path().join("audit.redb"),
        directory.path().join("old.redb"),
    )
    .unwrap();
    let mut store = store::Store::open(config.clone()).unwrap();
    store
        .acknowledge(store.generation, vec![first.clone()])
        .unwrap();
    let branch_a = store.submit_group(vec![event(2)]).remove(0).unwrap();
    store.close().unwrap();
    std::fs::copy(
        directory.path().join("old.redb"),
        directory.path().join("audit.redb"),
    )
    .unwrap();
    let mut store = store::Store::open(config.clone()).unwrap();
    store
        .acknowledge(store.generation, vec![first.clone()])
        .unwrap();
    let branch_b = store.submit_group(vec![event(20)]).remove(0).unwrap();
    assert_eq!(
        wire::parse_id(branch_a.as_str()).unwrap().1,
        wire::parse_id(branch_b.as_str()).unwrap().1
    );
    assert_ne!(branch_a, branch_b);
    store.close().unwrap();
    std::fs::copy(
        directory.path().join("old.redb"),
        directory.path().join("audit.redb"),
    )
    .unwrap();
    let mut store = store::Store::open(config).unwrap();
    store.acknowledge(store.generation, vec![first]).unwrap();
    assert_eq!(
        store.submit_group(vec![event(2)]).remove(0).unwrap(),
        branch_a
    );
}

#[test]
fn ambiguous_after_commit_is_recoverable_and_never_reported_success() {
    let directory = private_directory();
    let config = config(directory.path());
    let mut store = store::Store::open(config.clone()).unwrap();
    COMMIT_AFTER_FAILURE.with(|f| f.set(true));
    let results = store.submit_group(vec![event(1), event(2)]);
    COMMIT_AFTER_FAILURE.with(|f| f.set(false));
    assert!(results
        .iter()
        .all(|r| r.as_ref().unwrap_err().kind() == AuditErrorKind::CommitUncertain));
    drop(store);
    let store = store::Store::open(config).unwrap();
    assert_eq!(store.state.pending_records, 2);
}

#[tokio::test]
async fn cancellation_after_commit_keeps_record_and_no_early_completion() {
    use std::sync::atomic::Ordering;
    let directory = private_directory();
    let outbox = AuditOutbox::open(config(directory.path())).await.unwrap();
    outbox
        .shared
        .hooks
        .storage_pause
        .store(true, Ordering::Release);
    while !outbox.shared.hooks.storage_paused.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let task = {
        let clone = outbox.clone();
        tokio::spawn(async move { clone.submit(event(1)).await })
    };
    wait_status(&outbox, |s| s.queued_records == 1).await;
    assert!(!task.is_finished());
    assert_eq!(outbox.status().await.unwrap().pending_records, 0);
    outbox
        .shared
        .hooks
        .hold_completions
        .store(true, Ordering::Release);
    outbox
        .shared
        .hooks
        .storage_pause
        .store(false, Ordering::Release);
    wait_status(&outbox, |s| s.pending_records == 1).await;
    assert!(!task.is_finished());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    outbox
        .shared
        .hooks
        .hold_completions
        .store(false, Ordering::Release);
    outbox.close().await.unwrap();
    let reopened = AuditOutbox::open(config(directory.path())).await.unwrap();
    assert_eq!(reopened.status().await.unwrap().pending_records, 1);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn saturation_grouping_and_worker_exit_complete_waiters() {
    use std::sync::atomic::Ordering;
    let directory = private_directory();
    let outbox = AuditOutbox::open(config(directory.path())).await.unwrap();
    outbox
        .shared
        .hooks
        .storage_pause
        .store(true, Ordering::Release);
    while !outbox.shared.hooks.storage_paused.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut tasks = Vec::new();
    for n in 0..QUEUE_EVENTS + 8 {
        let clone = outbox.clone();
        tasks.push(tokio::spawn(
            async move { clone.submit(event(n as u64)).await },
        ));
    }
    let status = wait_status(&outbox, |s| s.queued_records == QUEUE_EVENTS).await;
    assert!(status.queued_bytes <= QUEUE_BYTES);
    assert!(!status.accepting);
    let flush = {
        let clone = outbox.clone();
        tokio::spawn(async move { clone.flush(Duration::from_secs(10)).await })
    };
    outbox
        .shared
        .hooks
        .storage_pause
        .store(false, Ordering::Release);
    let mut accepted = 0;
    let mut overloaded = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => accepted += 1,
            Err(e) if e.kind() == AuditErrorKind::QueueOverloaded => overloaded += 1,
            Err(e) => panic!("unexpected: {e}"),
        }
    }
    assert_eq!(accepted, QUEUE_EVENTS);
    assert_eq!(overloaded, 8);
    let status = outbox.status().await.unwrap();
    assert!(status.durable_commits <= QUEUE_EVENTS as u64 / 32);
    assert!(status.last_group_size <= MAX_BATCH_EVENTS);
    outbox
        .shared
        .hooks
        .storage_exit
        .store(true, Ordering::Release);
    let _ = outbox.shared.controls.try_send(worker::Control::Wake);
    assert_eq!(
        flush.await.unwrap().unwrap_err().kind(),
        AuditErrorKind::WorkerFailed
    );
    wait_status(&outbox, |s| s.closed).await;
    assert!(!outbox.status().await.unwrap().storage_worker_alive);
    assert_eq!(
        outbox.close().await.unwrap_err().kind(),
        AuditErrorKind::WorkerFailed
    );
    let reopened = AuditOutbox::open(config(directory.path())).await.unwrap();
    assert_eq!(
        reopened.status().await.unwrap().pending_records,
        QUEUE_EVENTS as u64
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn delivery_worker_exit_is_visible_and_pending_data_survive() {
    use std::sync::atomic::Ordering;
    let directory = private_directory();
    let config = config(directory.path());
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    outbox.submit(event(1)).await.unwrap();
    outbox
        .shared
        .hooks
        .delivery_exit
        .store(true, Ordering::Release);
    outbox.shared.signal();
    wait_status(&outbox, |s| !s.delivery_worker_alive).await;
    assert_eq!(
        outbox
            .flush(Duration::from_secs(2))
            .await
            .unwrap_err()
            .kind(),
        AuditErrorKind::WorkerFailed
    );
    let status = outbox.status().await.unwrap();
    assert_eq!(status.delivery_state, "worker_failed");
    assert!(status.storage_worker_alive);
    outbox.close().await.unwrap();
    let reopened = AuditOutbox::open(config).await.unwrap();
    assert_eq!(reopened.status().await.unwrap().pending_records, 1);
    reopened.close().await.unwrap();
}

#[test]
fn physical_space_reuse_and_durable_latency_measurements() {
    let directory = private_directory();
    let config = config(directory.path());
    let mut store = store::Store::open(config).unwrap();
    let mut sizes = Vec::new();
    let mut timings = Vec::new();
    for cycle in 0..12 {
        let events = (0..64)
            .map(|n| AuditEvent {
                timestamp_ms: cycle * 64 + n,
                kind: "fill".into(),
                payload: serde_json::json!({"data":"x".repeat(4096)}),
            })
            .collect();
        let start = std::time::Instant::now();
        let ids = store
            .submit_group(events)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        timings.push(start.elapsed().as_micros());
        store.acknowledge(store.generation, ids).unwrap();
        sizes.push(
            std::fs::metadata(directory.path().join("audit.redb"))
                .unwrap()
                .len(),
        );
    }
    let high_water = *sizes.iter().max().unwrap();
    assert!(
        high_water <= sizes[2] + 4 * 1024 * 1024,
        "physical space grew without reuse: {sizes:?}"
    );
    assert!(
        high_water <= 16 * 1024 * 1024,
        "physical footprint exceeded bounded test allowance"
    );
    timings.sort_unstable();
    println!(
        "audit durable64event/4KiB group commit p50={}us p99={}us; redb fill/drain bytes={sizes:?}",
        timings[6], timings[11]
    );
    let mut idle = Vec::new();
    for n in 0..50 {
        let start = std::time::Instant::now();
        let id = store.submit_group(vec![event(n)]).remove(0).unwrap();
        idle.push(start.elapsed().as_micros());
        store.acknowledge(store.generation, vec![id]).unwrap();
    }
    idle.sort_unstable();
    println!(
        "audit idle durable submit p50={}us p99={}us",
        idle[25], idle[49]
    );
}

#[test]
fn retry_after_seconds_dates_are_bounded_and_invalid_values_ignored() {
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
    assert_eq!(
        worker::retry_after("120", now),
        Some(Duration::from_secs(30))
    );
    assert_eq!(worker::retry_after("0", now), Some(Duration::ZERO));
    assert_eq!(
        worker::retry_after(&httpdate::fmt_http_date(now + Duration::from_secs(15)), now),
        Some(Duration::from_secs(15))
    );
    assert_eq!(
        worker::retry_after(
            &httpdate::fmt_http_date(now + Duration::from_secs(300)),
            now
        ),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        worker::retry_after(&httpdate::fmt_http_date(now - Duration::from_secs(15)), now),
        Some(Duration::ZERO)
    );
    assert_eq!(worker::retry_after("not-a-date", now), None);
}

#[test]
fn dead_letters_exhaust_budget_without_purge_and_failed_quarantine_preserves_pending() {
    let directory = private_directory();
    let config = config(directory.path());
    let mut store = store::Store::open(config.clone()).unwrap();
    let ids = store
        .submit_group((1..=3).map(event).collect())
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    for (index, id) in ids.iter().enumerate() {
        reject(&mut store, index as u64 + 1);
        store
            .quarantine(id.to_string(), "bad schema".into(), "operator".into())
            .unwrap();
    }
    let retained = store.state.retained();
    store.close().unwrap();
    let mut full_config = config.clone();
    full_config.max_retained_bytes = retained;
    let mut store = store::Store::open(full_config).unwrap();
    assert_eq!(store.state.dead_records, 3);
    assert_eq!(store.state.pending_records, 0);
    assert_eq!(
        store
            .submit_group(vec![event(4)])
            .remove(0)
            .unwrap_err()
            .kind(),
        AuditErrorKind::StorageFull
    );
    for id in &ids {
        assert!(!store.export(id.to_string()).unwrap().pending);
    }
    assert_eq!(store.state.retained(), retained);
    store.close().unwrap();
    let mut store = store::Store::open(config.clone()).unwrap();
    let id = store.submit_group(vec![event(4)]).remove(0).unwrap();
    reject(&mut store, 4);
    let retained_before = store.state.retained();
    store.close().unwrap();
    let mut tight = config;
    tight.max_retained_bytes = retained_before;
    let mut store = store::Store::open(tight).unwrap();
    assert_eq!(
        store
            .quarantine(id.to_string(), "bad schema".into(), "operator".into())
            .unwrap_err()
            .kind(),
        AuditErrorKind::StorageFull
    );
    assert!(store.export(id.to_string()).unwrap().pending);
    assert_eq!(store.state.pending_records, 1);
    assert_eq!(store.state.dead_records, 3);
    assert_eq!(store.state.retained(), retained_before);
}

#[tokio::test]
async fn concurrent_submit_close_completes_admitted_work_without_receiver() {
    use std::sync::atomic::Ordering;
    let directory = private_directory();
    let config = config(directory.path());
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    outbox
        .shared
        .hooks
        .storage_pause
        .store(true, Ordering::Release);
    while !outbox.shared.hooks.storage_paused.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut submissions = Vec::new();
    for n in 0..128 {
        let clone = outbox.clone();
        submissions.push(tokio::spawn(async move { clone.submit(event(n)).await }));
    }
    wait_status(&outbox, |s| s.queued_records == 128).await;
    let close = {
        let clone = outbox.clone();
        tokio::spawn(async move { clone.close().await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        outbox.submit(event(999)).await.unwrap_err().kind(),
        AuditErrorKind::Closed
    );
    outbox
        .shared
        .hooks
        .storage_pause
        .store(false, Ordering::Release);
    for submission in submissions {
        submission.await.unwrap().unwrap();
    }
    tokio::time::timeout(Duration::from_secs(3), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reopened = AuditOutbox::open(config).await.unwrap();
    assert_eq!(reopened.status().await.unwrap().pending_records, 128);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn cloned_cancelled_flush_credential_rotation_close_reopens_original_configuration() {
    let directory = private_directory();
    let receiver = fixture::Receiver::start(&directory.path().join("receiver.redb"), 7);
    let config = OutboxConfig::new(directory.path().join("outbox"), receiver.destination());
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    let clone = outbox.clone();
    let id = clone.submit(event(1)).await.unwrap();
    let mut rotated = receiver.destination();
    rotated.authorization = Some("Bearer rotated".into());
    outbox.reconfigure(rotated).await.unwrap();
    let flush = {
        let clone = clone.clone();
        tokio::spawn(async move { clone.flush(Duration::from_secs(10)).await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    flush.abort();
    assert!(flush.await.unwrap_err().is_cancelled());
    assert_eq!(
        outbox
            .flush(Duration::from_millis(10))
            .await
            .unwrap_err()
            .kind(),
        AuditErrorKind::Timeout
    );
    clone.close().await.unwrap();
    outbox.close().await.unwrap();
    receiver.mode.store(0, std::sync::atomic::Ordering::Release);
    let reopened = AuditOutbox::open(config).await.unwrap();
    reopened.flush(Duration::from_secs(5)).await.unwrap();
    assert!(receiver.contains(id.as_str()));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn canceled_open_and_last_handle_drop_release_directory() {
    let directory = private_directory();
    let config = config(directory.path());
    let task = {
        let config = config.clone();
        tokio::spawn(async move { AuditOutbox::open(config).await })
    };
    tokio::task::yield_now().await;
    task.abort();
    match task.await {
        Ok(Ok(handle)) => drop(handle),
        Err(error) => assert!(error.is_cancelled()),
        _ => {}
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let outbox = loop {
        match AuditOutbox::open(config.clone()).await {
            Ok(outbox) => break outbox,
            Err(error)
                if error.kind() == AuditErrorKind::Storage
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(error) => panic!("reopen after canceled open: {error}"),
        }
    };
    outbox.submit(event(1)).await.unwrap();
    drop(outbox);
    let reopened = loop {
        match AuditOutbox::open(config.clone()).await {
            Ok(outbox) => break outbox,
            Err(error)
                if error.kind() == AuditErrorKind::Storage
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(error) => panic!("reopen after final drop: {error}"),
        }
    };
    assert_eq!(reopened.status().await.unwrap().pending_records, 1);
    reopened.close().await.unwrap();
}

#[test]
fn config_and_resource_preflight_reject_invalid_inputs_without_queueing() {
    for endpoint in [
        "ftp://localhost/audit",
        "https://user:secret@localhost/audit",
        "https://@localhost/audit",
        "https://localhost/audit#fragment",
        " https://localhost/audit",
    ] {
        let destination = Destination {
            endpoint: endpoint.into(),
            receiver_identity: None,
            authorization: None,
        };
        assert_eq!(
            destination.fingerprint().unwrap_err().kind(),
            AuditErrorKind::InvalidConfig
        );
        assert!(!format!("{destination:?}").contains("secret"));
    }
    let mut oversized = event(1);
    oversized.payload = serde_json::json!({"data":"x".repeat(DEFAULT_EVENT_BYTES)});
    assert_eq!(
        wire::validate_event(&oversized, DEFAULT_EVENT_BYTES)
            .unwrap_err()
            .kind(),
        AuditErrorKind::EventTooLarge
    );
    let mut invalid = event(1);
    invalid.kind = "\n".into();
    assert_eq!(
        wire::validate_event(&invalid, DEFAULT_EVENT_BYTES)
            .unwrap_err()
            .kind(),
        AuditErrorKind::InvalidEvent
    );
    let mut wrong = event(1);
    wrong.payload = serde_json::json!([]);
    assert_eq!(
        wire::validate_event(&wrong, DEFAULT_EVENT_BYTES)
            .unwrap_err()
            .kind(),
        AuditErrorKind::InvalidEvent
    );
}

#[test]
fn child_process_entry() {
    let Some(path) = std::env::var_os("ASTER_AUDIT_CHILD_DIRECTORY") else {
        return;
    };
    let mode = std::env::var("ASTER_AUDIT_CHILD_MODE").unwrap();
    let marker = std::path::PathBuf::from(std::env::var_os("ASTER_AUDIT_CHILD_MARKER").unwrap());
    let mut config = config(std::path::Path::new(&path));
    if let Ok(endpoint) = std::env::var("ASTER_AUDIT_CHILD_ENDPOINT") {
        config.destination.endpoint = endpoint;
        config.destination.receiver_identity = Some("persistent-fixture-v1".into());
    }
    if mode == "lock" {
        let error = store::Store::open(config).err().unwrap();
        assert_eq!(error.kind(), AuditErrorKind::Storage);
        assert!(error.message.contains("already owned"));
        write_marker(&marker, b"locked");
        return;
    }
    if mode.starts_with("ack_") {
        let mut store = store::Store::open(config).unwrap();
        let ids = store
            .batch()
            .unwrap()
            .unwrap()
            .events
            .into_iter()
            .map(|event| event.id)
            .collect();
        ACK_PAUSE.with(|value| value.set(if mode == "ack_before" { 1 } else { 2 }));
        store.acknowledge(store.generation, ids).unwrap();
        unreachable!("acknowledgement crash boundary did not pause");
    }
    if mode.starts_with("quarantine_") {
        let mut store = store::Store::open(config).unwrap();
        let input: QuarantineExport = serde_json::from_slice(
            &std::fs::read(std::path::Path::new(&path).join("quarantine-input.json")).unwrap(),
        )
        .unwrap();
        QUARANTINE_PAUSE.with(|value| value.set(if mode == "quarantine_before" { 1 } else { 2 }));
        store
            .quarantine(
                input.event_id.to_string(),
                "crash remedy".into(),
                "operator".into(),
            )
            .unwrap();
        unreachable!("quarantine crash boundary did not pause");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let outbox = AuditOutbox::open(config).await.unwrap();
        let id = outbox.submit(event(7)).await.unwrap();
        write_marker(
            &marker,
            &serde_json::to_vec(&outbox.export(id.to_string()).await.unwrap()).unwrap(),
        );
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

#[tokio::test]
async fn submission_byte_budget_is_independent_of_count_budget() {
    use std::sync::atomic::Ordering;
    let directory = private_directory();
    let mut config = config(directory.path());
    config.max_event_bytes = MAX_EVENT_BYTES;
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    outbox
        .shared
        .hooks
        .storage_pause
        .store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !outbox.shared.hooks.storage_paused.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut tasks = Vec::new();
    for n in 0..10 {
        let clone = outbox.clone();
        tasks.push(tokio::spawn(async move {
            clone
                .submit(AuditEvent {
                    timestamp_ms: n,
                    kind: "byte-bound".into(),
                    payload: serde_json::json!({"data":"x".repeat(900_000)}),
                })
                .await
        }));
    }
    wait_status(&outbox, |s| s.queued_records == 9).await;
    let status = outbox.status().await.unwrap();
    assert!(status.queued_bytes <= QUEUE_BYTES);
    assert!(status.queued_records < QUEUE_EVENTS);
    outbox
        .shared
        .hooks
        .storage_pause
        .store(false, Ordering::Release);
    let mut accepted = 0;
    let mut overloaded = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => accepted += 1,
            Err(e) if e.kind() == AuditErrorKind::QueueOverloaded => overloaded += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!((accepted, overloaded), (9, 1));
    outbox.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "durable workload measurement; run explicitly after the fault matrix"]
async fn durable_submit_workload_measurements() {
    let directory = private_directory();
    let outbox = AuditOutbox::open(config(directory.path())).await.unwrap();
    let mut idle = Vec::new();
    let start = std::time::Instant::now();
    for n in 0..50 {
        let submitted = std::time::Instant::now();
        outbox.submit(event(n)).await.unwrap();
        idle.push(submitted.elapsed().as_micros());
    }
    let idle_elapsed = start.elapsed();
    idle.sort_unstable();
    println!(
        "outbox idle durable-submit p50={}us p99={}us throughput={:.1}/s",
        idle[25],
        idle[49],
        50.0 / idle_elapsed.as_secs_f64()
    );
    let before = outbox.status().await.unwrap().durable_commits;
    let mut concurrent = Vec::new();
    let start = std::time::Instant::now();
    for n in 0..512 {
        let clone = outbox.clone();
        concurrent.push(tokio::spawn(async move {
            let start = std::time::Instant::now();
            clone
                .submit(AuditEvent {
                    timestamp_ms: n,
                    kind: "concurrent".into(),
                    payload: serde_json::json!({"data":"x".repeat(4096)}),
                })
                .await
                .unwrap();
            start.elapsed().as_micros()
        }));
    }
    let mut latencies = Vec::new();
    for task in concurrent {
        latencies.push(task.await.unwrap());
    }
    latencies.sort_unstable();
    let elapsed = start.elapsed();
    let status = outbox.status().await.unwrap();
    let groups = status.durable_commits - before;
    #[cfg(unix)]
    let rss_bytes = {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
        let raw = usage.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            raw
        } else {
            raw * 1024
        }
    };
    #[cfg(not(unix))]
    let rss_bytes = 0;
    println!("outbox stalled-receiver concurrent512x4KiB durable-submit p50={}us p99={}us throughput={:.1}/s mean_group={:.2} retained={}bytes queue={}bytes peak_process_rss={}bytes",latencies[256],latencies[506],512.0/elapsed.as_secs_f64(),512.0/groups as f64,status.retained_bytes,status.queued_bytes,rss_bytes);
    outbox.close().await.unwrap();
}

#[tokio::test]
async fn write_failure_closes_admission_and_preserves_previous_durable_records() {
    use std::sync::atomic::Ordering;
    let directory = private_directory();
    let config = config(directory.path());
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    let id = outbox.submit(event(1)).await.unwrap();
    outbox
        .shared
        .hooks
        .write_failure
        .store(true, Ordering::Release);
    assert_eq!(
        outbox.submit(event(2)).await.unwrap_err().kind(),
        AuditErrorKind::Storage
    );
    wait_status(&outbox, |s| s.closed).await;
    assert!(!outbox.status().await.unwrap().accepting);
    let reopened = AuditOutbox::open(config).await.unwrap();
    assert_eq!(reopened.status().await.unwrap().pending_records, 1);
    assert_eq!(reopened.export(id.to_string()).await.unwrap().event_id, id);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn saturated_submission_queue_cannot_starve_ack_deletion_or_flush_controls() {
    use std::sync::atomic::Ordering;
    let directory = private_directory();
    let receiver = fixture::Receiver::start(&directory.path().join("receiver.redb"), 0);
    let outbox = AuditOutbox::open(OutboxConfig::new(
        directory.path().join("outbox"),
        receiver.destination(),
    ))
    .await
    .unwrap();
    outbox
        .shared
        .hooks
        .storage_pause
        .store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !outbox.shared.hooks.storage_paused.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut tasks = Vec::new();
    for n in 0..QUEUE_EVENTS {
        let clone = outbox.clone();
        tasks.push(tokio::spawn(
            async move { clone.submit(event(n as u64)).await },
        ));
    }
    wait_status(&outbox, |s| s.queued_records == QUEUE_EVENTS).await;
    let flush = {
        let clone = outbox.clone();
        tokio::spawn(async move { clone.flush(Duration::from_secs(5)).await })
    };
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(!flush.is_finished());
    outbox
        .shared
        .hooks
        .storage_pause
        .store(false, Ordering::Release);
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    flush.await.unwrap().unwrap();
    assert_eq!(receiver.count(), QUEUE_EVENTS as u64);
    assert_eq!(outbox.status().await.unwrap().pending_records, 0);
    outbox.close().await.unwrap();
}
/// Write a child's marker atomically, so the parent never reads a partial
/// JSON export: `wait_file` sees the marker only once it is complete.
fn write_marker(marker: &std::path::Path, contents: &[u8]) {
    let staging = marker.with_extension("partial");
    std::fs::write(&staging, contents).unwrap();
    std::fs::rename(&staging, marker).unwrap();
}

/// Generous bound for cross-process steps: a loaded parallel run can delay
/// a re-executed test binary's start, its worker and the receiver's durable
/// commit by seconds. Waits poll a condition; this only bounds a hang.
const CHILD_PATIENCE: Duration = Duration::from_secs(60);

/// A child test process that is killed and reaped however the test ends.
/// Several modes loop forever after writing their marker, so a panic between
/// spawn and an explicit kill would otherwise orphan the child.
struct ChildGuard(std::process::Child);

impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child(
    path: &std::path::Path,
    marker: &std::path::Path,
    mode: &str,
    endpoint: Option<&str>,
) -> ChildGuard {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "audit_outbox::tests::child_process_entry",
            "--nocapture",
        ])
        .env("ASTER_AUDIT_CHILD_DIRECTORY", path)
        .env("ASTER_AUDIT_CHILD_MARKER", marker)
        .env("ASTER_AUDIT_CHILD_MODE", mode)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    if let Some(endpoint) = endpoint {
        command.env("ASTER_AUDIT_CHILD_ENDPOINT", endpoint);
    }
    ChildGuard(command.spawn().unwrap())
}
fn wait_file(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + CHILD_PATIENCE;
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child wrote no marker within {CHILD_PATIENCE:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cross_process_exclusive_lock() {
    let directory = private_directory();
    let store = store::Store::open(config(directory.path())).unwrap();
    let marker = directory.path().join("marker");
    let mut child = child(directory.path(), &marker, "lock", None);
    assert!(child.wait().unwrap().success());
    assert_eq!(std::fs::read(marker).unwrap(), b"locked");
    store.close().unwrap();
    store::Store::open(config(directory.path())).unwrap();
}

#[test]
fn process_kill_before_and_after_quarantine_commit_preserves_original_and_receipt() {
    for (mode, after) in [("quarantine_before", false), ("quarantine_after", true)] {
        let directory = private_directory();
        let config = config(directory.path());
        let mut store = store::Store::open(config.clone()).unwrap();
        let ids = store
            .submit_group(vec![event(1), event(2)])
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        reject(&mut store, 1);
        let original = store.export(ids[0].to_string()).unwrap();
        std::fs::write(
            directory.path().join("quarantine-input.json"),
            serde_json::to_vec(&original).unwrap(),
        )
        .unwrap();
        store.close().unwrap();
        let marker = directory.path().join("marker");
        let mut process = child(directory.path(), &marker, mode, None);
        wait_file(&marker);
        process.kill().unwrap();
        process.wait().unwrap();
        let store = store::Store::open(config).unwrap();
        let exported = store.export(ids[0].to_string()).unwrap();
        assert_eq!(exported.envelope_base64, original.envelope_base64);
        assert_eq!(exported.pending, !after);
        assert_eq!(exported.receipts.len(), usize::from(after));
        assert_eq!(store.state.dead_records, u64::from(after));
        assert_eq!(store.state.pending_records, if after { 1 } else { 2 });
    }
}

#[tokio::test]
async fn process_kill_after_acceptance_and_after_receiver_commit_recovers_same_id() {
    for mode in [7, 9] {
        let directory = private_directory();
        let receiver = fixture::Receiver::start(&directory.path().join("receiver.redb"), mode);
        let outbox_path = directory.path().join("outbox");
        let marker = directory.path().join("marker");
        let mut process = child(&outbox_path, &marker, "submit", Some(&receiver.endpoint));
        wait_file(&marker);
        let exported: QuarantineExport =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        if mode == 9 {
            let deadline = std::time::Instant::now() + CHILD_PATIENCE;
            while !receiver.contains(exported.event_id.as_str()) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "receiver did not commit the child's event within {CHILD_PATIENCE:?}"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        process.kill().unwrap();
        process.wait().unwrap();
        receiver.mode.store(0, std::sync::atomic::Ordering::Release);
        let outbox = AuditOutbox::open(OutboxConfig::new(&outbox_path, receiver.destination()))
            .await
            .unwrap();
        // Receiver may accept the resend immediately; either retained export or durable
        // receiver original bytes demonstrate that the full accepted ID survived the kill.
        outbox.flush(Duration::from_secs(5)).await.unwrap();
        assert!(receiver.contains(exported.event_id.as_str()));
        assert_eq!(receiver.count(), 1);
        if mode == 9 {
            assert!(receiver.attempts.lock().unwrap().len() >= 2);
        }
        outbox.close().await.unwrap();
    }
}

#[test]
fn acknowledgement_log_records_each_event_and_honours_count_age_and_disable() {
    let directory = private_directory();
    let mut config = config(directory.path());
    let mut store = store::Store::open(config.clone()).unwrap();
    let ids = store
        .submit_group((1..=5).map(event).collect())
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    let before = now_ms();
    store
        .acknowledge(store.generation, ids[..2].to_vec())
        .unwrap();
    store
        .acknowledge(store.generation, ids[2..].to_vec())
        .unwrap();
    let log = store.acknowledgements(0, 100).unwrap();
    assert_eq!(log.len(), 5);
    for (index, entry) in log.iter().enumerate() {
        assert_eq!(entry.cursor, index as u64 + 1);
        assert_eq!(entry.event_id, ids[index]);
        assert_eq!(entry.sequence, index as u64 + 1);
        assert!(entry.acknowledged_at_ms >= before);
        let batch = if index < 2 { &ids[..2] } else { &ids[2..] };
        assert_eq!(
            entry.batch_id,
            wire::batch_id(batch.iter().map(EventId::as_str))
        );
    }
    // Cursor paging: strictly after the cursor, bounded by the limit.
    let page = store.acknowledgements(2, 2).unwrap();
    assert_eq!(
        page.iter().map(|e| e.cursor).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert!(store.acknowledgements(5, 10).unwrap().is_empty());
    assert!(store.acknowledgements(u64::MAX, 10).unwrap().is_empty());
    assert_eq!(store.state.pending_records, 0);
    assert_eq!(
        store.state.retained(),
        0,
        "the log is not charged to retention"
    );
    drop(store);

    // A smaller count bound prunes the oldest entries on open; cursors persist.
    config.acknowledgement_log_max_entries = 2;
    let mut store = store::Store::open(config.clone()).unwrap();
    assert_eq!(
        store
            .acknowledgements(0, 100)
            .unwrap()
            .iter()
            .map(|e| e.cursor)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );
    let next = store
        .submit_group(vec![event(6)])
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    store.acknowledge(store.generation, next.clone()).unwrap();
    let log = store.acknowledgements(0, 100).unwrap();
    assert_eq!(log.iter().map(|e| e.cursor).collect::<Vec<_>>(), vec![5, 6]);
    assert_eq!(log[1].event_id, next[0]);
    drop(store);

    // An age bound prunes entries acknowledged longer ago than it.
    let newest = log[1].acknowledged_at_ms;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while now_ms() <= newest + 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "clock did not advance"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    config.acknowledgement_log_max_age_ms = 1;
    let store = store::Store::open(config.clone()).unwrap();
    assert!(store.acknowledgements(0, 100).unwrap().is_empty());
    assert_eq!(store.ack_cursor, 6);
    drop(store);

    // Zero entries disables the log; the cursor does not advance.
    config.acknowledgement_log_max_age_ms = DEFAULT_ACK_LOG_AGE_MS;
    config.acknowledgement_log_max_entries = 0;
    let mut store = store::Store::open(config.clone()).unwrap();
    let more = store
        .submit_group(vec![event(7)])
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    store.acknowledge(store.generation, more).unwrap();
    assert!(store.acknowledgements(0, 100).unwrap().is_empty());
    assert_eq!(store.ack_cursor, 6);
    drop(store);

    config.acknowledgement_log_max_entries = MAX_ACK_LOG_ENTRIES + 1;
    assert_eq!(
        store::Store::open(config).err().unwrap().kind(),
        AuditErrorKind::InvalidConfig
    );
}

#[tokio::test]
async fn acknowledgement_wait_follows_live_delivery_and_resumes_after_reopen() {
    let directory = private_directory();
    let receiver = fixture::Receiver::start(&directory.path().join("receiver.redb"), 0);
    let config = OutboxConfig::new(directory.path().join("outbox"), receiver.destination());
    let outbox = AuditOutbox::open(config.clone()).await.unwrap();
    assert_eq!(
        outbox.acknowledgements(0, 0).await.unwrap_err().kind(),
        AuditErrorKind::InvalidEvent
    );
    assert!(outbox
        .wait_acknowledgements(0, 10, Duration::from_millis(20))
        .await
        .unwrap()
        .is_empty());
    let waiter = {
        let outbox = outbox.clone();
        tokio::spawn(async move {
            outbox
                .wait_acknowledgements(0, 10, Duration::from_secs(30))
                .await
        })
    };
    let id = outbox.submit(event(1)).await.unwrap();
    let page = waiter.await.unwrap().unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].event_id, id);
    assert_eq!(page[0].cursor, 1);
    assert_eq!(page[0].batch_id, wire::batch_id([id.as_str()]));
    assert!(receiver.contains(id.as_str()));
    outbox.close().await.unwrap();
    assert_eq!(
        outbox
            .wait_acknowledgements(1, 10, Duration::from_secs(30))
            .await
            .unwrap_err()
            .kind(),
        AuditErrorKind::Closed
    );

    let outbox = AuditOutbox::open(config).await.unwrap();
    let second = outbox.submit(event(2)).await.unwrap();
    outbox.flush(Duration::from_secs(10)).await.unwrap();
    let page = outbox
        .wait_acknowledgements(1, 10, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!((page[0].cursor, &page[0].event_id), (2, &second));
    assert_eq!(outbox.acknowledgements(0, 10).await.unwrap().len(), 2);
    outbox.close().await.unwrap();
}

#[test]
fn process_kill_before_and_after_ack_commit_keeps_deletion_and_log_together() {
    for (mode, after) in [("ack_before", false), ("ack_after", true)] {
        let directory = private_directory();
        let config = config(directory.path());
        let mut store = store::Store::open(config.clone()).unwrap();
        let ids = store
            .submit_group(vec![event(1), event(2), event(3)])
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        store.close().unwrap();
        let marker = directory.path().join("marker");
        let mut process = child(directory.path(), &marker, mode, None);
        wait_file(&marker);
        process.kill().unwrap();
        process.wait().unwrap();
        let store = store::Store::open(config).unwrap();
        let log = store.acknowledgements(0, 100).unwrap();
        if after {
            assert_eq!(store.state.pending_records, 0);
            assert!(store.batch().unwrap().is_none());
            assert_eq!(
                log.iter().map(|e| e.event_id.clone()).collect::<Vec<_>>(),
                ids
            );
            assert_eq!(store.ack_cursor, 3);
        } else {
            // Neither the deletion nor any log row survived the kill.
            assert_eq!(store.state.pending_records, 3);
            assert!(log.is_empty());
            assert_eq!(store.ack_cursor, 0);
            for id in &ids {
                assert!(store.export(id.to_string()).unwrap().pending);
            }
        }
    }
}
