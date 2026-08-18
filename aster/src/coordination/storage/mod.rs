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
//! | [`codec`] | the single decode site for persisted values |
//! | [`error`] | the named §11.1 failures, kept distinct on purpose |
//! | [`identity`] | the crate-private storage-open identity and policy |
//! | [`schema`] | layout, lifetime lock, table/meta allowlists, the open coordinator |
//! | [`log`] | `RaftLogStorage` over `raft.redb` (Checkpoint B) |
//! | [`machine`] | `RaftStateMachine` over `state.redb` (Checkpoint C) |
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
pub(crate) mod schema;
pub(crate) mod snapshot;

pub(crate) use snapshot::CoordinationSnapshotFile;

use error::StorageResult;
use identity::{OpenPolicy, StorageIdentity};
use log::{ActorShutdown, CoordinationLogStore};
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
    state: redb::Database,
    outcome: OpenOutcome,
    integrity: (IntegrityOutcome, IntegrityOutcome),
    /// Field order below is the shutdown order, and it is load-bearing.
    ///
    /// Rust drops fields in declaration order, so the actor is stopped and
    /// joined -- releasing its `Database` -- before the directory lock goes.
    /// Reversed, a restart would find the lock free but the database still
    /// open, and a second voter could be admitted while the first still holds
    /// the log.
    _shutdown: ActorShutdown,
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

        let (log, shutdown) = CoordinationLogStore::new(parts.raft)?;
        Ok(Self {
            layout: parts.layout,
            identity: parts.identity,
            log,
            state: parts.state,
            outcome: parts.outcome,
            integrity: parts.integrity,
            _shutdown: shutdown,
            _lock: parts.lock,
        })
    }

    pub(crate) fn log(&self) -> &CoordinationLogStore {
        &self.log
    }

    pub(crate) fn log_mut(&mut self) -> &mut CoordinationLogStore {
        &mut self.log
    }

    pub(crate) fn state(&self) -> &redb::Database {
        &self.state
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
        ("codec.rs", include_str!("codec.rs")),
        ("error.rs", include_str!("error.rs")),
        ("identity.rs", include_str!("identity.rs")),
        ("schema.rs", include_str!("schema.rs")),
        ("log.rs", include_str!("log.rs")),
        ("machine.rs", include_str!("machine.rs")),
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
