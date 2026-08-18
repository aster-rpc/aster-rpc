//! `RaftLogStorage` over `raft.redb` (§11.2).
//!
//! ## Why a dedicated thread rather than `spawn_blocking`
//!
//! §11.1 requires a single-writer blocking actor per database and forbids
//! running a redb transaction on a Tokio worker. `spawn_blocking` satisfies
//! neither on its own: its pool runs tasks concurrently, so two appends could
//! overlap, and OpenRaft's contract is explicit that *all* write IO must be
//! serialized — "saving a vote and appending a log entry must be serialized
//! too". One owned thread consuming one bounded channel gives that ordering by
//! construction instead of by convention.
//!
//! The channel is bounded because an unbounded one converts a slow disk into
//! unbounded memory growth, and the queue in front of a full disk is exactly
//! where that happens.
//!
//! ## Fatal is sticky
//!
//! §4.3 requires a durability or callback-path failure to move the local
//! runtime to fatal/not-ready rather than report success. A failed commit
//! leaves this store unable to say what is durable, so every later operation
//! fails too: continuing after one would let a vote be acknowledged on top of
//! a log write that never landed.

// See the `storage` module docs: nothing in a shipping build reaches
// `VoterStorage::open` until Slice 3 supplies a verified identity, so this
// module's items are exercised only by tests for now.
#![allow(dead_code)]

use std::ops::RangeBounds;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use openraft::storage::{LogFlushed, RaftLogReader, RaftLogStorage};
use openraft::{AnyError, Entry, LogId, LogState, StorageError, StorageIOError, Vote};
use redb::{Database, ReadableDatabase, ReadableTable};
use tokio::sync::{mpsc, oneshot};

use super::error::{Db, StorageError as AsterStorageError, StorageResult};
use super::schema::{KEY_COMMITTED, KEY_LAST_PURGED, KEY_VOTE, RAFT_LOG, RAFT_META};
use crate::coordination::contract::{payload_fory, LogIdV1};
use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::raft::{RaftEntryV1, VoteV1};
use crate::coordination::typeconfig::CoordinationTypeConfig;

/// Ceiling on one encoded log-entry value.
///
/// Applied before allocation on the way in and again after decode, per §4.2.
/// A stored value larger than this is a corrupt or foreign row, not an entry
/// worth trying to decode.
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling on one encoded metadata value (vote, committed, purge marker).
const MAX_META_BYTES: usize = 4 * 1024;

/// Depth of the actor's command queue.
const LOG_ACTOR_QUEUE: usize = 64;

// ── Codecs ───────────────────────────────────────────────────────────────────
//
// Every stored value goes through the declared Fory root and the existing
// mirror conversions in `super::super::convert`. There is no second encoding
// and no raw memory layout: a row this build wrote must be readable by any
// build that shares the schema and codec versions, and by no other.

fn encode_entry(entry: &Entry<CoordinationTypeConfig>) -> StorageResult<Vec<u8>> {
    let mirror = RaftEntryV1::from(entry);
    let bytes = payload_fory::<RaftEntryV1>()
        .serialize(&mirror)
        .map_err(|source| AsterStorageError::codec(Db::Raft, "RaftEntryV1", source))?;
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err(AsterStorageError::codec(
            Db::Raft,
            "RaftEntryV1",
            format!(
                "encoded entry is {} bytes, over the {MAX_ENTRY_BYTES}-byte ceiling",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

fn decode_entry(bytes: &[u8]) -> StorageResult<Entry<CoordinationTypeConfig>> {
    // Before allocation, not after: a length check that runs only once the
    // value is already decoded has not bounded anything.
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err(AsterStorageError::codec(
            Db::Raft,
            "RaftEntryV1",
            format!(
                "stored entry is {} bytes, over the {MAX_ENTRY_BYTES}-byte ceiling",
                bytes.len()
            ),
        ));
    }
    let mirror: RaftEntryV1 = payload_fory::<RaftEntryV1>()
        .deserialize(bytes)
        .map_err(|source| AsterStorageError::codec(Db::Raft, "RaftEntryV1", source))?;
    // The mirror conversion is fallible and stays fallible here. An unknown
    // payload variant is fatal rather than degraded to a blank: applying a
    // blank in its place would commit a gap in the replicated history.
    Entry::<CoordinationTypeConfig>::try_from(&mirror)
        .map_err(|err| AsterStorageError::codec(Db::Raft, "RaftEntryV1", format!("{err:?}")))
}

fn encode_vote(vote: &Vote<CoordinationNodeId>) -> StorageResult<Vec<u8>> {
    let mirror = VoteV1::from(vote);
    bounded(
        payload_fory::<VoteV1>()
            .serialize(&mirror)
            .map_err(|source| AsterStorageError::codec(Db::Raft, "VoteV1", source))?,
        "VoteV1",
    )
}

fn decode_vote(bytes: &[u8]) -> StorageResult<Vote<CoordinationNodeId>> {
    check_meta_len(bytes, "VoteV1")?;
    let mirror: VoteV1 = payload_fory::<VoteV1>()
        .deserialize(bytes)
        .map_err(|source| AsterStorageError::codec(Db::Raft, "VoteV1", source))?;
    Vote::<CoordinationNodeId>::try_from(&mirror)
        .map_err(|err| AsterStorageError::codec(Db::Raft, "VoteV1", format!("{err:?}")))
}

fn encode_log_id(log_id: &LogId<CoordinationNodeId>) -> StorageResult<Vec<u8>> {
    let mirror = LogIdV1::from(*log_id);
    bounded(
        payload_fory::<LogIdV1>()
            .serialize(&mirror)
            .map_err(|source| AsterStorageError::codec(Db::Raft, "LogIdV1", source))?,
        "LogIdV1",
    )
}

fn decode_log_id(bytes: &[u8]) -> StorageResult<LogId<CoordinationNodeId>> {
    check_meta_len(bytes, "LogIdV1")?;
    let mirror: LogIdV1 = payload_fory::<LogIdV1>()
        .deserialize(bytes)
        .map_err(|source| AsterStorageError::codec(Db::Raft, "LogIdV1", source))?;
    LogId::<CoordinationNodeId>::try_from(&mirror)
        .map_err(|err| AsterStorageError::codec(Db::Raft, "LogIdV1", format!("{err:?}")))
}

fn bounded(bytes: Vec<u8>, what: &'static str) -> StorageResult<Vec<u8>> {
    check_meta_len(&bytes, what)?;
    Ok(bytes)
}

fn check_meta_len(bytes: &[u8], what: &'static str) -> StorageResult<()> {
    if bytes.len() > MAX_META_BYTES {
        return Err(AsterStorageError::codec(
            Db::Raft,
            what,
            format!(
                "{} bytes, over the {MAX_META_BYTES}-byte metadata ceiling",
                bytes.len()
            ),
        ));
    }
    Ok(())
}

// ── Actor protocol ───────────────────────────────────────────────────────────

type Reply<T> = oneshot::Sender<StorageResult<T>>;

enum LogCommand {
    LogState(Reply<LogState<CoordinationTypeConfig>>),
    SaveVote {
        vote: Vote<CoordinationNodeId>,
        reply: Reply<()>,
    },
    ReadVote(Reply<Option<Vote<CoordinationNodeId>>>),
    SaveCommitted {
        committed: Option<LogId<CoordinationNodeId>>,
        reply: Reply<()>,
    },
    ReadCommitted(Reply<Option<LogId<CoordinationNodeId>>>),
    Append {
        entries: Vec<Entry<CoordinationTypeConfig>>,
        reply: Reply<()>,
    },
    Truncate {
        log_id: LogId<CoordinationNodeId>,
        reply: Reply<()>,
    },
    Purge {
        log_id: LogId<CoordinationNodeId>,
        reply: Reply<()>,
    },
    Read {
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
        reply: Reply<Vec<Entry<CoordinationTypeConfig>>>,
    },
    /// Stop the actor so it drops its `Database` before the directory lock is
    /// released.
    Shutdown,
}

/// The single-writer thread that owns `raft.redb`.
struct LogActor {
    database: Database,
}

impl LogActor {
    fn run(mut self, mut rx: mpsc::Receiver<LogCommand>, stop: Arc<AtomicBool>) {
        // `blocking_recv` is what keeps every redb transaction off the Tokio
        // worker pool: this loop runs on its own OS thread and blocks there.
        while let Some(command) = rx.blocking_recv() {
            // Checked before servicing anything, so a shutdown that arrives
            // while the queue still has work stops promptly rather than
            // draining it. Returning here drops the `Database`, which is the
            // whole point: the directory lock must not be released while this
            // thread still has the file open.
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match command {
                LogCommand::Shutdown => break,
                LogCommand::LogState(reply) => send(reply, self.log_state()),
                LogCommand::SaveVote { vote, reply } => send(reply, self.save_vote(&vote)),
                LogCommand::ReadVote(reply) => send(reply, self.read_vote()),
                LogCommand::SaveCommitted { committed, reply } => {
                    send(reply, self.save_committed(committed.as_ref()))
                }
                LogCommand::ReadCommitted(reply) => send(reply, self.read_committed()),
                LogCommand::Append { entries, reply } => send(reply, self.append(&entries)),
                LogCommand::Truncate { log_id, reply } => send(reply, self.truncate(&log_id)),
                LogCommand::Purge { log_id, reply } => send(reply, self.purge(&log_id)),
                LogCommand::Read { start, end, reply } => send(reply, self.read(start, end)),
            }
        }
    }

    // ── Reads ────────────────────────────────────────────────────────────────

    fn log_state(&self) -> StorageResult<LogState<CoordinationTypeConfig>> {
        let read = self
            .database
            .begin_read()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        let last_purged = self.marker(&read, KEY_LAST_PURGED)?;
        let last_retained = self.last_entry_log_id(&read)?;
        Ok(LogState {
            last_purged_log_id: last_purged,
            // OpenRaft's contract: with no entry present, the last log id is
            // the purge marker rather than `None`. Reporting `None` after a
            // purge that removed everything would look like an empty log that
            // had never held anything, and replication would restart from
            // zero.
            last_log_id: last_retained.or(last_purged),
        })
    }

    fn marker(
        &self,
        read: &redb::ReadTransaction,
        key: &str,
    ) -> StorageResult<Option<LogId<CoordinationNodeId>>> {
        let table = match read.open_table(RAFT_META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
        };
        let value = table
            .get(key)
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        match value {
            Some(v) => Ok(Some(decode_log_id(v.value())?)),
            None => Ok(None),
        }
    }

    fn last_entry_log_id(
        &self,
        read: &redb::ReadTransaction,
    ) -> StorageResult<Option<LogId<CoordinationNodeId>>> {
        let table = match read.open_table(RAFT_LOG) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
        };
        let last = table
            .last()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        match last {
            Some((_, value)) => Ok(Some(decode_entry(value.value())?.log_id)),
            None => Ok(None),
        }
    }

    fn last_entry_index(&self, read: &redb::ReadTransaction) -> StorageResult<Option<u64>> {
        let table = match read.open_table(RAFT_LOG) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
        };
        let last = table
            .last()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        Ok(last.map(|(key, _)| key.value()))
    }

    /// The lowest retained index strictly above `index`, if any.
    ///
    /// A bounded range lookup that stops at the first row, never a scan.
    fn first_index_above(
        &self,
        read: &redb::ReadTransaction,
        index: u64,
    ) -> StorageResult<Option<u64>> {
        use std::ops::Bound;
        let table = match read.open_table(RAFT_LOG) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
        };
        let mut range = table
            .range((Bound::Excluded(index), Bound::Unbounded))
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        match range.next() {
            Some(row) => {
                let (key, _) = row.map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
                Ok(Some(key.value()))
            }
            None => Ok(None),
        }
    }

    fn read_vote(&self) -> StorageResult<Option<Vote<CoordinationNodeId>>> {
        let read = self
            .database
            .begin_read()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        let table = match read.open_table(RAFT_META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
        };
        let value = table
            .get(KEY_VOTE)
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        match value {
            Some(v) => Ok(Some(decode_vote(v.value())?)),
            None => Ok(None),
        }
    }

    fn read_committed(&self) -> StorageResult<Option<LogId<CoordinationNodeId>>> {
        let read = self
            .database
            .begin_read()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        self.marker(&read, KEY_COMMITTED)
    }

    fn read(
        &self,
        start: std::ops::Bound<u64>,
        end: std::ops::Bound<u64>,
    ) -> StorageResult<Vec<Entry<CoordinationTypeConfig>>> {
        let read = self
            .database
            .begin_read()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        let table = match read.open_table(RAFT_LOG) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
        };
        let range = table
            .range((start, end))
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        let mut entries = Vec::new();
        for row in range {
            let (_, value) = row.map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            entries.push(decode_entry(value.value())?);
        }
        Ok(entries)
    }

    // ── Writes ───────────────────────────────────────────────────────────────

    fn save_vote(&mut self, vote: &Vote<CoordinationNodeId>) -> StorageResult<()> {
        let encoded = encode_vote(vote)?;
        self.put_meta(KEY_VOTE, &encoded)
    }

    fn save_committed(
        &mut self,
        committed: Option<&LogId<CoordinationNodeId>>,
    ) -> StorageResult<()> {
        match committed {
            Some(log_id) => {
                let encoded = encode_log_id(log_id)?;
                self.put_meta(KEY_COMMITTED, &encoded)
            }
            None => self.remove_meta(KEY_COMMITTED),
        }
    }

    fn put_meta(&mut self, key: &str, value: &[u8]) -> StorageResult<()> {
        let write = self.begin_durable_write()?;
        {
            let mut meta = write
                .open_table(RAFT_META)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            meta.insert(key, value)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        }
        write
            .commit()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))
    }

    fn remove_meta(&mut self, key: &str) -> StorageResult<()> {
        let write = self.begin_durable_write()?;
        {
            let mut meta = write
                .open_table(RAFT_META)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            meta.remove(key)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        }
        write
            .commit()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))
    }

    /// Every write transaction in this subsystem is immediate-durability.
    /// There is no `Durability::None` path, not even behind configuration:
    /// it defers the fsync and would turn OpenRaft's durability contract into
    /// a performance tunable.
    fn begin_durable_write(&self) -> StorageResult<redb::WriteTransaction> {
        let mut write = self
            .database
            .begin_write()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        Ok(write)
    }

    fn append(&mut self, entries: &[Entry<CoordinationTypeConfig>]) -> StorageResult<()> {
        if entries.is_empty() {
            return Ok(());
        }

        // Validate the whole batch before opening a transaction, so a rejected
        // batch cannot leave a partial write behind. §4.3 is explicit that a
        // gap must be refused *without* partially writing.
        let mut expected = entries[0].log_id.index;
        for entry in entries {
            if entry.log_id.index != expected {
                return Err(AsterStorageError::LogGap {
                    expected,
                    found: entry.log_id.index,
                    detail: "append batch is not internally contiguous",
                });
            }
            // Checked, because a batch ending at `u64::MAX` would otherwise
            // wrap to 0 and silently accept a following entry at index 0.
            expected = match expected.checked_add(1) {
                Some(next) => next,
                // Only reachable on the final entry; a further entry would
                // have to restart the log, which the contiguity check above
                // would then reject.
                None => break,
            };
        }

        let first = entries[0].log_id.index;
        let last = entries[entries.len() - 1].log_id.index;
        {
            let read = self
                .database
                .begin_read()
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;

            // A batch can leave a hole on either side of itself, and the two
            // need separate checks.
            //
            // Below: the frontier is the one index that continues the log
            // without a hole -- one past the last retained row, or, when
            // everything has been purged, one past the purge marker. A
            // completely fresh log has no frontier and may start anywhere,
            // which OpenRaft's own suite relies on
            // (`get_initial_state_re_apply_committed` appends from index 2 to
            // an empty store). `checked_add` rather than `saturating_add`: at
            // `u64::MAX` there is no next index at all, and saturating would
            // silently report the last index as its own successor.
            let frontier = match self.last_entry_index(&read)? {
                Some(retained) => retained.checked_add(1),
                None => self
                    .marker(&read, KEY_LAST_PURGED)?
                    .and_then(|purged| purged.index.checked_add(1)),
            };
            if let Some(frontier) = frontier {
                if first > frontier {
                    return Err(AsterStorageError::LogGap {
                        expected: frontier,
                        found: first,
                        detail: "append would leave a hole after the last retained entry",
                    });
                }
            }

            // Above: overwriting a middle section is legal -- Raft does it
            // after a conflict -- but only while the rows that survive above
            // the batch still continue it. A batch at index 0 landing under a
            // retained row at `u64::MAX` passes the frontier check and leaves
            // the whole index space hollow between them.
            if let Some(next_retained) = self.first_index_above(&read, last)? {
                let expected = last.checked_add(1);
                if expected != Some(next_retained) {
                    return Err(AsterStorageError::LogGap {
                        expected: expected.unwrap_or(u64::MAX),
                        found: next_retained,
                        detail: "append would leave a hole below the entries that survive it",
                    });
                }
            }
        }

        // Encode everything before the transaction as well: an encode failure
        // partway through would otherwise abort a transaction that had already
        // written earlier entries.
        let encoded: Vec<(u64, Vec<u8>)> = entries
            .iter()
            .map(|entry| Ok((entry.log_id.index, encode_entry(entry)?)))
            .collect::<StorageResult<_>>()?;

        let write = self.begin_durable_write()?;
        {
            let mut log = write
                .open_table(RAFT_LOG)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            for (index, value) in &encoded {
                log.insert(*index, value.as_slice())
                    .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            }
        }
        write
            .commit()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))
    }

    /// Remove `[log_id.index, +oo)`, in one transaction and one bounded range
    /// operation. Never a full-table scan.
    fn truncate(&mut self, log_id: &LogId<CoordinationNodeId>) -> StorageResult<()> {
        let write = self.begin_durable_write()?;
        {
            let mut log = write
                .open_table(RAFT_LOG)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            log.retain_in(log_id.index.., |_, _| false)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        }
        write
            .commit()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))
    }

    /// Remove `(-oo, log_id.index]` and advance the purge marker, in one
    /// transaction.
    ///
    /// The marker is what lets `get_log_state` report a last log id after
    /// everything has been deleted, which OpenRaft requires: purging past the
    /// end of the log is legal and must leave `last_log_id == last_purged`.
    fn purge(&mut self, log_id: &LogId<CoordinationNodeId>) -> StorageResult<()> {
        let marker = encode_log_id(log_id)?;
        let write = self.begin_durable_write()?;
        {
            let mut log = write
                .open_table(RAFT_LOG)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            log.retain_in(..=log_id.index, |_, _| false)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        }
        {
            let mut meta = write
                .open_table(RAFT_META)
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            meta.insert(KEY_LAST_PURGED, marker.as_slice())
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        }
        write
            .commit()
            .map_err(|source| AsterStorageError::redb(Db::Raft, source))
    }
}

fn send<T>(reply: Reply<T>, result: StorageResult<T>) {
    // A dropped receiver means the caller went away; nothing to report to.
    let _ = reply.send(result);
}

// ── Handle ───────────────────────────────────────────────────────────────────

/// Stops the log actor and waits for it to release its database.
///
/// This exists because of an ordering that is easy to get wrong and silent
/// when you do: the actor thread owns the `Database`, so dropping the store
/// only *asks* it to stop. If the directory lock were released before the
/// thread actually exited, the next open of that directory would find the file
/// still held — a restart failing with `DatabaseAlreadyOpen` in the best case,
/// and in the worst a second voter admitted while the first still has the log
/// open.
///
/// So this guard is dropped before the lock, and it joins.
#[derive(Debug)]
pub(crate) struct ActorShutdown {
    tx: mpsc::Sender<LogCommand>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ActorShutdown {
    fn drop(&mut self) {
        // The flag handles the case where the queue is full and `try_send`
        // cannot get a `Shutdown` in: the actor checks it before servicing
        // each command, so it converges either way. `try_send` is used rather
        // than a blocking send because `Drop` may run on a Tokio worker, where
        // blocking would stall the runtime.
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.try_send(LogCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A handle onto the log actor. Cloneable: OpenRaft hands log readers to
/// several replication tasks, and they all funnel to the one writer thread.
#[derive(Clone)]
pub(crate) struct LogHandle {
    tx: mpsc::Sender<LogCommand>,
    fatal: Arc<AtomicBool>,
}

impl std::fmt::Debug for LogHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogHandle")
            .field("fatal", &self.fatal.load(Ordering::SeqCst))
            .finish()
    }
}

impl LogHandle {
    /// Start the actor thread and return a handle onto it.
    pub(crate) fn spawn(database: Database) -> (Self, ActorShutdown) {
        let (tx, rx) = mpsc::channel(LOG_ACTOR_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let actor_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("aster-coordination-log".into())
            .spawn(move || LogActor { database }.run(rx, actor_stop))
            .expect("spawn the coordination log actor thread");
        let handle = Self {
            tx: tx.clone(),
            fatal: Arc::new(AtomicBool::new(false)),
        };
        (
            handle,
            ActorShutdown {
                tx,
                stop,
                thread: Some(thread),
            },
        )
    }

    /// Whether this store has failed in a way that makes its durability
    /// claims meaningless.
    pub(crate) fn is_fatal(&self) -> bool {
        self.fatal.load(Ordering::SeqCst)
    }

    fn mark_fatal(&self) {
        self.fatal.store(true, Ordering::SeqCst);
    }

    async fn call<T, F>(&self, build: F) -> StorageResult<T>
    where
        F: FnOnce(Reply<T>) -> LogCommand,
    {
        if self.is_fatal() {
            return Err(AsterStorageError::Fatal);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.tx.send(build(reply_tx)).await.is_err() {
            self.mark_fatal();
            return Err(AsterStorageError::ActorStopped { db: Db::Raft });
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.mark_fatal();
                Err(AsterStorageError::ActorStopped { db: Db::Raft })
            }
        }
    }

    /// Run a *write* through the actor, marking the store fatal if it fails.
    ///
    /// Reads are deliberately not sticky: a failed read reports itself but
    /// leaves the store usable, because it has not put the durable state in
    /// question. A failed write has.
    async fn call_write<F>(&self, build: F) -> StorageResult<()>
    where
        F: FnOnce(Reply<()>) -> LogCommand,
    {
        let result = self.call(build).await;
        if let Err(err) = &result {
            // Only failures that put durability in question are sticky. A
            // refused append is a rejected *request*, not a damaged store, and
            // OpenRaft may legitimately follow it with a correct batch --
            // making the first legal refusal permanently disable a healthy
            // voter would be far worse than the gap it refused.
            if err.puts_durability_in_question() {
                self.mark_fatal();
            }
        }
        result
    }
}

// ── OpenRaft error mapping ───────────────────────────────────────────────────

fn io_logs(err: AsterStorageError) -> StorageError<CoordinationNodeId> {
    StorageError::IO {
        source: StorageIOError::write_logs(AnyError::new(&err)),
    }
}

fn io_read_logs(err: AsterStorageError) -> StorageError<CoordinationNodeId> {
    StorageError::IO {
        source: StorageIOError::read_logs(AnyError::new(&err)),
    }
}

fn io_write_vote(err: AsterStorageError) -> StorageError<CoordinationNodeId> {
    StorageError::IO {
        source: StorageIOError::write_vote(AnyError::new(&err)),
    }
}

fn io_read_vote(err: AsterStorageError) -> StorageError<CoordinationNodeId> {
    StorageError::IO {
        source: StorageIOError::read_vote(AnyError::new(&err)),
    }
}

// ── The store ────────────────────────────────────────────────────────────────

/// `RaftLogStorage` over `raft.redb`.
#[derive(Clone, Debug)]
pub(crate) struct CoordinationLogStore {
    handle: LogHandle,
}

impl CoordinationLogStore {
    pub(crate) fn new(database: Database) -> (Self, ActorShutdown) {
        let (handle, shutdown) = LogHandle::spawn(database);
        (Self { handle }, shutdown)
    }

    pub(crate) fn handle(&self) -> &LogHandle {
        &self.handle
    }
}

impl RaftLogReader<CoordinationTypeConfig> for CoordinationLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + std::fmt::Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<CoordinationTypeConfig>>, StorageError<CoordinationNodeId>> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();
        self.handle
            .call(|reply| LogCommand::Read { start, end, reply })
            .await
            .map_err(io_read_logs)
    }
}

impl RaftLogStorage<CoordinationTypeConfig> for CoordinationLogStore {
    type LogReader = Self;

    async fn get_log_state(
        &mut self,
    ) -> Result<LogState<CoordinationTypeConfig>, StorageError<CoordinationNodeId>> {
        self.handle
            .call(LogCommand::LogState)
            .await
            .map_err(io_read_logs)
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(
        &mut self,
        vote: &Vote<CoordinationNodeId>,
    ) -> Result<(), StorageError<CoordinationNodeId>> {
        let vote = *vote;
        self.handle
            .call_write(|reply| LogCommand::SaveVote { vote, reply })
            .await
            .map_err(io_write_vote)
    }

    async fn read_vote(
        &mut self,
    ) -> Result<Option<Vote<CoordinationNodeId>>, StorageError<CoordinationNodeId>> {
        self.handle
            .call(LogCommand::ReadVote)
            .await
            .map_err(io_read_vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<CoordinationNodeId>>,
    ) -> Result<(), StorageError<CoordinationNodeId>> {
        self.handle
            .call_write(|reply| LogCommand::SaveCommitted { committed, reply })
            .await
            .map_err(io_logs)
    }

    async fn read_committed(
        &mut self,
    ) -> Result<Option<LogId<CoordinationNodeId>>, StorageError<CoordinationNodeId>> {
        self.handle
            .call(LogCommand::ReadCommitted)
            .await
            .map_err(io_read_logs)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<CoordinationTypeConfig>,
    ) -> Result<(), StorageError<CoordinationNodeId>>
    where
        I: IntoIterator<Item = Entry<CoordinationTypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let result = self
            .handle
            .call_write(|reply| LogCommand::Append { entries, reply })
            .await;

        match result {
            Ok(()) => {
                // Only now. §4.3: the callback fires after the durable commit
                // succeeds and never before -- OpenRaft treats it as proof the
                // entries are on disk, and a callback ahead of the commit is
                // indistinguishable from one behind it until a crash.
                callback.log_io_completed(Ok(()));
                Ok(())
            }
            Err(err) => {
                // Report the failure through the callback too, then fail the
                // call. Silently returning an error without completing the
                // callback would leave OpenRaft waiting on an IO that will
                // never be reported either way.
                callback.log_io_completed(Err(std::io::Error::other(err.to_string())));
                Err(io_logs(err))
            }
        }
    }

    async fn truncate(
        &mut self,
        log_id: LogId<CoordinationNodeId>,
    ) -> Result<(), StorageError<CoordinationNodeId>> {
        self.handle
            .call_write(|reply| LogCommand::Truncate { log_id, reply })
            .await
            .map_err(io_logs)
    }

    async fn purge(
        &mut self,
        log_id: LogId<CoordinationNodeId>,
    ) -> Result<(), StorageError<CoordinationNodeId>> {
        self.handle
            .call_write(|reply| LogCommand::Purge { log_id, reply })
            .await
            .map_err(io_logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::ids::CoordinationGroupId;
    use crate::coordination::model::shape;
    use crate::coordination::storage::identity::{OpenPolicy, StorageIdentity};
    use crate::coordination::storage::schema::StorageLayout;
    use crate::coordination::storage::VoterStorage;
    use openraft::entry::RaftEntry;
    use openraft::{CommittedLeaderId, EntryPayload};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("aster-coordination-log-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn identity() -> StorageIdentity {
        StorageIdentity::new(
            CoordinationGroupId::from_array([1u8; 32]),
            [2u8; 32],
            [3u8; 32],
            "authority.test",
            CoordinationNodeId::from_array([4u8; 32]),
            shape::PROFILE_PROD,
        )
    }

    fn node(byte: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([byte; 32])
    }

    fn log_id(term: u64, index: u64) -> LogId<CoordinationNodeId> {
        LogId::new(CommittedLeaderId::new(term, node(4)), index)
    }

    /// A real production entry, not a test double: it carries an
    /// `EntryPayload::Blank` through the same `RaftEntryV1` root the voter
    /// uses.
    fn blank(term: u64, index: u64) -> Entry<CoordinationTypeConfig> {
        Entry::new_blank(log_id(term, index))
    }

    fn open(root: &std::path::Path) -> VoterStorage {
        VoterStorage::open(
            StorageLayout::new(root),
            identity(),
            OpenPolicy::production(),
        )
        .expect("open voter storage")
    }

    /// `append` needs a `LogFlushed`, whose constructor is crate-private to
    /// OpenRaft. `RaftLogStorageExt::blocking_append` is the supported way in,
    /// and it also asserts what we care about: it does not return until the
    /// callback has fired.
    async fn append(
        store: &mut CoordinationLogStore,
        entries: Vec<Entry<CoordinationTypeConfig>>,
    ) -> Result<(), StorageError<CoordinationNodeId>> {
        use openraft::storage::RaftLogStorageExt;
        store.blocking_append(entries).await
    }

    // ── Vote and committed ───────────────────────────────────────────────────

    #[tokio::test]
    async fn a_vote_round_trips_through_the_production_codec() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        assert_eq!(store.read_vote().await.expect("read"), None);

        let vote = Vote::new(7, node(9));
        store.save_vote(&vote).await.expect("save");
        assert_eq!(store.read_vote().await.expect("read"), Some(vote));
    }

    #[tokio::test]
    async fn a_committed_marker_round_trips_and_can_be_cleared() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        assert_eq!(store.read_committed().await.expect("read"), None);

        store
            .save_committed(Some(log_id(1, 4)))
            .await
            .expect("save");
        assert_eq!(
            store.read_committed().await.expect("read"),
            Some(log_id(1, 4))
        );

        store.save_committed(None).await.expect("clear");
        assert_eq!(store.read_committed().await.expect("read"), None);
    }

    // ── Append, read, and continuity ─────────────────────────────────────────

    #[tokio::test]
    async fn appended_entries_are_readable_and_ordered() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(0, 0), blank(1, 1), blank(1, 2)])
            .await
            .expect("append");

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(read.len(), 3);
        let indexes: Vec<u64> = read.iter().map(|e| e.log_id.index).collect();
        assert_eq!(indexes, vec![0, 1, 2]);
        assert_eq!(read[2].log_id, log_id(1, 2));

        // redb stores u64 keys little-endian but compares them numerically.
        // Asserted across a byte-order boundary, where a byte-wise comparator
        // would put 256 before 255 and quietly corrupt replay order.
        append(store, vec![blank(1, 3)]).await.expect("append");
        let bounded = store.try_get_log_entries(1..3).await.expect("read");
        assert_eq!(
            bounded.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            vec![1, 2],
            "the end bound is exclusive"
        );
    }

    /// A batch with an internal hole is refused, and writes nothing.
    #[tokio::test]
    async fn an_internally_holed_batch_is_refused_atomically() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0)]).await.expect("seed");

        let err = append(store, vec![blank(1, 1), blank(1, 3)])
            .await
            .expect_err("a holed batch must be refused");
        assert!(
            err.to_string().contains("expected index 2"),
            "the failure must name the index it expected: {err}"
        );

        // Nothing from the rejected batch landed -- not even its first entry,
        // which was individually legal.
        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            vec![0],
            "a refused batch must not partially write"
        );
    }

    /// A batch starting past the last retained entry is refused.
    #[tokio::test]
    async fn a_gap_after_the_last_retained_entry_is_refused() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0), blank(1, 1)])
            .await
            .expect("seed");

        let err = append(store, vec![blank(1, 5)])
            .await
            .expect_err("a jump past the frontier must be refused");
        assert!(err.to_string().contains("expected index 2"), "{err}");

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(read.len(), 2);
    }

    /// Continuation at exactly `last_purged + 1` is legal once every row is
    /// gone -- this is what a follower does after a snapshot install.
    #[tokio::test]
    async fn continuation_at_the_purge_marker_is_accepted_and_a_jump_past_it_is_not() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0), blank(1, 1), blank(1, 2)])
            .await
            .expect("seed");
        store.purge(log_id(1, 2)).await.expect("purge everything");
        assert!(
            store
                .try_get_log_entries(0..)
                .await
                .expect("read")
                .is_empty(),
            "the purge removed every row"
        );

        let err = append(store, vec![blank(1, 9)])
            .await
            .expect_err("a jump past the marker with no row to account for it");
        assert!(err.to_string().contains("expected index 3"), "{err}");

        append(store, vec![blank(1, 3)])
            .await
            .expect("continuation at the marker + 1 is legal");
        assert_eq!(store.try_get_log_entries(0..).await.expect("read").len(), 1);
    }

    /// A completely fresh log may start anywhere.
    ///
    /// Not a liberty: OpenRaft's own suite appends from index 2 to an empty
    /// store in `get_initial_state_re_apply_committed`, so a store that
    /// insisted on index 0 would fail the official conformance suite.
    #[tokio::test]
    async fn a_fresh_log_may_start_at_a_nonzero_index() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 2), blank(1, 3)])
            .await
            .expect("a fresh log has no frontier to violate");
        assert_eq!(
            store.get_log_state().await.expect("state").last_log_id,
            Some(log_id(1, 3))
        );
    }

    /// Overwriting an existing suffix is legal; Raft does it after a conflict.
    #[tokio::test]
    async fn appending_over_an_existing_suffix_is_accepted() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0), blank(1, 1), blank(1, 2)])
            .await
            .expect("seed");
        append(store, vec![blank(5, 2)])
            .await
            .expect("a higher-term entry may replace index 2");

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(read.len(), 3);
        assert_eq!(read[2].log_id, log_id(5, 2), "the newer term wins");
    }

    /// Checked arithmetic at the top of the index space.
    ///
    /// A batch ending at `u64::MAX` must not wrap its expected-next index to
    /// 0, which would make an entry at index 0 look like a legal continuation.
    #[tokio::test]
    async fn an_append_at_the_index_ceiling_does_not_wrap() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, u64::MAX)])
            .await
            .expect("a fresh log accepts the ceiling");

        let err = append(store, vec![blank(1, 0)])
            .await
            .expect_err("index 0 is not a continuation of u64::MAX");
        assert!(err.to_string().contains("expected index"), "{err}");
    }

    // ── Truncate and purge ───────────────────────────────────────────────────

    #[tokio::test]
    async fn truncate_removes_the_suffix_from_the_given_index() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        let entries: Vec<_> = (0..=10).map(|i| blank(1, i)).collect();
        append(store, entries).await.expect("seed");

        store.truncate(log_id(1, 11)).await.expect("truncate");
        assert_eq!(
            store.try_get_log_entries(0..).await.expect("read").len(),
            11,
            "truncating from past the end removes nothing"
        );

        store.truncate(log_id(1, 5)).await.expect("truncate");
        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            (0..=4).collect::<Vec<_>>(),
            "truncate is inclusive of the given index"
        );
        assert_eq!(
            store.get_log_state().await.expect("state").last_log_id,
            Some(log_id(1, 4))
        );
    }

    #[tokio::test]
    async fn purge_removes_the_prefix_and_advances_the_marker() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        let entries: Vec<_> = (0..=10).map(|i| blank(1, i)).collect();
        append(store, entries).await.expect("seed");

        store.purge(log_id(1, 5)).await.expect("purge");
        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            (6..=10).collect::<Vec<_>>(),
            "purge is inclusive of the given index"
        );

        let state = store.get_log_state().await.expect("state");
        assert_eq!(state.last_purged_log_id, Some(log_id(1, 5)));
        assert_eq!(state.last_log_id, Some(log_id(1, 10)));
    }

    /// Purging past the end of the log is legal, and the marker then *is* the
    /// last log id.
    ///
    /// Reporting `None` here would look like a log that had never held
    /// anything, and replication would restart from zero.
    #[tokio::test]
    async fn purging_past_the_end_leaves_the_marker_as_the_last_log_id() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        let entries: Vec<_> = (0..=10).map(|i| blank(1, i)).collect();
        append(store, entries).await.expect("seed");

        store
            .purge(log_id(1, 20))
            .await
            .expect("purge past the end");

        assert!(store
            .try_get_log_entries(0..100)
            .await
            .expect("read")
            .is_empty());
        assert_eq!(
            store.get_log_state().await.expect("state"),
            LogState {
                last_purged_log_id: Some(log_id(1, 20)),
                last_log_id: Some(log_id(1, 20)),
            }
        );
    }

    #[tokio::test]
    async fn a_fresh_log_reports_an_empty_state() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        assert_eq!(
            store.get_log_state().await.expect("state"),
            LogState {
                last_purged_log_id: None,
                last_log_id: None,
            }
        );
    }

    // ── Restart ──────────────────────────────────────────────────────────────

    /// Genuine non-empty restart evidence.
    ///
    /// Deliberately not the reopened-empty builder shape: reopening a store
    /// that was empty before anything was written proves the open path works,
    /// not that anything survived it. Here every marker and the log suffix are
    /// written, every handle is dropped, and the store is reopened from disk.
    #[tokio::test]
    async fn a_non_empty_log_survives_a_real_close_and_reopen() {
        let root = scratch();
        let vote = Vote::new(9, node(11));

        {
            let mut storage = open(&root);
            let store = storage.log_mut();
            let entries: Vec<_> = (0..=10).map(|i| blank(1, i)).collect();
            append(store, entries).await.expect("seed");
            store.save_vote(&vote).await.expect("vote");
            store
                .save_committed(Some(log_id(1, 7)))
                .await
                .expect("committed");
            store.purge(log_id(1, 3)).await.expect("purge");
            store.truncate(log_id(1, 9)).await.expect("truncate");
            // Every handle, the actor, and the directory lock go here.
        }

        let mut storage = open(&root);
        let store = storage.log_mut();

        assert_eq!(
            store.read_vote().await.expect("vote"),
            Some(vote),
            "an acknowledged vote must not disappear across restart"
        );
        assert_eq!(
            store.read_committed().await.expect("committed"),
            Some(log_id(1, 7))
        );
        let state = store.get_log_state().await.expect("state");
        assert_eq!(state.last_purged_log_id, Some(log_id(1, 3)));
        assert_eq!(state.last_log_id, Some(log_id(1, 8)));

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            (4..=8).collect::<Vec<_>>(),
            "the retained suffix survives exactly"
        );
        assert!(
            matches!(read[0].payload, EntryPayload::Blank),
            "and its payload decodes through the production root"
        );
    }

    /// A reopened store continues the log rather than restarting it.
    #[tokio::test]
    async fn a_reopened_log_continues_from_its_frontier() {
        let root = scratch();
        {
            let mut storage = open(&root);
            append(storage.log_mut(), vec![blank(1, 0), blank(1, 1)])
                .await
                .expect("seed");
        }

        let mut storage = open(&root);
        let store = storage.log_mut();

        let err = append(store, vec![blank(1, 4)])
            .await
            .expect_err("the frontier is remembered across restart");
        assert!(err.to_string().contains("expected index 2"), "{err}");

        append(store, vec![blank(1, 2)])
            .await
            .expect("continuation is accepted");
    }

    // ── Failure behaviour ────────────────────────────────────────────────────

    /// A store whose actor has stopped reports it, and the failure sticks.
    ///
    /// The actor is stopped through the same shutdown path a real one uses,
    /// while a handle clone survives it. That clone is what OpenRaft would be
    /// holding as a log reader, and it must fail loudly rather than answer
    /// from nothing.
    #[tokio::test]
    async fn a_stopped_actor_makes_the_store_fatal() {
        let root = scratch();
        let storage = open(&root);

        let mut store = storage.log().clone();
        assert!(!store.handle().is_fatal());

        // Shuts the actor down and joins it, exactly as a restart would.
        drop(storage);

        let err = store
            .get_log_state()
            .await
            .expect_err("a store with no actor cannot answer");
        assert!(err.to_string().contains("stopped"), "{err}");
        assert!(
            store.handle().is_fatal(),
            "and the failure is sticky rather than retried into a wrong answer"
        );
        assert!(store.save_vote(&Vote::new(1, node(2))).await.is_err());
    }

    /// A refused append leaves the store usable.
    ///
    /// The counterpart to the test above, and the reason the two failure kinds
    /// are separated at all: an earlier version marked the store fatal on
    /// *any* write error, so the first legal gap rejection permanently
    /// disabled a healthy voter. OpenRaft rejects and retries as a matter of
    /// course.
    #[tokio::test]
    async fn a_refused_append_leaves_the_store_usable() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0)]).await.expect("seed");
        assert!(append(store, vec![blank(1, 9)]).await.is_err());

        assert!(
            !store.handle().is_fatal(),
            "a rejected request is not a durability failure"
        );
        append(store, vec![blank(1, 1)])
            .await
            .expect("a correct batch after a refused one still works");
        assert_eq!(store.try_get_log_entries(0..).await.expect("read").len(), 2);
    }

    /// The actor releases its database before the lock is released.
    ///
    /// Without the join, dropping the store only *asks* the actor to stop, and
    /// a restart raced it: the lock was already free while the thread still
    /// had `raft.redb` open, which surfaced as `DatabaseAlreadyOpen`. In a
    /// real deployment the same window admits a second voter while the first
    /// still holds the log.
    #[tokio::test]
    async fn shutdown_releases_the_database_before_the_lock() {
        let root = scratch();
        for round in 0..5 {
            let mut storage = open(&root);
            append(storage.log_mut(), vec![blank(1, round)])
                .await
                .unwrap_or_else(|e| panic!("append in round {round}: {e}"));
            drop(storage);
        }

        let mut storage = open(&root);
        assert_eq!(
            storage
                .log_mut()
                .try_get_log_entries(0..)
                .await
                .expect("read")
                .len(),
            5,
            "every round's entry survived its restart"
        );
    }

    /// The append callback fires only after the durable commit.
    ///
    /// `blocking_append` returns once the callback has completed, so a store
    /// that fired it early would let this observe entries that are not yet
    /// readable. Checked by reading immediately after the call returns.
    #[tokio::test]
    async fn the_flush_callback_follows_the_durable_commit() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0), blank(1, 1)])
            .await
            .expect("append");

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.len(),
            2,
            "entries must be readable by the time the callback has fired"
        );
    }

    /// A refused append reports through the callback as well as the return
    /// value, so OpenRaft is never left waiting on an IO nobody completes.
    #[tokio::test]
    async fn a_refused_append_still_completes_its_callback() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0)]).await.expect("seed");

        // `blocking_append` awaits the callback; if a refusal skipped it, this
        // would hang rather than return an error.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            append(store, vec![blank(1, 7)]),
        )
        .await
        .expect("the callback must be completed even when the append is refused");
        assert!(result.is_err());
    }

    // ── Codec ────────────────────────────────────────────────────────────────

    /// Oversized stored values are refused before decode, not after.
    #[test]
    fn an_oversized_stored_entry_is_refused_before_decode() {
        let too_big = vec![0u8; MAX_ENTRY_BYTES + 1];
        let err = decode_entry(&too_big).expect_err("over the ceiling");
        assert!(err.to_string().contains("ceiling"), "{err}");

        let too_big = vec![0u8; MAX_META_BYTES + 1];
        assert!(decode_vote(&too_big).is_err());
        assert!(decode_log_id(&too_big).is_err());
    }

    /// A corrupt stored value is a named codec failure, never a default.
    #[test]
    fn a_corrupt_stored_value_is_a_named_codec_failure() {
        let err = decode_entry(b"not a Fory RaftEntryV1").expect_err("corrupt");
        assert!(
            matches!(
                err,
                AsterStorageError::Codec {
                    what: "RaftEntryV1",
                    ..
                }
            ),
            "got: {err:?}"
        );
    }

    /// Round-tripping through the production codec preserves the entry.
    #[test]
    fn entries_round_trip_through_the_declared_root() {
        let entry = blank(3, 9);
        let bytes = encode_entry(&entry).expect("encode");
        let back = decode_entry(&bytes).expect("decode");
        assert_eq!(back.log_id, entry.log_id);
        assert!(matches!(back.payload, EntryPayload::Blank));
    }

    #[test]
    fn votes_and_log_ids_round_trip_through_their_declared_roots() {
        let vote = Vote::new(4, node(6));
        assert_eq!(
            decode_vote(&encode_vote(&vote).expect("encode")).expect("decode"),
            vote
        );

        let id = log_id(2, 5);
        assert_eq!(
            decode_log_id(&encode_log_id(&id).expect("encode")).expect("decode"),
            id
        );
    }
}
