//! API-owned request-ID reservation. A retry never silently starts a new job.

use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::Path;

pub const MAX_ACTIVE_JOBS: usize = 32;
pub const MAX_RETAINED_REQUESTS: usize = 65_536;
pub const MAX_RETAINED_OUTPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const REQUESTS: TableDefinition<&str, &[u8]> = TableDefinition::new("exec_requests_v1");

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReservationPhase {
    Preparing,
    Accepted,
    Terminal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub job_id: String,
    pub digest: [u8; 32],
    pub phase: ReservationPhase,
    pub accepted_at_ms: u64,
    pub terminal_at_ms: Option<u64>,
    /// Capacity reservation, retained until output expiry. Legacy records
    /// conservatively reserve the v1 per-job maximum.
    #[serde(default = "legacy_output_limit")]
    pub output_limit_bytes: u64,
    /// Stable request-admission failure. This is a request tombstone, not a
    /// claim that an ambiguously launched helper process has terminated.
    #[serde(default)]
    pub admission_error: Option<crate::contract::ExecErrorV1>,
}

fn legacy_output_limit() -> u64 {
    crate::contract::MAX_OUTPUT_BYTES
}

#[derive(Debug)]
pub enum LedgerError {
    Storage(String),
    Conflict,
    Missing,
    InvalidTransition,
    Capacity,
}
impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(message) => write!(f, "request ledger storage: {message}"),
            Self::Conflict => f.write_str("request ID reused with different content"),
            Self::Missing => f.write_str("request reservation is missing"),
            Self::InvalidTransition => f.write_str("invalid request reservation transition"),
            Self::Capacity => f.write_str("Exec admission capacity exhausted"),
        }
    }
}
impl std::error::Error for LedgerError {}
fn storage(error: impl std::fmt::Display) -> LedgerError {
    LedgerError::Storage(error.to_string())
}

pub struct RequestLedger {
    database: redb::Database,
}

pub fn request_key(caller_id: &str, target_id: &str, request_id: &str) -> String {
    let mut data = Vec::new();
    for field in [caller_id, target_id, request_id] {
        data.extend_from_slice(&(field.len() as u64).to_le_bytes());
        data.extend_from_slice(field.as_bytes());
    }
    blake3::hash(&data).to_hex().to_string()
}

fn expired(entry: &Reservation, now_ms: u64, retention: u64) -> bool {
    entry.phase == ReservationPhase::Terminal
        && entry
            .terminal_at_ms
            .is_some_and(|at| now_ms.saturating_sub(at) >= retention)
}

impl RequestLedger {
    /// Durable reservations used to reconcile terminal helper state after an
    /// API restart. Entries are never inferred from a client-supplied job ID.
    pub fn entries(&self) -> Result<Vec<(String, Reservation)>, LedgerError> {
        let read = self.database.begin_read().map_err(storage)?;
        let table = read.open_table(REQUESTS).map_err(storage)?;
        table
            .iter()
            .map_err(storage)?
            .map(|entry| {
                let (key, value) = entry.map_err(storage)?;
                Ok((
                    key.value().to_owned(),
                    serde_json::from_slice(value.value()).map_err(storage)?,
                ))
            })
            .collect()
    }
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let directory = path
            .parent()
            .ok_or_else(|| storage("ledger has no parent"))?;
        fs::create_dir_all(directory).map_err(storage)?;
        let existed = path.exists();
        if !existed {
            let mut options = OpenOptions::new();
            options.create_new(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path).map_err(storage)?;
        }
        let database = redb::Database::create(path).map_err(storage)?;
        let write = database.begin_write().map_err(storage)?;
        write.open_table(REQUESTS).map_err(storage)?;
        write.commit().map_err(storage)?;
        Ok(Self { database })
    }

    pub fn lookup(&self, key: &str) -> Result<Option<Reservation>, LedgerError> {
        let read = self.database.begin_read().map_err(storage)?;
        let table = read.open_table(REQUESTS).map_err(storage)?;
        let bytes = table
            .get(key)
            .map_err(storage)?
            .map(|value| value.value().to_vec());
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).map_err(storage))
            .transpose()
    }

    /// Reserve with the default output limit.
    #[cfg(test)]
    pub fn reserve(
        &self,
        key: &str,
        digest: [u8; 32],
        new_job_id: &str,
        now_ms: u64,
    ) -> Result<Reservation, LedgerError> {
        self.reserve_with_output(
            key,
            digest,
            new_job_id,
            now_ms,
            crate::contract::DEFAULT_OUTPUT_BYTES,
        )
    }

    /// Atomic first reservation. Same digest returns the existing job; a
    /// different digest is a conflict even when the first job is terminal.
    pub fn reserve_with_output(
        &self,
        key: &str,
        digest: [u8; 32],
        new_job_id: &str,
        now_ms: u64,
        output_limit_bytes: u64,
    ) -> Result<Reservation, LedgerError> {
        let mut write = self.database.begin_write().map_err(storage)?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(storage)?;
        let mut table = write.open_table(REQUESTS).map_err(storage)?;
        if let Some(value) = table.get(key).map_err(storage)? {
            let old: Reservation = serde_json::from_slice(value.value()).map_err(storage)?;
            return if old.digest == digest {
                Ok(old)
            } else {
                Err(LedgerError::Conflict)
            };
        }
        let entries = table
            .iter()
            .map_err(storage)?
            .map(|entry| {
                let (key, value) = entry.map_err(storage)?;
                Ok((
                    key.value().to_owned(),
                    serde_json::from_slice::<Reservation>(value.value()).map_err(storage)?,
                ))
            })
            .collect::<Result<Vec<_>, LedgerError>>()?;
        let mut active = 0usize;
        let mut retained = 0usize;
        let mut output_bytes = 0u64;
        for (old_key, entry) in entries {
            if expired(&entry, now_ms, crate::contract::REQUEST_RETENTION_MS) {
                table.remove(old_key.as_str()).map_err(storage)?;
                continue;
            }
            retained += 1;
            if entry.phase != ReservationPhase::Terminal {
                active += 1;
            }
            if !expired(&entry, now_ms, crate::contract::OUTPUT_RETENTION_MS) {
                output_bytes = output_bytes.saturating_add(entry.output_limit_bytes);
            }
        }
        if active >= MAX_ACTIVE_JOBS
            || retained >= MAX_RETAINED_REQUESTS
            || output_limit_bytes > crate::contract::MAX_OUTPUT_BYTES
            || output_bytes.saturating_add(output_limit_bytes) > MAX_RETAINED_OUTPUT_BYTES
        {
            return Err(LedgerError::Capacity);
        }
        let reservation = Reservation {
            job_id: new_job_id.to_owned(),
            digest,
            phase: ReservationPhase::Preparing,
            accepted_at_ms: now_ms,
            terminal_at_ms: None,
            output_limit_bytes,
            admission_error: None,
        };
        let bytes = serde_json::to_vec(&reservation).map_err(storage)?;
        table.insert(key, bytes.as_slice()).map_err(storage)?;
        drop(table);
        write.commit().map_err(storage)?;
        Ok(reservation)
    }

    pub fn advance(
        &self,
        key: &str,
        next: ReservationPhase,
        now_ms: u64,
    ) -> Result<Reservation, LedgerError> {
        let mut write = self.database.begin_write().map_err(storage)?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(storage)?;
        let mut table = write.open_table(REQUESTS).map_err(storage)?;
        let bytes = table
            .get(key)
            .map_err(storage)?
            .ok_or(LedgerError::Missing)?
            .value()
            .to_vec();
        let mut entry: Reservation = serde_json::from_slice(&bytes).map_err(storage)?;
        let legal = matches!(
            (entry.phase, next),
            (ReservationPhase::Preparing, ReservationPhase::Accepted)
                | (ReservationPhase::Accepted, ReservationPhase::Terminal)
        );
        if !legal {
            return Err(LedgerError::InvalidTransition);
        }
        entry.phase = next;
        if next == ReservationPhase::Terminal {
            entry.terminal_at_ms = Some(now_ms);
        }
        let bytes = serde_json::to_vec(&entry).map_err(storage)?;
        table.insert(key, bytes.as_slice()).map_err(storage)?;
        drop(table);
        write.commit().map_err(storage)?;
        Ok(entry)
    }

    /// Record that a request failed before its launch boundary. Only a
    /// Preparing reservation qualifies, and it owns no output. The request ID
    /// stays reserved: retries return this exact error and never launch.
    pub fn fail_preparing(
        &self,
        key: &str,
        error: crate::contract::ExecErrorV1,
        now_ms: u64,
    ) -> Result<Reservation, LedgerError> {
        self.tombstone(key, ReservationPhase::Preparing, error, now_ms)
    }

    /// Record that an accepted launch never produced a job. The caller must
    /// have proved that no launch is still in flight and that the lifetime
    /// owner has no record of the job.
    pub fn abandon_accepted(
        &self,
        key: &str,
        error: crate::contract::ExecErrorV1,
        now_ms: u64,
    ) -> Result<Reservation, LedgerError> {
        self.tombstone(key, ReservationPhase::Accepted, error, now_ms)
    }

    fn tombstone(
        &self,
        key: &str,
        expected: ReservationPhase,
        error: crate::contract::ExecErrorV1,
        now_ms: u64,
    ) -> Result<Reservation, LedgerError> {
        let mut write = self.database.begin_write().map_err(storage)?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(storage)?;
        let mut table = write.open_table(REQUESTS).map_err(storage)?;
        let bytes = table
            .get(key)
            .map_err(storage)?
            .ok_or(LedgerError::Missing)?
            .value()
            .to_vec();
        let mut entry: Reservation = serde_json::from_slice(&bytes).map_err(storage)?;
        if entry.phase != expected {
            return Err(LedgerError::InvalidTransition);
        }
        entry.phase = ReservationPhase::Terminal;
        entry.terminal_at_ms = Some(now_ms);
        entry.output_limit_bytes = 0;
        entry.admission_error = Some(error);
        let bytes = serde_json::to_vec(&entry).map_err(storage)?;
        table.insert(key, bytes.as_slice()).map_err(storage)?;
        drop(table);
        write.commit().map_err(storage)?;
        Ok(entry)
    }

    /// Prune only terminal tombstones after the full request retention window.
    /// Preparing and accepted records cannot expire while execution is uncertain.
    pub fn prune(&self, now_ms: u64) -> Result<usize, LedgerError> {
        let mut write = self.database.begin_write().map_err(storage)?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(storage)?;
        let mut table = write.open_table(REQUESTS).map_err(storage)?;
        let keys = table
            .iter()
            .map_err(storage)?
            .map(|entry| {
                entry.map_err(storage).and_then(|(key, value)| {
                    let reservation: Reservation =
                        serde_json::from_slice(value.value()).map_err(storage)?;
                    Ok(
                        expired(&reservation, now_ms, crate::contract::REQUEST_RETENTION_MS)
                            .then(|| key.value().to_owned()),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        for key in &keys {
            table.remove(key.as_str()).map_err(storage)?;
        }
        drop(table);
        write.commit().map_err(storage)?;
        Ok(keys.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_keeps_job_identity_and_rejects_changed_request_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("requests.redb");
        let key = request_key("caller", "target", "request");
        let ledger = RequestLedger::open(&path).unwrap();
        let first = ledger.reserve(&key, [1; 32], "job-1", 1).unwrap();
        assert_eq!(first.phase, ReservationPhase::Preparing);
        ledger.advance(&key, ReservationPhase::Accepted, 2).unwrap();
        drop(ledger);
        let reopened = RequestLedger::open(&path).unwrap();
        let retry = reopened.reserve(&key, [1; 32], "job-2", 3).unwrap();
        assert_eq!(retry.job_id, "job-1");
        assert_eq!(retry.phase, ReservationPhase::Accepted);
        assert!(matches!(
            reopened.reserve(&key, [2; 32], "job-3", 4),
            Err(LedgerError::Conflict)
        ));
        assert!(matches!(
            reopened.fail_preparing(&key, crate::contract::ExecErrorV1::invalid("late"), 4),
            Err(LedgerError::InvalidTransition)
        ));
    }
    #[test]
    fn active_capacity_is_atomic_and_retention_never_expires_uncertain_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = RequestLedger::open(&temp.path().join("ledger.redb")).unwrap();
        for n in 0..MAX_ACTIVE_JOBS {
            ledger
                .reserve(&format!("r{n}"), [1; 32], &format!("j{n}"), 1)
                .unwrap();
        }
        assert!(matches!(
            ledger.reserve("overflow", [1; 32], "overflow", 2),
            Err(LedgerError::Capacity)
        ));
        ledger.advance("r0", ReservationPhase::Accepted, 2).unwrap();
        ledger.advance("r0", ReservationPhase::Terminal, 3).unwrap();
        ledger.reserve("next", [1; 32], "next", 4).unwrap();
        assert_eq!(
            ledger
                .prune(3 + crate::contract::REQUEST_RETENTION_MS - 1)
                .unwrap(),
            0
        );
        assert_eq!(
            ledger
                .prune(3 + crate::contract::REQUEST_RETENTION_MS)
                .unwrap(),
            1
        );
        assert_eq!(ledger.entries().unwrap().len(), MAX_ACTIVE_JOBS);
        assert!(ledger.lookup("r1").unwrap().is_some());
    }
    #[test]
    fn retained_output_budget_blocks_admission_until_output_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = RequestLedger::open(&temp.path().join("ledger.redb")).unwrap();
        for n in 0..16 {
            let key = format!("r{n}");
            ledger
                .reserve_with_output(
                    &key,
                    [1; 32],
                    &format!("j{n}"),
                    1,
                    crate::contract::MAX_OUTPUT_BYTES,
                )
                .unwrap();
            ledger.advance(&key, ReservationPhase::Accepted, 2).unwrap();
            ledger.advance(&key, ReservationPhase::Terminal, 3).unwrap();
        }
        assert!(matches!(
            ledger.reserve("full", [1; 32], "full", 4),
            Err(LedgerError::Capacity)
        ));
        ledger
            .reserve(
                "after",
                [1; 32],
                "after",
                3 + crate::contract::OUTPUT_RETENTION_MS,
            )
            .unwrap();
        assert_eq!(
            ledger.entries().unwrap().len(),
            17,
            "request tombstones outlive output retention"
        );
    }
    #[test]
    fn failed_admission_is_durable_releases_slot_and_retains_conflict_tombstone() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ledger.redb");
        let ledger = RequestLedger::open(&path).unwrap();
        for n in 0..MAX_ACTIVE_JOBS {
            ledger
                .reserve(&format!("r{n}"), [1; 32], &format!("j{n}"), 1)
                .unwrap();
        }
        let failure = crate::contract::ExecErrorV1::invalid("admission failed");
        let failed = ledger.fail_preparing("r0", failure.clone(), 2).unwrap();
        assert_eq!(failed.phase, ReservationPhase::Terminal);
        assert_eq!(failed.output_limit_bytes, 0);
        ledger.reserve("new", [1; 32], "new", 3).unwrap();
        drop(ledger);
        let ledger = RequestLedger::open(&path).unwrap();
        assert_eq!(
            ledger.lookup("r0").unwrap().unwrap().admission_error,
            Some(failure)
        );
        assert!(matches!(
            ledger.reserve("r0", [2; 32], "duplicate", 4),
            Err(LedgerError::Conflict)
        ));
        assert_eq!(
            ledger
                .prune(2 + crate::contract::REQUEST_RETENTION_MS - 1)
                .unwrap(),
            0
        );
        assert_eq!(
            ledger
                .prune(2 + crate::contract::REQUEST_RETENTION_MS)
                .unwrap(),
            1
        );
    }
}
