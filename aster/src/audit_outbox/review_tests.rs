//! Independent store/worker review regressions. Detailed fixture matrix is in tests.rs.
use super::*;

fn config(path: &std::path::Path) -> OutboxConfig {
    OutboxConfig::new(
        path,
        Destination {
            endpoint: "http://127.0.0.1:1/audit".into(),
            receiver_identity: Some("review-fixture".into()),
            authorization: None,
        },
    )
}
fn event(n: u64) -> AuditEvent {
    AuditEvent {
        timestamp_ms: n,
        kind: "review".into(),
        payload: serde_json::json!({"n":n}),
    }
}
fn stall(sequence: u64, kind: DeliveryErrorKind) -> DeliveryError {
    DeliveryError {
        kind,
        http_status: Some(422),
        sequence,
        immediate_flush_failure: true,
        message: "review fixture stall".into(),
    }
}

#[test]
fn completed_flush_prefix_ignores_later_stalls_and_dispositions() {
    let dir = tests::private_directory();
    let mut store = store::Store::open(config(dir.path())).unwrap();
    let ids = store
        .submit_group(vec![event(1), event(2)])
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    store
        .acknowledge(store.generation, vec![ids[0].clone()])
        .unwrap();
    store
        .delivery_error(
            store.generation,
            stall(2, DeliveryErrorKind::Rejected),
            Some(2),
            true,
        )
        .unwrap();
    assert!(store.flush_result(1).unwrap());
    assert_eq!(
        store.flush_result(2).unwrap_err().kind(),
        AuditErrorKind::DeliveryStalled
    );
    store
        .quarantine(
            ids[1].to_string(),
            "fixture rejection".into(),
            "review-1".into(),
        )
        .unwrap();
    assert!(store.flush_result(1).unwrap());
    assert_eq!(
        store.flush_result(2).unwrap_err().kind(),
        AuditErrorKind::Disposition
    );
    store.close().unwrap();
    let store = store::Store::open(config(dir.path())).unwrap();
    assert!(store.flush_result(1).unwrap());
    assert_eq!(
        store.flush_result(2).unwrap_err().kind(),
        AuditErrorKind::Disposition
    );
}

#[test]
fn credential_repair_fences_old_errors_and_acks_without_changing_bytes() {
    let dir = tests::private_directory();
    let mut cfg = config(dir.path());
    let mut store = store::Store::open(cfg.clone()).unwrap();
    let id = store.submit_group(vec![event(1)]).remove(0).unwrap();
    let original = store.export(id.to_string()).unwrap().envelope_base64;
    let old_generation = store.generation;
    cfg.destination.authorization = Some("Bearer replacement".into());
    store.reconfigure(cfg.destination.clone()).unwrap();
    store
        .delivery_error(
            old_generation,
            stall(1, DeliveryErrorKind::Configuration),
            None,
            true,
        )
        .unwrap();
    assert!(store.last_error.is_none());
    assert!(store.batch().unwrap().is_some());
    assert_eq!(
        store
            .acknowledge(old_generation, vec![id.clone()])
            .unwrap_err()
            .kind(),
        AuditErrorKind::StaleDelivery
    );
    assert_eq!(
        store.export(id.to_string()).unwrap().envelope_base64,
        original
    );
    assert!(!store.flush_result(1).unwrap());
    store.acknowledge(store.generation, vec![id]).unwrap();
    assert!(store.flush_result(1).unwrap());
    store.close().unwrap();
    let store = store::Store::open(cfg).unwrap();
    assert_eq!(store.state.pending_records, 0);
}

#[test]
fn failed_quarantine_budget_preserves_pending_record_and_exact_evidence() {
    let dir = tests::private_directory();
    let mut cfg = config(dir.path());
    let mut store = store::Store::open(cfg.clone()).unwrap();
    let id = store.submit_group(vec![event(1)]).remove(0).unwrap();
    let original = store.export(id.to_string()).unwrap().envelope_base64;
    cfg.max_retained_bytes = store.state.pending_bytes;
    // Bound configuration requires >=256; consume all remaining allowed budget
    // with a second envelope if needed, then reopen at exactly retained bytes.
    if cfg.max_retained_bytes < 256 {
        store.submit_group(vec![event(2)]).remove(0).unwrap();
        cfg.max_retained_bytes = store.state.pending_bytes;
    }
    store.close().unwrap();
    let mut store = store::Store::open(cfg.clone()).unwrap();
    store
        .delivery_error(
            store.generation,
            stall(1, DeliveryErrorKind::Rejected),
            Some(1),
            true,
        )
        .unwrap();
    let retained = store.state.retained();
    assert_eq!(
        store
            .quarantine(id.to_string(), "rejection".into(), "review-2".into())
            .unwrap_err()
            .kind(),
        AuditErrorKind::StorageFull
    );
    assert_eq!(store.state.retained(), retained);
    assert_eq!(store.state.dead_records, 0);
    assert_eq!(store.state.receipt_records, 0);
    let export = store.export(id.to_string()).unwrap();
    assert!(export.pending);
    assert_eq!(export.envelope_base64, original);
    assert!(export.receipts.is_empty());
    assert_eq!(
        store
            .submit_group(vec![event(3)])
            .remove(0)
            .unwrap_err()
            .kind(),
        AuditErrorKind::StorageFull
    );
    store.close().unwrap();
    let store = store::Store::open(cfg).unwrap();
    assert!(store.export(id.to_string()).unwrap().pending);
    assert_eq!(
        store.export(id.to_string()).unwrap().envelope_base64,
        original
    );
}

#[tokio::test]
async fn invalid_operator_commands_are_rejected_before_the_bounded_control_queue() {
    use std::sync::atomic::Ordering;
    let dir = tests::private_directory();
    let outbox = AuditOutbox::open(config(dir.path())).await.unwrap();
    let id = outbox.submit(event(1)).await.unwrap();
    outbox
        .shared
        .hooks
        .storage_pause
        .store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !outbox.shared.hooks.storage_paused.load(Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            outbox
                .shared
                .hooks
                .storage_pause
                .store(false, Ordering::Release);
            panic!("storage pause was not acknowledged");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Storage cannot reply while paused. Immediate typed failures demonstrate
    // that unbounded control fields are refused before queue retention.
    let quarantine = tokio::time::timeout(
        Duration::from_millis(100),
        outbox.quarantine(id.to_string(), "x".repeat(1024 * 1024), "operator".into()),
    )
    .await;
    let export = tokio::time::timeout(
        Duration::from_millis(100),
        outbox.export("x".repeat(1024 * 1024)),
    )
    .await;
    let replay = tokio::time::timeout(
        Duration::from_millis(100),
        outbox.replay("x".repeat(1024 * 1024)),
    )
    .await;
    outbox
        .shared
        .hooks
        .storage_pause
        .store(false, Ordering::Release);
    assert_eq!(
        quarantine.unwrap().unwrap_err().kind(),
        AuditErrorKind::InvalidEvent
    );
    assert_eq!(
        export.unwrap().unwrap_err().kind(),
        AuditErrorKind::IntegrityConflict
    );
    assert_eq!(
        replay.unwrap().unwrap_err().kind(),
        AuditErrorKind::IntegrityConflict
    );
    outbox.close().await.unwrap();
}
