//! The durable client journal (§10, S4-D, S4-JOURNAL-DELIVERY).
//!
//! ## What this file is
//!
//! One actor/group's mutation journal: a single versioned record in one
//! directory, holding the client id, the next sequence, the **exact canonical
//! bytes** of the one in-flight request, and — after the server resolves it —
//! the exact resolved reply, until the application durably acknowledges it.
//!
//! ## The six states
//!
//! | State | Network action allowed | Exit |
//! | --- | --- | --- |
//! | `Ready` | fsync the exact next request as `Pending` before any send | `Pending` |
//! | `Pending` | send **only** the saved bytes; redirects/timeouts never replace them | `ResolvedAwaitingAck`, `Closing`, or remain `Pending` |
//! | `ResolvedAwaitingAck` | none — no new sequence until the application acknowledges | `Ready` |
//! | `Closing` | retry sessionless retirement through ambiguity | `ReplayAfterRetire`, `FatalJournal` (explicit 18), or remain `Closing` |
//! | `ReplayAfterRetire` | replay the exact original mutation — never `probe_client` | resolve, then rotate to `Ready` |
//! | `FatalJournal` | none | operator-visible failure |
//!
//! ## Delivery is the contract (S4-JOURNAL-DELIVERY, frozen)
//!
//! A resolved mutation is fsynced as `ResolvedAwaitingAck` **before** the
//! caller sees the result, and it survives process death. Until the
//! application explicitly acknowledges — after its *own* durable handling —
//! no new sequence may be issued on this journal. On restart, recovery
//! redelivers the exact saved reply. What v1 does **not** claim: exactly-once
//! application-side processing — that requires the application to make its
//! durable handling and its acknowledgement atomic, which only it can do.
//!
//! ## Crash points
//!
//! Every transition writes through [`Journal::persist`], and every network or
//! durability boundary passes a named [`CrashPoint`] first. In tests the hook
//! stops the run at that exact point; the suite then reopens the directory and
//! asserts recovery — which is the real claim, since a crash *is* a reopen.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use fory_derive::ForyStruct;

use crate::coordination::contract::payload_fory;
use crate::coordination::ids::CoordinationGroupId;
use crate::error::{Error, Result};

/// The journal schema version this build reads and writes.
pub const JOURNAL_FORMAT_VERSION: u32 = 1;

/// The §S4-D journal states, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum JournalState {
    Ready = 1,
    Pending = 2,
    ResolvedAwaitingAck = 3,
    Closing = 4,
    ReplayAfterRetire = 5,
    FatalJournal = 6,
}

impl JournalState {
    pub fn code(self) -> i32 {
        self as i32
    }

    fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            1 => JournalState::Ready,
            2 => JournalState::Pending,
            3 => JournalState::ResolvedAwaitingAck,
            4 => JournalState::Closing,
            5 => JournalState::ReplayAfterRetire,
            6 => JournalState::FatalJournal,
            _ => return None,
        })
    }
}

/// The stored record. A local storage root with the same declaration
/// discipline as every other frozen root: explicit ids, validated widths, a
/// version field first. The journal may contain resource values (inside the
/// pending request), so `Debug` is written by hand below to print lengths
/// only, and diagnostics never dump records.
#[derive(ForyStruct, aster_macros::AsterType, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ClientJournal")]
pub struct ClientJournalV1 {
    #[fory(id = 1)]
    pub format_version: u32,
    /// Exactly 32 bytes: the group this journal is bound to.
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// Exactly 32 bytes: the local actor this journal is bound to.
    #[fory(id = 3, bytes)]
    pub actor_node_id: Vec<u8>,
    /// Exactly 16 bytes: the current client id.
    #[fory(id = 4, bytes)]
    pub client_id: Vec<u8>,
    /// The [`JournalState`] code.
    #[fory(id = 5)]
    pub state_code: i32,
    /// The sequence of the entry described below (the next to consume when
    /// `Ready`, the in-flight one otherwise).
    #[fory(id = 6)]
    pub sequence: u64,
    /// The §5.2 method the pending bytes decode as, so a resend after restart
    /// dials the same method.
    #[fory(id = 7)]
    pub pending_method: String,
    /// The **exact** encoded request frame. Redirects, timeouts, resets and
    /// process restarts resend these bytes and no others.
    #[fory(id = 8, bytes)]
    pub pending_request: Vec<u8>,
    /// The canonical §5.2.1 action hash of the pending request; 32 bytes when
    /// a request is pending, empty in `Ready`.
    #[fory(id = 9, bytes)]
    pub request_hash: Vec<u8>,
    /// The encoded `MutationReplyWireV1` in `ResolvedAwaitingAck`; empty
    /// otherwise.
    #[fory(id = 10, bytes)]
    pub resolved_reply: Vec<u8>,
    /// In `ReplayAfterRetire`/`FatalJournal`: whether the retirement that got
    /// us here was *confirmed* (versus assumed) — recovery re-runs retirement
    /// when it was not.
    #[fory(id = 11)]
    pub retirement_confirmed: bool,
    /// The encoded terminal refusal that opened the closing corridor. When the
    /// replay proves the original never applied (`ClientRetired`), **this** is
    /// what resolves the entry: the business answer is the terminal reason the
    /// group actually gave, not the tombstone's generic refusal.
    #[fory(id = 12, bytes)]
    pub terminal_refusal: Vec<u8>,
}

impl std::fmt::Debug for ClientJournalV1 {
    /// Lengths only: the pending request and the resolved reply may carry
    /// resource values, and a record that reaches a log through `{:?}` must
    /// not take them with it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientJournalV1")
            .field("format_version", &self.format_version)
            .field("state_code", &self.state_code)
            .field("sequence", &self.sequence)
            .field("pending_method", &self.pending_method)
            .field("pending_request_len", &self.pending_request.len())
            .field("resolved_reply_len", &self.resolved_reply.len())
            .field("retirement_confirmed", &self.retirement_confirmed)
            .finish_non_exhaustive()
    }
}

/// A named stop the crash matrix can arm (test builds only).
pub type CrashPoint = &'static str;

/// The identity of one resolved entry, presented back at acknowledgement.
///
/// Without it a stale handle could acknowledge a *later* result: obtain two
/// handles for result A, acknowledge one, resolve B, and the surviving A
/// handle would silently mark B handled. The token binds the acknowledgement
/// to the exact `(client id, sequence, request hash)` the reply was fsynced
/// for, and the journal refuses any other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AckToken {
    pub(crate) client_id: Vec<u8>,
    pub(crate) sequence: u64,
    pub(crate) request_hash: Vec<u8>,
}

#[cfg(any(test, feature = "coordination-testing"))]
type CrashHook = std::sync::Arc<dyn Fn(CrashPoint) -> Result<()> + Send + Sync>;

/// The durable journal: one record, one directory, one lifetime lock.
pub struct Journal {
    dir: PathBuf,
    /// Held for the journal's lifetime; dropping releases it.
    _lock: File,
    record: ClientJournalV1,
    #[cfg(any(test, feature = "coordination-testing"))]
    crash: Option<CrashHook>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately shallow: the record may contain resource values.
        f.debug_struct("Journal")
            .field("dir", &self.dir)
            .field("state", &self.record.state_code)
            .field("sequence", &self.record.sequence)
            .finish()
    }
}

fn journal_error(what: impl Into<String>) -> Error {
    Error::InvalidArgument(format!("coordination client journal: {}", what.into()))
}

fn io_error(what: &str, path: &Path, source: std::io::Error) -> Error {
    Error::Transport(anyhow::anyhow!(
        "coordination client journal: {what} at {}: {source}",
        path.display()
    ))
}

/// Owner-only permissions (S4-D). On non-Unix platforms the default ACLs of
/// the created files apply; the contract's threat model is the local user
/// boundary, which Windows test coverage exercises separately.
fn restrict_permissions(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|source| io_error("restricting permissions", path, source))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// The stored record's own invariants: widths, the fields each state is
/// required to hold, and — because the record is the resend/redeliver
/// authority — that every saved reply it claims to hold actually decodes.
/// A record that fails is refused rather than repaired — a journal whose
/// `Pending` entry has no bytes cannot resend anything, and discovering that
/// at the resend site would look like a network fault.
fn validate_record(record: &ClientJournalV1) -> Result<()> {
    let state = JournalState::from_code(record.state_code)
        .ok_or_else(|| journal_error("the stored state code is unknown"))?;
    if record.group_id.len() != 32 {
        return Err(journal_error("the stored group id is not 32 bytes"));
    }
    if record.actor_node_id.len() != 32 {
        return Err(journal_error("the stored actor id is not 32 bytes"));
    }
    if record.client_id.len() != 16 {
        return Err(journal_error("the stored client id is not 16 bytes"));
    }
    let has_entry = matches!(
        state,
        JournalState::Pending
            | JournalState::Closing
            | JournalState::ReplayAfterRetire
            | JournalState::ResolvedAwaitingAck
    );
    if has_entry {
        if record.request_hash.len() != 32 {
            return Err(journal_error(
                "a state carrying an entry must hold its 32-byte request hash",
            ));
        }
        if record.pending_request.is_empty() {
            return Err(journal_error(
                "a state carrying an entry must hold the exact request bytes",
            ));
        }
        if record.pending_method.is_empty() {
            return Err(journal_error(
                "a state carrying an entry must name the method its bytes resend to",
            ));
        }
    }
    // A decodable saved reply is part of the delivery contract: the bytes in
    // these fields are what `resume`/`unacknowledged` hand back as typed
    // replies, so bytes that do not decode are corruption discovered now,
    // not a decode failure blamed on the network later.
    let decodes = |bytes: &[u8]| {
        let decoded: std::result::Result<crate::coordination::store::wire::MutationReplyWireV1, _> =
            payload_fory::<crate::coordination::store::wire::MutationReplyWireV1>()
                .deserialize(bytes);
        decoded.is_ok()
    };
    match state {
        JournalState::ResolvedAwaitingAck
            if record.resolved_reply.is_empty() || !decodes(&record.resolved_reply) =>
        {
            return Err(journal_error(
                "a resolved state must hold the exact resolved reply, decodable",
            ));
        }
        JournalState::Closing | JournalState::ReplayAfterRetire
            if record.terminal_refusal.is_empty() || !decodes(&record.terminal_refusal) =>
        {
            return Err(journal_error(
                "the closing corridor must hold the decodable terminal refusal \
                 that opened it",
            ));
        }
        _ => {}
    }
    // The corridor's own invariant: `retirement_confirmed` is set by exactly
    // one transition, `Closing` → `ReplayAfterRetire`. A `ReplayAfterRetire`
    // without it would replay on a client id whose retirement was never
    // confirmed; a `Closing` with it would skip the confirmation step.
    match state {
        JournalState::ReplayAfterRetire if !record.retirement_confirmed => {
            return Err(journal_error(
                "ReplayAfterRetire requires a confirmed retirement",
            ));
        }
        JournalState::Closing if record.retirement_confirmed => {
            return Err(journal_error(
                "Closing precedes the retirement confirmation, not follows it",
            ));
        }
        JournalState::Ready
            if !record.pending_method.is_empty()
                || !record.pending_request.is_empty()
                || !record.request_hash.is_empty()
                || !record.resolved_reply.is_empty()
                || record.retirement_confirmed =>
        {
            return Err(journal_error("a Ready record must carry no entry residue"));
        }
        _ => {}
    }
    Ok(())
}

const RECORD: &str = "journal.v1";
const RECORD_TMP: &str = "journal.v1.tmp";
const LOCK: &str = "LOCK";

/// The on-disk envelope (§10): a magic tag, the BLAKE3 of the encoded record,
/// then the record. The plan's "records carry format version and checksum"
/// splits across the two layers — the version lives in the record (so an
/// unknown newer format is a *versioning* refusal, distinguishable from
/// damage), and the checksum lives out here where it can vouch for the whole
/// encoding. A file that fails the magic or the hash is **corruption and
/// fails closed**: the library never deletes or reinitializes it, because a
/// lost ambiguous journal is an operator reconciliation decision, not an
/// automatic retry.
const ENVELOPE_MAGIC: &[u8; 8] = b"AJRNL1\0\0";

fn envelope(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 32 + bytes.len());
    out.extend_from_slice(ENVELOPE_MAGIC);
    out.extend_from_slice(blake3::hash(bytes).as_bytes());
    out.extend_from_slice(bytes);
    out
}

fn open_envelope(file: &[u8]) -> Result<&[u8]> {
    if file.len() < 8 + 32 || &file[..8] != ENVELOPE_MAGIC {
        return Err(journal_error(
            "the stored record is not a journal envelope; refusing to guess (corrupt, \
             or written by an unknown newer format)",
        ));
    }
    let (claimed, payload) = file[8..].split_at(32);
    if blake3::hash(payload).as_bytes() != claimed {
        return Err(journal_error(
            "the stored record fails its checksum; the journal is corrupt and is \
             refused, never repaired",
        ));
    }
    Ok(payload)
}

impl Journal {
    /// Open (or create) the journal for `(actor, group)` in `dir`, taking the
    /// lifetime lock.
    ///
    /// A directory bound to another actor or group is refused, not repaired:
    /// §3.4 makes the binding part of the journal's identity.
    pub fn open(
        dir: impl Into<PathBuf>,
        actor: [u8; 32],
        group: CoordinationGroupId,
        fresh_client_id: [u8; 16],
    ) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .map_err(|source| io_error("creating the journal directory", &dir, source))?;
        restrict_permissions(&dir, 0o700)?;

        // The lifetime lock, exactly as the voter's directory lock takes it:
        // std advisory locking, owner-only permissions, `WouldBlock` meaning
        // another live owner.
        let lock_path = dir.join(LOCK);
        let existed = lock_path.exists();
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| io_error("opening the lock", &lock_path, source))?;
        if !existed {
            restrict_permissions(&lock_path, 0o600)?;
        }
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(journal_error(
                    "another process holds this actor/group journal's lifetime lock",
                ))
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(io_error("locking", &lock_path, source))
            }
        }

        let record_path = dir.join(RECORD);
        let record = if record_path.exists() {
            let bytes = std::fs::read(&record_path)
                .map_err(|source| io_error("reading the record", &record_path, source))?;
            let payload = open_envelope(&bytes)?;
            let record: ClientJournalV1 = payload_fory::<ClientJournalV1>()
                .deserialize(payload)
                .map_err(|_| journal_error("the stored record does not decode"))?;
            if record.format_version != JOURNAL_FORMAT_VERSION {
                return Err(journal_error(format!(
                    "record format {} is newer than this build's {}",
                    record.format_version, JOURNAL_FORMAT_VERSION
                )));
            }
            if record.group_id != group.as_bytes().to_vec() {
                return Err(journal_error("this journal is bound to another group"));
            }
            if record.actor_node_id != actor.to_vec() {
                return Err(journal_error("this journal is bound to another actor"));
            }
            validate_record(&record)?;
            record
        } else {
            // First use: fsync a fresh random client id before anything else
            // can reference it (§10 step 1).
            let record = ClientJournalV1 {
                format_version: JOURNAL_FORMAT_VERSION,
                group_id: group.as_bytes().to_vec(),
                actor_node_id: actor.to_vec(),
                client_id: fresh_client_id.to_vec(),
                state_code: JournalState::Ready.code(),
                sequence: 0,
                ..Default::default()
            };
            persist_record(&dir, &record)?;
            record
        };

        Ok(Self {
            dir,
            _lock: lock,
            record,
            #[cfg(any(test, feature = "coordination-testing"))]
            crash: None,
        })
    }

    /// Arm the crash matrix: `hook` runs at every named boundary and an `Err`
    /// stops the run there, exactly as process death would.
    #[cfg(any(test, feature = "coordination-testing"))]
    pub fn with_crash_hook(
        mut self,
        hook: impl Fn(CrashPoint) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.crash = Some(std::sync::Arc::new(hook));
        self
    }

    /// Pass one named boundary.
    pub(crate) fn boundary(&self, point: CrashPoint) -> Result<()> {
        #[cfg(any(test, feature = "coordination-testing"))]
        if let Some(hook) = &self.crash {
            return hook(point);
        }
        let _ = point;
        Ok(())
    }

    pub fn state(&self) -> JournalState {
        JournalState::from_code(self.record.state_code).expect("validated at every write")
    }

    pub fn record(&self) -> &ClientJournalV1 {
        &self.record
    }

    pub fn client_id(&self) -> [u8; 16] {
        self.record
            .client_id
            .as_slice()
            .try_into()
            .expect("validated at open")
    }

    /// The sequence currently described by the record.
    pub fn sequence(&self) -> u64 {
        self.record.sequence
    }

    /// Persist `record` atomically and fsync both the file and the directory,
    /// with the §S4-D rename boundaries named so the crash matrix can stop on
    /// either side of the commit point.
    fn persist(&mut self, record: ClientJournalV1) -> Result<()> {
        let bytes = envelope(
            &payload_fory::<ClientJournalV1>()
                .serialize(&record)
                .map_err(|_| journal_error("the record does not encode"))?,
        );
        let tmp = self.dir.join(RECORD_TMP);
        let target = self.dir.join(RECORD);
        {
            let mut file =
                File::create(&tmp).map_err(|source| io_error("creating", &tmp, source))?;
            restrict_permissions(&tmp, 0o600)?;
            file.write_all(&bytes)
                .map_err(|source| io_error("writing", &tmp, source))?;
            file.sync_all()
                .map_err(|source| io_error("fsyncing", &tmp, source))?;
        }
        // A kill here leaves the *old* record: the temporary is never read.
        self.boundary("persist_tmp_fsynced")?;
        atomic_replace(&tmp, &target)?;
        // A kill here leaves the *new* record: the rename is the commit point.
        self.boundary("persist_renamed")?;
        fsync_dir(&self.dir)?;
        self.record = record;
        Ok(())
    }

    /// `Ready` → `Pending`: fsync the exact next request before network I/O
    /// (§10 step 2).
    pub fn begin(
        &mut self,
        sequence: u64,
        method: &str,
        request: Vec<u8>,
        request_hash: [u8; 32],
    ) -> Result<()> {
        if self.state() != JournalState::Ready {
            return Err(journal_error(format!(
                "a new sequence is not permitted in state {:?}",
                self.state()
            )));
        }
        if sequence != self.record.sequence + 1 {
            return Err(journal_error("a journal issues consecutive sequences"));
        }
        self.boundary("before_pending_write")?;
        let mut next = self.record.clone();
        next.state_code = JournalState::Pending.code();
        next.sequence = sequence;
        next.pending_method = method.to_string();
        next.pending_request = request;
        next.request_hash = request_hash.to_vec();
        next.resolved_reply = Vec::new();
        next.retirement_confirmed = false;
        self.persist(next)?;
        self.boundary("after_pending_fsync")?;
        Ok(())
    }

    /// `Pending`/`ReplayAfterRetire` → `ResolvedAwaitingAck`: fsync the exact
    /// resolved reply before the caller can observe it. Returns the token an
    /// acknowledgement of exactly this entry must present.
    pub fn resolve(&mut self, encoded_reply: Vec<u8>) -> Result<AckToken> {
        let state = self.state();
        if state != JournalState::Pending && state != JournalState::ReplayAfterRetire {
            return Err(journal_error(format!(
                "resolve is not a transition out of {state:?}"
            )));
        }
        self.boundary("before_resolve_write")?;
        let mut next = self.record.clone();
        next.state_code = JournalState::ResolvedAwaitingAck.code();
        next.resolved_reply = encoded_reply;
        self.persist(next)?;
        self.boundary("after_resolve_fsync")?;
        Ok(self.token_of_current())
    }

    /// The saved reply this journal is holding for the application, if any,
    /// with the token that binds an acknowledgement to exactly this entry.
    pub fn unacknowledged(&self) -> Option<(&[u8], AckToken)> {
        (self.state() == JournalState::ResolvedAwaitingAck).then(|| {
            (
                self.record.resolved_reply.as_slice(),
                self.token_of_current(),
            )
        })
    }

    /// The identity of the record's current entry — what an acknowledgement
    /// must present to prove it acknowledges *this* result and not a later
    /// one that resolved after a stale handle was obtained.
    fn token_of_current(&self) -> AckToken {
        AckToken {
            client_id: self.record.client_id.clone(),
            sequence: self.record.sequence,
            request_hash: self.record.request_hash.clone(),
        }
    }

    /// `ResolvedAwaitingAck` → `Ready`: the application has durably handled
    /// the result (S4-JOURNAL-DELIVERY).
    ///
    /// Refused for a result the closing corridor produced: that entry's
    /// acknowledgement **is** [`rotate`](Self::rotate), because the client id
    /// it was resolved on is a tombstone and continuing on it would strand the
    /// next sequence behind `ClientRetired` forever.
    pub fn acknowledge(&mut self, token: &AckToken) -> Result<()> {
        if self.state() != JournalState::ResolvedAwaitingAck {
            return Err(journal_error("nothing is awaiting acknowledgement"));
        }
        if self.token_of_current() != *token {
            return Err(journal_error(
                "stale acknowledgement: the journal's resolved entry is not the one \
                 this handle was delivered for",
            ));
        }
        if self.record.retirement_confirmed {
            return Err(journal_error(
                "this result was resolved through retirement; acknowledge it by rotating",
            ));
        }
        self.boundary("before_ack_write")?;
        let mut next = self.record.clone();
        next.state_code = JournalState::Ready.code();
        next.pending_method = String::new();
        next.pending_request = Vec::new();
        next.request_hash = Vec::new();
        next.resolved_reply = Vec::new();
        self.persist(next)?;
        self.boundary("after_ack_fsync")?;
        Ok(())
    }

    /// `Pending` → `Closing`: a terminal `NotConsumed` ended this client id
    /// (§10). The pending bytes stay — the replay step needs them — and the
    /// refusal that ended it is kept so the eventual resolution can name it.
    pub fn close_for_retirement(&mut self, terminal_refusal: Vec<u8>) -> Result<()> {
        if self.state() != JournalState::Pending {
            return Err(journal_error("closing starts from Pending"));
        }
        self.boundary("before_closing_write")?;
        let mut next = self.record.clone();
        next.state_code = JournalState::Closing.code();
        next.retirement_confirmed = false;
        next.terminal_refusal = terminal_refusal;
        self.persist(next)?;
        self.boundary("after_closing_fsync")?;
        Ok(())
    }

    /// `Closing` → `ReplayAfterRetire`: retirement was **confirmed** by a
    /// reply, not assumed from a timeout.
    pub fn retirement_confirmed(&mut self) -> Result<()> {
        if self.state() != JournalState::Closing {
            return Err(journal_error("retirement confirmation follows Closing"));
        }
        self.boundary("before_retired_write")?;
        let mut next = self.record.clone();
        next.state_code = JournalState::ReplayAfterRetire.code();
        next.retirement_confirmed = true;
        self.persist(next)?;
        self.boundary("after_retired_fsync")?;
        Ok(())
    }

    /// Any state → `FatalJournal`, with the reason recorded by the caller's
    /// error path. Used for the explicit `18` proof (§10): nothing the client
    /// can do closes the id, so the failure surfaces and mutation stops.
    pub fn fatal(&mut self) -> Result<()> {
        self.boundary("before_fatal_write")?;
        let mut next = self.record.clone();
        next.state_code = JournalState::FatalJournal.code();
        self.persist(next)?;
        Ok(())
    }

    /// The closing corridor's acknowledgement: `ResolvedAwaitingAck` (via
    /// `ReplayAfterRetire`) → `Ready` under a **fresh client id** (§10's
    /// rotate step). The application calls this after durably handling the
    /// redelivered result, exactly as [`acknowledge`](Self::acknowledge) for
    /// the ordinary path. The sequence restarts; the first request on the new
    /// id is 1.
    pub fn rotate(&mut self, token: &AckToken, fresh_client_id: [u8; 16]) -> Result<()> {
        if self.state() != JournalState::ResolvedAwaitingAck || !self.record.retirement_confirmed {
            return Err(journal_error(
                "rotation acknowledges a result the closing corridor resolved, and nothing else",
            ));
        }
        if self.token_of_current() != *token {
            return Err(journal_error(
                "stale acknowledgement: the journal's resolved entry is not the one \
                 this handle was delivered for",
            ));
        }
        self.boundary("before_rotate_write")?;
        let mut next = self.record.clone();
        next.client_id = fresh_client_id.to_vec();
        next.state_code = JournalState::Ready.code();
        next.sequence = 0;
        next.pending_method = String::new();
        next.pending_request = Vec::new();
        next.request_hash = Vec::new();
        next.resolved_reply = Vec::new();
        next.retirement_confirmed = false;
        next.terminal_refusal = Vec::new();
        self.persist(next)?;
        self.boundary("after_rotate_fsync")?;
        Ok(())
    }
}

/// Make `to` name `from`'s content, atomically **and durably** — the same
/// platform split the §11.4 snapshot replacement uses, because the guarantee
/// lives in different places. On Unix the rename is atomic and the directory
/// entry is fsynced separately. On Windows `std::fs::rename` is `MoveFileExW`
/// with `MOVEFILE_REPLACE_EXISTING` and nothing else — atomic but **not**
/// write-through, and there is no directory handle to synchronize instead —
/// so `MOVEFILE_WRITE_THROUGH` supplies the durability half and has to be
/// asked for. A journal without it would fsync its bytes into a rename the
/// disk had not seen, and the §S4-D crash claim would be false on exactly one
/// platform.
#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(|source| io_error("renaming", to, source))
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let (source, destination) = (wide(from), wide(to));
    // SAFETY: both buffers are NUL-terminated and outlive the call, which is
    // the whole of `MoveFileExW`'s contract on its two path arguments.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io_error("renaming", to, std::io::Error::last_os_error()));
    }
    Ok(())
}

/// fsync the journal directory so a rename within it is durable. Unix only —
/// on Windows there is no directory handle to synchronize, and the
/// write-through flag on the replacement above is the platform's equivalent.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<()> {
    let handle = File::open(dir).map_err(|source| io_error("opening", dir, source))?;
    handle
        .sync_all()
        .map_err(|source| io_error("fsyncing the directory", dir, source))
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Write → fsync file → rename → fsync directory. The record is complete-old
/// or complete-new at every kill point in between.
fn persist_record(dir: &Path, record: &ClientJournalV1) -> Result<()> {
    let bytes = envelope(
        &payload_fory::<ClientJournalV1>()
            .serialize(record)
            .map_err(|_| journal_error("the record does not encode"))?,
    );
    let tmp = dir.join(RECORD_TMP);
    let target = dir.join(RECORD);
    {
        let mut file = File::create(&tmp).map_err(|source| io_error("creating", &tmp, source))?;
        restrict_permissions(&tmp, 0o600)?;
        file.write_all(&bytes)
            .map_err(|source| io_error("writing", &tmp, source))?;
        file.sync_all()
            .map_err(|source| io_error("fsyncing", &tmp, source))?;
    }
    atomic_replace(&tmp, &target)?;
    fsync_dir(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group() -> CoordinationGroupId {
        CoordinationGroupId::from_array([0xAB; 32])
    }

    const ACTOR: [u8; 32] = [0x11; 32];
    const CLIENT_A: [u8; 16] = [0xA1; 16];
    const CLIENT_B: [u8; 16] = [0xB2; 16];

    fn open(dir: &Path) -> Journal {
        Journal::open(dir, ACTOR, group(), CLIENT_A).expect("the journal opens")
    }

    /// A genuinely decodable `MutationReplyWireV1`, tagged so two replies
    /// are distinguishable. Validation decodes every saved reply, so the
    /// tests store what production stores — encoded replies, never labels.
    fn reply_bytes(tag: u64) -> Vec<u8> {
        let reply = crate::coordination::store::wire::MutationReplyWireV1 {
            sequence: tag,
            ..Default::default()
        };
        payload_fory::<crate::coordination::store::wire::MutationReplyWireV1>()
            .serialize(&reply)
            .expect("a reply encodes")
    }

    /// The §S4-D transition walk, with a reopen after every persisted step —
    /// because a crash *is* a reopen, the reopen is the assertion.
    #[test]
    fn every_transition_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("a directory");

        // First open mints and fsyncs the client id (Ready, sequence 0).
        {
            let journal = open(dir.path());
            assert_eq!(journal.state(), JournalState::Ready);
            assert_eq!(journal.sequence(), 0);
            assert_eq!(journal.client_id(), CLIENT_A);
        }

        // Ready → Pending.
        {
            let mut journal = open(dir.path());
            journal
                .begin(1, "acquire", b"exact-request-bytes".to_vec(), [0x33; 32])
                .expect("begin");
        }
        {
            let journal = open(dir.path());
            assert_eq!(journal.state(), JournalState::Pending);
            assert_eq!(journal.sequence(), 1);
            assert_eq!(journal.record().pending_request, b"exact-request-bytes");
            assert_eq!(journal.record().pending_method, "acquire");
        }

        // Pending → ResolvedAwaitingAck, and the reply survives death.
        {
            let mut journal = open(dir.path());
            journal.resolve(reply_bytes(1)).expect("resolve");
        }
        {
            let journal = open(dir.path());
            assert_eq!(journal.state(), JournalState::ResolvedAwaitingAck);
            let (reply, _) = journal.unacknowledged().expect("held");
            assert_eq!(reply, reply_bytes(1));
        }

        // ResolvedAwaitingAck → Ready on acknowledgement; the reply is gone
        // and the sequence is consumed. The acknowledgement presents the
        // token the redelivery came with.
        {
            let mut journal = open(dir.path());
            let (_, token) = journal.unacknowledged().expect("held");
            journal.acknowledge(&token).expect("acknowledge");
        }
        {
            let journal = open(dir.path());
            assert_eq!(journal.state(), JournalState::Ready);
            assert_eq!(journal.sequence(), 1);
            assert!(journal.unacknowledged().is_none());
        }
    }

    /// The closing corridor: Pending → Closing → ReplayAfterRetire →
    /// (resolve) → rotate, each surviving a reopen, and the pending bytes
    /// retained until the replay resolves them.
    #[test]
    fn the_closing_corridor_retains_the_exact_bytes_until_replay_resolves() {
        let dir = tempfile::tempdir().expect("a directory");
        {
            let mut journal = open(dir.path());
            journal
                .begin(1, "set_admin", b"refused-request".to_vec(), [0x44; 32])
                .expect("begin");
            journal
                .close_for_retirement(reply_bytes(24))
                .expect("close");
        }
        {
            let journal = open(dir.path());
            assert_eq!(journal.state(), JournalState::Closing);
            assert!(!journal.record().retirement_confirmed);
            assert_eq!(
                journal.record().terminal_refusal,
                reply_bytes(24),
                "the refusal that opened the corridor is retained for the resolution"
            );
            assert_eq!(
                journal.record().pending_request,
                b"refused-request",
                "the replay step still needs the exact bytes"
            );
        }
        {
            let mut journal = open(dir.path());
            journal.retirement_confirmed().expect("retired");
        }
        {
            let journal = open(dir.path());
            assert_eq!(journal.state(), JournalState::ReplayAfterRetire);
            assert!(journal.record().retirement_confirmed);
        }
        // The replay resolves — with whatever the tombstone answered. Its
        // acknowledgement is the rotation, and the ordinary acknowledge is
        // refused: continuing on a tombstoned id would strand the next
        // sequence.
        {
            let mut journal = open(dir.path());
            let token = journal.resolve(reply_bytes(7)).expect("resolve");
            let (reply, redelivered) = journal.unacknowledged().expect("held");
            assert_eq!(
                reply,
                reply_bytes(7),
                "the redelivered result is readable until acknowledged"
            );
            assert_eq!(token, redelivered, "one entry, one token");
            assert!(
                journal.acknowledge(&token).is_err(),
                "a retirement-resolved result is acknowledged by rotating"
            );
            journal
                .rotate(&token, CLIENT_B)
                .expect("rotate acknowledges it");
            assert_eq!(journal.client_id(), CLIENT_B);
            assert_eq!(journal.state(), JournalState::Ready);
        }
        // Rotation is not reachable outside the corridor.
        {
            let mut journal = open(dir.path());
            let stale = AckToken {
                client_id: CLIENT_B.to_vec(),
                sequence: 1,
                request_hash: vec![0x44; 32],
            };
            assert!(journal.rotate(&stale, CLIENT_A).is_err());
        }
    }

    /// Rotation replaces the client id and restarts the sequence.
    #[test]
    fn rotation_mints_the_next_client_identity() {
        let dir = tempfile::tempdir().expect("a directory");
        {
            let mut journal = open(dir.path());
            journal
                .begin(1, "acquire", b"x".to_vec(), [0x55; 32])
                .expect("begin");
            journal
                .close_for_retirement(reply_bytes(26))
                .expect("close");
            journal.retirement_confirmed().expect("retired");
            let token = journal.resolve(reply_bytes(9)).expect("resolve");
            journal.rotate(&token, CLIENT_B).expect("rotate");
        }
        let journal = open(dir.path());
        assert_eq!(journal.client_id(), CLIENT_B);
        assert_eq!(journal.state(), JournalState::Ready);
        assert_eq!(journal.sequence(), 0);
        assert!(journal.record().pending_request.is_empty());
    }

    /// The delivery contract: no new sequence while a result awaits
    /// acknowledgement, and none while one is pending.
    #[test]
    fn no_new_sequence_before_the_last_is_acknowledged() {
        let dir = tempfile::tempdir().expect("a directory");
        let mut journal = open(dir.path());
        journal
            .begin(1, "acquire", b"a".to_vec(), [0x66; 32])
            .expect("begin");
        assert!(
            journal
                .begin(2, "acquire", b"b".to_vec(), [0x67; 32])
                .is_err(),
            "one outstanding request per client id"
        );
        let token = journal.resolve(reply_bytes(3)).expect("resolve");
        assert!(
            journal
                .begin(2, "acquire", b"b".to_vec(), [0x67; 32])
                .is_err(),
            "no new sequence until the application acknowledges"
        );
        journal.acknowledge(&token).expect("acknowledge");
        journal
            .begin(2, "acquire", b"b".to_vec(), [0x67; 32])
            .expect("the next sequence is 2");
    }

    /// The reviewer's race, made unspellable: a token captured for result A
    /// cannot acknowledge result B. Acknowledge A, resolve B at the next
    /// sequence, and the stale A token is refused while B's own succeeds.
    #[test]
    fn a_stale_token_cannot_acknowledge_a_later_result() {
        let dir = tempfile::tempdir().expect("a directory");
        let mut journal = open(dir.path());
        journal
            .begin(1, "acquire", b"a".to_vec(), [0x71; 32])
            .expect("begin A");
        let token_a = journal.resolve(reply_bytes(101)).expect("resolve A");
        // Two handles for A exist in the reviewer's scenario; the first one
        // acknowledges.
        journal.acknowledge(&token_a).expect("A acknowledged");
        journal
            .begin(2, "acquire", b"b".to_vec(), [0x72; 32])
            .expect("begin B");
        let token_b = journal.resolve(reply_bytes(102)).expect("resolve B");
        assert!(
            journal.acknowledge(&token_a).is_err(),
            "the surviving A handle must not mark B handled"
        );
        assert_eq!(
            journal.state(),
            JournalState::ResolvedAwaitingAck,
            "B still awaits its own acknowledgement"
        );
        journal
            .acknowledge(&token_b)
            .expect("B's own token succeeds");
    }

    /// The journal binds `(actor, group)`: another actor's or group's
    /// directory refuses to open rather than being repaired.
    #[test]
    fn the_binding_is_part_of_the_identity() {
        let dir = tempfile::tempdir().expect("a directory");
        drop(open(dir.path()));

        let other_actor = Journal::open(dir.path(), [0x99; 32], group(), CLIENT_A);
        assert!(other_actor.is_err());

        let other_group = Journal::open(
            dir.path(),
            ACTOR,
            CoordinationGroupId::from_array([0xCD; 32]),
            CLIENT_A,
        );
        assert!(other_group.is_err());
    }

    /// The lifetime lock: a second live owner is refused while the first
    /// holds the journal, and admitted once it is gone.
    #[test]
    fn one_process_owns_the_journal_at_a_time() {
        let dir = tempfile::tempdir().expect("a directory");
        let first = open(dir.path());
        let second = Journal::open(dir.path(), ACTOR, group(), CLIENT_A);
        assert!(second.is_err(), "the lifetime lock excludes a second owner");
        drop(first);
        let third = Journal::open(dir.path(), ACTOR, group(), CLIENT_A);
        assert!(third.is_ok(), "the lock is released with the journal");
    }

    /// A record from a future format version refuses rather than guessing —
    /// the migration policy is explicit.
    #[test]
    fn a_newer_format_version_is_refused() {
        let dir = tempfile::tempdir().expect("a directory");
        {
            let mut journal = open(dir.path());
            let mut record = journal.record().clone();
            record.format_version = JOURNAL_FORMAT_VERSION + 1;
            journal.persist(record).expect("persist");
        }
        assert!(Journal::open(dir.path(), ACTOR, group(), CLIENT_A).is_err());
    }

    /// Write `record` to `dir` with a **valid** envelope, bypassing the
    /// transition rules — the corruption tests' way of minting records no
    /// healthy transition produces.
    fn write_raw(dir: &Path, record: &ClientJournalV1) {
        let bytes = envelope(
            &payload_fory::<ClientJournalV1>()
                .serialize(record)
                .expect("encodes"),
        );
        std::fs::write(dir.join(RECORD), bytes).expect("writes");
    }

    /// §10: "Corruption … fails closed; the library never
    /// deletes/reinitializes it automatically." One flipped byte anywhere in
    /// the record fails the checksum; truncation and a non-envelope file are
    /// refused as corruption too — and the file is left exactly as found.
    #[test]
    fn corruption_fails_closed_and_is_never_repaired() {
        let dir = tempfile::tempdir().expect("a directory");
        {
            let mut journal = open(dir.path());
            journal
                .begin(1, "acquire", b"exact".to_vec(), [0x21; 32])
                .expect("begin");
        }
        let path = dir.path().join(RECORD);
        let pristine = std::fs::read(&path).expect("the record reads");

        // A flipped byte in the payload half.
        let mut flipped = pristine.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x01;
        std::fs::write(&path, &flipped).expect("write");
        let refused = Journal::open(dir.path(), ACTOR, group(), CLIENT_A);
        assert!(refused.is_err(), "a flipped byte must fail the checksum");
        assert_eq!(
            std::fs::read(&path).expect("still there"),
            flipped,
            "fail closed means the corrupt file is refused, not rewritten"
        );

        // Truncation.
        std::fs::write(&path, &pristine[..pristine.len() / 2]).expect("write");
        assert!(Journal::open(dir.path(), ACTOR, group(), CLIENT_A).is_err());

        // Not an envelope at all — an unknown newer container included.
        std::fs::write(&path, b"not-a-journal-envelope-at-all-....").expect("write");
        assert!(Journal::open(dir.path(), ACTOR, group(), CLIENT_A).is_err());

        // The pristine bytes still open: the refusals above were the
        // envelope's verdicts, not collateral state damage.
        std::fs::write(&path, &pristine).expect("write");
        assert!(Journal::open(dir.path(), ACTOR, group(), CLIENT_A).is_ok());
    }

    /// The state-dependent invariants fail closed at open: a
    /// `ReplayAfterRetire` without a confirmed retirement, a `Pending` with
    /// no method to resend to, an undecodable saved reply or terminal
    /// refusal, and entry residue on a `Ready` record are each refused.
    #[test]
    fn invalid_state_combinations_are_refused_at_open() {
        let base = |state: JournalState| ClientJournalV1 {
            format_version: JOURNAL_FORMAT_VERSION,
            group_id: group().as_bytes().to_vec(),
            actor_node_id: ACTOR.to_vec(),
            client_id: CLIENT_A.to_vec(),
            state_code: state.code(),
            sequence: 1,
            pending_method: "acquire".to_string(),
            pending_request: b"exact".to_vec(),
            request_hash: vec![0x31; 32],
            resolved_reply: Vec::new(),
            retirement_confirmed: false,
            terminal_refusal: Vec::new(),
        };
        let refuses = |record: &ClientJournalV1, why: &str| {
            let dir = tempfile::tempdir().expect("a directory");
            write_raw(dir.path(), record);
            assert!(
                Journal::open(dir.path(), ACTOR, group(), CLIENT_A).is_err(),
                "{why}"
            );
        };

        // ReplayAfterRetire without retirement_confirmed.
        let mut record = base(JournalState::ReplayAfterRetire);
        record.terminal_refusal = reply_bytes(24);
        record.retirement_confirmed = false;
        refuses(
            &record,
            "a replay on an unconfirmed retirement must be refused",
        );

        // Closing that claims a confirmed retirement.
        let mut record = base(JournalState::Closing);
        record.terminal_refusal = reply_bytes(24);
        record.retirement_confirmed = true;
        refuses(&record, "Closing precedes confirmation");

        // Pending with an empty method: nothing to resend to.
        let mut record = base(JournalState::Pending);
        record.pending_method = String::new();
        refuses(&record, "a Pending entry with no method cannot resend");

        // A resolved state whose saved reply is not a decodable reply.
        let mut record = base(JournalState::ResolvedAwaitingAck);
        record.resolved_reply = b"garbage-not-a-reply".to_vec();
        refuses(&record, "an undecodable saved reply is corruption");

        // A closing state whose terminal refusal is not decodable.
        let mut record = base(JournalState::Closing);
        record.terminal_refusal = b"garbage-not-a-refusal".to_vec();
        refuses(&record, "an undecodable terminal refusal is corruption");

        // Ready with entry residue.
        let mut record = base(JournalState::Ready);
        record.resolved_reply = reply_bytes(3);
        refuses(&record, "a Ready record must carry no entry residue");

        // And the healthy spellings of the same states still open.
        let mut record = base(JournalState::ReplayAfterRetire);
        record.terminal_refusal = reply_bytes(24);
        record.retirement_confirmed = true;
        let dir = tempfile::tempdir().expect("a directory");
        write_raw(dir.path(), &record);
        assert!(Journal::open(dir.path(), ACTOR, group(), CLIENT_A).is_ok());
    }

    /// Owner-only permissions on the directory and every file in it (S4-D).
    #[cfg(unix)]
    #[test]
    fn the_journal_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a directory");
        let journal_dir = dir.path().join("j");
        drop(Journal::open(&journal_dir, ACTOR, group(), CLIENT_A).expect("open"));

        let mode = |p: &Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&journal_dir), 0o700, "the directory is owner-only");
        assert_eq!(mode(&journal_dir.join(RECORD)), 0o600);
        assert_eq!(mode(&journal_dir.join(LOCK)), 0o600);
    }

    /// The crash matrix: stop at **every** named boundary of the full walk,
    /// reopen, and require a state the protocol can continue from. This is
    /// the §S4-D table executed, not sampled — a new boundary that is not
    /// listed here fails the completeness assertion below.
    #[test]
    fn a_kill_at_every_boundary_reopens_into_a_continuable_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Walk the full protocol, counting boundaries.
        let boundaries: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let dir = tempfile::tempdir().expect("a directory");
            let seen = Arc::clone(&boundaries);
            let mut journal = Journal::open(dir.path(), ACTOR, group(), CLIENT_A)
                .expect("open")
                .with_crash_hook(move |point| {
                    seen.lock().expect("mutex").push(point);
                    Ok(())
                });
            full_walk(&mut journal);
        }
        let walked = boundaries.lock().expect("mutex").clone();
        let persisted = |name: &'static str| {
            let (before, after) = match name {
                "pending" => ("before_pending_write", "after_pending_fsync"),
                "resolve" => ("before_resolve_write", "after_resolve_fsync"),
                "ack" => ("before_ack_write", "after_ack_fsync"),
                "closing" => ("before_closing_write", "after_closing_fsync"),
                "retired" => ("before_retired_write", "after_retired_fsync"),
                "rotate" => ("before_rotate_write", "after_rotate_fsync"),
                other => panic!("unknown transition {other}"),
            };
            vec![before, "persist_tmp_fsynced", "persist_renamed", after]
        };
        let expected: Vec<&'static str> = [
            "pending", "resolve", "ack", "pending", "closing", "retired", "resolve", "rotate",
        ]
        .into_iter()
        .flat_map(persisted)
        .collect();
        assert_eq!(
            walked, expected,
            "the walk passes every declared boundary — including both sides of              every rename commit point — in order"
        );

        // Now kill at each boundary in turn and reopen.
        for (kill_at, kill_name) in walked.iter().enumerate() {
            let dir = tempfile::tempdir().expect("a directory");
            let counter = Arc::new(AtomicUsize::new(0));
            {
                let counter = Arc::clone(&counter);
                let mut journal = Journal::open(dir.path(), ACTOR, group(), CLIENT_A)
                    .expect("open")
                    .with_crash_hook(move |_| {
                        if counter.fetch_add(1, Ordering::SeqCst) == kill_at {
                            Err(Error::InvalidArgument("killed".into()))
                        } else {
                            Ok(())
                        }
                    });
                // The walk stops at the armed boundary; everything before it
                // persisted, nothing after it ran.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    full_walk(&mut journal)
                }));
            }
            let reopened =
                Journal::open(dir.path(), ACTOR, group(), CLIENT_A).expect("the crash reopens");
            let state = reopened.state();
            assert!(
                matches!(
                    state,
                    JournalState::Ready
                        | JournalState::Pending
                        | JournalState::ResolvedAwaitingAck
                        | JournalState::Closing
                        | JournalState::ReplayAfterRetire
                ),
                "kill at boundary {kill_at} ({kill_name}) reopened into {state:?}, which \
                 the protocol cannot continue from"
            );
            // The invariant the resend rule rests on: whenever the record says
            // Pending / Closing / ReplayAfterRetire, the exact bytes are there.
            if matches!(
                state,
                JournalState::Pending | JournalState::Closing | JournalState::ReplayAfterRetire
            ) {
                assert!(
                    !reopened.record().pending_request.is_empty(),
                    "kill at {kill_at}: a resendable state must hold the exact bytes"
                );
            }
            // And whenever it says ResolvedAwaitingAck, the reply is there.
            if state == JournalState::ResolvedAwaitingAck {
                assert!(
                    !reopened.record().resolved_reply.is_empty(),
                    "kill at {kill_at}: a resolved state must hold the exact reply"
                );
            }
        }
    }

    /// One full protocol walk: an acknowledged mutation, then a refused one
    /// through the closing corridor and rotation. Panics on the first armed
    /// kill, which is what the crash matrix catches.
    fn full_walk(journal: &mut Journal) {
        journal
            .begin(1, "acquire", b"first".to_vec(), [0x71; 32])
            .expect("begin 1");
        let token = journal.resolve(reply_bytes(11)).expect("resolve 1");
        journal.acknowledge(&token).expect("ack 1");
        journal
            .begin(2, "set_admin", b"second".to_vec(), [0x72; 32])
            .expect("begin 2");
        journal
            .close_for_retirement(reply_bytes(12))
            .expect("close");
        journal.retirement_confirmed().expect("retired");
        let token = journal.resolve(reply_bytes(13)).expect("resolve 2");
        journal.rotate(&token, CLIENT_B).expect("rotate");
    }
}
