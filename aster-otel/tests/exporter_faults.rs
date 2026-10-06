//! Actual OTLP application-log faults, isolated in child processes.
#![cfg(feature = "otlp-logs")]
mod common;
#[path = "../../tests/support/otlp_faults.rs"]
mod faults;
use aster_otel::bridge::{AppLogRecord, LogLevel};
use aster_otel::{ExporterKind, ObservabilityConfig};
use faults::{Collector, Fault};
use std::time::{Duration, Instant};

#[test]
fn child() {
    let Some(case) = common::child_case() else {
        return;
    };
    let fault = match case.as_str() {
        "http503" => Fault::Http503,
        "stall" => Fault::Stall,
        "refused" => Fault::Refused,
        _ => panic!("unknown fault"),
    };
    let mut receiver = Collector::new(fault);
    if matches!(fault, Fault::Refused) {
        receiver.assert_refused();
    }
    let mut cfg = ObservabilityConfig {
        from_env: false,
        log_bridge: Some(true),
        logs_exporter: Some(vec![ExporterKind::Otlp]),
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![]),
        ..Default::default()
    };
    cfg.otlp.endpoint = Some(receiver.url());
    cfg.log_batch.scheduled_delay = Some(Duration::from_secs(3600));
    let _pipeline = aster_otel::init(cfg).unwrap();
    let mut drain = aster_otel::bridge::drain().unwrap();
    let emit = |message: &str| {
        aster_otel::bridge::emit_app_log(&AppLogRecord {
            level: Some(LogLevel::Warn),
            timestamp_unix_ns: 0,
            logger: "app.native",
            message,
            fields: &[("phase", "recovered")],
            context: Some(([7; 16], [8; 8], 1)),
        })
        .unwrap()
    };
    let start = Instant::now();
    for i in 0..1000 {
        emit(&format!("native-outage-{i}"));
    }
    assert!(start.elapsed() < Duration::from_secs(1));
    let start = Instant::now();
    assert!(aster_otel::flush(Duration::from_secs(1)).is_err());
    assert!(start.elapsed() < Duration::from_secs(3));
    if !matches!(fault, Fault::Refused) {
        assert!(receiver.failed_requests() > 0);
    }
    assert!(receiver.accepted().is_empty());
    receiver.recover();
    emit("native-recovered");
    let _ = aster_otel::flush(Duration::from_secs(3));
    let recovered = receiver
        .wait_for(b"native-recovered", Duration::from_secs(10))
        .expect("new record after recovery");
    for expected in [b"app.native".as_slice(), b"phase", &[7; 16], &[8; 8]] {
        assert!(recovered.windows(expected.len()).any(|w| w == expected));
    }
    let records = drain.try_drain(4096);
    assert!(!records
        .iter()
        .any(|r| r.message.contains("native-outage") || r.message.contains("native-recovered")));
    receiver.fail(fault);
    if matches!(fault, Fault::Refused) {
        receiver.assert_refused();
    }
    let failed_before_shutdown = receiver.failed_requests();
    emit("native-shutdown-outage");
    let start = Instant::now();
    let _ = aster_otel::shutdown();
    assert!(start.elapsed() < Duration::from_secs(6));
    assert!(aster_otel::state().is_none());
    if !matches!(fault, Fault::Refused) {
        assert!(receiver.failed_requests() > failed_before_shutdown);
    }
    receiver.recover();
    println!("case-ok");
}
#[test]
fn app_logs_http503_recover() {
    common::run("http503", &[]);
}
#[test]
fn app_logs_connection_refusal_recover() {
    common::run("refused", &[]);
}
#[test]
fn app_logs_stalled_collector_recover() {
    common::run("stall", &[]);
}
