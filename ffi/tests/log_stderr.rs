//! O0 smoke test: `ASTER_LOG_STDERR=1` makes `aster_runtime_new` install the
//! Rust stderr log sink (the C ABI path Java, .NET and Go load through). The
//! global subscriber is once per process, so the check runs in a child.

use std::process::Command;
use std::ptr;
use std::time::Duration;

use aster_transport_ffi::*;

const CHILD: &str = "ASTER_FFI_LOG_CHILD";

#[test]
fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    unsafe {
        let mut rt = 0;
        assert_eq!(
            aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
            0
        );
        assert_eq!(aster_runtime_close_and_wait(rt, Duration::from_secs(10)), 0);
    }
}

fn child_stderr(enable: bool) -> String {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env_remove("ASTER_LOG");
    if enable {
        cmd.env("ASTER_LOG_STDERR", "1");
    } else {
        cmd.env_remove("ASTER_LOG_STDERR");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "child failed: {out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn runtime_new_installs_the_stderr_sink_when_asked() {
    let on = child_stderr(true);
    assert!(
        on.contains("aster::otel") && on.contains("stderr logging enabled"),
        "{on}"
    );
    let off = child_stderr(false);
    assert!(!off.contains("stderr logging enabled"), "{off}");
}
