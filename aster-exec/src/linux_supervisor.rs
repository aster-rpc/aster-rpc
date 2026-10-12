//! Per-job Linux supervisor. A systemd transient unit owns this process.

use crate::contract::{
    CancelReceiptV1, DEFAULT_CANCEL_GRACE_MS, ExecRequestV1, JobStateV1, JobStatusV1,
    MAX_CANCEL_GRACE_MS, OutputStreamV1, ResourceLimitsV1,
};
use crate::linux_cgroup::{DelegatedJobCgroup, PayloadIdentity};
use crate::local_control::{ControlRequestV1, ControlResponseV1};
use crate::spool::Spool;
use crate::state::{JobStore, StoreError};
use crate::unix_millis as now_ms;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorSpecV1 {
    pub request: ExecRequestV1,
    pub status: JobStatusV1,
    pub request_digest: [u8; 32],
    pub job_directory: PathBuf,
    pub control_socket: PathBuf,
    pub api_uid: u32,
    pub api_gid: u32,
    pub payload_uid: u32,
    pub payload_gid: u32,
    pub payload_groups: Vec<u32>,
    pub resource_limits: ResourceLimitsV1,
}

#[derive(Clone, Copy)]
enum StopReason {
    Cancelled,
    TimedOut,
    MemoryLimit,
}

fn terminal_result(stop: Option<StopReason>, successful_exit: bool) -> (JobStateV1, &'static str) {
    match stop {
        Some(StopReason::Cancelled) => (JobStateV1::Cancelled, "cancelled"),
        Some(StopReason::TimedOut) => (JobStateV1::TimedOut, "deadline"),
        Some(StopReason::MemoryLimit) => (JobStateV1::Failed, "memory_limit"),
        None if successful_exit => (JobStateV1::Succeeded, "exited"),
        None => (JobStateV1::Failed, "nonzero_exit"),
    }
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials.uid)
}

fn read_pipe(
    mut pipe: impl Read + Send + 'static,
    stream: OutputStreamV1,
    tx: SyncSender<(OutputStreamV1, Vec<u8>)>,
) {
    let mut buffer = [0u8; crate::spool::MAX_FRAME_BYTES];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                if tx.send((stream, buffer[..size].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
}

fn drain_output(
    rx: &Receiver<(OutputStreamV1, Vec<u8>)>,
    spool: &mut Spool,
    store: &mut JobStore,
) -> io::Result<bool> {
    for _ in 0..64 {
        match rx.try_recv() {
            Ok((stream, bytes)) => {
                let appended = spool.append(stream, &bytes)?;
                store
                    .advance_output(appended.next_sequence, appended.truncated)
                    .map_err(other)?;
            }
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => return Ok(true),
        }
    }
    Ok(false)
}

fn handle_control(
    mut stream: UnixStream,
    store: &JobStore,
    spool: &mut Spool,
    cgroup: &DelegatedJobCgroup,
    stop: &mut Option<(StopReason, Instant)>,
) -> io::Result<()> {
    // Only the root helper talks to supervisors.
    if peer_uid(&stream)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control peer is not the Exec helper",
        ));
    }
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut bytes = Vec::new();
    let read_deadline = Instant::now() + Duration::from_millis(50);
    loop {
        let remaining = read_deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "control read deadline"))?;
        stream.set_read_timeout(Some(remaining))?;
        let mut buffer = [0u8; 4097];
        let size = stream.read(&mut buffer[..(4097 - bytes.len())])?;
        if size == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..size]);
        if bytes.len() > 4096 {
            break;
        }
    }
    if bytes.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control request too large",
        ));
    }
    let request: ControlRequestV1 = serde_json::from_slice(&bytes).map_err(other)?;
    let response = match request {
        ControlRequestV1::Status => {
            ControlResponseV1::Status(Box::new(store.state().status.clone()))
        }
        ControlRequestV1::Output { cursor, max_bytes } => {
            match spool.page(cursor, max_bytes, store.state().status.state.is_terminal()) {
                Ok(page) => ControlResponseV1::Output(page),
                Err(error) => ControlResponseV1::Error(crate::linux_helper::safe_error(&error)),
            }
        }
        ControlRequestV1::Cancel { grace_ms } => {
            let already_terminal =
                store.state().status.state.is_terminal() || cgroup.payload_empty()?;
            if !already_terminal && stop.is_none() {
                let grace = grace_ms.unwrap_or(DEFAULT_CANCEL_GRACE_MS);
                if grace > MAX_CANCEL_GRACE_MS {
                    let error =
                        io::Error::new(io::ErrorKind::InvalidInput, "cancel grace exceeds limit");
                    let response =
                        ControlResponseV1::Error(crate::linux_helper::safe_error(&error));
                    stream.write_all(&serde_json::to_vec(&response).map_err(other)?)?;
                    return stream.flush();
                }
                cgroup.signal_payload(libc::SIGTERM)?;
                *stop = Some((
                    StopReason::Cancelled,
                    Instant::now() + Duration::from_millis(grace as u64),
                ));
            }
            ControlResponseV1::Cancel(CancelReceiptV1 {
                job: store.state().status.job.clone(),
                accepted_at_ms: now_ms(),
                already_terminal,
            })
        }
    };
    let bytes = serde_json::to_vec(&response).map_err(other)?;
    let write_deadline = Instant::now() + Duration::from_millis(500);
    let mut written = 0;
    while written < bytes.len() {
        let remaining = write_deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "control write deadline"))?;
        stream.set_write_timeout(Some(remaining))?;
        let count = stream.write(&bytes[written..])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "control socket closed",
            ));
        }
        written += count;
    }
    stream.flush()
}

/// The durable audit outcome for a terminal job. The helper uses the same
/// record when it settles a job whose supervisor was lost.
pub(crate) fn terminal_outcome(status: &JobStatusV1) -> io::Result<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({
        "timestamp_ms": status.finished_at_ms.unwrap_or_else(now_ms),
        "kind": "exec.outcome.v1",
        "payload": {
        "version": 1,
        "job_id": status.job.job_id,
        "target_id": status.job.target_id,
        "state": status.state,
        "reason_code": status.reason_code,
        "finished_at_ms": status.finished_at_ms,
        "exit_code": status.exit_code,
        "signal": status.signal,
        }
    }))
    .map_err(other)
}

/// Run one sealed job. Only the privileged unit-creation helper may write the
/// spec, and this binary refuses execution outside its fixed systemd unit.
pub fn run(spec: SupervisorSpecV1) -> io::Result<JobStatusV1> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "supervisor requires privileged unit identity",
        ));
    }
    spec.request.validate().map_err(other)?;
    if spec.status.state != JobStateV1::Accepted
        || spec.request.target_id != spec.status.job.target_id
        || !std::path::Path::new(&spec.request.cwd).is_absolute()
        || spec.payload_uid == 0
        || spec.payload_uid == spec.api_uid
        || spec.payload_gid == spec.api_gid
        || spec.payload_groups.contains(&spec.api_gid)
        || spec
            .request
            .canonical_digest(&spec.status.guarantees.profile)
            .map_err(other)?
            != spec.request_digest
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sealed job does not match accepted request",
        ));
    }
    let cgroup = DelegatedJobCgroup::enter(&spec.resource_limits)?;
    if !cgroup.matches_job(&spec.status.job.job_id) {
        return Err(other("supervisor unit does not match sealed job"));
    }
    let mut store = JobStore::open(&spec.job_directory).map_err(other)?;
    if store.state().request_digest != spec.request_digest || store.state().status != spec.status {
        return Err(other("durable admission does not match sealed spec"));
    }
    // Only root (the helper) reaches the job directory and its socket.
    chown(&spec.job_directory, Some(0), Some(0))?;
    fs::set_permissions(&spec.job_directory, fs::Permissions::from_mode(0o700))?;
    let mut spool = Spool::open(
        &spec.job_directory.join("output.log"),
        0,
        spec.request
            .output_limit_bytes
            .unwrap_or(crate::contract::DEFAULT_OUTPUT_BYTES),
    )?;
    if spec.control_socket.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "control endpoint already exists",
        ));
    }
    let listener = UnixListener::bind(&spec.control_socket)?;
    fs::set_permissions(&spec.control_socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    store
        .transition(JobStateV1::Starting, now_ms(), None, None, None, None)
        .map_err(other)?;
    let mut command = Command::new(&spec.request.executable);
    command
        .args(&spec.request.argv)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PAGER", "cat")
        .env("CI", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if spec.request.stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    for (key, value) in &spec.request.environment_overrides {
        command.env(key, value);
    }
    let identity = PayloadIdentity {
        uid: spec.payload_uid,
        gid: spec.payload_gid,
        supplementary_groups: spec.payload_groups.clone(),
    };
    let mut child = match cgroup.spawn(
        &mut command,
        identity,
        std::path::Path::new(&spec.request.cwd),
    ) {
        Ok(child) => child,
        Err(error) => {
            let finished_at = now_ms();
            let mut status = store.state().status.clone();
            status.state = JobStateV1::Failed;
            status.finished_at_ms = Some(finished_at);
            status.reason_code = Some("launch_error".into());
            let outcome = terminal_outcome(&status)?;
            store
                .transition(
                    JobStateV1::Failed,
                    finished_at,
                    None,
                    None,
                    status.reason_code.clone(),
                    Some(outcome),
                )
                .map_err(other)?;
            let _ = fs::remove_file(&spec.control_socket);
            return Err(error);
        }
    };
    store
        .transition(JobStateV1::Running, now_ms(), None, None, None, None)
        .map_err(other)?;
    if let (Some(bytes), Some(mut stdin)) = (spec.request.stdin_bytes.as_ref(), child.stdin.take())
    {
        let bytes = bytes.clone();
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let (tx, rx) = mpsc::sync_channel(32);
    if let Some(stdout) = child.stdout.take() {
        let sender = tx.clone();
        thread::spawn(move || read_pipe(stdout, OutputStreamV1::Stdout, sender));
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = tx.clone();
        thread::spawn(move || read_pipe(stderr, OutputStreamV1::Stderr, sender));
    }
    drop(tx);
    let timeout = Duration::from_millis(
        spec.request
            .execution_timeout_ms
            .unwrap_or(crate::contract::DEFAULT_TIMEOUT_MS),
    );
    let deadline = Instant::now() + timeout;
    let mut stop: Option<(StopReason, Instant)> = None;
    let mut exit = None;
    let exit = loop {
        let output_closed = drain_output(&rx, &mut spool, &mut store)?;
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = handle_control(stream, &store, &mut spool, &cgroup, &mut stop);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => eprintln!("supervisor accept failed: {error}"),
        }
        if exit.is_none() {
            exit = child.try_wait()?;
        }
        if stop.is_none()
            && spec.resource_limits.memory_bytes.is_some()
            && cgroup.payload_oom_killed()?
        {
            cgroup.kill_payload()?;
            stop = Some((StopReason::MemoryLimit, Instant::now()));
        }
        if stop.is_none() && Instant::now() >= deadline {
            cgroup.signal_payload(libc::SIGTERM)?;
            stop = Some((
                StopReason::TimedOut,
                Instant::now() + Duration::from_millis(DEFAULT_CANCEL_GRACE_MS as u64),
            ));
        }
        if let Some((_, hard_deadline)) = stop
            && Instant::now() >= hard_deadline
            && !cgroup.payload_empty()?
        {
            cgroup.kill_payload()?;
        }
        if let Some(exit) = exit
            && cgroup.payload_empty()?
            && output_closed
        {
            break exit;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let (state, reason) = terminal_result(stop.map(|(reason, _)| reason), exit.success());
    let finished_at = now_ms();
    let mut status = store.state().status.clone();
    status.state = state;
    status.finished_at_ms = Some(finished_at);
    status.exit_code = exit.code();
    status.reason_code = Some(reason.into());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal = exit.signal();
    }
    let outcome = terminal_outcome(&status)?;
    store
        .transition(
            state,
            finished_at,
            status.exit_code,
            status.signal,
            status.reason_code.clone(),
            Some(outcome),
        )
        .map_err(|e: StoreError| other(e))?;
    let _ = fs::remove_file(&spec.control_socket);
    Ok(store.state().status.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_and_payload_oom_classify_as_terminal_outcomes() {
        assert_eq!(
            terminal_result(Some(StopReason::Cancelled), false),
            (JobStateV1::Cancelled, "cancelled")
        );
        assert_eq!(
            terminal_result(Some(StopReason::MemoryLimit), false),
            (JobStateV1::Failed, "memory_limit")
        );
    }
}
