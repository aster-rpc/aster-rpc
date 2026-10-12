//! Delegated systemd unit layout: empty root, supervisor/ and payload/ leaves.

use crate::contract::ResourceLimitsV1;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command};

#[derive(Clone, Debug)]
pub struct PayloadIdentity {
    pub uid: u32,
    pub gid: u32,
    pub supplementary_groups: Vec<u32>,
}

pub struct DelegatedJobCgroup {
    root: PathBuf,
    payload: PathBuf,
    initial_oom_kill: u64,
    file_bytes: Option<u64>,
}

fn unavailable(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

fn cgroup_root_from_proc(contents: &str) -> io::Result<PathBuf> {
    let mut lines = contents.lines();
    let line = lines
        .next()
        .ok_or_else(|| unavailable("missing cgroup v2 membership"))?;
    let relative = line
        .strip_prefix("0::/")
        .ok_or_else(|| unavailable("not in a unified cgroup v2 hierarchy"))?;
    let unit_name = Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str());
    if lines.next().is_some()
        || Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || !unit_name
            .is_some_and(|name| name.starts_with("aster-exec-job-") && name.ends_with(".service"))
    {
        return Err(unavailable("ambiguous cgroup membership"));
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

fn metric(path: &Path, name: &str) -> io::Result<u64> {
    let contents = fs::read_to_string(path)?;
    contents
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find_map(|(key, value)| (key == name).then(|| value.parse::<u64>().ok()).flatten())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing cgroup metric"))
}

fn cpu_max(millis_per_second: u32) -> io::Result<String> {
    if millis_per_second == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero CPU quota",
        ));
    }
    Ok(format!("{} 100000", u64::from(millis_per_second) * 100))
}

impl DelegatedJobCgroup {
    /// Called by the per-job supervisor as the only initial process in its
    /// systemd unit. Refuses missing delegation/controllers rather than
    /// launching without the advertised cgroup guarantees.
    pub fn enter(limits: &ResourceLimitsV1) -> io::Result<Self> {
        let root = cgroup_root_from_proc(&fs::read_to_string("/proc/self/cgroup")?)?;
        if !root.join("cgroup.kill").exists() {
            return Err(unavailable("cgroup.kill is unavailable"));
        }
        let controllers = fs::read_to_string(root.join("cgroup.controllers"))?;
        for required in ["cpu", "memory", "pids"] {
            if !controllers
                .split_whitespace()
                .any(|controller| controller == required)
            {
                return Err(unavailable("required cgroup controller unavailable"));
            }
        }
        let supervisor = root.join("supervisor");
        let payload = root.join("payload");
        fs::create_dir(&supervisor)?;
        fs::create_dir(&payload)?;
        fs::write(supervisor.join("cgroup.procs"), b"0")?;
        if !fs::read_to_string(root.join("cgroup.procs"))?
            .trim()
            .is_empty()
        {
            return Err(unavailable("delegated unit root is populated"));
        }
        fs::write(root.join("cgroup.subtree_control"), b"+cpu +memory +pids")?;
        if let Some(cpu) = limits.cpu_millis_per_second {
            fs::write(payload.join("cpu.max"), cpu_max(cpu)?)?;
        }
        if let Some(memory) = limits.memory_bytes {
            fs::write(payload.join("memory.max"), memory.to_string())?;
            fs::write(payload.join("memory.swap.max"), b"0")?;
        }
        if let Some(processes) = limits.processes {
            fs::write(payload.join("pids.max"), processes.to_string())?;
        }
        fs::write(payload.join("memory.oom.group"), b"1")?;
        let initial_oom_kill = metric(&payload.join("memory.events.local"), "oom_kill")?;
        Ok(Self {
            root,
            payload,
            initial_oom_kill,
            file_bytes: limits.file_bytes,
        })
    }

    /// Move the child before exec, so no payload instruction runs in the
    /// supervisor cgroup. The open descriptor is kept alive across fork until
    /// `spawn` returns; the child writes "0" for its own PID before exec.
    pub fn spawn(
        &self,
        command: &mut Command,
        identity: PayloadIdentity,
        cwd: &Path,
    ) -> io::Result<Child> {
        let procs = OpenOptions::new()
            .write(true)
            .open(self.payload.join("cgroup.procs"))?;
        let fd = procs.as_raw_fd();
        let file_bytes = self.file_bytes;
        use std::os::unix::ffi::OsStrExt;
        let cwd = std::ffi::CString::new(cwd.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cwd contains NUL"))?;
        // SAFETY: pre-exec uses only raw syscalls on preexisting memory and a
        // descriptor opened in the parent. Account switching follows cgroup
        // placement, so an unprivileged payload never needs cgroup write access.
        unsafe {
            command.pre_exec(move || {
                let written = libc::write(fd, b"0".as_ptr().cast(), 1);
                if written != 1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setgroups(
                    identity.supplementary_groups.len(),
                    identity.supplementary_groups.as_ptr().cast(),
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if let Some(bytes) = file_bytes {
                    let limit = libc::rlimit {
                        rlim_cur: bytes as libc::rlim_t,
                        rlim_max: bytes as libc::rlim_t,
                    };
                    if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                // Prevent setuid/file-capability executables from reacquiring privilege.
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setgid(identity.gid) != 0 || libc::setuid(identity.uid) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // Folder access must be checked with the payload identity.
                if libc::chdir(cwd.as_ptr()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn()
    }

    pub fn signal_payload(&self, signal: i32) -> io::Result<()> {
        for line in fs::read_to_string(self.payload.join("cgroup.procs"))?.lines() {
            let pid = line
                .parse::<i32>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid cgroup PID"))?;
            // Pin process identity before signalling so PID reuse cannot make
            // the privileged supervisor signal an unrelated host process.
            use std::os::fd::{FromRawFd, OwnedFd};
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(error);
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // The payload cannot write its cgroup membership; still check it
            // after opening the pidfd to cover a PID recycled before open.
            let expected = self
                .payload
                .strip_prefix("/sys/fs/cgroup")
                .map_err(io::Error::other)?;
            let membership = match fs::read_to_string(format!("/proc/{pid}/cgroup")) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if membership.trim() != format!("0::{}", expected.display()) {
                continue;
            }
            if unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            } != 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub fn kill_payload(&self) -> io::Result<()> {
        fs::write(self.payload.join("cgroup.kill"), b"1")
    }

    pub fn payload_oom_killed(&self) -> io::Result<bool> {
        Ok(metric(&self.payload.join("memory.events.local"), "oom_kill")? > self.initial_oom_kill)
    }

    pub fn payload_empty(&self) -> io::Result<bool> {
        Ok(metric(&self.payload.join("cgroup.events"), "populated")? == 0)
    }

    pub fn matches_job(&self, job_id: &str) -> bool {
        self.root
            .file_name()
            .is_some_and(|name| crate::systemd_unit::matches_unit(name, job_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires explicit root qualification in an isolated temporary directory"]
    fn privilege_drop_precedes_cwd_traversal() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        assert_eq!(
            std::env::var("ASTER_EXEC_PRIVILEGED_TEST").as_deref(),
            Ok("1")
        );
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let cwd = temp.path().join("visible-child");
        fs::create_dir(&cwd).unwrap();
        fs::set_permissions(&cwd, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(temp.path().join("cgroup.procs"), "").unwrap();
        let cgroup = DelegatedJobCgroup {
            root: temp.path().into(),
            payload: temp.path().into(),
            initial_oom_kill: 0,
            file_bytes: None,
        };
        let error = cgroup
            .spawn(
                &mut Command::new("/usr/bin/pwd"),
                PayloadIdentity {
                    uid: 65534,
                    gid: 65534,
                    supplementary_groups: vec![],
                },
                &cwd,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn rejects_ambiguous_membership_and_calculates_cpu_quota() {
        assert_eq!(
            cgroup_root_from_proc("0::/system.slice/aster-exec-job-123.service\n").unwrap(),
            Path::new("/sys/fs/cgroup/system.slice/aster-exec-job-123.service")
        );
        assert!(cgroup_root_from_proc("1:name=systemd:/x\n").is_err());
        assert!(cgroup_root_from_proc("0::/system.slice/other.service\n").is_err());
        assert_eq!(cpu_max(1000).unwrap(), "100000 100000");
        assert_eq!(cpu_max(250).unwrap(), "25000 100000");
    }
}
