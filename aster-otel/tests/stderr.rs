//! The O0 stderr sink, end to end. The global subscriber is once per process,
//! so each case re-runs this test binary as a child with its own environment
//! and inspects the child's stderr.

use std::process::Command;

use aster_otel::{init_stderr_from_env, StderrInit};

const CHILD: &str = "ASTER_OTEL_TEST_CHILD";

/// The child side: initialise, emit one event per level on an `aster` target
/// and one on a foreign target, and report the outcome on stdout.
#[test]
fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let outcome = init_stderr_from_env();
    assert_eq!(init_stderr_from_env(), outcome, "idempotent");
    tracing::debug!(target: "aster::probe", "probe-debug");
    tracing::info!(target: "aster::probe", "probe-info");
    tracing::warn!(target: "other::probe", "probe-foreign-warn");
    tracing::info!(target: "other::probe", "probe-foreign-info");
    println!("outcome={outcome:?}");
}

fn run(env: &[(&str, &str)]) -> (String, String) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env_remove("ASTER_LOG_STDERR")
        .env_remove("ASTER_LOG");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "child failed: {out:?}");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn disabled_by_default_and_silent() {
    let (stdout, stderr) = run(&[]);
    assert!(
        stdout.contains(&format!("outcome={:?}", StderrInit::Disabled)),
        "{stdout}"
    );
    assert!(!stderr.contains("probe-"), "{stderr}");
}

#[test]
fn enabled_uses_the_default_filter() {
    let (stdout, stderr) = run(&[("ASTER_LOG_STDERR", "1")]);
    assert!(
        stdout.contains(&format!("outcome={:?}", StderrInit::Installed)),
        "{stdout}"
    );
    assert!(
        stderr.contains("aster::otel") && stderr.contains("stderr logging enabled"),
        "{stderr}"
    );
    // warn,aster=info: aster info yes, aster debug no, foreign warn yes, foreign info no.
    assert!(stderr.contains("probe-info"), "{stderr}");
    assert!(!stderr.contains("probe-debug"), "{stderr}");
    assert!(stderr.contains("probe-foreign-warn"), "{stderr}");
    assert!(!stderr.contains("probe-foreign-info"), "{stderr}");
}

#[test]
fn aster_log_directives_apply() {
    let (_, stderr) = run(&[
        ("ASTER_LOG_STDERR", "true"),
        ("ASTER_LOG", "off,aster::probe=debug"),
    ]);
    assert!(
        stderr.contains("probe-debug") && stderr.contains("probe-info"),
        "{stderr}"
    );
    assert!(!stderr.contains("probe-foreign"), "{stderr}");
}

#[test]
fn an_invalid_filter_falls_back_with_a_note() {
    let (stdout, stderr) = run(&[("ASTER_LOG_STDERR", "1"), ("ASTER_LOG", "aster=notalevel")]);
    assert!(
        stdout.contains(&format!("outcome={:?}", StderrInit::Installed)),
        "{stdout}"
    );
    assert!(stderr.contains("ignoring invalid ASTER_LOG"), "{stderr}");
    assert!(
        stderr.contains("probe-info") && !stderr.contains("probe-debug"),
        "{stderr}"
    );
}
