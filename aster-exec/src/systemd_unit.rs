//! Fixed systemd transient-unit template used only by the privileged helper.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const UNIT_PREFIX: &str = "aster-exec-job-";

pub fn unit_name(job_id: &str) -> io::Result<String> {
    let uuid = uuid::Uuid::parse_str(job_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid job UUID"))?;
    Ok(format!("{UNIT_PREFIX}{}.service", uuid.simple()))
}

fn unit_args(job_id: &str, supervisor: &Path, sealed_spec: &Path) -> io::Result<Vec<OsString>> {
    if !supervisor.is_absolute() || !sealed_spec.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unit paths must be absolute",
        ));
    }
    let name = unit_name(job_id)?;
    Ok(vec![
        OsString::from("--quiet"),
        OsString::from(format!("--unit={name}")),
        OsString::from("--property=Type=exec"),
        OsString::from("--property=Delegate=yes"),
        OsString::from("--property=Restart=no"),
        OsString::from("--property=CollectMode=inactive-or-failed"),
        OsString::from("--property=RuntimeMaxSec=25h"),
        OsString::from("--property=KillMode=control-group"),
        OsString::from("--property=TimeoutStopSec=10s"),
        OsString::from("--property=OOMPolicy=continue"),
        OsString::from("--"),
        supervisor.as_os_str().to_owned(),
        sealed_spec.as_os_str().to_owned(),
    ])
}

/// Create a fixed transient service. The caller must be the root-owned local
/// helper; never pass arguments from a remote request directly to systemd-run.
pub fn create(
    job_id: &str,
    supervisor: &Path,
    sealed_spec: &Path,
    systemd_run: &Path,
) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unit helper must run as root",
        ));
    }
    if !systemd_run.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "systemd-run path must be absolute",
        ));
    }
    let args = unit_args(job_id, supervisor, sealed_spec)?;
    let mut child = Command::new(systemd_run)
        .args(&args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(io::Error::other(format!(
                "systemd-run exited with {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "systemd-run timed out",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Fail closed while systemd still owns a live or starting supervisor. A dead
/// endpoint alone is never evidence that payload descendants have stopped.
pub fn confirm_stopped(job_id: &str) -> io::Result<()> {
    let name = unit_name(job_id)?;
    let mut child = Command::new("/usr/bin/systemctl")
        .args(["show", "--property=ActiveState", "--value", &name])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other("cannot verify supervisor unit state"));
            }
            let mut state = String::new();
            use std::io::Read;
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing systemctl output"))?
                .take(1024)
                .read_to_string(&mut state)?;
            return match state.trim() {
                "inactive" | "failed" => Ok(()),
                _ => Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "supervisor unit still active; retry its control endpoint",
                )),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "unit recovery check timed out",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn matches_unit(name: &OsStr, job_id: &str) -> bool {
    unit_name(job_id).is_ok_and(|expected| name == OsStr::new(&expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires explicit root systemd qualification"]
    fn back_to_back_units_are_started_before_create_returns() {
        assert_eq!(
            std::env::var("ASTER_EXEC_PRIVILEGED_TEST").as_deref(),
            Ok("1")
        );
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let supervisor = temp.path().join("supervisor");
        std::fs::write(&supervisor, "#!/bin/sh\nexec /usr/bin/sleep 10\n").unwrap();
        std::fs::set_permissions(&supervisor, std::fs::Permissions::from_mode(0o700)).unwrap();
        let spec = temp.path().join("spec");
        std::fs::write(&spec, "{}").unwrap();
        struct Units(Vec<String>);
        impl Drop for Units {
            fn drop(&mut self) {
                for id in &self.0 {
                    let _ = Command::new("/usr/bin/systemctl")
                        .args(["stop", &unit_name(id).unwrap()])
                        .status();
                }
            }
        }
        let mut units = Units(Vec::new());
        for _ in 0..2 {
            let id = uuid::Uuid::new_v4().to_string();
            units.0.push(id.clone());
            create(&id, &supervisor, &spec, Path::new("/usr/bin/systemd-run")).unwrap();
            assert_eq!(
                confirm_stopped(&id).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }

    #[test]
    fn unit_template_delegates_without_limit_on_supervisor() {
        let id = "f8a1f89d-5312-4c97-9070-a3659ed77b3b";
        let args = unit_args(
            id,
            Path::new("/usr/libexec/aster-exec-supervisor"),
            Path::new("/var/lib/aster-exec/sealed/job.json"),
        )
        .unwrap();
        let texts: Vec<_> = args
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert!(texts.contains(&"--property=Delegate=yes".into()));
        assert!(texts.contains(&"--property=OOMPolicy=continue".into()));
        assert!(
            !texts
                .iter()
                .any(|arg| arg.starts_with("--property=MemoryMax="))
        );
        assert!(
            !texts
                .iter()
                .any(|arg| arg.starts_with("--property=CPUQuota="))
        );
        assert_eq!(texts[2], "--property=Type=exec");
        assert!(matches_unit(
            OsStr::new("aster-exec-job-f8a1f89d53124c979070a3659ed77b3b.service"),
            id
        ));
    }
}
