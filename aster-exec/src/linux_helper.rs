//! Root-owned fixed-configuration launch and recovery boundary.
use crate::{
    contract::*,
    linux_supervisor::{SupervisorSpecV1, terminal_outcome},
    local_control::{self, ControlRequestV1, ControlResponseV1},
    spool::Spool,
    state::JobStore,
    unix_millis,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
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
    sync::{Arc, Mutex, MutexGuard, Weak, mpsc},
    time::Duration,
};

const MAX_MESSAGE: u64 = MAX_LOCAL_MESSAGE_BYTES as u64;
/// Responses are read by the unprivileged API, and job enumeration grows
/// with retained jobs, so they get a larger bound than root-side reads.
const MAX_RESPONSE: u64 = 32 * 1024 * 1024;
/// Concurrent request workers. Further connections wait in the listen queue.
const WORKERS: usize = 8;
/// Finished jobs whose final status is kept in memory.
const MAX_FINISHED: usize = 4096;
/// Finished jobs whose output spool stays open for paging.
const MAX_OPEN_SPOOLS: usize = 16;
/// Pending audit outcomes returned per request.
const MAX_OUTCOMES_PER_CALL: usize = 256;
/// Active jobs and retained output the helper admits, matching the ledger.
const MAX_ACTIVE_JOBS: usize = crate::ledger::MAX_ACTIVE_JOBS;
const MAX_RETAINED_BYTES: u64 = crate::ledger::MAX_RETAINED_OUTPUT_BYTES;
/// A job folder without durable state, older than this and with no running
/// unit, is a launch that failed part way and is moved aside.
const INCOMPLETE_LAUNCH_AGE: Duration = Duration::from_secs(600);
const QUARANTINE: &str = "quarantine";

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
    AckOutcome {
        job_id: String,
    },
    Ping,
    /// Terminal audit outcomes not yet acknowledged, at most
    /// [`MAX_OUTCOMES_PER_CALL`] per call.
    PendingOutcomes,
    /// Jobs that are not terminal. `complete` is false when some job's state
    /// could not be read.
    Enumerate,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingOutcomeV1 {
    pub job_id: String,
    #[serde(with = "base64_bytes")]
    pub outcome: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HelperResponseV1 {
    Status(Box<JobStatusV1>),
    Output(OutputPageV1),
    Cancel(CancelReceiptV1),
    Outcomes(Vec<PendingOutcomeV1>),
    Ack,
    Jobs {
        jobs: Vec<JobStatusV1>,
        complete: bool,
    },
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

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "job not found")
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

/// Canonical job ID; anything that is not a UUID is rejected.
fn id(id: &str) -> io::Result<String> {
    Ok(uuid::Uuid::parse_str(id)
        .map_err(other)?
        .hyphenated()
        .to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
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
            Path::new(crate::systemd_unit::SYSTEMCTL),
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

    /// Validate the configuration and serve the API until the listener fails.
    pub fn serve(&self) -> io::Result<()> {
        self.validate()?;
        private_root(
            self.socket
                .parent()
                .ok_or_else(|| other("socket parent missing"))?,
            self.api_gid,
        )?;
        let _lifetime = self.lifetime_lock()?;
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
        Helper::new(self.clone()).serve(listener)
    }

    fn lifetime_lock(&self) -> io::Result<fs::File> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.socket.with_extension("lock"))?;
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
        Ok(lock)
    }
}

/// What the helper remembers about a job proven stopped.
struct Finished {
    status: JobStatusV1,
    output_truncated: bool,
    outcome_acknowledged: bool,
}

/// Helper state shared by its request workers.
struct Helper {
    config: HelperConfig,
    /// Serializes admission and retention cleanup.
    launch: Mutex<()>,
    /// One lock per job, held for every use of that job's durable state, so
    /// concurrent requests never contend for its database lock.
    jobs: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    /// Jobs proven stopped. Requests for them need neither systemd nor the
    /// supervisor.
    finished: Mutex<HashMap<String, Finished>>,
    /// Open output spools of recently paged finished jobs.
    spools: Mutex<VecDeque<(String, Spool)>>,
}

impl Helper {
    fn new(config: HelperConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            launch: Mutex::new(()),
            jobs: Mutex::new(HashMap::new()),
            finished: Mutex::new(HashMap::new()),
            spools: Mutex::new(VecDeque::new()),
        })
    }

    fn serve(self: Arc<Self>, listener: UnixListener) -> io::Result<()> {
        let (release, permits) = mpsc::sync_channel::<()>(WORKERS);
        for _ in 0..WORKERS {
            let _ = release.send(());
        }
        loop {
            // Wait for a free worker before accepting, so callers queue
            // rather than being dropped.
            permits.recv().map_err(other)?;
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) => {
                    eprintln!("helper accept failed: {error}");
                    let _ = release.send(());
                    // Running out of descriptors must not become a busy loop.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let helper = self.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                helper.respond(stream);
                let _ = release.send(());
            });
        }
    }

    fn respond(&self, mut stream: UnixStream) {
        if peer_uid(&stream).ok() != Some(self.config.api_uid)
            || stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .is_err()
            || stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .is_err()
        {
            return;
        }
        let response = read_request(&mut stream)
            .and_then(|request| self.handle(request))
            .unwrap_or_else(|error| HelperResponseV1::Error(safe_error(&error)));
        match serde_json::to_vec(&response) {
            Ok(bytes) => {
                let _ = stream.write_all(&bytes);
            }
            Err(error) => eprintln!("helper response encoding failed: {error}"),
        }
    }

    fn handle(&self, request: HelperRequestV1) -> io::Result<HelperResponseV1> {
        match request {
            HelperRequestV1::Ping => Ok(HelperResponseV1::Ack),
            HelperRequestV1::Launch {
                request,
                status,
                request_digest,
            } => {
                let _launch = lock(&self.launch);
                self.launch(*request, *status, request_digest)
            }
            HelperRequestV1::Enumerate => Ok(self.enumerate()),
            HelperRequestV1::PendingOutcomes => Ok(self.pending_outcomes()),
            HelperRequestV1::AckOutcome { job_id } => {
                let job = id(&job_id)?;
                let _job = self.lock_job(&job);
                self.acknowledge_outcome(&job)
            }
            HelperRequestV1::Status { job_id } => self.job(&job_id, ControlRequestV1::Status),
            HelperRequestV1::Output {
                job_id,
                cursor,
                max_bytes,
            } => self.job(&job_id, ControlRequestV1::Output { cursor, max_bytes }),
            HelperRequestV1::Cancel { job_id, grace_ms } => {
                self.job(&job_id, ControlRequestV1::Cancel { grace_ms })
            }
        }
    }

    fn lock_job(&self, job: &str) -> Arc<Mutex<()>> {
        let mut jobs = lock(&self.jobs);
        if let Some(existing) = jobs.get(job).and_then(Weak::upgrade) {
            return existing;
        }
        jobs.retain(|_, held| held.strong_count() > 0);
        let fresh = Arc::new(Mutex::new(()));
        jobs.insert(job.to_owned(), Arc::downgrade(&fresh));
        fresh
    }

    fn directory(&self, job: &str) -> PathBuf {
        self.config.state_root.join(job)
    }

    fn sealed_path(&self, job: &str) -> PathBuf {
        self.config.state_root.join(format!("{job}.json"))
    }

    /// The sealed launch specification, read without following links.
    fn read_sealed(&self, job: &str) -> io::Result<SupervisorSpecV1> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.sealed_path(job))?;
        let mut bytes = Vec::new();
        file.take(MAX_MESSAGE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MESSAGE {
            return Err(other("sealed spec exceeds limit"));
        }
        serde_json::from_slice(&bytes).map_err(other)
    }

    /// Answer a request for one job under its lock.
    fn job(&self, job_id: &str, control: ControlRequestV1) -> io::Result<HelperResponseV1> {
        let job = id(job_id)?;
        let held = self.lock_job(&job);
        let _job = lock(&held);
        self.job_locked(&job, control)
    }

    fn job_locked(&self, job: &str, control: ControlRequestV1) -> io::Result<HelperResponseV1> {
        if lock(&self.finished).contains_key(job) {
            return self.answer_finished(job, control);
        }
        match local_control::call(&self.directory(job).join("control.sock"), &control) {
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
        self.recover(job)?;
        self.answer_finished(job, control)
    }

    /// Settle a job whose supervisor has stopped: a non-terminal record
    /// becomes Lost with a durable audit outcome. Caller holds the job lock.
    fn recover(&self, job: &str) -> io::Result<()> {
        let directory = self.directory(job);
        if !directory.join("state.redb").is_file() {
            return Err(not_found());
        }
        crate::systemd_unit::confirm_stopped(job)?;
        let mut store = JobStore::open(&directory).map_err(other)?;
        if !store.state().status.state.is_terminal() {
            let at = unix_millis();
            let mut status = store.state().status.clone();
            status.state = JobStateV1::Lost;
            status.finished_at_ms = Some(at);
            status.reason_code = Some("supervisor_lost".into());
            store
                .transition(
                    JobStateV1::Lost,
                    at,
                    None,
                    None,
                    status.reason_code.clone(),
                    Some(terminal_outcome(&status)?),
                )
                .map_err(other)?;
        }
        let state = store.state();
        self.remember_finished(
            job,
            Finished {
                status: state.status.clone(),
                output_truncated: state.output_truncated,
                outcome_acknowledged: state.pending_audit_outcome.is_none(),
            },
        );
        Ok(())
    }

    fn remember_finished(&self, job: &str, finished: Finished) {
        let mut all = lock(&self.finished);
        if all.len() >= MAX_FINISHED
            && !all.contains_key(job)
            && let Some(oldest) = all
                .iter()
                .min_by_key(|(_, known)| known.status.finished_at_ms)
                .map(|(job, _)| job.clone())
        {
            all.remove(&oldest);
        }
        all.insert(job.to_owned(), finished);
    }

    fn forget(&self, job: &str) {
        lock(&self.finished).remove(job);
        lock(&self.spools).retain(|(open, _)| open != job);
    }

    /// Serve a request for a finished job from its durable state. Caller
    /// holds the job lock and the job is in `finished`.
    fn answer_finished(
        &self,
        job: &str,
        control: ControlRequestV1,
    ) -> io::Result<HelperResponseV1> {
        let (status, truncated) = {
            let finished = lock(&self.finished);
            let known = finished.get(job).ok_or_else(not_found)?;
            (known.status.clone(), known.output_truncated)
        };
        Ok(match control {
            ControlRequestV1::Status => HelperResponseV1::Status(Box::new(status)),
            ControlRequestV1::Cancel { .. } => HelperResponseV1::Cancel(CancelReceiptV1 {
                job: status.job,
                accepted_at_ms: unix_millis(),
                already_terminal: true,
            }),
            ControlRequestV1::Output { cursor, max_bytes } => {
                let expired = status
                    .finished_at_ms
                    .is_some_and(|at| unix_millis().saturating_sub(at) >= OUTPUT_RETENTION_MS);
                if expired {
                    lock(&self.spools).retain(|(open, _)| open != job);
                    let _ = fs::remove_file(self.directory(job).join("output.log"));
                    return Ok(HelperResponseV1::Error(ExecErrorV1 {
                        code: ExecErrorCodeV1::OutputExpired,
                        safe_message: "job output expired".into(),
                        request_id: None,
                        job_id: Some(job.to_owned()),
                        retryable: false,
                        retry_after_ms: None,
                    }));
                }
                let mut spool = self.take_spool(job, &status, truncated)?;
                let page = spool.page(cursor, max_bytes, true);
                let mut spools = lock(&self.spools);
                spools.push_back((job.to_owned(), spool));
                while spools.len() > MAX_OPEN_SPOOLS {
                    spools.pop_front();
                }
                HelperResponseV1::Output(page?)
            }
        })
    }

    /// An open spool for a finished job: a cached one, or one opened (and
    /// verified) now.
    fn take_spool(&self, job: &str, status: &JobStatusV1, truncated: bool) -> io::Result<Spool> {
        {
            let mut spools = lock(&self.spools);
            if let Some(position) = spools.iter().position(|(open, _)| open == job)
                && let Some((_, spool)) = spools.remove(position)
            {
                return Ok(spool);
            }
        }
        let limit = self
            .read_sealed(job)?
            .request
            .output_limit_bytes
            .unwrap_or(DEFAULT_OUTPUT_BYTES);
        let mut spool = Spool::open(
            &self.directory(job).join("output.log"),
            status.output_cursor,
            limit,
        )?;
        spool.restore_truncated(truncated);
        Ok(spool)
    }

    fn acknowledge_outcome(&self, job: &str) -> io::Result<HelperResponseV1> {
        let mut store = JobStore::open(&self.directory(job)).map_err(other)?;
        store.mark_audit_outcome_submitted().map_err(other)?;
        if let Some(known) = lock(&self.finished).get_mut(job) {
            known.outcome_acknowledged = true;
        }
        Ok(HelperResponseV1::Ack)
    }

    /// Job IDs of the folders under the state root.
    fn job_ids(&self) -> io::Result<Vec<String>> {
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&self.config.state_root)? {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            if let Some(job) = entry.file_name().to_str().and_then(|name| id(name).ok()) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    /// Jobs that are not terminal. A job whose state cannot be read is
    /// skipped and the result marked incomplete; it never hides the others.
    fn enumerate(&self) -> HelperResponseV1 {
        let Ok(ids) = self.job_ids() else {
            return HelperResponseV1::Jobs {
                jobs: Vec::new(),
                complete: false,
            };
        };
        let mut jobs = Vec::new();
        let mut complete = true;
        for job in ids {
            if lock(&self.finished).contains_key(&job) {
                continue;
            }
            match self.job(&job, ControlRequestV1::Status) {
                Ok(HelperResponseV1::Status(status)) if !status.state.is_terminal() => {
                    jobs.push(*status);
                }
                Ok(HelperResponseV1::Status(_)) => {}
                _ => complete = false,
            }
        }
        HelperResponseV1::Jobs { jobs, complete }
    }

    /// Unacknowledged terminal outcomes of finished jobs, bounded per call.
    fn pending_outcomes(&self) -> HelperResponseV1 {
        let candidates: Vec<String> = lock(&self.finished)
            .iter()
            .filter(|(_, known)| !known.outcome_acknowledged)
            .map(|(job, _)| job.clone())
            .collect();
        let mut outcomes = Vec::new();
        for job in candidates {
            if outcomes.len() >= MAX_OUTCOMES_PER_CALL {
                break;
            }
            let held = self.lock_job(&job);
            let _job = lock(&held);
            match JobStore::open(&self.directory(&job)) {
                Ok(store) => match store.state().pending_audit_outcome.clone() {
                    Some(outcome) => outcomes.push(PendingOutcomeV1 {
                        job_id: job,
                        outcome,
                    }),
                    None => {
                        if let Some(known) = lock(&self.finished).get_mut(&job) {
                            known.outcome_acknowledged = true;
                        }
                    }
                },
                Err(error) => eprintln!("outcome state unavailable for {job}: {error}"),
            }
        }
        HelperResponseV1::Outcomes(outcomes)
    }

    fn launch(
        &self,
        request: ExecRequestV1,
        status: JobStatusV1,
        digest: [u8; 32],
    ) -> io::Result<HelperResponseV1> {
        request.validate().map_err(other)?;
        let (active, reserved) = self.cleanup_and_budget();
        if active >= MAX_ACTIVE_JOBS
            || reserved.saturating_add(request.output_limit_bytes.unwrap_or(DEFAULT_OUTPUT_BYTES))
                > MAX_RETAINED_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "job storage admission limit reached",
            ));
        }
        let job = id(&status.job.job_id)?;
        if status.state != JobStateV1::Accepted
            || status.output_cursor != 0
            || status.guarantees.resource_limits != self.config.resource_limits
            || request.target_id != status.job.target_id
            || request
                .canonical_digest(&status.guarantees.profile)
                .map_err(other)?
                != digest
        {
            return Err(other("launch does not match admitted job"));
        }
        let directory = self.directory(&job);
        let held = self.lock_job(&job);
        let _job = lock(&held);
        drop(JobStore::create(&directory, status.clone(), digest).map_err(other)?);
        // Only root (this helper and the supervisor) reaches job state.
        std::os::unix::fs::chown(&directory, Some(0), Some(0))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let sealed = self.sealed_path(&job);
        let spec = SupervisorSpecV1 {
            request,
            status: status.clone(),
            request_digest: digest,
            job_directory: directory.clone(),
            control_socket: directory.join("control.sock"),
            api_uid: self.config.api_uid,
            api_gid: self.config.api_gid,
            payload_uid: self.config.payload_uid,
            payload_gid: self.config.payload_gid,
            payload_groups: self.config.payload_groups.clone(),
            resource_limits: self.config.resource_limits.clone(),
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&sealed)?;
        file.write_all(&serde_json::to_vec(&spec).map_err(other)?)?;
        file.sync_all()?;
        fs::File::open(&self.config.state_root)?.sync_all()?;
        crate::systemd_unit::create(
            &job,
            &self.config.supervisor,
            &sealed,
            &self.config.systemd_run,
        )?;
        Ok(HelperResponseV1::Status(Box::new(status)))
    }

    /// Count active jobs and retained bytes for admission, expiring old
    /// output and job folders on the way. Each folder is handled on its own;
    /// one that cannot be read is counted as a running job at the maximum
    /// output size, so it never makes admission more permissive.
    fn cleanup_and_budget(&self) -> (usize, u64) {
        let mut active = 0usize;
        let mut reserved = 0u64;
        let ids = match self.job_ids() {
            Ok(ids) => ids,
            // Without a listing nothing can be proved free.
            Err(_) => return (MAX_ACTIVE_JOBS, MAX_RETAINED_BYTES),
        };
        for job in ids {
            let held = self.lock_job(&job);
            let _job = lock(&held);
            match self.budget_job(&job) {
                Ok((running, bytes)) => {
                    active += usize::from(running);
                    reserved = reserved.saturating_add(bytes);
                }
                Err(error) => {
                    eprintln!("job {job} unreadable during admission: {error}");
                    active += 1;
                    reserved = reserved.saturating_add(MAX_OUTPUT_BYTES);
                }
            }
        }
        (active, reserved)
    }

    /// Whether one job is running and how many bytes it retains. Caller
    /// holds the job lock.
    fn budget_job(&self, job: &str) -> io::Result<(bool, u64)> {
        let directory = self.directory(job);
        let status = match self.job_locked(job, ControlRequestV1::Status) {
            Ok(HelperResponseV1::Status(status)) => *status,
            Ok(HelperResponseV1::Error(error)) if error.code == ExecErrorCodeV1::JobNotFound => {
                return self.quarantine_incomplete(job).map(|()| (false, 0));
            }
            Ok(_) => return Err(other("unexpected job status response")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return self.quarantine_incomplete(job).map(|()| (false, 0));
            }
            Err(error) => return Err(error),
        };
        let size = |path: PathBuf| fs::metadata(path).map_or(0, |m| m.len());
        let mut reserved = size(directory.join("state.redb")) + size(self.sealed_path(job));
        if !status.state.is_terminal() {
            let limit = self
                .read_sealed(job)
                .map(|spec| {
                    spec.request
                        .output_limit_bytes
                        .unwrap_or(DEFAULT_OUTPUT_BYTES)
                })
                .unwrap_or(MAX_OUTPUT_BYTES);
            return Ok((true, reserved.saturating_add(limit)));
        }
        let now = unix_millis();
        let age = now.saturating_sub(status.finished_at_ms.unwrap_or(now));
        if age >= OUTPUT_RETENTION_MS {
            lock(&self.spools).retain(|(open, _)| open != job);
            match fs::remove_file(directory.join("output.log")) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        } else {
            reserved = reserved.saturating_add(size(directory.join("output.log")));
        }
        let acknowledged = lock(&self.finished)
            .get(job)
            .is_some_and(|known| known.outcome_acknowledged);
        if age >= REQUEST_RETENTION_MS && acknowledged {
            self.forget(job);
            fs::remove_dir_all(&directory)?;
            match fs::remove_file(self.sealed_path(job)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            return Ok((false, 0));
        }
        Ok((false, reserved))
    }

    /// Move aside a job folder that never received durable state, once no
    /// unit can still be starting it. Caller holds the job lock.
    fn quarantine_incomplete(&self, job: &str) -> io::Result<()> {
        let directory = self.directory(job);
        let age = fs::metadata(&directory)?
            .modified()?
            .elapsed()
            .unwrap_or_default();
        if age < INCOMPLETE_LAUNCH_AGE {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "job launch may still be in progress",
            ));
        }
        crate::systemd_unit::confirm_stopped(job)?;
        let quarantine = self.config.state_root.join(QUARANTINE);
        match fs::create_dir(&quarantine) {
            Ok(()) => fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        fs::rename(&directory, quarantine.join(job))?;
        let _ = fs::rename(
            self.sealed_path(job),
            quarantine.join(format!("{job}.json")),
        );
        eprintln!("job {job} never received durable state; moved to quarantine");
        Ok(())
    }
}

fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

fn read_request(stream: &mut UnixStream) -> io::Result<HelperRequestV1> {
    let mut bytes = Vec::new();
    stream.take(MAX_MESSAGE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MESSAGE {
        return Err(other("helper request too large"));
    }
    serde_json::from_slice(&bytes).map_err(other)
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
    stream.take(MAX_RESPONSE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RESPONSE {
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

    fn limits() -> ResourceLimitsV1 {
        ResourceLimitsV1 {
            cpu_millis_per_second: None,
            memory_bytes: None,
            processes: None,
            file_bytes: None,
        }
    }

    /// A finished job's durable state, output spool and sealed spec, laid
    /// out as a stopped supervisor leaves them.
    fn finished_job(state_root: &Path) -> (String, JobStatusV1) {
        let job = uuid::Uuid::new_v4().to_string();
        let accepted = JobStatusV1 {
            job: JobRefV1 {
                job_id: job.clone(),
                target_id: "target".into(),
            },
            state: JobStateV1::Accepted,
            caller_id: "caller".into(),
            resolved_target: "target".into(),
            execution_identity: "runner".into(),
            accepted_at_ms: 1,
            started_at_ms: None,
            finished_at_ms: None,
            exit_code: None,
            signal: None,
            reason_code: None,
            output_cursor: 0,
            guarantees: GuaranteesV1 {
                profile: "open".into(),
                provider: "linux-systemd-cgroup-v2".into(),
                network: NetworkGuaranteeV1::Inherited,
                proxy_id: None,
                filesystem: FilesystemGuaranteeV1::Inherited,
                filesystem_policy_id: None,
                execution_identity: "runner".into(),
                resource_limits: limits(),
                descendant_scope: DescendantScopeV1::Cgroup,
                restart_survival: RestartSurvivalV1::ApiRestart,
            },
        };
        let directory = state_root.join(&job);
        let mut store = JobStore::create(&directory, accepted.clone(), [7; 32]).unwrap();
        let mut spool = Spool::open(&directory.join("output.log"), 0, 4096).unwrap();
        for text in ["one", "two", "three"] {
            spool
                .append(OutputStreamV1::Stdout, text.as_bytes())
                .unwrap();
        }
        drop(spool);
        let at = unix_millis();
        store
            .transition(JobStateV1::Starting, at, None, None, None, None)
            .unwrap();
        store
            .transition(JobStateV1::Running, at, None, None, None, None)
            .unwrap();
        store.advance_output(3, false).unwrap();
        store
            .transition(
                JobStateV1::Succeeded,
                at,
                Some(0),
                None,
                Some("exited".into()),
                Some(b"outcome".to_vec()),
            )
            .unwrap();
        let request = ExecRequestV1 {
            request_id: uuid::Uuid::new_v4().to_string(),
            target_id: "target".into(),
            profile: Some("open".into()),
            cwd: "/".into(),
            executable: "/bin/true".into(),
            argv: vec![],
            environment_overrides: Default::default(),
            stdin_bytes: None,
            execution_timeout_ms: None,
            output_limit_bytes: Some(4096),
        };
        let spec = SupervisorSpecV1 {
            request,
            status: accepted,
            request_digest: [7; 32],
            job_directory: directory.clone(),
            control_socket: directory.join("control.sock"),
            api_uid: 1000,
            api_gid: 1000,
            payload_uid: 2000,
            payload_gid: 2000,
            payload_groups: vec![],
            resource_limits: limits(),
        };
        std::fs::write(
            state_root.join(format!("{job}.json")),
            serde_json::to_vec(&spec).unwrap(),
        )
        .unwrap();
        let status = store.state().status.clone();
        (job, status)
    }

    #[test]
    fn finished_jobs_are_served_concurrently_from_durable_state() {
        let temp = tempfile::tempdir().unwrap();
        let helper = Helper::new(HelperConfig {
            socket: temp.path().join("helper.sock"),
            state_root: temp.path().into(),
            supervisor: "/bin/true".into(),
            systemd_run: "/bin/true".into(),
            api_uid: 1000,
            api_gid: 1000,
            payload_uid: 2000,
            payload_gid: 2000,
            payload_groups: vec![],
            resource_limits: limits(),
        });
        let (job, status) = finished_job(temp.path());
        helper.remember_finished(
            &job,
            Finished {
                status,
                output_truncated: false,
                outcome_acknowledged: false,
            },
        );
        // Concurrent pages share one job lock and one cached spool; none
        // fails on the job database's exclusive lock.
        let pages: Vec<_> = (0..8)
            .map(|_| {
                let helper = helper.clone();
                let job = job.clone();
                std::thread::spawn(move || {
                    helper.job(
                        &job,
                        ControlRequestV1::Output {
                            cursor: 0,
                            max_bytes: 4096,
                        },
                    )
                })
            })
            .collect();
        for page in pages {
            let HelperResponseV1::Output(page) = page.join().unwrap().unwrap() else {
                panic!("expected an output page");
            };
            assert_eq!(page.chunks.len(), 3);
            assert_eq!(page.chunks[2].bytes, b"three");
            assert!(page.eof);
        }
        assert!(matches!(
            helper
                .job(&job, ControlRequestV1::Cancel { grace_ms: None })
                .unwrap(),
            HelperResponseV1::Cancel(CancelReceiptV1 {
                already_terminal: true,
                ..
            })
        ));
        let HelperResponseV1::Outcomes(outcomes) = helper.pending_outcomes() else {
            panic!("expected outcomes");
        };
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].outcome, b"outcome");
        assert!(matches!(
            helper
                .handle(HelperRequestV1::AckOutcome {
                    job_id: job.clone()
                })
                .unwrap(),
            HelperResponseV1::Ack
        ));
        let HelperResponseV1::Outcomes(outcomes) = helper.pending_outcomes() else {
            panic!("expected outcomes");
        };
        assert!(outcomes.is_empty());
    }

    #[test]
    fn helper_messages_carry_outcome_bytes_as_base64() {
        let response = HelperResponseV1::Outcomes(vec![PendingOutcomeV1 {
            job_id: "job".into(),
            outcome: vec![0xFF; 3],
        }]);
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(encoded.contains("\"////\""), "{encoded}");
    }
}
