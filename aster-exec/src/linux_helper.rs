//! Root-owned fixed-configuration launch and recovery boundary.
use crate::{
    contract::*,
    linux_supervisor::SupervisorSpecV1,
    local_control::{self, ControlRequestV1, ControlResponseV1},
    state::JobStore,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn terminal_index() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, JobStatusV1>> {
    static INDEX: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, JobStatusV1>>,
    > = std::sync::OnceLock::new();
    INDEX.get_or_init(Default::default)
}
const MAX_MESSAGE: u64 = MAX_LOCAL_MESSAGE_BYTES as u64;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperConfig {
    pub socket: PathBuf,
    pub state_root: PathBuf,
    pub supervisor: PathBuf,
    pub systemd_run: PathBuf,
    pub api_uid: u32,
    pub api_gid: u32,
    pub payload_uid: u32,
    pub payload_gid: u32,
    pub payload_groups: Vec<u32>,
    pub resource_limits: ResourceLimitsV1,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperRequestV1 {
    Launch {
        request: Box<ExecRequestV1>,
        status: Box<JobStatusV1>,
        request_digest: [u8; 32],
    },
    Status {
        job_id: String,
    },
    Output {
        job_id: String,
        cursor: u64,
        max_bytes: u32,
    },
    Cancel {
        job_id: String,
        grace_ms: Option<u32>,
    },
    Recover {
        job_id: String,
    },
    Outcome {
        job_id: String,
    },
    AckOutcome {
        job_id: String,
    },
    Ping,
    EnumerateOutcomes,
    Enumerate,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HelperResponseV1 {
    Status(Box<JobStatusV1>),
    Output(OutputPageV1),
    Cancel(CancelReceiptV1),
    Outcome(Option<Vec<u8>>),
    Ack,
    Jobs(Vec<JobStatusV1>),
    Error(ExecErrorV1),
}
pub fn safe_error(error: &io::Error) -> ExecErrorV1 {
    let (code, message) = match error.kind() {
        io::ErrorKind::OutOfMemory => (
            ExecErrorCodeV1::ResourceExhausted,
            "job storage admission limit reached",
        ),
        io::ErrorKind::NotFound => (ExecErrorCodeV1::JobNotFound, "job not found"),
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => (
            ExecErrorCodeV1::InvalidArgument,
            "invalid local job request",
        ),
        io::ErrorKind::PermissionDenied => {
            (ExecErrorCodeV1::PermissionDenied, "job operation denied")
        }
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => (
            ExecErrorCodeV1::SupervisorUnavailable,
            "supervisor unavailable",
        ),
        _ => (ExecErrorCodeV1::BackendFailure, "job backend failed"),
    };
    ExecErrorV1 {
        code,
        safe_message: message.into(),
        request_id: None,
        job_id: None,
        retryable: matches!(code, ExecErrorCodeV1::SupervisorUnavailable),
        retry_after_ms: None,
    }
}
fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn private_root(path: &Path, gid: u32) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(other("helper paths must be absolute"));
    }
    // Every ancestor must be root-owned and not writable by group/other.
    for ancestor in path.ancestors() {
        let m = fs::symlink_metadata(ancestor)?;
        if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(other("unsafe helper path ancestor"));
        }
    }
    std::os::unix::fs::chown(path, Some(0), Some(gid))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o710))
}
// Linux pathname Unix sockets include a trailing NUL in sun_path[108].
fn validate_socket_path(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if !path.is_absolute() || path.as_os_str().as_bytes().len() >= 108 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "helper socket pathname exceeds Linux Unix socket capacity",
        ));
    }
    Ok(())
}
fn id(id: &str) -> io::Result<String> {
    Ok(uuid::Uuid::parse_str(id)
        .map_err(other)?
        .hyphenated()
        .to_string())
}
impl HelperConfig {
    pub fn validate(&self) -> io::Result<()> {
        validate_socket_path(&self.socket)?;
        validate_socket_path(
            &self
                .state_root
                .join("ffffffff-ffff-ffff-ffff-ffffffffffff")
                .join("control.sock"),
        )?;
        if unsafe { libc::geteuid() } != 0
            || self.api_uid == 0
            || self.payload_uid == 0
            || self.payload_gid == 0
            || self.payload_groups.contains(&0)
            || self.api_uid == self.payload_uid
            || self.api_gid == self.payload_gid
            || self.payload_groups.contains(&self.api_gid)
        {
            return Err(other("invalid helper execution identity"));
        }
        private_root(&self.state_root, self.api_gid)?;
        for binary in [
            &self.supervisor,
            &self.systemd_run,
            Path::new("/usr/bin/systemctl"),
        ] {
            if !binary.is_absolute() {
                return Err(other("helper binary must be absolute"));
            }
            let m = fs::symlink_metadata(binary)?;
            if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 {
                return Err(other("unsafe helper executable"));
            }
            for ancestor in binary
                .parent()
                .ok_or_else(|| other("missing binary parent"))?
                .ancestors()
            {
                let m = fs::symlink_metadata(ancestor)?;
                if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
                    return Err(other("unsafe helper binary ancestor"));
                }
            }
        }
        Ok(())
    }
    fn directory(&self, job: &str) -> io::Result<PathBuf> {
        Ok(self.state_root.join(id(job)?))
    }
    fn cleanup_and_budget(&self) -> io::Result<(usize, u64)> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut active = 0usize;
        let mut reserved = 0u64;
        for entry in fs::read_dir(&self.state_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if id(name).is_err() {
                continue;
            }
            let directory = entry.path();
            let status = match self.handle(HelperRequestV1::Status {
                job_id: name.into(),
            }) {
                Ok(HelperResponseV1::Status(status)) => status,
                _ => {
                    // Unreadable jobs may still run: retain a conservative reservation.
                    active += 1;
                    reserved = reserved.saturating_add(MAX_OUTPUT_BYTES);
                    continue;
                }
            };
            reserved = reserved.saturating_add(
                fs::metadata(directory.join("state.redb"))
                    .map(|m| m.len())
                    .unwrap_or(0),
            );
            reserved = reserved.saturating_add(
                fs::metadata(self.state_root.join(format!("{}.json", id(name)?)))
                    .map(|m| m.len())
                    .unwrap_or(0),
            );
            if !status.state.is_terminal() {
                active += 1;
                let sealed = fs::read(self.state_root.join(format!("{}.json", id(name)?)))
                    .ok()
                    .filter(|bytes| bytes.len() <= MAX_LOCAL_MESSAGE_BYTES)
                    .and_then(|bytes| serde_json::from_slice::<SupervisorSpecV1>(&bytes).ok());
                let limit = sealed
                    .map(|spec| {
                        spec.request
                            .output_limit_bytes
                            .unwrap_or(DEFAULT_OUTPUT_BYTES)
                    })
                    .unwrap_or(MAX_OUTPUT_BYTES);
                reserved = reserved.saturating_add(limit);
                continue;
            }
            let age = now.saturating_sub(status.finished_at_ms.unwrap_or(now));
            if age >= 24 * 60 * 60 * 1000 {
                match fs::remove_file(directory.join("output.log")) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                };
            } else {
                reserved = reserved.saturating_add(
                    fs::metadata(directory.join("output.log"))
                        .map(|m| m.len())
                        .unwrap_or(0),
                );
            }
            if age >= 7 * 24 * 60 * 60 * 1000 {
                let store = JobStore::open(&directory).map_err(other)?;
                let acknowledged = store.state().pending_audit_outcome.is_none();
                drop(store);
                if acknowledged {
                    fs::remove_dir_all(&directory)?;
                    if let Ok(mut index) = terminal_index().lock() {
                        index.remove(&directory);
                    }
                    match fs::remove_file(self.state_root.join(format!("{}.json", id(name)?))) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    };
                }
            }
        }
        Ok((active, reserved))
    }
    fn launch(
        &self,
        request: ExecRequestV1,
        status: JobStatusV1,
        digest: [u8; 32],
    ) -> io::Result<HelperResponseV1> {
        request.validate().map_err(other)?;
        let (active, reserved) = self.cleanup_and_budget()?;
        if active >= 32
            || reserved.saturating_add(request.output_limit_bytes.unwrap_or(DEFAULT_OUTPUT_BYTES))
                > 4 * 1024 * 1024 * 1024
        {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "job storage admission limit reached",
            ));
        }

        let directory = self.directory(&status.job.job_id)?;
        if status.state != JobStateV1::Accepted
            || status.output_cursor != 0
            || status.guarantees.resource_limits != self.resource_limits
            || request.target_id != status.job.target_id
            || request
                .canonical_digest(&status.guarantees.profile)
                .map_err(other)?
                != digest
        {
            return Err(other("launch does not match admitted job"));
        }
        let store = JobStore::create(&directory, status.clone(), digest).map_err(other)?;
        drop(store);
        std::os::unix::fs::chown(&directory, Some(0), Some(self.api_gid))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o710))?;
        let sealed = self
            .state_root
            .join(format!("{}.json", id(&status.job.job_id)?));
        let spec = SupervisorSpecV1 {
            request,
            status: status.clone(),
            request_digest: digest,
            job_directory: directory.clone(),
            control_socket: directory.join("control.sock"),
            api_uid: self.api_uid,
            api_gid: self.api_gid,
            payload_uid: self.payload_uid,
            payload_gid: self.payload_gid,
            payload_groups: self.payload_groups.clone(),
            resource_limits: self.resource_limits.clone(),
        };
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&sealed)?;
        f.write_all(&serde_json::to_vec(&spec).map_err(other)?)?;
        f.sync_all()?;
        fs::File::open(&self.state_root)?.sync_all()?;
        crate::systemd_unit::create(
            &status.job.job_id,
            &self.supervisor,
            &sealed,
            &self.systemd_run,
        )?;
        Ok(HelperResponseV1::Status(Box::new(status)))
    }
    pub fn handle(&self, request: HelperRequestV1) -> io::Result<HelperResponseV1> {
        let (job, control) = match request {
            HelperRequestV1::Ping => return Ok(HelperResponseV1::Ack),
            HelperRequestV1::EnumerateOutcomes => {
                let mut jobs = Vec::new();
                for entry in fs::read_dir(&self.state_root)? {
                    let Ok(entry) = entry else { continue };
                    if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        continue;
                    }
                    let name = entry.file_name();
                    if name.to_str().is_none_or(|name| id(name).is_err()) {
                        continue;
                    }
                    let Some(job_id) = name.to_str() else {
                        continue;
                    };
                    if let Some(status) = terminal_index()
                        .lock()
                        .map_err(other)?
                        .get(&entry.path())
                        .cloned()
                    {
                        jobs.push(status);
                        continue;
                    }
                    if crate::systemd_unit::confirm_stopped(job_id).is_err() {
                        continue;
                    }
                    match JobStore::open(&entry.path()) {
                        Ok(store) if store.state().status.state.is_terminal() => {
                            jobs.push(store.state().status.clone())
                        }
                        Ok(_) => {}
                        Err(error) => eprintln!("outcome state unavailable: {error}"),
                    }
                }
                return Ok(HelperResponseV1::Jobs(jobs));
            }
            HelperRequestV1::Enumerate => {
                let mut jobs = Vec::new();
                for entry in fs::read_dir(&self.state_root)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if id(name).is_err() {
                        continue;
                    }
                    match self.handle(HelperRequestV1::Recover {
                        job_id: name.into(),
                    })? {
                        HelperResponseV1::Status(status) => jobs.push(*status),
                        HelperResponseV1::Error(error) => {
                            return Ok(HelperResponseV1::Error(error));
                        }
                        _ => return Err(other("incomplete job enumeration")),
                    }
                }
                return Ok(HelperResponseV1::Jobs(jobs));
            }
            HelperRequestV1::Launch {
                request,
                status,
                request_digest,
            } => return self.launch(*request, *status, request_digest),
            HelperRequestV1::Status { job_id } | HelperRequestV1::Recover { job_id } => {
                (job_id, ControlRequestV1::Status)
            }
            HelperRequestV1::Output {
                job_id,
                cursor,
                max_bytes,
            } => (job_id, ControlRequestV1::Output { cursor, max_bytes }),
            HelperRequestV1::Cancel { job_id, grace_ms } => {
                (job_id, ControlRequestV1::Cancel { grace_ms })
            }
            HelperRequestV1::Outcome { job_id } => {
                let s = JobStore::open(&self.directory(&job_id)?).map_err(other)?;
                return Ok(HelperResponseV1::Outcome(
                    s.state().pending_audit_outcome.clone(),
                ));
            }
            HelperRequestV1::AckOutcome { job_id } => {
                let mut s = JobStore::open(&self.directory(&job_id)?).map_err(other)?;
                s.mark_audit_outcome_submitted().map_err(other)?;
                return Ok(HelperResponseV1::Ack);
            }
        };
        let directory = self.directory(&job)?;
        if matches!(control, ControlRequestV1::Status)
            && let Some(status) = terminal_index()
                .lock()
                .map_err(other)?
                .get(&directory)
                .cloned()
        {
            return Ok(HelperResponseV1::Status(Box::new(status)));
        }
        match local_control::call(&directory.join("control.sock"), &control) {
            Ok(response) => {
                return Ok(match response {
                    ControlResponseV1::Status(s) => HelperResponseV1::Status(s),
                    ControlResponseV1::Output(p) => HelperResponseV1::Output(p),
                    ControlResponseV1::Cancel(c) => HelperResponseV1::Cancel(c),
                    ControlResponseV1::Error(e) => HelperResponseV1::Error(e),
                });
            }
            // A supervisor can exit after accepting a control connection but
            // before replying. Recover from its durable state only after
            // systemd proves the unit stopped; an active unit stays unavailable.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => return Err(e),
        }
        if !directory.join("state.redb").is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "job not found"));
        }
        crate::systemd_unit::confirm_stopped(&job)?;
        // Opening redb also takes its exclusive lock: never mutate a live supervisor's state.
        let mut store = JobStore::open(&directory).map_err(other)?;
        if !store.state().status.state.is_terminal() {
            let at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let mut status = store.state().status.clone();
            status.state = JobStateV1::Lost;
            status.finished_at_ms = Some(at);
            status.reason_code = Some("supervisor_lost".into());
            let outcome = serde_json::to_vec(
                &serde_json::json!({"timestamp_ms":at,"kind":"exec.outcome.v1","payload":status}),
            )
            .map_err(other)?;
            store
                .transition(
                    JobStateV1::Lost,
                    at,
                    None,
                    None,
                    Some("supervisor_lost".into()),
                    Some(outcome),
                )
                .map_err(other)?;
        }
        if let Ok(mut index) = terminal_index().lock() {
            if index.len() >= 4096
                && !index.contains_key(&directory)
                && let Some(oldest) = index
                    .iter()
                    .min_by_key(|(_, status)| status.finished_at_ms)
                    .map(|(path, _)| path.clone())
            {
                index.remove(&oldest);
            }
            index.insert(directory.clone(), store.state().status.clone());
        }
        Ok(match control {
            ControlRequestV1::Status => {
                HelperResponseV1::Status(Box::new(store.state().status.clone()))
            }
            ControlRequestV1::Cancel { .. } => HelperResponseV1::Cancel(CancelReceiptV1 {
                job: store.state().status.job.clone(),
                accepted_at_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                already_terminal: true,
            }),
            ControlRequestV1::Output { cursor, max_bytes } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if store
                    .state()
                    .status
                    .finished_at_ms
                    .is_some_and(|at| now.saturating_sub(at) >= 24 * 60 * 60 * 1000)
                {
                    let _ = fs::remove_file(directory.join("output.log"));
                    return Ok(HelperResponseV1::Error(ExecErrorV1 {
                        code: ExecErrorCodeV1::OutputExpired,
                        safe_message: "job output expired".into(),
                        request_id: None,
                        job_id: Some(job),
                        retryable: false,
                        retry_after_ms: None,
                    }));
                }
                let sealed_path = self.state_root.join(format!("{}.json", id(&job)?));
                let sealed_file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(sealed_path)?;
                let mut sealed_bytes = Vec::new();
                sealed_file
                    .take(MAX_MESSAGE + 1)
                    .read_to_end(&mut sealed_bytes)?;
                if sealed_bytes.len() as u64 > MAX_MESSAGE {
                    return Err(other("sealed spec exceeds limit"));
                }
                let sealed: SupervisorSpecV1 =
                    serde_json::from_slice(&sealed_bytes).map_err(other)?;
                let mut spool = crate::spool::Spool::open(
                    &directory.join("output.log"),
                    store.state().status.output_cursor,
                    sealed
                        .request
                        .output_limit_bytes
                        .unwrap_or(crate::contract::DEFAULT_OUTPUT_BYTES),
                )?;
                spool.restore_truncated(store.state().output_truncated);
                HelperResponseV1::Output(spool.page(cursor, max_bytes, true)?)
            }
        })
    }
    pub fn serve(&self) -> io::Result<()> {
        self.validate()?;
        private_root(
            self.socket
                .parent()
                .ok_or_else(|| other("socket parent missing"))?,
            self.api_gid,
        )?;
        let lock_path = self.socket.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock_path)?;
        let metadata = lock.metadata()?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
            return Err(other("unsafe helper lifetime lock"));
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "helper already running",
            ));
        }
        if let Ok(metadata) = fs::symlink_metadata(&self.socket) {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() || metadata.uid() != 0 {
                return Err(other("unsafe stale helper endpoint"));
            }
            fs::remove_file(&self.socket)?;
        }
        let listener = UnixListener::bind(&self.socket)?;
        std::os::unix::fs::chown(&self.socket, Some(0), Some(self.api_gid))?;
        fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o660))?;
        let workers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let launch_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("helper accept failed: {error}");
                    continue;
                }
            };
            if workers.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= 8 {
                workers.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                continue;
            }
            let workers = workers.clone();
            let launch_lock = launch_lock.clone();
            let config = self.clone();
            std::thread::spawn(move || {
                struct WorkerGuard<'a>(&'a std::sync::atomic::AtomicUsize);
                impl Drop for WorkerGuard<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    }
                }
                let _worker = WorkerGuard(&workers);
                let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_PEERCRED,
                        (&mut cred as *mut libc::ucred).cast(),
                        &mut len,
                    )
                } != 0
                    || len as usize != std::mem::size_of::<libc::ucred>()
                    || cred.uid != config.api_uid
                {
                    return;
                }
                if stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .is_err()
                    || stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .is_err()
                {
                    return;
                }
                let response = (|| {
                    let mut bytes = Vec::new();
                    (&mut stream)
                        .take(MAX_MESSAGE + 1)
                        .read_to_end(&mut bytes)?;
                    if bytes.len() as u64 > MAX_MESSAGE {
                        return Err(other("helper request too large"));
                    }
                    let request = serde_json::from_slice(&bytes).map_err(other)?;
                    // Only admission and retention are serialized; cancel/status remain responsive.
                    let _launch = if matches!(request, HelperRequestV1::Launch { .. }) {
                        Some(launch_lock.lock().map_err(other)?)
                    } else {
                        None
                    };
                    config.handle(request)
                })();
                let response = response.unwrap_or_else(|e| HelperResponseV1::Error(safe_error(&e)));
                match serde_json::to_vec(&response) {
                    Ok(bytes) => {
                        let _ = stream.write_all(&bytes);
                    }
                    Err(error) => eprintln!("helper response encoding failed: {error}"),
                }
            });
        }
        Ok(())
    }
}
pub fn call(path: &Path, request: &HelperRequestV1) -> io::Result<HelperResponseV1> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let bytes = serde_json::to_vec(request).map_err(other)?;
    if bytes.len() as u64 > MAX_MESSAGE {
        return Err(other("helper request too large"));
    }
    stream.write_all(&bytes)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut bytes = Vec::new();
    stream.take(MAX_MESSAGE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MESSAGE {
        return Err(other("helper response too large"));
    }
    serde_json::from_slice(&bytes).map_err(other)
}

#[cfg(test)]
mod tests {
    #[test]
    fn socket_paths_reserve_the_trailing_nul_and_job_suffix() {
        assert!(super::validate_socket_path(Path::new(&format!("/{}", "a".repeat(106)))).is_ok());
        assert!(super::validate_socket_path(Path::new(&format!("/{}", "a".repeat(107)))).is_err());
        assert!(super::validate_socket_path(Path::new("relative.sock")).is_err());
        let long_root = Path::new("/var/lib").join("a".repeat(60));
        assert!(
            super::validate_socket_path(
                &long_root
                    .join("ffffffff-ffff-ffff-ffff-ffffffffffff")
                    .join("control.sock")
            )
            .is_err()
        );
    }
    use super::*;
    #[test]
    fn helper_identifiers_cannot_select_host_paths() {
        for value in ["../root", "/etc/passwd", "job.service", ""] {
            assert!(id(value).is_err());
        }
    }
    #[test]
    fn helper_errors_preserve_codes_without_disclosing_host_details() {
        let missing = safe_error(&io::Error::new(
            io::ErrorKind::NotFound,
            "/private/host/path",
        ));
        assert_eq!(missing.code, ExecErrorCodeV1::JobNotFound);
        assert!(!missing.safe_message.contains("/private"));
        assert_eq!(
            safe_error(&io::Error::new(io::ErrorKind::OutOfMemory, "storage cap")).code,
            ExecErrorCodeV1::ResourceExhausted
        );
        assert_eq!(
            safe_error(&io::Error::new(io::ErrorKind::InvalidInput, "cursor")).code,
            ExecErrorCodeV1::InvalidArgument
        );
    }
}
