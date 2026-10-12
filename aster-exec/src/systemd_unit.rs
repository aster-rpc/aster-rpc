//! Fixed systemd transient-unit template used only by the privileged helper.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const UNIT_PREFIX: &str = "aster-exec-job-";
/// The fixed systemctl the helper validates and runs.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// A supervisor that outlives the longest job by this much is stopped.
const RUNTIME_MAX: &str = "RuntimeMaxSec=25h";

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
        OsString::from(format!("--property={RUNTIME_MAX}")),
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
    match run_bounded(systemd_run, &args, Duration::from_secs(5)) {
        Ok(_) => Ok(()),
        Err(error) => {
            if error.kind() == io::ErrorKind::TimedOut {
                // systemd may still hold the queued start. Cancel it so the
                // supervisor cannot start after this launch reported failure.
                if let Err(stop_error) = stop(job_id) {
                    eprintln!("cannot cancel timed-out start of job {job_id}: {stop_error}");
                }
            }
            Err(error)
        }
    }
}

/// Stop a job unit, cancelling any queued start.
pub fn stop(job_id: &str) -> io::Result<()> {
    let name = unit_name(job_id)?;
    run_bounded(
        Path::new(SYSTEMCTL),
        &[OsString::from("stop"), OsString::from(name)],
        Duration::from_secs(15),
    )
    .map(|_| ())
}

/// Run a fixed command with a cleared environment and a deadline. Returns its
/// standard output (bounded); a failure carries its standard error.
fn run_bounded(program: &Path, args: &[OsString], limit: Duration) -> io::Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            let read = |pipe: Option<&mut dyn Read>| {
                let mut text = String::new();
                if let Some(pipe) = pipe {
                    let _ = pipe.take(4096).read_to_string(&mut text);
                }
                text
            };
            let stdout = read(child.stdout.as_mut().map(|p| p as &mut dyn Read));
            if status.success() {
                return Ok(stdout);
            }
            let stderr = read(child.stderr.as_mut().map(|p| p as &mut dyn Read));
            return Err(io::Error::other(format!(
                "{} exited with {status}: {}",
                program.display(),
                stderr.trim()
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} timed out", program.display()),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Fail closed while systemd still owns a live or starting supervisor. A dead
/// endpoint alone is never evidence that payload descendants have stopped.
pub fn confirm_stopped(job_id: &str) -> io::Result<()> {
    let name = unit_name(job_id)?;
    let state = run_bounded(
        Path::new(SYSTEMCTL),
        &[
            OsString::from("show"),
            OsString::from("--property=ActiveState"),
            OsString::from("--value"),
            OsString::from(name),
        ],
        Duration::from_secs(2),
    )
    .map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut => error,
        _ => io::Error::other("cannot verify supervisor unit state"),
    })?;
    match state.trim() {
        "inactive" | "failed" => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "supervisor unit still active; retry its control endpoint",
        )),
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
