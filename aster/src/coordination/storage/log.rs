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

use super::actor::ActorShutdown;
use super::codec::{decode_persisted_bounded, encode_bounded};
use super::error::{Db, StorageError as AsterStorageError, StorageResult};
use super::schema::{KEY_COMMITTED, KEY_LAST_PURGED, KEY_VOTE, RAFT_LOG, RAFT_META};
use crate::coordination::contract::LogIdV1;
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
    encode_bounded(Db::Raft, "RaftEntryV1", &mirror, MAX_ENTRY_BYTES)
}

/// Decode a stored entry.
///
/// Every failure here is
/// [`StoredCorruption`](AsterStorageError::StoredCorruption) rather than a
/// retryable codec error: these bytes are already on disk, so "try again" is
/// not something the caller can usefully do.
fn decode_entry(bytes: &[u8]) -> StorageResult<Entry<CoordinationTypeConfig>> {
    // Before allocation, not after: a length check that runs only once the
    // value is already decoded has not bounded anything.
    let mirror: RaftEntryV1 =
        decode_persisted_bounded(Db::Raft, "RaftEntryV1", bytes, MAX_ENTRY_BYTES)?;
    // The mirror conversion is fallible and stays fallible here. An unknown
    // payload variant is fatal rather than degraded to a blank: applying a
    // blank in its place would commit a gap in the replicated history.
    Entry::<CoordinationTypeConfig>::try_from(&mirror)
        .map_err(|err| AsterStorageError::corrupt(Db::Raft, "RaftEntryV1", format!("{err:?}")))
}

fn encode_vote(vote: &Vote<CoordinationNodeId>) -> StorageResult<Vec<u8>> {
    let mirror = VoteV1::from(vote);
    encode_bounded(Db::Raft, "VoteV1", &mirror, MAX_META_BYTES)
}

fn decode_vote(bytes: &[u8]) -> StorageResult<Vote<CoordinationNodeId>> {
    let mirror: VoteV1 = decode_persisted_bounded(Db::Raft, "VoteV1", bytes, MAX_META_BYTES)?;
    Vote::<CoordinationNodeId>::try_from(&mirror)
        .map_err(|err| AsterStorageError::corrupt(Db::Raft, "VoteV1", format!("{err:?}")))
}

fn encode_log_id(log_id: &LogId<CoordinationNodeId>) -> StorageResult<Vec<u8>> {
    let mirror = LogIdV1::from(*log_id);
    encode_bounded(Db::Raft, "LogIdV1", &mirror, MAX_META_BYTES)
}

fn decode_log_id(bytes: &[u8]) -> StorageResult<LogId<CoordinationNodeId>> {
    let mirror: LogIdV1 = decode_persisted_bounded(Db::Raft, "LogIdV1", bytes, MAX_META_BYTES)?;
    LogId::<CoordinationNodeId>::try_from(&mirror)
        .map_err(|err| AsterStorageError::corrupt(Db::Raft, "LogIdV1", format!("{err:?}")))
}

// ── Actor protocol ───────────────────────────────────────────────────────────

type Reply<T> = oneshot::Sender<StorageResult<T>>;

pub(crate) enum LogCommand {
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
    /// Force a stored-corruption failure through the ordinary `serve` path.
    ///
    /// A deliberate test seam. The fatal path is otherwise unreachable at
    /// runtime now that `validate_persisted_log` catches corrupt durable state
    /// before an actor exists -- which is exactly what that validation is for.
    /// Simulating the ordering in a harness would prove the harness; routing
    /// through the real `serve` proves the code that ships.
    #[cfg(test)]
    InjectFatal(Reply<()>),
    /// Block the actor until the test releases it.
    ///
    /// Without this there is no way to enqueue commands *behind* a known
    /// failure in a known order: a caller checks the fatal latch before
    /// sending, so by the time a test could react to the failure, nothing new
    /// can be enqueued. Pausing the actor first makes the queue observable.
    #[cfg(test)]
    Pause(std::sync::mpsc::Receiver<()>),
}

/// Whether the actor may keep serving after one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Continue,
    Fatal,
}

/// The single-writer thread that owns `raft.redb`.
struct LogActor {
    database: Database,
    /// Shared with every handle. Set by the actor *before* it replies, so a
    /// caller can never observe success after durability became uncertain.
    fatal: Arc<AtomicBool>,
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
            let outcome = match command {
                LogCommand::Shutdown => break,
                LogCommand::LogState(reply) => self.serve(reply, |a| a.log_state()),
                LogCommand::SaveVote { vote, reply } => self.serve(reply, |a| a.save_vote(&vote)),
                LogCommand::ReadVote(reply) => self.serve(reply, |a| a.read_vote()),
                LogCommand::SaveCommitted { committed, reply } => {
                    self.serve(reply, |a| a.save_committed(committed.as_ref()))
                }
                LogCommand::ReadCommitted(reply) => self.serve(reply, |a| a.read_committed()),
                LogCommand::Append { entries, reply } => self.serve(reply, |a| a.append(&entries)),
                LogCommand::Truncate { log_id, reply } => {
                    self.serve(reply, |a| a.truncate(&log_id))
                }
                LogCommand::Purge { log_id, reply } => self.serve(reply, |a| a.purge(&log_id)),
                LogCommand::Read { start, end, reply } => self.serve(reply, |a| a.read(start, end)),
                #[cfg(test)]
                LogCommand::InjectFatal(reply) => self.serve(reply, |_| {
                    Err(AsterStorageError::corrupt(
                        Db::Raft,
                        "RaftEntryV1",
                        "injected durability failure",
                    ))
                }),
                #[cfg(test)]
                LogCommand::Pause(gate) => {
                    let _ = gate.recv();
                    Outcome::Continue
                }
            };

            if outcome == Outcome::Fatal {
                // The latch was set before the reply went out. Stopping here
                // is the other half: the commands already queued behind this
                // failure were enqueued while the store still looked healthy,
                // and serving them would acknowledge writes after durability
                // had become uncertain.
                self.drain_fatal(&mut rx);
                break;
            }
        }
    }

    /// Run one command, latch fatality before replying, and report which it
    /// was.
    ///
    /// The ordering is the whole point. Classifying in the Tokio caller --
    /// which is what this used to do -- leaves a window: the actor sends the
    /// failure and immediately picks up the next command while the caller has
    /// not yet set the latch. A vote or append queued behind a failed commit
    /// could be serviced and acknowledged inside it.
    fn serve<T, F>(&mut self, reply: Reply<T>, run: F) -> Outcome
    where
        F: FnOnce(&mut Self) -> StorageResult<T>,
    {
        let result = run(self);
        let outcome = match &result {
            Err(err) if err.puts_durability_in_question() => {
                self.fatal.store(true, Ordering::SeqCst);
                Outcome::Fatal
            }
            _ => Outcome::Continue,
        };
        let _ = reply.send(result);
        outcome
    }

    /// Answer everything already queued with the fatal error, rather than
    /// leaving senders waiting on a dropped receiver.
    fn drain_fatal(&mut self, rx: &mut mpsc::Receiver<LogCommand>) {
        rx.close();
        while let Some(command) = rx.blocking_recv() {
            match command {
                LogCommand::Shutdown => {}
                LogCommand::LogState(reply) => fail(reply),
                LogCommand::SaveVote { reply, .. } => fail(reply),
                LogCommand::ReadVote(reply) => fail(reply),
                LogCommand::SaveCommitted { reply, .. } => fail(reply),
                LogCommand::ReadCommitted(reply) => fail(reply),
                LogCommand::Append { reply, .. } => fail(reply),
                LogCommand::Truncate { reply, .. } => fail(reply),
                LogCommand::Purge { reply, .. } => fail(reply),
                LogCommand::Read { reply, .. } => fail(reply),
                #[cfg(test)]
                LogCommand::InjectFatal(reply) => fail(reply),
                #[cfg(test)]
                LogCommand::Pause(_) => {}
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
        super::schema::durable_write(&self.database, Db::Raft)
    }

    fn append(&mut self, entries: &[Entry<CoordinationTypeConfig>]) -> StorageResult<()> {
        if entries.is_empty() {
            return Ok(());
        }

        // Validate the whole batch before opening a transaction, so a rejected
        // batch cannot leave a partial write behind. §4.3 is explicit that a
        // gap must be refused *without* partially writing.
        // Adjacent pairs, with no early exit. An earlier version advanced an
        // `expected` counter and `break`ed when it overflowed, so a batch of
        // `[u64::MAX, 0]` stopped validating before it ever looked at the
        // second entry and was accepted whole.
        for pair in entries.windows(2) {
            let (previous, next) = (pair[0].log_id.index, pair[1].log_id.index);
            if previous.checked_add(1) != Some(next) {
                return Err(AsterStorageError::LogGap {
                    expected: previous.wrapping_add(1),
                    found: next,
                    detail: "append batch is not internally contiguous",
                });
            }
        }

        let purged = {
            let read = self
                .database
                .begin_read()
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            self.marker(&read, KEY_LAST_PURGED)?
        };

        // Entries at or below the purge marker are already accounted for.
        // OpenRaft legitimately re-offers them, so this is a successful no-op
        // rather than a rejection -- but writing them back resurrects rows
        // beneath the marker, and `get_log_state` then reports a `last_log_id`
        // *below* `last_purged`, which is not a state Raft has any rule for.
        let writable: Vec<&Entry<CoordinationTypeConfig>> = match &purged {
            Some(marker) => entries
                .iter()
                .filter(|entry| entry.log_id.index > marker.index)
                .collect(),
            None => entries.iter().collect(),
        };
        if writable.is_empty() {
            return Ok(());
        }

        let first = writable[0].log_id.index;
        let last = writable[writable.len() - 1].log_id.index;
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
            // an empty store). `checked_add` because at `u64::MAX` there is no
            // next index at all -- though note the ceiling case is actually
            // caught by the hole check below, so swapping this for
            // `saturating_add` is not observable and no test here would fail.
            // It stays as the honest expression of the bound.
            let frontier = match self.last_entry_index(&read)? {
                Some(retained) => retained.checked_add(1),
                None => purged.as_ref().and_then(|p| p.index.checked_add(1)),
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
        let encoded: Vec<(u64, Vec<u8>)> = writable
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
        // A purge that does not advance the marker is a no-op, not a rewrite.
        // Moving it backwards is worse than useless: the rows above the lower
        // marker were already deleted by the higher purge, so the log would
        // report a boundary with nothing behind it and OpenRaft would try to
        // replicate from indexes that no longer exist.
        {
            let read = self
                .database
                .begin_read()
                .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
            if let Some(existing) = self.marker(&read, KEY_LAST_PURGED)? {
                if log_id.index <= existing.index {
                    return Ok(());
                }
            }
        }

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

/// Answer a queued command that will never run because the store is fatal.
fn fail<T>(reply: Reply<T>) {
    let _ = reply.send(Err(AsterStorageError::Fatal));
}

// ── Handle ───────────────────────────────────────────────────────────────────

/// The log actor's shutdown guard.
///
/// See [`ActorShutdown`](super::actor::ActorShutdown) for the ordering it
/// enforces and why it is shared with the state machine's actor rather than
/// written twice.
pub(crate) type LogShutdown = ActorShutdown<LogCommand>;

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
    pub(crate) fn spawn(database: Database) -> StorageResult<(Self, LogShutdown)> {
        let (tx, rx) = mpsc::channel(LOG_ACTOR_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let actor_stop = Arc::clone(&stop);
        // One latch, shared: the actor sets it before replying, every handle
        // reads it.
        let fatal = Arc::new(AtomicBool::new(false));
        let actor_fatal = Arc::clone(&fatal);
        let thread = std::thread::Builder::new()
            .name("aster-coordination-log".into())
            .spawn(move || {
                LogActor {
                    database,
                    fatal: actor_fatal,
                }
                .run(rx, actor_stop)
            })
            // A thread that will not start is a startup failure with a cause
            // worth reporting, not a panic inside a library.
            .map_err(|source| AsterStorageError::ActorSpawn {
                db: Db::Raft,
                source,
            })?;
        Ok((
            Self {
                tx: tx.clone(),
                fatal,
            },
            ActorShutdown::new(tx, stop, thread, || LogCommand::Shutdown),
        ))
    }

    /// Force a fatal failure through the real command path.
    #[cfg(test)]
    async fn inject_fatal(&self) -> StorageResult<()> {
        self.call(LogCommand::InjectFatal).await
    }

    /// Block the actor and return the release handle.
    #[cfg(test)]
    async fn pause(&self) -> std::sync::mpsc::Sender<()> {
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        self.tx
            .send(LogCommand::Pause(gate_rx))
            .await
            .expect("the actor is running");
        gate_tx
    }

    /// Enqueue a command without awaiting its reply.
    ///
    /// The ordering guarantee this exists for: with the actor paused, two
    /// successive sends land in the channel in exactly the order issued, which
    /// is what makes "queued behind a failure" a testable statement rather
    /// than a race.
    #[cfg(test)]
    async fn enqueue<T, F>(&self, build: F) -> oneshot::Receiver<StorageResult<T>>
    where
        F: FnOnce(Reply<T>) -> LogCommand,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(build(reply_tx))
            .await
            .expect("the actor is running");
        reply_rx
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

    /// Run a write through the actor.
    ///
    /// Identical to [`Self::call`] apart from the return type. Stickiness is
    /// not decided here and is not decided by read-versus-write: the actor
    /// classifies every command by its *failure*, so a read that hits stored
    /// corruption or a redb error is sticky too — correctly, since a store
    /// that cannot read what it holds cannot say what is durable either.
    ///
    /// (An earlier comment here claimed reads were never sticky. That was
    /// true when the caller classified, and stopped being true when the
    /// classification moved into the actor.)
    async fn call_write<F>(&self, build: F) -> StorageResult<()>
    where
        F: FnOnce(Reply<()>) -> LogCommand,
    {
        // No classification here. The actor latches fatality before it
        // replies, so by the time this sees an error the latch is already set
        // and nothing queued behind it can still be served.
        self.call(build).await
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
    pub(crate) fn new(database: Database) -> StorageResult<(Self, LogShutdown)> {
        let (handle, shutdown) = LogHandle::spawn(database)?;
        Ok((Self { handle }, shutdown))
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

// ── Startup validation ───────────────────────────────────────────────────────

/// Validate everything `raft.redb` already holds, before any actor is
/// published.
///
/// Schema and identity validation say the file is *ours*; they say nothing
/// about whether its contents are readable. Without this, a correctly bound
/// database full of undecodable entries opened cleanly and returned a live
/// store, and the corruption surfaced on the first replication read -- by
/// which time the voter had already told the group it was up.
///
/// Runs against a `&Database` the open coordinator still owns, so a failure
/// here means no actor was ever started.
pub(crate) fn validate_persisted_log(database: &Database) -> StorageResult<()> {
    let read = database
        .begin_read()
        .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;

    // Markers first: they bound everything the entry scan then checks.
    let mut last_purged = None;
    match read.open_table(RAFT_META) {
        Ok(meta) => {
            for key in [KEY_VOTE, KEY_COMMITTED, KEY_LAST_PURGED] {
                let value = meta
                    .get(key)
                    .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
                let Some(value) = value else { continue };
                if key == KEY_VOTE {
                    decode_vote(value.value())?;
                } else {
                    let decoded = decode_log_id(value.value())?;
                    if key == KEY_LAST_PURGED {
                        last_purged = Some(decoded);
                    }
                }
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
    }

    let log = match read.open_table(RAFT_LOG) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(source) => return Err(AsterStorageError::redb(Db::Raft, source)),
    };

    let mut previous: Option<u64> = None;
    let rows = log
        .iter()
        .map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
    for row in rows {
        let (key, value) = row.map_err(|source| AsterStorageError::redb(Db::Raft, source))?;
        let index = key.value();

        // Size and decode. Both report corruption rather than a retryable
        // codec error, because these bytes are already durable.
        let entry = decode_entry(value.value())?;

        // The key *is* the index. A row filed under a key that disagrees with
        // its own decoded log id makes every range query silently wrong:
        // truncate, purge and replay all address rows by key.
        if entry.log_id.index != index {
            return Err(AsterStorageError::corrupt(
                Db::Raft,
                "RaftEntryV1",
                format!(
                    "row filed at key {index} decodes to log index {}",
                    entry.log_id.index
                ),
            ));
        }

        // Nothing may survive at or below the purge marker: those indexes are
        // reported as gone, and a row there contradicts the marker.
        if let Some(marker) = &last_purged {
            if index <= marker.index {
                return Err(AsterStorageError::corrupt(
                    Db::Raft,
                    "raft_log",
                    format!(
                        "row {index} is at or below the purge marker {}",
                        marker.index
                    ),
                ));
            }
        }

        match previous {
            // Raft examines only the last log id, so a hole here is not a
            // local inconsistency -- it makes every later "up to date"
            // conclusion wrong.
            Some(prev) if prev.checked_add(1) != Some(index) => {
                return Err(AsterStorageError::corrupt(
                    Db::Raft,
                    "raft_log",
                    format!("retained rows jump from {prev} to {index}"),
                ));
            }
            // The first retained row must continue the marker exactly.
            None => {
                if let Some(marker) = &last_purged {
                    if marker.index.checked_add(1) != Some(index) {
                        return Err(AsterStorageError::corrupt(
                            Db::Raft,
                            "raft_log",
                            format!(
                                "first retained row is {index}, but the purge marker {} \
                                 leaves a hole",
                                marker.index
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
        previous = Some(index);
    }

    Ok(())
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

    fn try_open(root: &std::path::Path) -> StorageResult<VoterStorage> {
        VoterStorage::open(
            StorageLayout::new(root),
            identity(),
            OpenPolicy::production(),
        )
    }

    /// Write a raw log row, bypassing every validation the store applies. A
    /// corruption test that can only produce states the writer produces proves
    /// nothing about what a crash or a foreign build leaves behind.
    fn write_log_row(root: &std::path::Path, index: u64, value: &[u8]) {
        let db = Database::create(root.join("raft.redb")).expect("open raft");
        let write = db.begin_write().expect("write");
        {
            let mut log = write.open_table(RAFT_LOG).expect("open log");
            log.insert(index, value).expect("insert");
        }
        write.commit().expect("commit");
    }

    fn delete_log_row(root: &std::path::Path, index: u64) {
        let db = Database::create(root.join("raft.redb")).expect("open raft");
        let write = db.begin_write().expect("write");
        {
            let mut log = write.open_table(RAFT_LOG).expect("open log");
            log.remove(index).expect("remove");
        }
        write.commit().expect("commit");
    }

    fn write_raft_meta(root: &std::path::Path, key: &str, value: &[u8]) {
        let db = Database::create(root.join("raft.redb")).expect("open raft");
        let write = db.begin_write().expect("write");
        {
            let mut meta = write.open_table(RAFT_META).expect("open meta");
            meta.insert(key, value).expect("insert");
        }
        write.commit().expect("commit");
    }

    fn open(root: &std::path::Path) -> VoterStorage {
        VoterStorage::open(
            StorageLayout::new(root),
            identity(),
            OpenPolicy::production(),
        )
        .expect("open voter storage")
    }

    /// The directory lock outlives every read transaction handed out over
    /// `state.redb`.
    ///
    /// A redb `ReadTransaction` is `'static` — it keeps its version alive
    /// through its own `Arc` rather than borrowing the `Database` — so a
    /// snapshot builder holding one is a reader nothing in the store's shutdown
    /// path could see. Without the read drain, closing the store released
    /// `LOCK` while that builder was still reading the old database, and a
    /// second voter could be admitted on top of it and then publish `CURRENT`
    /// from a version the new owner knew nothing about.
    ///
    /// The negative half is a timing observation, and it fails in the right
    /// direction: with no drain the close completes at once and the first
    /// `recv_timeout` succeeds. The `LockContended` assertion under it is not
    /// timing-dependent — the closing thread is parked inside the drain, which
    /// runs before the lock is released.
    #[tokio::test]
    async fn the_directory_lock_is_held_until_every_read_transaction_ends() {
        use std::time::Duration;

        let dir = scratch();
        let storage = open(&dir);
        let read = storage
            .state()
            .handle()
            .read_transaction()
            .await
            .expect("a read transaction");

        let (tx, rx) = std::sync::mpsc::channel();
        let closing = std::thread::spawn(move || {
            drop(storage);
            tx.send(()).expect("report the close");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "closing the store must not complete while a read is still open"
        );
        assert!(
            matches!(try_open(&dir), Err(AsterStorageError::LockContended { .. })),
            "and no second opener may be admitted meanwhile"
        );

        // Giving up the *transaction* does not give up the store. A builder
        // reaches this point with its body streamed and its publication still
        // to do, and publishing into a directory a new owner had been admitted
        // to is the failure this half guards.
        let lease = read.release_transaction();
        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "releasing the transaction must not release the store"
        );
        assert!(
            matches!(try_open(&dir), Err(AsterStorageError::LockContended { .. })),
            "the publication window is still inside the lock"
        );

        drop(lease);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the close completes once the lease ends");
        closing.join().expect("join the closing thread");
        try_open(&dir).expect("and the directory opens again afterwards");
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

    /// Appended entries are readable and durable once `append` returns.
    ///
    /// ## What this does *not* prove, and why no test here can
    ///
    /// It does not prove the callback fires after the commit rather than
    /// before. Observing that needs a `LogFlushed` whose completion can be
    /// watched independently, and `LogFlushed::new` is `pub(crate)` to
    /// OpenRaft — the only way in is `blocking_append`, which awaits the
    /// callback and then returns, so both orderings look identical from out
    /// here.
    ///
    /// The ordering is instead enforced by construction: `callback` is
    /// invoked on the line after `commit()` returns `Ok`, with no branch
    /// between them. A mutation moving it earlier is not detectable by this
    /// suite, and saying so is better than a test whose name claims the
    /// coverage. Checkpoint E's crash matrix is where the property gets real
    /// evidence, since a callback ahead of the commit is exactly what a
    /// process death between them exposes.
    #[tokio::test]
    async fn appended_entries_are_readable_when_append_returns() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0), blank(1, 1)])
            .await
            .expect("append");

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(read.len(), 2);
    }

    /// A refused append returns promptly rather than hanging.
    ///
    /// The implementation also completes the callback on the refusal path.
    /// That is deliberate but *unverified here*: `blocking_append` propagates
    /// the `Err` from `append` with `?` and never awaits the callback, so
    /// removing the completion changes nothing this test can see. It is kept
    /// because it costs nothing and is correct if OpenRaft ever does await.
    #[tokio::test]
    async fn a_refused_append_returns_without_hanging() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0)]).await.expect("seed");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            append(store, vec![blank(1, 7)]),
        )
        .await
        .expect("a refused append must not hang");
        assert!(result.is_err());
    }

    // ── Boundary holes ───────────────────────────────────────────────────────

    /// A batch that wraps the index space is refused.
    ///
    /// `[u64::MAX, 0]` used to be accepted: the validation loop advanced an
    /// `expected` counter, overflowed, and `break`ed before ever looking at
    /// the second entry.
    #[tokio::test]
    async fn a_batch_that_wraps_the_index_space_is_refused() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        let err = append(store, vec![blank(1, u64::MAX), blank(1, 0)])
            .await
            .expect_err("index 0 does not follow u64::MAX");
        assert!(err.to_string().contains("expected index"), "{err}");
        assert!(
            store
                .try_get_log_entries(0..)
                .await
                .expect("read")
                .is_empty(),
            "and the refused batch wrote nothing"
        );
    }

    /// Entries at or below the purge marker are a successful no-op.
    ///
    /// Writing them back resurrects rows beneath the marker, and
    /// `get_log_state` then reports a `last_log_id` *below* `last_purged` --
    /// a state Raft has no rule for.
    #[tokio::test]
    async fn appending_history_below_the_purge_marker_is_a_no_op() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(
            store,
            vec![blank(1, 0), blank(1, 1), blank(1, 2), blank(1, 3)],
        )
        .await
        .expect("seed");
        store.purge(log_id(1, 2)).await.expect("purge through 2");

        append(store, vec![blank(1, 1)])
            .await
            .expect("re-offering a purged index succeeds without writing it");

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            vec![3],
            "the purged row must not be recreated"
        );
        let state = store.get_log_state().await.expect("state");
        assert_eq!(state.last_purged_log_id, Some(log_id(1, 2)));
        assert_eq!(
            state.last_log_id,
            Some(log_id(1, 3)),
            "last_log_id must never fall below last_purged"
        );
    }

    /// A batch straddling the marker writes only the part above it.
    #[tokio::test]
    async fn a_batch_straddling_the_purge_marker_writes_only_its_suffix() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        append(store, vec![blank(1, 0), blank(1, 1), blank(1, 2)])
            .await
            .expect("seed");
        store.purge(log_id(1, 2)).await.expect("purge everything");

        append(
            store,
            vec![blank(1, 1), blank(1, 2), blank(1, 3), blank(1, 4)],
        )
        .await
        .expect("the suffix continues the marker");

        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    /// A purge that does not advance the marker is a no-op.
    #[tokio::test]
    async fn a_regressive_purge_does_not_move_the_marker() {
        let root = scratch();
        let mut storage = open(&root);
        let store = storage.log_mut();

        let entries: Vec<_> = (0..=10).map(|i| blank(1, i)).collect();
        append(store, entries).await.expect("seed");

        store.purge(log_id(1, 5)).await.expect("purge to 5");
        store
            .purge(log_id(1, 3))
            .await
            .expect("a lower purge is a no-op");

        // Asserted *before* any further purge. An earlier version of this test
        // re-purged to 5 at the end, which restored the marker and hid the
        // regression it was supposed to catch -- a mutation removing the guard
        // passed it.
        let after_regression = store.get_log_state().await.expect("state");
        assert_eq!(
            after_regression.last_purged_log_id,
            Some(log_id(1, 5)),
            "a lower purge must not drag the marker backwards"
        );

        store.purge(log_id(1, 5)).await.expect("an equal purge too");

        let state = store.get_log_state().await.expect("state");
        assert_eq!(state.last_purged_log_id, Some(log_id(1, 5)));
        let read = store.try_get_log_entries(0..).await.expect("read");
        assert_eq!(
            read.iter().map(|e| e.log_id.index).collect::<Vec<_>>(),
            (6..=10).collect::<Vec<_>>(),
            "and nothing above the marker went missing"
        );
    }

    // ── Startup validation ───────────────────────────────────────────────────

    /// A corrupt persisted log refuses the open, before any actor exists.
    #[tokio::test]
    async fn a_corrupt_persisted_log_refuses_the_open() {
        let root = scratch();
        {
            let mut storage = open(&root);
            append(storage.log_mut(), vec![blank(1, 0)])
                .await
                .expect("seed");
        }
        write_log_row(&root, 0, b"not a Fory RaftEntryV1");

        let err = try_open(&root).expect_err("a corrupt log must not open");
        assert!(
            matches!(err, AsterStorageError::StoredCorruption { .. }),
            "got: {err:?}"
        );
    }

    /// Each structural rule refuses the open on its own.
    #[tokio::test]
    async fn the_persisted_log_structure_is_validated_at_open() {
        // A row filed under a key that disagrees with its decoded log index.
        {
            let root = scratch();
            {
                let mut storage = open(&root);
                append(storage.log_mut(), vec![blank(1, 0)])
                    .await
                    .expect("seed");
            }
            let value = encode_entry(&blank(1, 7)).expect("encode");
            write_log_row(&root, 0, &value);
            let err = try_open(&root).expect_err("key/index disagreement");
            assert!(err.to_string().contains("decodes to log index"), "{err}");
        }

        // A hole between retained rows.
        {
            let root = scratch();
            {
                let mut storage = open(&root);
                append(storage.log_mut(), vec![blank(1, 0), blank(1, 1)])
                    .await
                    .expect("seed");
            }
            let value = encode_entry(&blank(1, 9)).expect("encode");
            write_log_row(&root, 9, &value);
            let err = try_open(&root).expect_err("hole between rows");
            assert!(err.to_string().contains("jump from"), "{err}");
        }

        // A row surviving at or below the purge marker.
        {
            let root = scratch();
            {
                let mut storage = open(&root);
                let store = storage.log_mut();
                append(store, vec![blank(1, 0), blank(1, 1), blank(1, 2)])
                    .await
                    .expect("seed");
                store.purge(log_id(1, 1)).await.expect("purge");
            }
            let value = encode_entry(&blank(1, 1)).expect("encode");
            write_log_row(&root, 1, &value);
            let err = try_open(&root).expect_err("row under the marker");
            assert!(
                err.to_string().contains("at or below the purge marker"),
                "{err}"
            );
        }

        // A gap between the marker and the first retained row.
        {
            let root = scratch();
            {
                let mut storage = open(&root);
                let store = storage.log_mut();
                append(store, vec![blank(1, 0), blank(1, 1), blank(1, 2)])
                    .await
                    .expect("seed");
                store.purge(log_id(1, 0)).await.expect("purge");
            }
            delete_log_row(&root, 1);
            let err = try_open(&root).expect_err("marker leaves a hole");
            assert!(err.to_string().contains("leaves a hole"), "{err}");
        }

        // A corrupt marker, which is what used to be retryable.
        {
            let root = scratch();
            drop(open(&root));
            write_raft_meta(&root, KEY_LAST_PURGED, b"not a Fory LogIdV1");
            let err = try_open(&root).expect_err("corrupt purge marker");
            assert!(
                matches!(
                    err,
                    AsterStorageError::StoredCorruption {
                        what: "LogIdV1",
                        ..
                    }
                ),
                "got: {err:?}"
            );
        }
    }

    /// A well-formed store still opens, so the validation is not simply
    /// refusing everything.
    #[tokio::test]
    async fn a_clean_persisted_log_still_opens() {
        let root = scratch();
        {
            let mut storage = open(&root);
            let store = storage.log_mut();
            let entries: Vec<_> = (0..=5).map(|i| blank(1, i)).collect();
            append(store, entries).await.expect("seed");
            store.purge(log_id(1, 2)).await.expect("purge");
            store.save_vote(&Vote::new(3, node(8))).await.expect("vote");
            store
                .save_committed(Some(log_id(1, 4)))
                .await
                .expect("committed");
        }
        try_open(&root).expect("a well-formed store opens");
    }

    // ── Fatal ordering ───────────────────────────────────────────────────────

    /// The latch is set by the time the caller observes the failure.
    ///
    /// ## What this does *not* prove
    ///
    /// It does not prove the latch is set *before* the reply is sent rather
    /// than immediately after. The actor runs on its own thread, so it wins
    /// that race in practice either way and a mutation moving the store after
    /// the send passes this test -- verified. The ordering is kept because it
    /// closes the window on principle and costs nothing, but the property with
    /// teeth is the next test: once fatal, work already queued is refused
    /// rather than served. That one is deterministic, because the actor stops
    /// the loop instead of racing anything.
    #[tokio::test]
    async fn the_fatal_latch_is_set_by_the_time_the_caller_sees_the_failure() {
        let root = scratch();
        let storage = open(&root);

        let err = storage
            .log()
            .handle()
            .inject_fatal()
            .await
            .expect_err("the injected failure is reported");
        assert!(err.to_string().contains("unusable"), "{err}");
        assert!(storage.log().handle().is_fatal());
    }

    /// A command queued *behind* a fatal failure is refused and never
    /// executed.
    ///
    /// This is the drain, tested directly. The ordering is forced rather than
    /// raced: the actor is paused, the failure and the write are enqueued in
    /// that exact order while it cannot service either, and only then is it
    /// released. So the write is unambiguously in the channel when the failure
    /// is classified -- the case the previous version of this test could not
    /// reach, because a caller checks the latch before sending and therefore
    /// never enqueues anything once the store is fatal. That test passed a
    /// mutation which kept serving the queue.
    ///
    /// Both halves are asserted, and the second is the one with teeth: the
    /// write is refused, *and* its effect is absent from disk. An actor that
    /// keeps serving after a fatal command fails on the disk check, which no
    /// caller-side latch can prevent.
    #[tokio::test]
    async fn a_command_queued_behind_a_fatal_failure_is_never_executed() {
        let root = scratch();

        {
            let storage = open(&root);
            let handle = storage.log().handle().clone();

            let release = handle.pause().await;

            // Enqueued in this order, into a channel the actor cannot drain.
            let fatal_rx = handle.enqueue(LogCommand::InjectFatal).await;
            let vote_rx = handle
                .enqueue(|reply| LogCommand::SaveVote {
                    vote: Vote::new(4242, node(7)),
                    reply,
                })
                .await;

            release.send(()).expect("release the actor");

            assert!(
                fatal_rx.await.expect("reply").is_err(),
                "the injected failure is reported"
            );
            assert!(
                vote_rx.await.expect("reply").is_err(),
                "a write queued behind a fatal failure must not be acknowledged"
            );
            assert!(handle.is_fatal());
        }

        // And it must not have reached the disk.
        let db = Database::create(root.join("raft.redb")).expect("reopen raft");
        let read = db.begin_read().expect("read");
        let meta = read.open_table(RAFT_META).expect("open meta");
        assert!(
            meta.get(KEY_VOTE).expect("get").is_none(),
            "the queued vote was executed despite the store already being fatal"
        );
    }

    /// After a fatal failure nothing further is acknowledged.
    #[tokio::test]
    async fn a_fatal_failure_stops_the_actor_before_serving_queued_work() {
        let root = scratch();
        let mut storage = open(&root);

        let _ = storage.log().handle().inject_fatal().await;

        let store = storage.log_mut();
        assert!(
            store.save_vote(&Vote::new(99, node(3))).await.is_err(),
            "no write may be acknowledged once the store is fatal"
        );
        assert!(
            append(store, vec![blank(1, 0)]).await.is_err(),
            "including an append that would otherwise be perfectly legal"
        );
        assert!(
            store.get_log_state().await.is_err(),
            "and no read may answer from a store that cannot say what is durable"
        );
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

    /// A corrupt *stored* value is corruption, not a retryable codec error.
    ///
    /// The two used to share a variant, which made a corrupt purge marker
    /// retryable: the store reported the failure, stayed healthy, and was
    /// asked the same unanswerable question again.
    #[test]
    fn a_corrupt_stored_value_is_reported_as_corruption() {
        for err in [
            decode_entry(b"not a Fory RaftEntryV1").expect_err("corrupt entry"),
            decode_vote(b"not a Fory VoteV1").expect_err("corrupt vote"),
            decode_log_id(b"not a Fory LogIdV1").expect_err("corrupt marker"),
        ] {
            assert!(
                matches!(err, AsterStorageError::StoredCorruption { .. }),
                "got: {err:?}"
            );
            assert!(
                err.puts_durability_in_question(),
                "stored corruption must be sticky: {err:?}"
            );
        }
    }

    /// An encode failure is a rejected input and stays retryable.
    #[test]
    fn an_encode_failure_is_retryable_but_stored_corruption_is_not() {
        assert!(
            !AsterStorageError::encode_failed(Db::Raft, "VoteV1", "oversized")
                .puts_durability_in_question()
        );
        assert!(
            AsterStorageError::corrupt(Db::Raft, "VoteV1", "undecodable")
                .puts_durability_in_question()
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
