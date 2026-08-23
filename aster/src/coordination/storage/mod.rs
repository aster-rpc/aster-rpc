//! Durable voter storage: redb schema, OpenRaft log and state machine,
//! snapshots (§11).
//!
//! Voter-only. A client evaluates no transition and stores no log, so
//! `coordination-client` compiles none of this.
//!
//! ## Shape
//!
//! | Module | Owns |
//! |---|---|
//! | [`actor`] | the shared single-writer shutdown guard |
//! | [`codec`] | the single decode site for persisted values |
//! | [`error`] | the named §11.1 failures, kept distinct on purpose |
//! | [`identity`] | the crate-private storage-open identity and policy |
//! | [`schema`] | layout, lifetime lock, table/meta allowlists, the open coordinator |
//! | [`log`] | `RaftLogStorage` over `raft.redb` (Checkpoint B) |
//! | [`rows`] | typed §11.3 row I/O, the whole-state load, and its validator |
//! | [`machine`] | `RaftStateMachine` over `state.redb` (Checkpoint C) |
//! | [`reads`] | outstanding read transactions, and waiting them out |
//! | [`snapshot`] | the role-safe snapshot handle and catalog (Checkpoint D) |
//!
//! Everything here is crate-private. Slice 3 owns the public start
//! configuration that supplies verified genesis, authority, start mode, and
//! admission; exposing a storage type before then would fix a public contract
//! this slice cannot verify.
//!
//! ## Unreached-by-shipping-code allowances
//!
//! Each module below carries its own module-scoped `allow(dead_code)`, and
//! this one does not. A crate-root allowance would silently cover every
//! storage module added after it, and a test suite proves exercise, not
//! shipping reachability.
//!
//! **These do not come off in Checkpoint B, and an earlier note here was
//! wrong to say they would.** Nothing in a shipping build reaches
//! [`VoterStorage::open`] yet, because opening storage needs a verified
//! [`identity::StorageIdentity`] — group id, genesis hash, authority key and
//! domain — and the signed bootstrap that produces one is Slice 3's. The
//! public
//! [`builtin_coordination`](crate::rpc::AsterServerBuilder::builtin_coordination)
//! accepts a [`CoordinationConfig`](crate::coordination::host::CoordinationConfig)
//! that deliberately carries none of it, precisely so no field reads as
//! configured while nothing verifies it.
//!
//! Making the call reachable now would mean inventing that bootstrap here,
//! which is the Slice 3 boundary the plan draws. The allowances come off when
//! Slice 3 wires the voter runtime, not before.

pub(crate) mod actor;
pub(crate) mod codec;
pub(crate) mod crash;
pub(crate) mod error;
/// The OpenRaft storage-suite fixture. Test-only: it exists to hold one group
/// identity fixed across every store a builder makes, which is what
/// `transfer_snapshot` requires.
#[cfg(test)]
pub(crate) mod fixture;
pub(crate) mod identity;
pub(crate) mod log;
pub(crate) mod machine;
pub(crate) mod reads;
pub(crate) mod rows;
pub(crate) mod schema;
/// The owning scratch directory every test module here builds its stores in.
#[cfg(test)]
pub(crate) mod scratch;
pub(crate) mod snapshot;

pub(crate) use snapshot::CoordinationSnapshotFile;

use error::StorageResult;
#[cfg(test)]
use fixture::SuiteFixture;
use identity::{OpenPolicy, StorageIdentity};
use log::{CoordinationLogStore, LogShutdown};
#[cfg(test)]
use machine::SuiteStateMachine;
use machine::{CoordinationStateMachine, StateShutdown};
use reads::ReadDrain;
use schema::{DirectoryLock, IntegrityOutcome, OpenOutcome, StorageLayout};

// ── OpenRaft's own storage suite ─────────────────────────────────────────────

/// A [`StoreBuilder`] over [`SuiteFixture`].
///
/// `Fresh` hands the suite a new directory each time, which is what every
/// storage implementation's own suite run does. `Reopen` builds a store, closes
/// it -- both actors joined, both databases released -- and opens the same
/// directory again before handing it over, so every store the suite is given has
/// already been through the open path once and been validated as an existing
/// store rather than created as a new one.
///
/// It is worth being exact about what that does and does not show. The reopen
/// happens *before* the suite writes anything, so this is not evidence that a
/// suite case's writes survive a restart -- the store is empty at the moment it
/// is closed and reopened. What it covers is the second open itself: schema and
/// identity revalidation, actor restart, and the suite's whole contract holding
/// against a store that was recovered rather than created. Evidence that written
/// state survives a restart is a separate test, `machine.rs`'s
/// `a_normal_command_and_a_non_empty_log_survive_a_restart`, and the crash
/// matrix beside it.
#[cfg(test)]
pub(crate) struct SuiteBuilder {
    fixture: SuiteFixture,
    reopen: bool,
}

#[cfg(test)]
impl SuiteBuilder {
    pub(crate) fn fresh(root: impl Into<std::path::PathBuf>, seed: u8) -> Self {
        Self {
            fixture: SuiteFixture::new(root, seed),
            reopen: false,
        }
    }

    pub(crate) fn reopened(root: impl Into<std::path::PathBuf>, seed: u8) -> Self {
        Self {
            fixture: SuiteFixture::new(root, seed),
            reopen: true,
        }
    }
}

#[cfg(test)]
impl
    openraft::testing::StoreBuilder<
        crate::coordination::typeconfig::CoordinationTypeConfig,
        CoordinationLogStore,
        SuiteStateMachine,
        SuiteGuard,
    > for SuiteBuilder
{
    async fn build(
        &self,
    ) -> Result<
        (SuiteGuard, CoordinationLogStore, SuiteStateMachine),
        openraft::StorageError<crate::coordination::ids::CoordinationNodeId>,
    > {
        let (dir, store) = self.fixture.build_at();
        if self.reopen {
            // Closed before it is used: both actors joined, both databases
            // released, the lock gone. What the suite then drives is a store
            // that read its own files back.
            drop(store);
            let reopened = self.fixture.open_existing(&dir);
            return Ok(reopened.into_suite_parts());
        }
        Ok(store.into_suite_parts())
    }
}

/// A validated, locked storage directory with its log actor running.
///
/// Composition lives here rather than in [`schema`] because the open
/// coordinator must own each database by value while checking it, and
/// [`schema`] cannot hand one to an actor it does not know about without
/// depending on [`log`].
///
/// `state` is still a plain database: Checkpoint C is what gives it an actor.
///
/// The allowance is on this item, not on the module: a module-wide one here
/// would mask every later addition to the storage root, which is the thing the
/// per-module scoping above exists to avoid.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct VoterStorage {
    layout: StorageLayout,
    identity: StorageIdentity,
    log: CoordinationLogStore,
    state: CoordinationStateMachine,
    outcome: OpenOutcome,
    integrity: (IntegrityOutcome, IntegrityOutcome),
    /// Field order below is the shutdown order, and it is load-bearing.
    ///
    /// Rust drops fields in declaration order, so **both** actors are stopped
    /// and joined -- releasing their `Database` handles -- before the directory
    /// lock goes. Reversed, a restart would find the lock free but a database
    /// still open, and a second voter could be admitted while the first still
    /// has the file open.
    ///
    /// [`ReadDrain`] sits between them for the same reason and a subtler one. A
    /// redb `ReadTransaction` is `'static` -- it keeps its version alive
    /// through its own `Arc` and does not borrow the `Database` -- so a
    /// snapshot builder holding one is a reader this store cannot see. Draining
    /// first means the state actor does not release the database, and the lock
    /// does not go, while a builder is still reading a version of it.
    _log_shutdown: LogShutdown,
    _reads: ReadDrain,
    _state_shutdown: StateShutdown,
    _lock: DirectoryLock,
}

/// What a store that has been split into its two halves still owns.
///
/// Same four values in the same order as [`VoterStorage`], for the same
/// reason: dropping this stops and joins both actors, releasing their database
/// handles, before the directory lock is released.
#[cfg(test)]
pub(crate) struct SuiteGuard {
    _log_shutdown: LogShutdown,
    _reads: ReadDrain,
    _state_shutdown: StateShutdown,
    _lock: DirectoryLock,
}

#[allow(dead_code)]
impl VoterStorage {
    /// Open the storage directory and start the log actor.
    pub(crate) fn open(
        layout: StorageLayout,
        identity: StorageIdentity,
        policy: OpenPolicy,
    ) -> StorageResult<Self> {
        let parts = schema::open(layout, identity, policy)?.into_parts();

        // Before any actor exists. Schema and identity validation prove the
        // file is ours; they prove nothing about whether its contents decode.
        // A store published first and validated later has already told the
        // group it is up by the time the first replication read finds the
        // corruption.
        log::validate_persisted_log(&parts.raft)?;

        let (log, log_shutdown) = CoordinationLogStore::new(parts.raft)?;
        // The state machine's own load validates every §11.3 row and the whole
        // state before its actor exists, for the same reason.
        let (state, state_shutdown, reads) =
            CoordinationStateMachine::new(parts.state, parts.layout.clone())?;
        Ok(Self {
            layout: parts.layout,
            identity: parts.identity,
            log,
            state,
            outcome: parts.outcome,
            integrity: parts.integrity,
            _log_shutdown: log_shutdown,
            _reads: reads,
            _state_shutdown: state_shutdown,
            _lock: parts.lock,
        })
    }

    pub(crate) fn log(&self) -> &CoordinationLogStore {
        &self.log
    }

    pub(crate) fn log_mut(&mut self) -> &mut CoordinationLogStore {
        &mut self.log
    }

    pub(crate) fn state(&self) -> &CoordinationStateMachine {
        &self.state
    }

    /// Both stores at once, for the recovery OpenRaft performs on reopen.
    ///
    /// `open` starts the actors and validates what is on disk; it does **not**
    /// replay the committed log, which is `StorageHelper::get_initial_state`'s
    /// job and needs the two together. A caller that inspected this store
    /// without running it would be observing the state as it was *before*
    /// recovery -- which for a voter killed between an append and its apply is
    /// the wrong state entirely.
    /// Split into the two halves OpenRaft's `StoreBuilder` hands back, plus a
    /// guard that owns the shutdown.
    ///
    /// The guard exists because the shutdown ordering documented on the fields
    /// above is a property of *this* struct, and handing out the log store and
    /// the state machine separately would leave nothing holding it. Inside
    /// `SuiteGuard` the same four values sit in the same order, so both actors
    /// are still stopped and joined before the directory lock goes.
    #[cfg(test)]
    pub(crate) fn into_suite_parts(self) -> (SuiteGuard, CoordinationLogStore, SuiteStateMachine) {
        let VoterStorage {
            layout: _,
            identity: _,
            log,
            state,
            outcome: _,
            integrity: _,
            _log_shutdown,
            _reads,
            _state_shutdown,
            _lock,
        } = self;
        (
            SuiteGuard {
                _log_shutdown,
                _reads,
                _state_shutdown,
                _lock,
            },
            log,
            SuiteStateMachine(state),
        )
    }

    pub(crate) fn recovery_parts(
        &mut self,
    ) -> (&mut CoordinationLogStore, &mut CoordinationStateMachine) {
        (&mut self.log, &mut self.state)
    }

    pub(crate) fn state_mut(&mut self) -> &mut CoordinationStateMachine {
        &mut self.state
    }

    pub(crate) fn layout(&self) -> &StorageLayout {
        &self.layout
    }

    pub(crate) fn identity(&self) -> &StorageIdentity {
        &self.identity
    }

    pub(crate) fn outcome(&self) -> OpenOutcome {
        self.outcome
    }

    /// How each database's integrity check resolved, as `(raft, state)`.
    ///
    /// Slice 3 emits the §11.1 recovery event from this. Dropping it here
    /// would make that event unreportable: by the time anything else can look,
    /// the databases have been reopened clean and the evidence that a crash
    /// happened is gone.
    pub(crate) fn integrity(&self) -> (IntegrityOutcome, IntegrityOutcome) {
        self.integrity
    }
}

#[cfg(test)]
mod durability_guard {
    //! §11.1: `Durability::None` must not appear anywhere in this subsystem,
    //! "not even as a throughput option behind configuration".
    //!
    //! Guarded at the source level because it cannot be guarded at runtime.
    //! `Durability::None` defers the fsync; every read in this process still
    //! sees the data, and so does a clean close and reopen. Only a process
    //! death between the commit and the fsync tells the two apart, which is
    //! Checkpoint E's crash matrix. Swapping the setting today fails no
    //! behavioural test — verified by mutation — so the rule needs an
    //! assertion of the shape it actually has: a statement about the code.

    /// Every source file in this module tree, checked as text.
    const SOURCES: &[(&str, &str)] = &[
        ("mod.rs", include_str!("mod.rs")),
        ("actor.rs", include_str!("actor.rs")),
        ("codec.rs", include_str!("codec.rs")),
        ("crash.rs", include_str!("crash.rs")),
        ("error.rs", include_str!("error.rs")),
        ("identity.rs", include_str!("identity.rs")),
        ("schema.rs", include_str!("schema.rs")),
        ("log.rs", include_str!("log.rs")),
        ("machine.rs", include_str!("machine.rs")),
        ("reads.rs", include_str!("reads.rs")),
        ("rows.rs", include_str!("rows.rs")),
        ("scratch.rs", include_str!("scratch.rs")),
        ("snapshot.rs", include_str!("snapshot.rs")),
        ("fixture.rs", include_str!("fixture.rs")),
    ];

    /// Every guard here reads only what [`production_end`] admits, so the
    /// boundary itself needs a guard.
    ///
    /// Taking the *last* `#[cfg(test)]` module fixes truncation and introduces
    /// the opposite assumption: that nothing follows it. Production appended
    /// after the test module sits outside every check above, silently, in a
    /// file that still compiles and still reports clean.
    ///
    /// This finds the module's **closing brace** rather than enumerating what
    /// production looks like. A list of item prefixes is a list that is wrong
    /// as soon as someone writes `async fn`, `unsafe fn`, or `mod` -- each of
    /// which escapes a prefix check while compiling perfectly. The close is one
    /// fact and covers all of them: under rustfmt a top-level module ends with
    /// a `}` in column zero, everything inside it is indented, and so the first
    /// such line is the end of the file's test code. After it, only whitespace.
    #[test]
    fn nothing_follows_the_test_module() {
        for (name, source) in SOURCES {
            let end = production_end(source);
            if end == source.len() {
                continue;
            }
            let before = source[..end].lines().count();
            let tail: Vec<&str> = source[end..].lines().collect();
            let close = tail
                .iter()
                .position(|line| *line == "}")
                .unwrap_or_else(|| panic!("{name}: the test module has no column-zero close"));

            for (n, line) in tail.iter().enumerate().skip(close + 1) {
                assert!(
                    line.trim().is_empty(),
                    "{name}:{} is outside the test module and outside every guard in this \
                     file: {line:?}",
                    before + n + 1
                );
            }
        }
    }

    /// `SOURCES` is hand-written, so it can fall behind the directory.
    ///
    /// A new storage source that nobody adds here is exempt from every guard
    /// above -- the deferred fsync, the single write site, the bounded decoder
    /// -- and nothing says so.
    ///
    /// Compared against the **filesystem**, not against `mod` declarations
    /// parsed out of the source. Four versions of that parser were four
    /// different wrong answers -- `pub(self)`, then `pub(in ...)`, then raw and
    /// non-ASCII names, then a trailing block comment -- each fixing the last
    /// counterexample. The set of files in a directory is not a question that
    /// needs a Rust lexer, and every escape found so far was an escape from the
    /// lexer rather than from the rule.
    #[test]
    fn every_source_in_this_directory_is_guarded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/coordination/storage");
        let mut found = Vec::new();
        sources_on_disk(&dir, "", &mut found);

        let on_disk: std::collections::BTreeSet<String> = found.into_iter().collect();
        let guarded: std::collections::BTreeSet<String> = SOURCES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();

        assert_eq!(
            on_disk,
            guarded,
            "every .rs file under {} must be in SOURCES, and nothing else; a source \
             missing here is exempt from every guard in this module",
            dir.display()
        );
    }

    /// No source may be pulled in from outside this directory.
    ///
    /// The one thing the directory walk cannot see. `#[path = "../elsewhere.rs"]`
    /// and `include!` both make a file part of this subsystem while leaving it
    /// somewhere the walk never looks, so the compiled module tree and the
    /// guarded file set would quietly stop being the same thing.
    ///
    /// `cfg_attr` is prohibited with them, because it reaches the same place by
    /// a route no substring check on `path` can follow:
    /// `#[cfg_attr(all(), path = "../outside.rs")]` compiles, survives rustfmt,
    /// and has a condition that is simply true. Recognising *that* needs
    /// attribute-aware parsing, and hand-parsing Rust here has been wrong every
    /// time it has been tried. Nothing under this directory uses `cfg_attr`
    /// today, so forbidding it costs nothing and the day it is wanted is the
    /// day someone decides deliberately what it may carry.
    ///
    /// A substring prohibition rather than a parser: the point is that none of
    /// the three appears at all. `include_str!` is unaffected -- a different
    /// token, and the mechanism this guard is built on.
    #[test]
    fn no_source_is_pulled_in_from_outside_this_directory() {
        for (name, source) in SOURCES {
            for (n, line) in production_lines(source) {
                for escape in ["#[path", "include!", "cfg_attr"] {
                    assert!(
                        !line.contains(escape),
                        "{name}:{} uses {escape:?} to pull in a source the directory walk \
                         cannot see: {:?}",
                        n + 1,
                        line.trim()
                    );
                }
            }
        }
    }

    #[test]
    fn no_source_in_this_subsystem_selects_the_deferred_fsync_level() {
        // Built rather than written, so this file does not trip its own check.
        let forbidden = format!("{}::{}", "Durability", "None");
        for (name, source) in SOURCES {
            for (n, line) in source.lines().enumerate() {
                // Prose that names the level in order to explain why it is
                // forbidden is not a use of it.
                let code = line.split("//").next().unwrap_or("");
                assert!(
                    !code.contains(&forbidden),
                    "{name}:{} selects the deferred-fsync durability level, \
                     which turns OpenRaft's durability contract into a tunable",
                    n + 1
                );
            }
        }
    }

    /// And the setting that *is* required appears where writes happen.
    ///
    /// The pair matters: a file with neither is not "safe by omission", it is
    /// a file relying on whatever redb defaults to today.
    /// Production code creates a write transaction in exactly one place.
    ///
    /// The previous version of this check was per *file*, and a mutation
    /// proved it hollow: removing the durability setting from one of two write
    /// paths in `schema.rs` still passed, because the other occurrence kept
    /// the file matching. Counting occurrences cannot express "every
    /// transaction"; having one construction site can.
    #[test]
    fn production_code_creates_write_transactions_in_exactly_one_place() {
        let opener = "begin_write()";
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            for (n, line) in production_lines(source) {
                if line.contains(opener) {
                    sites.push(format!("{name}:{}", n + 1));
                }
            }
        }
        assert_eq!(
            sites.len(),
            1,
            "write transactions must be created only by schema::durable_write, \
             so the durability setting has one place to be got wrong; found: {sites:?}"
        );
        assert!(
            sites[0].starts_with("schema.rs"),
            "the single construction site must be the helper: {sites:?}"
        );
    }

    /// And that one site selects immediate durability.
    #[test]
    fn the_single_write_site_selects_immediate_durability() {
        let mut settings = 0;
        for (_, source) in SOURCES {
            for (_, line) in production_lines(source) {
                if line.contains("set_durability") {
                    settings += 1;
                    assert!(
                        line.contains("Durability::Immediate"),
                        "the only durability selection must be Immediate: {line}"
                    );
                }
            }
        }
        assert_eq!(
            settings, 1,
            "exactly one durability selection in production code"
        );
    }

    /// Persisted values are decoded in exactly one module.
    ///
    /// The same shape as the write-transaction guard above, and for the same
    /// reason. The previous attempt matched `try_into` on the constructor's
    /// line; rustfmt puts them on different lines, so mutating
    /// `codec::decode_array32` back to the retryable variant passed both
    /// guards and all 513 tests. A line window cannot express "every decode",
    /// and widening it only moves where it fails.
    ///
    /// One decode site can. `codec` has no way to raise the retryable variant
    /// from a decode, so routing every persisted value through it makes the
    /// classification structural rather than remembered -- which matters most
    /// for Checkpoint C, which adds persisted roots this guard has never seen.
    #[test]
    fn persisted_values_are_decoded_only_in_the_codec_module() {
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            if *name == "codec.rs" {
                continue;
            }
            for (n, line) in production_lines(source) {
                if line.contains(".deserialize(") {
                    sites.push(format!("{name}:{}", n + 1));
                }
            }
        }
        assert!(
            sites.is_empty(),
            "persisted Fory values must be decoded through codec::decode_persisted, \
             which cannot report a retryable failure; found: {sites:?}"
        );
    }

    /// No caller reaches the unbounded decode primitive.
    ///
    /// `codec` exports only the bounded entry point, so this is enforced by
    /// visibility already; the test states it because the failure it guards
    /// against is quiet. `state.redb`'s metadata was copied and decoded with
    /// no ceiling for an entire checkpoint while the log's was bounded, and
    /// nothing failed -- an unbounded path looks exactly like a bounded one
    /// until a corrupt row arrives.
    #[test]
    fn only_the_bounded_decoder_is_reachable() {
        for (name, source) in SOURCES {
            if *name == "codec.rs" {
                continue;
            }
            for (n, line) in production_lines(source) {
                assert!(
                    !line.contains("decode_persisted(")
                        || line.contains("decode_persisted_bounded("),
                    "{name}:{} decodes a persisted value without a ceiling",
                    n + 1
                );
            }
        }
    }

    /// Every encode on the §11.3 apply path is the fatal one.
    ///
    /// `encode_bounded` and `encode_committed` differ only in classification,
    /// and both are correct somewhere: the log store legitimately uses the
    /// retryable one, because an entry OpenRaft hands it to append has not been
    /// committed yet and refusing it writes nothing. Once an entry *is*
    /// committed the group has already decided it applies, so a state machine
    /// that cannot encode the result has no retry to offer and must not report
    /// one — §4.4 forbids continuing with memory ahead of disk.
    ///
    /// Stated as a rule about the two files that own that path rather than as
    /// a count, for the same reason the write-transaction and decode guards
    /// are: a count cannot express "every encode", and a per-line window is
    /// what rustfmt defeated last time. Written alongside the actor rather
    /// than after it, so the first apply-path encode was already covered.
    #[test]
    fn apply_path_encodes_are_fatal_not_retryable() {
        const APPLY_PATH: &[&str] = &["rows.rs", "machine.rs"];
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            if !APPLY_PATH.contains(name) {
                continue;
            }
            for (n, line) in production_lines(source) {
                for retryable in ["encode_bounded(", "encode_owned("] {
                    if line.contains(retryable) {
                        sites.push(format!("{name}:{}", n + 1));
                    }
                }
            }
        }
        assert!(
            sites.is_empty(),
            "the apply path must encode through codec::encode_committed, which \
             classifies a failure as fatal rather than retryable; found: {sites:?}"
        );

        // And it does encode: a guard that passed because nothing on this path
        // encodes at all would say nothing about the classification.
        let committed: usize = SOURCES
            .iter()
            .filter(|(name, _)| APPLY_PATH.contains(name))
            .flat_map(|(_, source)| production_lines(source))
            .filter(|(_, line)| line.contains("encode_committed("))
            .count();
        // Five §11.3 tables carry a Fory root -- resources, sessions, intents,
        // routes and audit -- plus the meta row. `group_admins` and
        // `resource_acl` do not: their values are redb's own `()` and `u64`,
        // so there is nothing to encode and nothing to classify.
        assert_eq!(
            committed, 6,
            "the five Fory-valued §11.3 tables and the meta row each encode once \
             on this path; a different count means a table stopped being written, \
             gained a second write site, or the guard is looking at the wrong files"
        );
    }

    /// And the retryable variant is raised only from the codec module's
    /// encode helper.
    #[test]
    fn the_retryable_variant_is_raised_only_when_encoding() {
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            for (n, line) in production_lines(source) {
                // The constructor's own declaration is not a use of it.
                if line.contains("encode_failed(") && !line.contains("fn encode_failed") {
                    sites.push((*name, n + 1));
                }
            }
        }
        for (name, line) in &sites {
            assert_eq!(
                *name, "codec.rs",
                "{name}:{line} raises the retryable variant outside the encode helper"
            );
        }
        // Not "exactly one": `codec` raises it from both the encode itself and
        // the ceiling check that follows it. The invariant is the *module*,
        // not the count -- everything in `codec` is on the encode side by
        // construction, and nothing outside it can reach the constructor's
        // classification at all.
        assert!(
            !sites.is_empty(),
            "the retryable variant vanished entirely; the encode path should still raise it"
        );
    }

    /// Source lines outside the trailing `#[cfg(test)]` module.
    ///
    /// Tests legitimately open their own write transactions to build corrupt
    /// fixtures the production writer cannot produce, so counting them would
    /// make the guards above unsatisfiable.
    fn production_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
        source[..production_end(source)].lines().enumerate()
    }

    /// Where a file's production code ends: at its **last** `#[cfg(test)]`
    /// module.
    ///
    /// Not the first one. A file may carry a test-only module long before its
    /// test module -- `snapshot.rs` declares its fault-injection seam at line
    /// 365 of nearly two thousand -- and cutting there narrowed every guard
    /// below to the first fifth of that file. A guard that scans less than it
    /// claims to reports clean for the same reason one that scans nothing does.
    ///
    /// Not the *name* `tests` either: this module is `durability_guard`, so
    /// keying on the name would leave `mod.rs` scanning its own guards, whose
    /// bodies contain every literal they search for.
    ///
    /// The cost of taking the last is that an earlier `#[cfg(test)]` module is
    /// scanned as production. That is the safe direction -- more is checked,
    /// not less -- and the seam it admits contains nothing any guard forbids.
    pub(super) fn production_end(source: &str) -> usize {
        source
            .rfind("\n#[cfg(test)]\nmod ")
            .map(|at| at + 1)
            .unwrap_or(source.len())
    }

    /// Every `.rs` file beneath this directory, relative to it.
    ///
    /// The successor to a parser that read `mod` declarations out of the
    /// source. That parser was rewritten four times and was wrong four times --
    /// it missed `pub(self)`, then `pub(in ...)`, then raw and non-ASCII names,
    /// then a trailing block comment -- because each fix answered the last
    /// counterexample rather than the question. Hand-lexing Rust to find out
    /// which files exist is the wrong instrument for a question the filesystem
    /// answers exactly.
    fn sources_on_disk(dir: &std::path::Path, prefix: &str, into: &mut Vec<String>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()));
        for entry in entries {
            let entry = entry.expect("a directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_dir() {
                sources_on_disk(&path, &format!("{prefix}{name}/"), into);
            } else if name.ends_with(".rs") {
                into.push(format!("{prefix}{name}"));
            }
        }
    }
}
