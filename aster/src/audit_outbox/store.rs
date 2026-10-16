use super::{types::*, wire::*};
use crate::durable_storage::{self, DirectoryLock};
use base64::{engine::general_purpose::STANDARD, Engine};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("audit_meta_v1");
const PENDING: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_pending_v1");
const DEAD: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_dead_v1");
const RECEIPTS: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_receipts_v1");
const MAX_RECEIPT_BYTES: usize = 32 * 1024;
/// Acknowledgement log, keyed by a monotonic cursor. Each row is fixed width:
/// sequence (u64 BE), acknowledged-at ms (u64 BE), batch digest, envelope digest.
const ACKS: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_acks_v1");
/// META key holding the last assigned acknowledgement cursor (u64 BE). Kept
/// apart from `state` so the logical accounting record is unchanged.
const ACK_CURSOR_KEY: &str = "ack_cursor_v1";
const ACK_RECORD_BYTES: usize = 80;
/// Age pruning per transaction is bounded so one acknowledgement never turns
/// into an unbounded delete; the count bound is always enforced in full.
const ACK_AGE_PRUNE_PER_COMMIT: usize = 4096;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct State {
    version: u8,
    pub identity: String,
    pub last_sequence: u64,
    pub fingerprint: String,
    pub pending_records: u64,
    pub pending_bytes: u64,
    pub dead_records: u64,
    pub dead_bytes: u64,
    pub receipt_records: u64,
    pub receipt_bytes: u64,
    pub isolation_until: u64,
    pub stall: Option<DeliveryError>,
}
impl State {
    pub fn retained(&self) -> u64 {
        self.pending_bytes + self.dead_bytes + self.receipt_bytes
    }
}
pub(crate) struct StoredEvent {
    pub sequence: u64,
    pub id: EventId,
    pub bytes: Vec<u8>,
}
pub(crate) struct Batch {
    pub events: Vec<StoredEvent>,
    pub destination: Destination,
    pub generation: u64,
    pub singleton: bool,
}
pub(crate) struct Store {
    database: redb::Database,
    _lock: DirectoryLock,
    pub state: State,
    pub config: OutboxConfig,
    pub generation: u64,
    pub last_error: Option<DeliveryError>,
    pub commits: u64,
    pub last_group_size: usize,
    oldest_cache: Option<(u64, u64)>,
    pub storage_recovered: bool,
    /// Last committed acknowledgement-log cursor (0 = none ever recorded).
    pub ack_cursor: u64,
}
fn charge(bytes: &[u8]) -> u64 {
    bytes.len() as u64 + 8
}
fn json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(AuditError::storage)
}
fn write_state(write: &redb::WriteTransaction, state: &State) -> Result<()> {
    write
        .open_table(META)
        .map_err(AuditError::storage)?
        .insert("state", json(state)?.as_slice())
        .map_err(AuditError::storage)?;
    Ok(())
}
fn commit(write: redb::WriteTransaction) -> Result<()> {
    write.commit().map_err(|_| AuditError::new(AuditErrorKind::CommitUncertain,
        "durable commit failed; acceptance outcome may be uncertain; reopen storage before retrying"))
}
impl Store {
    pub fn open(config: OutboxConfig) -> Result<Self> {
        config.validate()?;
        durable_storage::ensure_directory(&config.directory, true).map_err(AuditError::storage)?;
        let lock = DirectoryLock::acquire(config.directory.join("LOCK"), true).map_err(
            |error| match error {
                durable_storage::StorageError::LockContended { .. } => AuditError::new(
                    AuditErrorKind::Storage,
                    "audit storage directory is already owned by another opener",
                ),
                other => AuditError::storage(other),
            },
        )?;
        let path = config.directory.join("audit.redb");
        durable_storage::reject_symlink(&path).map_err(AuditError::storage)?;
        let existed = path.exists();
        if existed {
            durable_storage::check_permissions(&path, true).map_err(AuditError::storage)?;
        }
        // Create through restrictive open before redb so a permissive umask cannot expose a new DB.
        if !existed {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path).map_err(AuditError::storage)?;
            durable_storage::restrict_permissions(&path, durable_storage::FILE_MODE)
                .map_err(AuditError::storage)?;
        }
        let mut database = redb::Database::create(&path).map_err(AuditError::storage)?;
        let storage_recovered = !database.check_integrity().map_err(AuditError::storage)?;
        // Like coordination's storage adapter, accept redb's successful repair only
        // for a pre-existing database, followed by every logical/counter check below.
        if storage_recovered && !existed {
            return Err(AuditError::new(
                AuditErrorKind::Storage,
                "fresh audit database failed its integrity check",
            ));
        }
        let fingerprint = config.destination.fingerprint()?;
        let write = durable_storage::durable_write(&database).map_err(AuditError::storage)?;
        write.open_table(PENDING).map_err(AuditError::storage)?;
        write.open_table(DEAD).map_err(AuditError::storage)?;
        write.open_table(RECEIPTS).map_err(AuditError::storage)?;
        write.open_table(ACKS).map_err(AuditError::storage)?;
        let existing = write
            .open_table(META)
            .map_err(AuditError::storage)?
            .get("state")
            .map_err(AuditError::storage)?
            .map(|v| {
                if v.value().len() > 16384 {
                    Err(AuditError::storage("metadata limit"))
                } else {
                    Ok(v.value().to_vec())
                }
            })
            .transpose()?;
        let recovered = existing.is_some();
        let state: State = if let Some(bytes) = existing {
            let state: State = serde_json::from_slice(&bytes).map_err(AuditError::storage)?;
            if state.version != 1 {
                return Err(AuditError::storage("unsupported storage version"));
            }
            if state.fingerprint != fingerprint
                && (state.pending_records > 0 || state.dead_records > 0)
            {
                return Err(AuditError::new(
                    AuditErrorKind::DestinationMismatch,
                    "destination differs while retained envelopes exist",
                ));
            }
            state
        } else {
            for definition in [PENDING, DEAD, RECEIPTS, ACKS] {
                if write
                    .open_table(definition)
                    .map_err(AuditError::storage)?
                    .len()
                    .map_err(AuditError::storage)?
                    != 0
                {
                    return Err(AuditError::storage("records exist without metadata"));
                }
            }
            let mut random = [0; 16];
            getrandom::getrandom(&mut random).map_err(AuditError::storage)?;
            State {
                version: 1,
                identity: hex::encode(random),
                last_sequence: 0,
                fingerprint,
                pending_records: 0,
                pending_bytes: 0,
                dead_records: 0,
                dead_bytes: 0,
                receipt_records: 0,
                receipt_bytes: 0,
                isolation_until: 0,
                stall: None,
            }
        };
        let ack_cursor = read_ack_cursor(&write)?;
        if write
            .open_table(ACKS)
            .map_err(AuditError::storage)?
            .last()
            .map_err(AuditError::storage)?
            .is_some_and(|(key, _)| key.value() > ack_cursor)
        {
            return Err(AuditError::storage(
                "acknowledgement log ahead of its cursor",
            ));
        }
        if !recovered {
            write_state(&write, &state)?;
            commit(write)?;
        } else {
            drop(write);
        }
        let mut store = Self {
            database,
            _lock: lock,
            last_error: state.stall.clone(),
            state,
            config,
            generation: 1,
            commits: 0,
            last_group_size: 0,
            oldest_cache: None,
            storage_recovered,
            ack_cursor,
        };
        store.validate_recovered()?;
        store.reload_oldest()?;
        store.prune_acknowledgements_on_open()?;
        if store.state.fingerprint != store.config.destination.fingerprint()? {
            store.reconfigure(store.config.destination.clone())?;
        }
        Ok(store)
    }

    fn validate_recovered(&self) -> Result<()> {
        let read = self.database.begin_read().map_err(AuditError::storage)?;
        for (definition, expected_count, expected_bytes, is_receipt) in [
            (
                PENDING,
                self.state.pending_records,
                self.state.pending_bytes,
                false,
            ),
            (DEAD, self.state.dead_records, self.state.dead_bytes, false),
            (
                RECEIPTS,
                self.state.receipt_records,
                self.state.receipt_bytes,
                true,
            ),
        ] {
            let mut count = 0u64;
            let mut bytes = 0u64;
            for row in read
                .open_table(definition)
                .map_err(AuditError::storage)?
                .iter()
                .map_err(AuditError::storage)?
            {
                let (key, value) = row.map_err(AuditError::storage)?;
                let sequence = key.value();
                let data = value.value();
                if sequence == 0 || sequence > self.state.last_sequence {
                    return Err(AuditError::storage("invalid stored sequence"));
                }
                if is_receipt {
                    if data.len() > MAX_RECEIPT_BYTES {
                        return Err(AuditError::storage("receipt limit"));
                    }
                    let receipts: Vec<DispositionReceipt> =
                        serde_json::from_slice(data).map_err(AuditError::storage)?;
                    if receipts.is_empty()
                        || receipts.iter().any(|r| {
                            parse_id(r.event_id.as_str())
                                .map(|(i, s, _)| i != self.state.identity || s != sequence)
                                .unwrap_or(true)
                        })
                    {
                        return Err(AuditError::storage("invalid stored receipts"));
                    }
                } else {
                    if data.len() > MAX_EVENT_BYTES {
                        return Err(AuditError::storage("envelope limit"));
                    }
                    let envelope: EnvelopeV1 =
                        serde_json::from_slice(data).map_err(AuditError::storage)?;
                    if envelope.version != 1
                        || envelope.outbox_id != self.state.identity
                        || envelope.sequence != sequence
                    {
                        return Err(AuditError::storage("invalid stored envelope"));
                    }
                }
                count = count
                    .checked_add(1)
                    .ok_or_else(|| AuditError::storage("count overflow"))?;
                bytes = bytes
                    .checked_add(charge(data))
                    .ok_or_else(|| AuditError::storage("byte overflow"))?;
            }
            if count != expected_count || bytes != expected_bytes {
                return Err(AuditError::storage("storage accounting mismatch"));
            }
        }
        if self
            .state
            .pending_bytes
            .checked_add(self.state.dead_bytes)
            .and_then(|b| b.checked_add(self.state.receipt_bytes))
            .is_none()
        {
            return Err(AuditError::storage("retained accounting overflow"));
        }
        Ok(())
    }

    /// Each returned success follows the single immediate-durability group commit.
    pub fn submit_group(&mut self, events: Vec<AuditEvent>) -> Vec<Result<EventId>> {
        let mut outcomes = Vec::with_capacity(events.len());
        let mut state = self.state.clone();
        let mut rows = Vec::new();
        for event in events {
            let result = (|| {
                let sequence = state.last_sequence.checked_add(1).ok_or_else(|| {
                    AuditError::new(AuditErrorKind::StorageFull, "sequence space exhausted")
                })?;
                let envelope = EnvelopeV1 {
                    version: 1,
                    outbox_id: state.identity.clone(),
                    sequence,
                    timestamp_ms: event.timestamp_ms,
                    kind: event.kind,
                    payload: event.payload,
                };
                let bytes = bounded_json(&envelope, self.config.max_event_bytes)?;
                if state.retained().saturating_add(charge(&bytes)) > self.config.max_retained_bytes
                {
                    return Err(AuditError::new(
                        AuditErrorKind::StorageFull,
                        "retained envelope and receipt budget exhausted",
                    ));
                }
                let id = id_for(&state.identity, sequence, &bytes);
                state.last_sequence = sequence;
                state.pending_records += 1;
                state.pending_bytes += charge(&bytes);
                rows.push((sequence, bytes));
                Ok(id)
            })();
            outcomes.push(result);
        }
        if rows.is_empty() {
            return outcomes;
        }
        let result: Result<()> = (|| {
            #[cfg(test)]
            if super::tests::WRITE_FAILURE.with(|f| f.get()) {
                return Err(AuditError::new(
                    AuditErrorKind::Storage,
                    "injected write failure",
                ));
            }
            let write =
                durable_storage::durable_write(&self.database).map_err(AuditError::storage)?;
            {
                let mut table = write.open_table(PENDING).map_err(AuditError::storage)?;
                for (seq, bytes) in &rows {
                    table
                        .insert(*seq, bytes.as_slice())
                        .map_err(AuditError::storage)?;
                }
            }
            write_state(&write, &state)?;
            #[cfg(test)]
            if super::tests::COMMIT_FAILURE.with(|f| f.get()) {
                return Err(AuditError::new(
                    AuditErrorKind::CommitUncertain,
                    "injected commit failure",
                ));
            }
            commit(write)?;
            #[cfg(test)]
            if super::tests::COMMIT_AFTER_FAILURE.with(|f| f.get()) {
                return Err(AuditError::new(
                    AuditErrorKind::CommitUncertain,
                    "injected failure after durable commit",
                ));
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.state = state;
                self.commits += 1;
                self.last_group_size = rows.len();
                if self.oldest_cache.is_none() {
                    if let Err(error) = self.reload_oldest() {
                        for outcome in &mut outcomes {
                            if outcome.is_ok() {
                                *outcome = Err(error.clone());
                            }
                        }
                    }
                }
            }
            Err(error) => {
                for outcome in &mut outcomes {
                    if outcome.is_ok() {
                        *outcome = Err(error.clone());
                    }
                }
            }
        }
        outcomes
    }

    pub fn oldest(&self) -> Result<Option<(u64, u64)>> {
        Ok(self.oldest_cache)
    }
    fn reload_oldest(&mut self) -> Result<()> {
        let read = self.database.begin_read().map_err(AuditError::storage)?;
        let table = read.open_table(PENDING).map_err(AuditError::storage)?;
        let first = table
            .iter()
            .map_err(AuditError::storage)?
            .next()
            .transpose()
            .map_err(AuditError::storage)?;
        #[derive(serde::Deserialize)]
        struct Header {
            timestamp_ms: u64,
        }
        self.oldest_cache = first
            .map(|(key, value)| {
                // serde skips unknown fields without constructing a payload Value tree.
                let envelope: Header =
                    serde_json::from_slice(value.value()).map_err(AuditError::storage)?;
                Ok((key.value(), envelope.timestamp_ms))
            })
            .transpose()?;
        Ok(())
    }
    pub fn minimum_admission_charge(&self) -> u64 {
        let Some(sequence) = self.state.last_sequence.checked_add(1) else {
            return u64::MAX;
        };
        let minimal = EnvelopeV1 {
            version: 1,
            outbox_id: self.state.identity.clone(),
            sequence,
            timestamp_ms: 0,
            kind: "a".into(),
            payload: serde_json::json!({}),
        };
        serde_json::to_vec(&minimal)
            .map(|b| charge(&b))
            .unwrap_or(u64::MAX)
    }
    pub fn batch(&self) -> Result<Option<Batch>> {
        if self.state.stall.is_some() {
            return Ok(None);
        }
        let read = self.database.begin_read().map_err(AuditError::storage)?;
        let table = read.open_table(PENDING).map_err(AuditError::storage)?;
        let mut events = Vec::new();
        let mut total = 0;
        let singleton = self.state.isolation_until > 0;
        for row in table.iter().map_err(AuditError::storage)? {
            let (key, value) = row.map_err(AuditError::storage)?;
            let bytes = value.value();
            if events.len() == MAX_BATCH_EVENTS
                || total + bytes.len() > MAX_BATCH_BYTES
                || (singleton && !events.is_empty())
            {
                break;
            }
            total += bytes.len();
            events.push(StoredEvent {
                sequence: key.value(),
                id: id_for(&self.state.identity, key.value(), bytes),
                bytes: bytes.to_vec(),
            });
        }
        let singleton = singleton || events.len() == 1;
        Ok((!events.is_empty()).then(|| Batch {
            events,
            destination: self.config.destination.clone(),
            generation: self.generation,
            singleton,
        }))
    }
    pub fn acknowledge(&mut self, generation: u64, ids: Vec<EventId>) -> Result<()> {
        if generation != self.generation {
            return Err(AuditError::new(
                AuditErrorKind::StaleDelivery,
                "destination changed while request was in flight",
            ));
        }
        let write = durable_storage::durable_write(&self.database).map_err(AuditError::storage)?;
        let mut state = self.state.clone();
        let mut acknowledged = Vec::with_capacity(ids.len());
        {
            let mut table = write.open_table(PENDING).map_err(AuditError::storage)?;
            for id in &ids {
                let (_, sequence, _) = parse_id(id.as_str())?;
                let bytes = table
                    .get(sequence)
                    .map_err(AuditError::storage)?
                    .map(|v| v.value().to_vec())
                    .ok_or_else(|| {
                        AuditError::new(
                            AuditErrorKind::StaleDelivery,
                            "acknowledged record changed while request was in flight",
                        )
                    })?;
                if id_for(&state.identity, sequence, &bytes) != *id {
                    return Err(AuditError::new(
                        AuditErrorKind::StaleDelivery,
                        "acknowledged full ID does not match current record",
                    ));
                }
                table.remove(sequence).map_err(AuditError::storage)?;
                state.pending_records -= 1;
                state.pending_bytes -= charge(&bytes);
                acknowledged.push(sequence);
            }
            if state.isolation_until > 0 {
                let first = table
                    .iter()
                    .map_err(AuditError::storage)?
                    .next()
                    .transpose()
                    .map_err(AuditError::storage)?;
                if first.is_none_or(|(key, _)| key.value() > state.isolation_until) {
                    state.isolation_until = 0;
                }
            }
        }
        state.stall = None;
        write_state(&write, &state)?;
        // The acknowledgement log rides in the deleting transaction: after any
        // crash either both the deletion and its log rows exist, or neither.
        let ack_cursor = self.log_acknowledgements(&write, &ids, &acknowledged)?;
        #[cfg(test)]
        super::tests::ack_crash_point(1);
        commit(write)?;
        #[cfg(test)]
        super::tests::ack_crash_point(2);
        self.state = state;
        self.ack_cursor = ack_cursor;
        self.last_error = None;
        self.reload_oldest()?;
        Ok(())
    }

    /// Append one row per acknowledged record and enforce the log bounds, inside
    /// the caller's write transaction. Returns the new last cursor.
    fn log_acknowledgements(
        &self,
        write: &redb::WriteTransaction,
        ids: &[EventId],
        sequences: &[u64],
    ) -> Result<u64> {
        let mut cursor = self.ack_cursor;
        if self.config.acknowledgement_log_max_entries > 0 {
            // The receiver's acknowledgement was checked to echo exactly this digest.
            let batch = decode_digest(&batch_id(ids.iter().map(EventId::as_str)))?;
            let now = now_ms();
            let mut table = write.open_table(ACKS).map_err(AuditError::storage)?;
            for (id, sequence) in ids.iter().zip(sequences) {
                let (_, _, digest) = parse_id(id.as_str())?;
                cursor = cursor
                    .checked_add(1)
                    .ok_or_else(|| AuditError::storage("acknowledgement cursor exhausted"))?;
                let mut row = [0u8; ACK_RECORD_BYTES];
                row[..8].copy_from_slice(&sequence.to_be_bytes());
                row[8..16].copy_from_slice(&now.to_be_bytes());
                row[16..48].copy_from_slice(&batch);
                row[48..].copy_from_slice(&decode_digest(digest)?);
                table
                    .insert(cursor, row.as_slice())
                    .map_err(AuditError::storage)?;
            }
            drop(table);
            write
                .open_table(META)
                .map_err(AuditError::storage)?
                .insert(ACK_CURSOR_KEY, cursor.to_be_bytes().as_slice())
                .map_err(AuditError::storage)?;
        }
        self.prune_acknowledgements(write, ACK_AGE_PRUNE_PER_COMMIT)?;
        Ok(cursor)
    }

    /// Remove the oldest rows beyond the count bound, then rows older than the
    /// age bound (lowest cursor first, at most `age_limit` of them).
    fn prune_acknowledgements(
        &self,
        write: &redb::WriteTransaction,
        age_limit: usize,
    ) -> Result<usize> {
        let mut table = write.open_table(ACKS).map_err(AuditError::storage)?;
        let mut excess = table
            .len()
            .map_err(AuditError::storage)?
            .saturating_sub(self.config.acknowledgement_log_max_entries);
        let cutoff = match self.config.acknowledgement_log_max_age_ms {
            0 => 0,
            age => now_ms().saturating_sub(age),
        };
        let mut aged = 0;
        let mut removed = 0;
        loop {
            let Some(at) = table
                .first()
                .map_err(AuditError::storage)?
                .map(|(_, value)| ack_time(value.value()))
                .transpose()?
            else {
                break;
            };
            let expired = at < cutoff && aged < age_limit;
            if excess == 0 && !expired {
                break;
            }
            table.pop_first().map_err(AuditError::storage)?;
            if excess > 0 {
                excess -= 1;
            } else {
                aged += 1;
            }
            removed += 1;
        }
        Ok(removed)
    }

    fn prune_acknowledgements_on_open(&mut self) -> Result<()> {
        let write = durable_storage::durable_write(&self.database).map_err(AuditError::storage)?;
        if self.prune_acknowledgements(&write, usize::MAX)? > 0 {
            commit(write)?;
        }
        Ok(())
    }

    /// Up to `limit` acknowledgements with a cursor greater than `after`, in cursor order.
    pub fn acknowledgements(&self, after: u64, limit: usize) -> Result<Vec<Acknowledgement>> {
        let read = self.database.begin_read().map_err(AuditError::storage)?;
        let mut out = Vec::new();
        // A database written before the log existed has no table until the
        // first acknowledgement commits one.
        let table = match read.open_table(ACKS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
            Err(error) => return Err(AuditError::storage(error)),
        };
        let Some(start) = after.checked_add(1) else {
            return Ok(out);
        };
        for row in table.range(start..).map_err(AuditError::storage)? {
            if out.len() == limit {
                break;
            }
            let (key, value) = row.map_err(AuditError::storage)?;
            let row = value.value();
            let acknowledged_at_ms = ack_time(row)?;
            let sequence = u64::from_be_bytes(row[..8].try_into().unwrap());
            out.push(Acknowledgement {
                cursor: key.value(),
                event_id: EventId(format!(
                    "aster-audit-v1:{}:{sequence}:{}",
                    self.state.identity,
                    hex::encode(&row[48..])
                )),
                sequence,
                batch_id: hex::encode(&row[16..48]),
                acknowledged_at_ms,
            });
        }
        Ok(out)
    }
    pub fn delivery_error(
        &mut self,
        generation: u64,
        error: DeliveryError,
        isolate_until: Option<u64>,
        stall: bool,
    ) -> Result<()> {
        if generation != self.generation {
            return Ok(());
        }
        self.last_error = Some(error.clone());
        if isolate_until.is_some() || stall {
            let mut state = self.state.clone();
            if let Some(until) = isolate_until {
                state.isolation_until = state.isolation_until.max(until);
            }
            if stall {
                state.stall = Some(error);
            }
            let write =
                durable_storage::durable_write(&self.database).map_err(AuditError::storage)?;
            write_state(&write, &state)?;
            commit(write)?;
            self.state = state;
        }
        Ok(())
    }
    pub fn reconfigure(&mut self, destination: Destination) -> Result<()> {
        let fingerprint = destination.fingerprint()?;
        if fingerprint != self.state.fingerprint
            && (self.state.pending_records > 0 || self.state.dead_records > 0)
        {
            return Err(AuditError::new(
                AuditErrorKind::DestinationMismatch,
                "destination differs while retained envelopes exist",
            ));
        }
        let mut state = self.state.clone();
        state.fingerprint = fingerprint;
        state.stall = None;
        let write = durable_storage::durable_write(&self.database).map_err(AuditError::storage)?;
        write_state(&write, &state)?;
        commit(write)?;
        self.state = state;
        self.config.destination = destination;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(AuditError::worker)?;
        self.last_error = None;
        Ok(())
    }
    pub fn quarantine(&mut self, id: String, reason: String, operator: String) -> Result<()> {
        if reason.is_empty() || reason.len() > 512 || operator.is_empty() || operator.len() > 256 {
            return Err(AuditError::new(
                AuditErrorKind::InvalidEvent,
                "quarantine requires bounded reason and operator reference",
            ));
        }
        let (identity, sequence, _) = parse_id(&id)?;
        if identity != self.state.identity {
            return Err(AuditError::new(
                AuditErrorKind::NotFound,
                "event ID belongs to another outbox",
            ));
        }
        let write = durable_storage::durable_write(&self.database).map_err(AuditError::storage)?;
        let dead_bytes = write
            .open_table(DEAD)
            .map_err(AuditError::storage)?
            .get(sequence)
            .map_err(AuditError::storage)?
            .map(|v| v.value().to_vec());
        if let Some(bytes) = dead_bytes {
            return if id_for(identity, sequence, &bytes).as_str() == id {
                Ok(())
            } else {
                Err(AuditError::new(
                    AuditErrorKind::NotFound,
                    "full event ID differs",
                ))
            };
        }
        let bytes = write
            .open_table(PENDING)
            .map_err(AuditError::storage)?
            .get(sequence)
            .map_err(AuditError::storage)?
            .map(|v| v.value().to_vec())
            .ok_or_else(|| AuditError::new(AuditErrorKind::NotFound, "pending event not found"))?;
        if id_for(identity, sequence, &bytes).as_str() != id {
            return Err(AuditError::new(
                AuditErrorKind::NotFound,
                "full event ID differs",
            ));
        }
        // Only the current blocker can be bypassed; later records retain strict order.
        if self.oldest()?.map(|v| v.0) != Some(sequence) {
            return Err(AuditError::new(
                AuditErrorKind::InvalidEvent,
                "only the oldest pending event can be quarantined",
            ));
        }
        if !self.state.stall.as_ref().is_some_and(|stall| {
            stall.sequence == sequence
                && matches!(
                    stall.kind,
                    DeliveryErrorKind::Rejected | DeliveryErrorKind::Integrity
                )
        }) {
            return Err(AuditError::new(
                AuditErrorKind::InvalidEvent,
                "quarantine requires a rejected or integrity-conflicting singleton stall",
            ));
        }
        let mut receipts = receipt_row(&write, sequence)?;
        let old_charge = if receipts.is_empty() {
            0
        } else {
            charge(&json(&receipts)?)
        };
        receipts.push(DispositionReceipt {
            event_id: EventId(id),
            action: "quarantine".into(),
            timestamp_ms: now_ms(),
            reason,
            operator_reference: operator,
            destination_fingerprint: self.state.fingerprint.clone(),
            rejection: self.last_error.clone(),
        });
        let receipt_bytes = bounded_json(&receipts, MAX_RECEIPT_BYTES).map_err(|_| {
            AuditError::new(
                AuditErrorKind::StorageFull,
                "disposition trail limit exhausted",
            )
        })?;
        let mut state = self.state.clone();
        state.receipt_bytes = state.receipt_bytes - old_charge + charge(&receipt_bytes);
        if old_charge == 0 {
            state.receipt_records += 1;
        }
        if state.retained() > self.config.max_retained_bytes {
            return Err(AuditError::new(
                AuditErrorKind::StorageFull,
                "no budget for durable disposition receipt",
            ));
        }
        write
            .open_table(DEAD)
            .map_err(AuditError::storage)?
            .insert(sequence, bytes.as_slice())
            .map_err(AuditError::storage)?;
        write
            .open_table(PENDING)
            .map_err(AuditError::storage)?
            .remove(sequence)
            .map_err(AuditError::storage)?;
        write
            .open_table(RECEIPTS)
            .map_err(AuditError::storage)?
            .insert(sequence, receipt_bytes.as_slice())
            .map_err(AuditError::storage)?;
        state.pending_records -= 1;
        state.pending_bytes -= charge(&bytes);
        state.dead_records += 1;
        state.dead_bytes += charge(&bytes);
        state.stall = None;
        write_state(&write, &state)?;
        #[cfg(test)]
        super::tests::quarantine_crash_point(1);
        commit(write)?;
        #[cfg(test)]
        super::tests::quarantine_crash_point(2);
        self.state = state;
        self.last_error = None;
        self.generation += 1; // fence any acceptance still in flight for the moved event
        self.reload_oldest()?;
        Ok(())
    }
    pub fn export(&self, id: String) -> Result<QuarantineExport> {
        let (identity, sequence, _) = parse_id(&id)?;
        let read = self.database.begin_read().map_err(AuditError::storage)?;
        let mut pending = false;
        let mut bytes = read
            .open_table(DEAD)
            .map_err(AuditError::storage)?
            .get(sequence)
            .map_err(AuditError::storage)?
            .map(|v| v.value().to_vec());
        if bytes.is_none() {
            pending = true;
            bytes = read
                .open_table(PENDING)
                .map_err(AuditError::storage)?
                .get(sequence)
                .map_err(AuditError::storage)?
                .map(|v| v.value().to_vec());
        }
        let bytes = bytes.ok_or_else(|| {
            AuditError::new(AuditErrorKind::NotFound, "retained envelope not found")
        })?;
        if identity != self.state.identity || id_for(identity, sequence, &bytes).as_str() != id {
            return Err(AuditError::new(
                AuditErrorKind::NotFound,
                "full event ID differs",
            ));
        }
        let receipts = read
            .open_table(RECEIPTS)
            .map_err(AuditError::storage)?
            .get(sequence)
            .map_err(AuditError::storage)?
            .map(|v| serde_json::from_slice(v.value()).map_err(AuditError::storage))
            .transpose()?
            .unwrap_or_default();
        Ok(QuarantineExport {
            event_id: EventId(id),
            envelope_base64: STANDARD.encode(bytes),
            pending,
            receipts,
        })
    }
    pub fn replay(&mut self, id: String) -> Result<()> {
        let (identity, sequence, _) = parse_id(&id)?;
        let write = durable_storage::durable_write(&self.database).map_err(AuditError::storage)?;
        let bytes = write
            .open_table(DEAD)
            .map_err(AuditError::storage)?
            .get(sequence)
            .map_err(AuditError::storage)?
            .map(|v| v.value().to_vec());
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => {
                let exported = self.export(id)?;
                return if exported.pending && !exported.receipts.is_empty() {
                    Ok(())
                } else {
                    Err(AuditError::new(
                        AuditErrorKind::NotFound,
                        "dead-letter event not found",
                    ))
                };
            }
        };
        if identity != self.state.identity || id_for(identity, sequence, &bytes).as_str() != id {
            return Err(AuditError::new(
                AuditErrorKind::NotFound,
                "full event ID differs",
            ));
        }
        let mut receipts = receipt_row(&write, sequence)?;
        let old_charge = charge(&json(&receipts)?);
        receipts.push(DispositionReceipt {
            event_id: EventId(id),
            action: "replay".into(),
            timestamp_ms: now_ms(),
            reason: "explicit replay".into(),
            operator_reference: "replay API".into(),
            destination_fingerprint: self.state.fingerprint.clone(),
            rejection: None,
        });
        let receipt_bytes = bounded_json(&receipts, MAX_RECEIPT_BYTES).map_err(|_| {
            AuditError::new(
                AuditErrorKind::StorageFull,
                "disposition trail limit exhausted",
            )
        })?;
        let mut state = self.state.clone();
        state.receipt_bytes = state.receipt_bytes - old_charge + charge(&receipt_bytes);
        if state.retained() > self.config.max_retained_bytes {
            return Err(AuditError::new(
                AuditErrorKind::StorageFull,
                "no budget for replay disposition receipt",
            ));
        }
        write
            .open_table(PENDING)
            .map_err(AuditError::storage)?
            .insert(sequence, bytes.as_slice())
            .map_err(AuditError::storage)?;
        write
            .open_table(DEAD)
            .map_err(AuditError::storage)?
            .remove(sequence)
            .map_err(AuditError::storage)?;
        write
            .open_table(RECEIPTS)
            .map_err(AuditError::storage)?
            .insert(sequence, receipt_bytes.as_slice())
            .map_err(AuditError::storage)?;
        state.pending_records += 1;
        state.pending_bytes += charge(&bytes);
        state.dead_records -= 1;
        state.dead_bytes -= charge(&bytes);
        state.stall = None;
        state.isolation_until = state.isolation_until.max(sequence);
        write_state(&write, &state)?;
        commit(write)?;
        self.state = state;
        self.last_error = None;
        self.generation += 1;
        self.reload_oldest()?;
        Ok(())
    }
    pub fn flush_result(&self, watermark: u64) -> Result<bool> {
        let read = self.database.begin_read().map_err(AuditError::storage)?;
        if read
            .open_table(RECEIPTS)
            .map_err(AuditError::storage)?
            .range(..=watermark)
            .map_err(AuditError::storage)?
            .next()
            .transpose()
            .map_err(AuditError::storage)?
            .is_some()
        {
            return Err(AuditError::new(
                AuditErrorKind::Disposition,
                "flush prefix includes an explicit dead-letter disposition",
            ));
        }
        if self.oldest()?.is_none_or(|(seq, _)| seq > watermark) {
            return Ok(true);
        }
        if let Some(error) = &self.last_error {
            if error.immediate_flush_failure && error.sequence <= watermark {
                return Err(AuditError::new(
                    AuditErrorKind::DeliveryStalled,
                    error.message.clone(),
                ));
            }
        }
        Ok(false)
    }
    pub fn close(self) -> Result<()> {
        drop(self.database);
        self._lock.unlock().map_err(AuditError::storage)
    }
}
fn read_ack_cursor(write: &redb::WriteTransaction) -> Result<u64> {
    write
        .open_table(META)
        .map_err(AuditError::storage)?
        .get(ACK_CURSOR_KEY)
        .map_err(AuditError::storage)?
        .map(|value| {
            <[u8; 8]>::try_from(value.value())
                .map(u64::from_be_bytes)
                .map_err(|_| AuditError::storage("invalid acknowledgement cursor"))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}
/// Validates the fixed row width and returns its acknowledged-at time.
fn ack_time(row: &[u8]) -> Result<u64> {
    if row.len() != ACK_RECORD_BYTES {
        return Err(AuditError::storage("invalid acknowledgement row"));
    }
    Ok(u64::from_be_bytes(row[8..16].try_into().unwrap()))
}
fn decode_digest(hex_digest: &str) -> Result<[u8; 32]> {
    hex::decode(hex_digest)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| AuditError::storage("invalid digest"))
}
fn receipt_row(write: &redb::WriteTransaction, sequence: u64) -> Result<Vec<DispositionReceipt>> {
    write
        .open_table(RECEIPTS)
        .map_err(AuditError::storage)?
        .get(sequence)
        .map_err(AuditError::storage)?
        .map(|v| serde_json::from_slice(v.value()).map_err(AuditError::storage))
        .transpose()
        .map(|v| v.unwrap_or_default())
}
