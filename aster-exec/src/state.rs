//! One-writer durable job state. The live supervisor owns its database.

use crate::contract::{JobStateV1, JobStatusV1};
use redb::{ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("exec_job_meta_v1");
const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableJob {
    pub version: u32,
    pub request_digest: [u8; 32],
    pub status: JobStatusV1,
    pub output_truncated: bool,
    pub pending_audit_outcome: Option<Vec<u8>>,
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Redb(String),
    InvalidState(&'static str),
    Serialization(serde_json::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "job storage IO: {error}"),
            Self::Redb(error) => write!(f, "job redb: {error}"),
            Self::InvalidState(error) => write!(f, "invalid job state: {error}"),
            Self::Serialization(error) => write!(f, "job serialization: {error}"),
        }
    }
}

impl std::error::Error for StoreError {}
impl From<io::Error> for StoreError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value)
    }
}
fn redb(error: impl std::fmt::Display) -> StoreError {
    StoreError::Redb(error.to_string())
}

pub struct JobStore {
    database: redb::Database,
    state: DurableJob,
}

fn private_file(path: &Path) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?;
    Ok(())
}

impl JobStore {
    pub fn create(
        directory: &Path,
        status: JobStatusV1,
        request_digest: [u8; 32],
    ) -> Result<Self, StoreError> {
        if status.state != JobStateV1::Accepted || status.output_cursor != 0 {
            return Err(StoreError::InvalidState(
                "new jobs start accepted at cursor zero",
            ));
        }
        fs::create_dir(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let file = directory.join("state.redb");
        private_file(&file)?;
        let database = redb::Database::create(&file).map_err(redb)?;
        let state = DurableJob {
            version: STATE_VERSION,
            request_digest,
            status,
            output_truncated: false,
            pending_audit_outcome: None,
        };
        let store = Self { database, state };
        store.persist()?;
        Ok(store)
    }

    pub fn open(directory: &Path) -> Result<Self, StoreError> {
        let database = redb::Database::open(directory.join("state.redb")).map_err(redb)?;
        let read = database.begin_read().map_err(redb)?;
        let table = read.open_table(META).map_err(redb)?;
        let bytes = table
            .get("state")
            .map_err(redb)?
            .ok_or(StoreError::InvalidState("missing job metadata"))?
            .value()
            .to_vec();
        if bytes.len() > 128 * 1024 {
            return Err(StoreError::InvalidState("job metadata exceeds limit"));
        }
        let state: DurableJob = serde_json::from_slice(&bytes)?;
        if state.version != STATE_VERSION {
            return Err(StoreError::InvalidState("unsupported job state version"));
        }
        drop(table);
        drop(read);
        Ok(Self { database, state })
    }

    pub fn state(&self) -> &DurableJob {
        &self.state
    }

    pub fn transition(
        &mut self,
        next: JobStateV1,
        at_ms: u64,
        exit_code: Option<i32>,
        signal: Option<i32>,
        reason_code: Option<String>,
        pending_audit_outcome: Option<Vec<u8>>,
    ) -> Result<(), StoreError> {
        if !self.state.status.state.can_transition_to(next) {
            return Err(StoreError::InvalidState("illegal job-state transition"));
        }
        let mut changed = self.state.clone();
        changed.status.state = next;
        if next == JobStateV1::Running {
            changed.status.started_at_ms = Some(at_ms);
        }
        if next.is_terminal() {
            changed.status.finished_at_ms = Some(at_ms);
            changed.status.exit_code = exit_code;
            changed.status.signal = signal;
            changed.status.reason_code = reason_code;
            changed.pending_audit_outcome = pending_audit_outcome;
        }
        self.persist_state(&changed)?;
        self.state = changed;
        Ok(())
    }

    pub fn advance_output(&mut self, cursor: u64, truncated: bool) -> Result<(), StoreError> {
        if cursor < self.state.status.output_cursor || self.state.status.state.is_terminal() {
            return Err(StoreError::InvalidState("invalid output cursor advance"));
        }
        let mut changed = self.state.clone();
        changed.status.output_cursor = cursor;
        changed.output_truncated |= truncated;
        self.persist_state(&changed)?;
        self.state = changed;
        Ok(())
    }

    pub fn mark_audit_outcome_submitted(&mut self) -> Result<(), StoreError> {
        if !self.state.status.state.is_terminal() {
            return Err(StoreError::InvalidState(
                "job has no terminal audit outcome",
            ));
        }
        let mut changed = self.state.clone();
        changed.pending_audit_outcome = None;
        self.persist_state(&changed)?;
        self.state = changed;
        Ok(())
    }

    fn persist(&self) -> Result<(), StoreError> {
        self.persist_state(&self.state)
    }

    fn persist_state(&self, state: &DurableJob) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(state)?;
        if bytes.len() > 128 * 1024 {
            return Err(StoreError::InvalidState("job metadata exceeds limit"));
        }
        let mut write = self.database.begin_write().map_err(redb)?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(redb)?;
        write
            .open_table(META)
            .map_err(redb)?
            .insert("state", bytes.as_slice())
            .map_err(redb)?;
        write.commit().map_err(redb)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        DescendantScopeV1, FilesystemGuaranteeV1, GuaranteesV1, JobRefV1, NetworkGuaranteeV1,
        ResourceLimitsV1, RestartSurvivalV1,
    };

    fn accepted() -> JobStatusV1 {
        JobStatusV1 {
            job: JobRefV1 {
                job_id: "job".into(),
                target_id: "target".into(),
            },
            state: JobStateV1::Accepted,
            caller_id: "caller".into(),
            principal_id: None,
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
                provider: "native".into(),
                network: NetworkGuaranteeV1::Inherited,
                proxy_id: None,
                filesystem: FilesystemGuaranteeV1::Inherited,
                filesystem_policy_id: None,
                execution_identity: "runner".into(),
                resource_limits: ResourceLimitsV1 {
                    cpu_millis_per_second: None,
                    memory_bytes: None,
                    processes: None,
                    file_bytes: None,
                },
                descendant_scope: DescendantScopeV1::Cgroup,
                restart_survival: RestartSurvivalV1::ApiRestart,
            },
        }
    }

    #[test]
    fn transitions_and_pending_outcome_survive_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("job");
        let mut store = JobStore::create(&path, accepted(), [7; 32]).unwrap();
        store
            .transition(JobStateV1::Starting, 2, None, None, None, None)
            .unwrap();
        store
            .transition(JobStateV1::Running, 3, None, None, None, None)
            .unwrap();
        store.advance_output(1, false).unwrap();
        store
            .transition(
                JobStateV1::Cancelled,
                4,
                None,
                Some(15),
                Some("cancelled".into()),
                Some(b"outcome".to_vec()),
            )
            .unwrap();
        assert!(
            store
                .transition(JobStateV1::Running, 5, None, None, None, None)
                .is_err()
        );
        drop(store);
        let reopened = JobStore::open(&path).unwrap();
        assert_eq!(reopened.state().status.state, JobStateV1::Cancelled);
        assert_eq!(reopened.state().status.output_cursor, 1);
        assert_eq!(
            reopened.state().pending_audit_outcome.as_deref(),
            Some(b"outcome".as_slice())
        );
    }
}
