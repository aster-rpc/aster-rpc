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
pub(crate) mod snapshot;

pub(crate) use snapshot::CoordinationSnapshotFile;

use error::StorageResult;
use identity::{OpenPolicy, StorageIdentity};
use log::{CoordinationLogStore, LogShutdown};
use machine::{CoordinationStateMachine, StateShutdown};
use reads::ReadDrain;
use schema::{DirectoryLock, IntegrityOutcome, OpenOutcome, StorageLayout};

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
        ("error.rs", include_str!("error.rs")),
        ("identity.rs", include_str!("identity.rs")),
        ("schema.rs", include_str!("schema.rs")),
        ("log.rs", include_str!("log.rs")),
        ("machine.rs", include_str!("machine.rs")),
        ("reads.rs", include_str!("reads.rs")),
        ("rows.rs", include_str!("rows.rs")),
        ("snapshot.rs", include_str!("snapshot.rs")),
        ("fixture.rs", include_str!("fixture.rs")),
    ];

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
        let end = source.find("\n#[cfg(test)]\n").unwrap_or(source.len());
        source[..end].lines().enumerate()
    }
}
