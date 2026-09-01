//! **Measured cost of the §13 recovery import and the §11.4 snapshot** (S5-E).
//!
//! ## What is measured
//!
//! Two operations, and the second was added because the first checkpoint
//! reported the snapshot only as part of an operator-visible catch-up and could
//! not say the install path had been taken at all.
//!
//! ### The §11.4 snapshot, built and installed
//!
//! [`the_snapshot_build_and_install_cost_what_they_cost`] drives the production
//! `RaftSnapshotBuilder::build_snapshot` and the production
//! `prepare_install` → `RaftStateMachine::install_snapshot` pair over every
//! §12.1 shape below.
//!
//! **The install here cannot be satisfied by log replay, because there is no
//! log.** The receiver is a state machine, not a Raft; nothing replays into it.
//! Each run asserts the receiver's `last_applied` moved from *behind* the
//! snapshot to *exactly* the snapshot's `last_log_id`, which is the state
//! machine reporting that it adopted the body. An implementation that dropped
//! the body on the floor would leave the receiver where it started and fail
//! here.
//!
//! It is also the `Replace` verdict specifically — the expensive one, where the
//! database file is closed, renamed over, reopened and reloaded — and not the
//! `Repair` verdict, which only rewrites a marker. That is arranged by
//! [`AHEAD_BY`] and asserted by the applied-index move: a `Repair` cannot move
//! `last_applied`, so a fixture that accidentally produced one would fail the
//! same assertion.
//!
//! **What it excludes, and where that is measured instead.** The receiver is
//! handed the sender's published body directly, so the QUIC transfer and
//! OpenRaft's own decision to send a snapshot are not in these numbers. Those
//! are measured whole, over the real network, by
//! `tests/coordination_catchup.rs`, which strands a follower behind the
//! leader's purge point so replay is impossible and reports the whole
//! catch-up. The two together are the operation; neither alone is.
//!
//! ### The whole production import
//!
//! A fresh directory, a sealed capability, and
//! `VoterStorage::open_with_recovery` — which binds the store, reads the export
//! through one pinned descriptor, hashes it against the genesis binding,
//! decodes it canonically, transforms it, proves the result loadable, and
//! commits it in one `Durability::Immediate` transaction. Then the store is
//! reopened and loaded, because a recovery that could not be reopened would not
//! be a recovery.
//!
//! An earlier version of this module timed `read_body`, `transform` and
//! `validate_before_commit` in isolation and described the result as a whole
//! import through the durable path. It was not: none of those three functions
//! writes anything, so the durability claim was false and the totals omitted
//! the read, the hash, the commit and the reopen. The component measurements
//! are kept — they are how a regression is localised — but they are labelled as
//! components and the threshold comes from the whole operation.
//!
//! ## The first sample is reported separately, and it is not cold
//!
//! A recovery import runs **once in a group's lifetime**, on a process that has
//! just started, against a file no cache has seen. Neither condition holds
//! here: every shape runs a probe import first, and the export was written
//! moments earlier, so the file is in the page cache and the process is warm.
//! The first sample is therefore labelled `first`, not `cold`, because calling
//! it cold would claim a measurement nothing here takes. **Process-cold and
//! page-cache-cold remain unmeasured**, and a genuine cold number needs a fresh
//! subprocess against a file evicted from cache.
//!
//! ## Not a CI gate
//!
//! `#[ignore]`d and run on demand. A millisecond assertion in CI would either
//! be loose enough to catch nothing or tight enough to fail on a busy runner.
//! What these produce is the distribution that a threshold is chosen *from*.
//!
//! Nothing here is measured with a bound relaxed: every shape is a legal §12.1
//! shape and every check is enabled. That is a statement about the fixtures,
//! not about the timing harness, and it is now true of the code being timed.
//!
//! Run with:
//!
//! ```text
//! cargo test -p aster --lib --release --no-default-features \
//!     --features coordination -- --ignored --nocapture --test-threads=1 \
//!     coordination::storage::bench
//! ```

#![cfg(test)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use super::fixture::{encode_recovery_export, recovery_export_parts};
use super::scratch::ScratchDir;
use crate::coordination::model::limits;

/// Warm samples per shape.
///
/// Twenty is the floor at which a nearest-rank p95 is a percentile rather than
/// a synonym for the maximum: below it the 95th-percentile rank lands on the
/// last sample. The largest shapes cost seconds per run, so they use
/// [`HEAVY_RUNS`] and their spread is reported as a range, not as a p95 that
/// would be the maximum wearing a percentile's name.
const RUNS: usize = 20;

/// Warm samples for shapes whose single run is measured in seconds.
const HEAVY_RUNS: usize = 5;

struct Measured {
    what: &'static str,
    shape: String,
    cold: Duration,
    samples: Vec<Duration>,
}

impl Measured {
    fn percentile(&self, p: f64) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
        sorted[rank.min(sorted.len()) - 1]
    }

    fn report(&self) {
        let spread = if self.samples.len() >= RUNS {
            format!("p95={:>10.3?}", self.percentile(95.0))
        } else {
            // Honest: with n < 20 the nearest-rank p95 *is* the maximum, so it
            // is reported as one.
            format!("max={:>10.3?}", self.percentile(100.0))
        };
        println!(
            "  {:<30} {:<26} first={:>10.3?}  n={:<3} p50={:>10.3?} {spread}",
            self.what,
            self.shape,
            self.cold,
            self.samples.len(),
            self.percentile(50.0),
        );
    }
}

/// Time only the production of a value; drop it after the clock stops.
///
/// The plain [`measure`] times the whole closure, which for anything returning
/// a `VoterStorage` includes the drop — stopping both actors and releasing the
/// directory lock. That is ordered shutdown, a separate operation with its own
/// runbook step, and folding it into an open made the open look more expensive
/// than it is.
fn measure_open<T>(
    what: &'static str,
    shape: impl Into<String>,
    runs: usize,
    mut body: impl FnMut() -> T,
) -> Measured {
    let at = Instant::now();
    let first = body();
    let cold = at.elapsed();
    drop(first);

    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let at = Instant::now();
        let produced = black_box(body());
        samples.push(at.elapsed());
        drop(produced);
    }
    let measured = Measured {
        what,
        shape: shape.into(),
        cold,
        samples,
    };
    measured.report();
    measured
}

/// Time an explicitly delimited inner region, so setup a run genuinely needs
/// (a fresh directory, a fresh database, a cloned input) is charged to the
/// harness and not to the operation.
///
/// The closure returns the elapsed time of the region it chose to time. That is
/// weaker than `measure`, which cannot be lied to — so it is used only where the
/// per-run setup is unavoidable, and every use says what it excludes.
fn measure_region(
    what: &'static str,
    shape: impl Into<String>,
    runs: usize,
    mut body: impl FnMut() -> Duration,
) -> Measured {
    let cold = body();
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        samples.push(body());
    }
    let measured = Measured {
        what,
        shape: shape.into(),
        cold,
        samples,
    };
    measured.report();
    measured
}

/// Time `body` once, then `runs` times.
fn measure(
    what: &'static str,
    shape: impl Into<String>,
    runs: usize,
    mut body: impl FnMut(),
) -> Measured {
    let at = Instant::now();
    body();
    let cold = at.elapsed();

    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let at = Instant::now();
        body();
        samples.push(at.elapsed());
    }
    let measured = Measured {
        what,
        shape: shape.into(),
        cold,
        samples,
    };
    measured.report();
    measured
}

// ── Shapes ───────────────────────────────────────────────────────────────────

/// One legal §12.1 shape, described by what it maximises.
struct Shape {
    label: &'static str,
    resources: u64,
    /// Grants per resource. §12.1 caps this at 256 and the group ACL table at
    /// 100,000 rows, so `resources * grants_each` must stay inside both.
    grants_each: u64,
    value_bytes: usize,
    admins: u64,
    runs: usize,
}

/// The shapes worth measuring, and why each exists.
///
/// "Maximum legal shape" is not one point. The §12.1 caps are independent — row
/// counts, the 192 MiB growth budget, the per-resource grant cap and the
/// administrator cap can each be approached while the others stay small — and a
/// fixture that maximises only the resource count is not the worst case for the
/// per-resource grant pass or for the byte budget. Each row below maximises a
/// different axis.
fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            // Not "default": nothing declares a default group size. This is a
            // small representative group, and it is here to expose fixed costs.
            label: "small representative",
            resources: 2,
            grants_each: 2,
            value_bytes: 64,
            admins: 1,
            runs: RUNS,
        },
        Shape {
            label: "mid (10k res + 10k grants)",
            resources: 10_000,
            grants_each: 1,
            value_bytes: 64,
            admins: 1,
            runs: RUNS,
        },
        Shape {
            // Maximises the resource and ACL row counts.
            label: "max rows (100k res + 100k grants)",
            resources: limits::MAX_RESOURCES_PER_GROUP,
            grants_each: 1,
            value_bytes: 64,
            admins: 1,
            runs: HEAVY_RUNS,
        },
        Shape {
            // Maximises bytes rather than rows: ~2 KiB values approaching the
            // 192 MiB growth budget, which is the bound `transform` checks and
            // the one a row-count fixture never exercises.
            //
            // The count is derived from a measurement, not guessed. 90,000
            // resources at this value size charged 209,340,512 bytes against a
            // 201,326,592 budget — the import refused it, correctly, and the
            // fixture was illegal rather than maximal. At ~2,326 charged bytes
            // per resource, 82,000 sits at ~95% of budget: the largest legal
            // shape on this axis rather than one just past it.
            label: "value-heavy (82k res, 2KiB)",
            resources: 82_000,
            grants_each: 0,
            value_bytes: 2_048,
            admins: 1,
            runs: HEAVY_RUNS,
        },
        Shape {
            // The controlled pair for "does validation cost track rows or
            // bytes?". Identical to `paired B` in every dimension — same
            // resources, same grants, same administrators — differing only in
            // value size. The withdrawn comparison varied row count, row type
            // and value size at once, so it could attribute the difference to
            // none of them.
            label: "paired A (10k res, 64B values)",
            resources: 10_000,
            grants_each: 1,
            value_bytes: 64,
            admins: 1,
            runs: RUNS,
        },
        Shape {
            label: "paired B (10k res, 4KiB values)",
            resources: 10_000,
            grants_each: 1,
            value_bytes: 4_096,
            admins: 1,
            runs: RUNS,
        },
        Shape {
            // Maximises grant *concentration* and the administrator set: 390
            // resources at the 256-grant per-resource cap is 99,840 ACL rows,
            // which is where the ordered per-resource pass earns its keep. A
            // scan-per-resource implementation is quadratic here and nowhere
            // else in this list.
            //
            // Sixty-one exported administrators, not sixty-four. §12.1's cap is
            // on the **union** of the exported set and the new genesis's
            // `initial_admins`, and this fixture's genesis contributes three —
            // so a fixture carrying the full sixty-four is one over the cap and
            // is refused, which is what the first version of this shape did.
            // The maximal legal value on this axis is the cap minus whatever
            // the genesis brings.
            label: "grant-concentrated + max admins",
            resources: 390,
            grants_each: limits::MAX_GRANTS_PER_RESOURCE,
            value_bytes: 64,
            admins: limits::MAX_ADMINS_PER_GROUP - 3,
            runs: RUNS,
        },
    ]
}

fn export_for(shape: &Shape) -> Vec<u8> {
    use crate::coordination::contract::ResourceKeyV1;
    use crate::coordination::rowstream::{
        AdminRowV1, GrantRowV1, RecoveryBodyRowV1, RecoveryResourceRowV1,
    };

    let (mut header, _) = recovery_export_parts();
    let key = |n: u64| ResourceKeyV1 {
        namespace: "portal.roaming".into(),
        key: n.to_be_bytes().to_vec(),
    };

    let mut rows = Vec::new();
    for n in 0..shape.resources {
        rows.push(RecoveryBodyRowV1::Resource(RecoveryResourceRowV1 {
            key: key(n),
            source_epoch: 9,
            ttl_ms: 30_000,
            lease_revision: 10,
            value_revision: 7,
            value: vec![0x5A; shape.value_bytes],
            acl_revision: 2,
        }));
    }
    for n in 0..shape.resources {
        for g in 0..shape.grants_each {
            let mut subject = [0u8; 32];
            subject[..8].copy_from_slice(&g.to_be_bytes());
            subject[8] = 0x21;
            rows.push(RecoveryBodyRowV1::Grant(GrantRowV1 {
                key: key(n),
                subject_node_id: subject.to_vec(),
                rights: 0x3f,
            }));
        }
    }
    for a in 0..shape.admins {
        let mut subject = [0x31u8; 32];
        subject[..8].copy_from_slice(&a.to_be_bytes());
        rows.push(RecoveryBodyRowV1::Admin(AdminRowV1 {
            subject_node_id: subject.to_vec(),
        }));
    }

    header.row_counts.resources = shape.resources;
    header.row_counts.grants = shape.resources * shape.grants_each;
    header.row_counts.admins = shape.admins;
    encode_recovery_export(&header, &rows)
}

/// The node the fixture imports as — one of the genesis's own initial voters,
/// which `RecoveryImport::seal` requires.
const LOCAL: [u8; 32] = [1u8; 32];

/// A capability sealed the only way production can seal one.
fn capability(
    path: std::path::PathBuf,
    export_hash: [u8; 32],
) -> (
    super::import::RecoveryImport,
    super::identity::StorageIdentity,
) {
    use crate::coordination::model::authority::{testing as auth, verify_genesis};

    let voters: Vec<[u8; 32]> = vec![LOCAL, [2u8; 32], [3u8; 32]];
    let mut genesis = auth::genesis_for(&voters, crate::coordination::model::shape::PROFILE_PROD);
    genesis.recovery_export_hash = Some(export_hash.to_vec());
    let signed = auth::sign_genesis(&genesis);
    let authority = auth::authority();
    let verified =
        verify_genesis(&authority.public_key, &authority.domain, &signed).expect("verifies");
    let identity = super::identity::StorageIdentity::new(
        crate::coordination::ids::CoordinationGroupId::from_array(*verified.group_id()),
        *verified.genesis_hash(),
        authority.public_key,
        verified.authority_domain(),
        crate::coordination::ids::CoordinationNodeId::from_array(LOCAL),
        verified.deployment_profile_code(),
    );
    let sealed = super::import::RecoveryImport::seal(&identity, &verified, path).expect("seals");
    (sealed, identity)
}

/// The canonical reader's visitor, collecting one decoded body.
#[derive(Default)]
struct Collect {
    header: Option<crate::coordination::rowstream::RecoveryHeaderV1>,
    rows: Vec<crate::coordination::rowstream::RecoveryBodyRowV1>,
}

impl crate::coordination::rowstream::BodyVisitor<crate::coordination::rowstream::RecoveryBodyRowV1>
    for Collect
{
    fn header(
        &mut self,
        header: &crate::coordination::rowstream::RecoveryHeaderV1,
    ) -> Result<(), crate::coordination::rowstream::BodyError> {
        self.header = Some(header.clone());
        Ok(())
    }
    fn row(
        &mut self,
        _section: u64,
        row: crate::coordination::rowstream::RecoveryBodyRowV1,
    ) -> Result<(), crate::coordination::rowstream::BodyError> {
        self.rows.push(row);
        Ok(())
    }
}

/// Decode one shape's export and run the production `transform` over it,
/// yielding the exact `DurableState` the import would commit.
fn transformed(shape: &Shape) -> (Vec<u8>, [u8; 32], super::rows::DurableState) {
    use crate::coordination::rowstream::{read_body, RecoveryBodyRowV1};

    let bytes = export_for(shape);
    let hash = *blake3::hash(&bytes).as_bytes();
    let mut collected = Collect::default();
    read_body::<_, RecoveryBodyRowV1>(&mut &bytes[..], &mut collected).expect("canonical");
    let header = collected.header.clone().expect("a header");
    let (_, identity) = capability("/unused".into(), hash);
    let state =
        super::import::transform(&header, &collected.rows, &identity, &[[0x41u8; 32]], &hash)
            .expect("the transform succeeds");
    (bytes, hash, state)
}

/// **Where a persisted row's cost actually goes.**
///
/// `rows::commit` and `rows::load` each do two things per row: run the storage
/// codec, and touch redb. Separating those two answers a question the sweep
/// below cannot: whether the cost that grows with scale is the B-tree or the
/// encoder.
///
/// Three measurements, all on the same `ResourceRowV1`:
///
/// * building one `payload_fory::<ResourceRowV1>()` runtime — a fresh
///   `PayloadRegistry`, a `register_by_name`, and the type meta that comes with
///   it;
/// * `encode_committed` / `decode_persisted_bounded`, the production calls,
///   each of which builds that runtime again;
/// * the same encode and decode against a runtime built **once**.
///
/// And then redb alone, at four scales, with the codec removed entirely:
/// pre-encoded bytes inserted into one table in ascending key order inside one
/// `Durability::Immediate` transaction. A flat per-row column there puts the
/// scale-dependence in the caller; a rising one puts it in the B-tree.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn where_the_per_row_cost_goes() {
    use crate::coordination::contract::payload_fory;
    use crate::coordination::model::state::ResourceRow;
    use crate::coordination::raft::ResourceRowV1;
    use crate::coordination::storage::codec::{decode_persisted_bounded, encode_committed};
    use redb::{ReadableDatabase, ReadableTable};

    println!(
        "\n§13 import — per-row cost attribution\n{}",
        "-".repeat(112)
    );

    let row = ResourceRowV1::from(&ResourceRow {
        epoch: 9,
        held: false,
        holder: None,
        ttl_ms: 30_000,
        lease_revision: 10,
        value_revision: 7,
        value: vec![0x5A; 64],
        acl_revision: 2,
    });
    let encoded = encode_committed(
        super::error::Db::State,
        "ResourceRowV1",
        &row,
        super::rows::MAX_RESOURCE_ROW_BYTES,
    )
    .expect("encodes");
    println!(
        "\none ResourceRowV1 with a 64-byte value encodes to {} B",
        encoded.len()
    );

    measure("payload_fory::<Row>() alone", "one runtime", 200, || {
        black_box(payload_fory::<ResourceRowV1>());
    });
    measure("encode_committed (production)", "one row", 200, || {
        black_box(
            encode_committed(
                super::error::Db::State,
                "ResourceRowV1",
                &row,
                super::rows::MAX_RESOURCE_ROW_BYTES,
            )
            .expect("encodes"),
        );
    });
    measure("decode_persisted_bounded (prod)", "one row", 200, || {
        black_box(
            decode_persisted_bounded::<ResourceRowV1>(
                super::error::Db::State,
                "ResourceRowV1",
                &encoded,
                super::rows::MAX_RESOURCE_ROW_BYTES,
            )
            .expect("decodes"),
        );
    });

    let reused = payload_fory::<ResourceRowV1>();
    measure("serialize (runtime reused)", "one row", 200, || {
        black_box(reused.serialize(&row).expect("encodes"));
    });
    measure("deserialize (runtime reused)", "one row", 200, || {
        black_box(
            reused
                .deserialize::<ResourceRowV1>(&encoded)
                .expect("decodes"),
        );
    });

    // redb alone: the same row bytes, the same ascending key order, and the
    // subsystem's **one** durable write site — `schema::durable_write` /
    // `durable_commit`, `Durability::Immediate` and all — with no codec
    // anywhere in the timed region. Reaching for `begin_write` here would
    // measure a transaction production never opens, and the storage guards
    // refuse it for exactly that reason.
    const PROBE: redb::TableDefinition<(&str, &[u8]), &[u8]> =
        redb::TableDefinition::new("bench_probe");
    println!("\nredb alone — pre-encoded bytes, one transaction, Durability::Immediate");
    for rows in [25_000usize, 50_000, 100_000, 200_000] {
        let keys: Vec<Vec<u8>> = (0..rows as u64).map(|n| n.to_be_bytes().to_vec()).collect();
        let runs = if rows >= 100_000 { HEAVY_RUNS } else { RUNS };
        let label = format!("{rows} rows");

        let insert = measure_region("redb insert", label.clone(), runs, || {
            let dir = ScratchDir::new("bench-redb");
            let database = redb::Database::create(dir.join("probe.redb")).expect("a database");
            let at = Instant::now();
            let write = super::schema::durable_write(&database, super::error::Db::State)
                .expect("a durable write transaction");
            {
                let mut table = write.open_table(PROBE).expect("the table");
                for key in &keys {
                    table
                        .insert(("portal.roaming", key.as_slice()), encoded.as_slice())
                        .expect("inserts");
                }
            }
            super::schema::durable_commit(write, super::error::Db::State).expect("commits");
            at.elapsed()
        });

        let home = ScratchDir::new("bench-redb-read");
        let database = redb::Database::create(home.join("probe.redb")).expect("a database");
        {
            let write = super::schema::durable_write(&database, super::error::Db::State)
                .expect("a durable write transaction");
            {
                let mut table = write.open_table(PROBE).expect("the table");
                for key in &keys {
                    table
                        .insert(("portal.roaming", key.as_slice()), encoded.as_slice())
                        .expect("inserts");
                }
            }
            super::schema::durable_commit(write, super::error::Db::State).expect("commits");
        }
        let scan = measure("redb scan", label.clone(), runs.min(RUNS), || {
            let read = database.begin_read().expect("a read transaction");
            let table = read.open_table(PROBE).expect("the table");
            let mut total = 0usize;
            for entry in table.iter().expect("iterates") {
                let (_, value) = entry.expect("an entry");
                total += value.value().len();
            }
            black_box(total);
        });

        println!(
            "  {:<30} {:<26} insert={:>8.3?}/row   scan={:>8.3?}/row",
            "per-row (p50)",
            label,
            insert.percentile(50.0) / rows as u32,
            scan.percentile(50.0) / rows as u32,
        );
    }
}

/// **The two halves of the import's storage cost, measured apart.**
///
/// The withdrawn measurement reported one number for `open_with_recovery` and
/// observed that ten times the row scale cost roughly twenty-nine times the
/// latency. One number cannot say whether that is the durable write, the
/// read-back, or both, so no threshold could be drawn from it and none was.
///
/// This sweeps one axis — resource count, at a fixed 64-byte value and one
/// grant each — across four scales and times `rows::commit` and `rows::load`
/// separately at each, alongside the two pure stages. The per-row column is the
/// point: a constant column is linear scaling, and a rising one localises the
/// super-linearity to the stage whose column rises.
///
/// Per-run setup is real here (`commit` needs a database that has never been
/// committed into, and consumes the state by value), so the timed region is
/// delimited explicitly and excludes the scratch directory, the
/// `Database::create` and the `DurableState::clone`.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn the_commit_and_the_read_back_scale_apart() {
    println!(
        "\n§13 import — rows::commit and rows::load, apart\n{}",
        "-".repeat(112)
    );
    for resources in [12_500u64, 25_000, 50_000, 100_000] {
        let shape = Shape {
            label: "scaling sweep",
            resources,
            grants_each: 1,
            value_bytes: 64,
            admins: 1,
            runs: if resources >= 50_000 {
                HEAVY_RUNS
            } else {
                RUNS
            },
        };
        let (bytes, _, state) = transformed(&shape);
        let changes = super::import::every_key(&state);
        // Rows actually written: resources + grants + administrators. The
        // per-row column divides by this, not by the resource count, because
        // "10,000 rows" meaning 20,001 is the mislabelling that made the first
        // scaling claim unreadable.
        let rows = state.model.resources.len() + state.model.acl.len() + state.model.admins.len();
        println!(
            "\n{rows} rows ({resources} resources + {resources} grants + 1 admin), body {} B",
            bytes.len()
        );

        let label = format!("{rows} rows");
        let commit = measure_region("rows::commit", label.clone(), shape.runs, || {
            let dir = ScratchDir::new("bench-commit");
            let database = redb::Database::create(dir.join("state.redb")).expect("a database");
            let cloned = state.clone();
            let at = Instant::now();
            let committed = super::rows::commit(&database, cloned, &changes).expect("commits");
            let elapsed = at.elapsed();
            black_box(committed);
            elapsed
        });

        let committed_home = ScratchDir::new("bench-committed");
        let database =
            redb::Database::create(committed_home.join("state.redb")).expect("a database");
        super::rows::commit(&database, state.clone(), &changes).expect("commits");
        let load = measure("rows::load", label.clone(), shape.runs.min(RUNS), || {
            black_box(super::rows::load(&database).expect("loads"));
        });

        println!(
            "  {:<30} {:<26} commit={:>8.3?}/row   load={:>8.3?}/row",
            "per-row (p50)",
            label,
            commit.percentile(50.0) / rows as u32,
            load.percentile(50.0) / rows as u32,
        );
    }
}

/// **The whole production import, end to end, at every legal shape.**
///
/// Each run is a fresh directory: bind, read through the pinned descriptor,
/// hash against the genesis binding, decode canonically, transform, prove
/// loadable, commit durably — then drop the store, reopen it, and load, because
/// an import that could not be reopened has not recovered anything.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn the_whole_recovery_import_costs_what_it_costs() {
    println!(
        "\n§13 recovery import — whole production path\n{}",
        "-".repeat(112)
    );
    for shape in shapes() {
        let bytes = export_for(&shape);
        let hash = *blake3::hash(&bytes).as_bytes();

        // What this shape actually charges, so the label is checkable rather
        // than asserted: a "value-heavy" fixture that charged nothing would be
        // mislabelled and the reader could not tell.
        let charged = {
            let dir = ScratchDir::new("bench-probe");
            let path = dir.join("export.recovery");
            std::fs::write(&path, &bytes).unwrap();
            let (sealed, _) = capability(path, hash);
            let store = super::VoterStorage::open_with_recovery(
                super::schema::StorageLayout::new(dir.to_path_buf()),
                super::identity::OpenPolicy::production(),
                &sealed,
            )
            .expect("the probe import succeeds");
            drop(store);
            let database = redb::Database::create(dir.join("state.redb")).unwrap();
            let state = super::rows::load(&database).expect("loads");
            (
                state.model.growth_bytes,
                state.model.resources.len(),
                state.model.acl.len(),
                state.model.admins.len(),
            )
        };
        println!(
            "\n{} — body {} B, growth {} B ({:.1}% of budget), {} resources / {} grants / {} admins",
            shape.label,
            bytes.len(),
            charged.0,
            100.0 * charged.0 as f64 / limits::GROWTH_BUDGET as f64,
            charged.1,
            charged.2,
            charged.3,
        );

        // Setup, hoisted out of every timed region. Writing a 43 MB export and
        // minting a signed genesis are the harness's costs, not the import's,
        // and leaving them inside the closure measured the fixture.
        let export_home = ScratchDir::new("bench-export");
        let export_path = export_home.join("export.recovery");
        std::fs::write(&export_path, &bytes).expect("stage the export");
        let (sealed, identity) = capability(export_path.clone(), hash);

        // `measure_open`, so the clock stops when the store exists. Dropping a
        // `VoterStorage` stops both actors and releases the directory lock —
        // ordered shutdown, which is its own runbook step and its own
        // measurement, not part of an open.
        //
        // The `ScratchDir` is created inside the closure and so is charged
        // here; it is a `create_dir_all`, and the alternative — reusing one
        // directory — would measure an import into a store that already exists,
        // which the `Fresh` guard refuses.
        measure_open("storage open: import", shape.label, shape.runs, || {
            let dir = ScratchDir::new("bench-import");
            let store = super::VoterStorage::open_with_recovery(
                super::schema::StorageLayout::new(dir.to_path_buf()),
                super::identity::OpenPolicy::production(),
                &sealed,
            )
            .expect("the import succeeds");
            (dir, store)
        });

        // The reopen alone. The directory is imported once, outside every
        // timed region, and then reopened repeatedly — so this is what a
        // restart of an already-recovered voter costs, with no import in it.
        // The earlier version timed an import *and* a reopen and labelled the
        // result "reopen", which made the reopen look like the more expensive
        // of the two when it contained the other.
        let recovered = ScratchDir::new("bench-recovered");
        drop(
            super::VoterStorage::open_with_recovery(
                super::schema::StorageLayout::new(recovered.to_path_buf()),
                super::identity::OpenPolicy::production(),
                &sealed,
            )
            .expect("the import succeeds"),
        );
        measure_open(
            "storage open: reopen",
            shape.label,
            shape.runs.min(RUNS),
            || {
                super::VoterStorage::open(
                    super::schema::StorageLayout::new(recovered.to_path_buf()),
                    identity.clone(),
                    super::identity::OpenPolicy::production(),
                )
                .expect("the imported store reopens")
            },
        );

        // The two halves the super-linear question turns on, measured apart.
        // Ten times the row scale produced roughly twenty-nine times the open
        // latency, and nothing so far says whether that is the durable write,
        // the read-back, or both — so neither is accepted as unavoidable until
        // they are separated.
        let database =
            redb::Database::create(recovered.join("state.redb")).expect("open the state database");
        measure(
            "rows::load (read-back only)",
            shape.label,
            shape.runs.min(RUNS),
            || {
                black_box(super::rows::load(&database).expect("the imported store loads"));
            },
        );
    }
}

// ── Process-cold ────────────────────────────────────────────────────────────

/// The export the child imports, and the hash its genesis must bind. Passed as
/// environment rather than recomputed, so the child's **first** read of the
/// file is inside the timed region.
const COLD_EXPORT_VAR: &str = "ASTER_S5E_COLD_EXPORT";
const COLD_HASH_VAR: &str = "ASTER_S5E_COLD_HASH";
/// The line the child prints, and the parent parses.
const COLD_MARKER: &str = "S5E-COLD-IMPORT-NANOS";

/// The child half of the process-cold measurement.
///
/// A no-op unless the parent names an export, so the ordinary suite pays one
/// skipped test. When it is armed it seals one capability, opens one fresh
/// directory through `open_with_recovery`, and prints the elapsed nanoseconds
/// of that call alone.
///
/// **This is process-cold and nothing more.** The process is new — no warmed
/// allocator, no touched pages, no constructed payload runtimes — but the file
/// was written by the parent moments earlier and is in the page cache.
/// Evicting it needs privileges this suite does not take, so page-cache-cold
/// stays unmeasured and is not claimed.
#[test]
fn process_cold_import_child() {
    let Ok(export) = std::env::var(COLD_EXPORT_VAR) else {
        return;
    };
    let hex = std::env::var(COLD_HASH_VAR).expect("the parent named the export hash");
    let mut hash = [0u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("hex");
    }
    let dir = ScratchDir::new("cold-import");
    let (sealed, _) = capability(std::path::PathBuf::from(&export), hash);

    let at = Instant::now();
    let store = super::VoterStorage::open_with_recovery(
        super::schema::StorageLayout::new(dir.to_path_buf()),
        super::identity::OpenPolicy::production(),
        &sealed,
    )
    .expect("the cold import succeeds");
    let elapsed = at.elapsed();
    println!("{COLD_MARKER} {}", elapsed.as_nanos());
    drop(store);
}

/// **The §13 import from a genuinely cold process**, at every legal shape.
///
/// Each sample is a fresh subprocess doing exactly one import. That is what
/// makes the label honest: the withdrawn measurement called its first
/// in-process sample "cold" when the process had already run a complete probe
/// import, so every allocator arena, every page and every payload runtime was
/// warm. Here nothing is.
///
/// Page-cache-cold is a different claim and is still not made: the export was
/// written by this parent, and evicting it needs privileges the suite does not
/// take.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn the_import_from_a_cold_process_costs_what_it_costs() {
    /// Fresh processes per shape. Each is a full test-binary start, so this is
    /// reported as a range over a handful of samples rather than as a
    /// percentile it cannot support.
    const CHILDREN: usize = 5;

    println!(
        "\n§13 recovery import — process-cold (fresh subprocess per sample)\n{}",
        "-".repeat(112)
    );
    let exe = std::env::current_exe().expect("the test binary");
    for shape in shapes() {
        let bytes = export_for(&shape);
        let hash = *blake3::hash(&bytes).as_bytes();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let home = ScratchDir::new("cold-export");
        let path = home.join("export.recovery");
        std::fs::write(&path, &bytes).expect("stage the export");

        let mut samples: Vec<Duration> = Vec::with_capacity(CHILDREN);
        for _ in 0..CHILDREN {
            let output = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "coordination::storage::bench::process_cold_import_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(COLD_EXPORT_VAR, &path)
                .env(COLD_HASH_VAR, &hex)
                .output()
                .expect("spawn the cold child");
            assert!(
                output.status.success(),
                "the cold child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Found as a substring, not as a line prefix: under `--nocapture`
            // the child's harness prints the test's name and the test's own
            // output on one line, so the marker is mid-line.
            let text = String::from_utf8_lossy(&output.stdout);
            let nanos: u64 = text
                .split(COLD_MARKER)
                .nth(1)
                .unwrap_or_else(|| panic!("the child printed no {COLD_MARKER}: {text}"))
                .split_whitespace()
                .next()
                .unwrap_or_else(|| panic!("the {COLD_MARKER} carried no value: {text}"))
                .parse()
                .expect("nanoseconds");
            samples.push(Duration::from_nanos(nanos));
        }
        samples.sort_unstable();
        println!(
            "  {:<30} {:<26} n={:<3} min={:>10.3?} p50={:>10.3?} max={:>10.3?}",
            "storage open: import (cold)",
            shape.label,
            samples.len(),
            samples[0],
            samples[samples.len() / 2],
            samples[samples.len() - 1],
        );
    }
}

/// **The import's components**, for localising a regression the whole-path
/// number only reveals the existence of.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn the_import_components_cost_what_they_cost() {
    use crate::coordination::rowstream::{read_body, RecoveryBodyRowV1};

    println!("\n§13 import components\n{}", "-".repeat(112));
    for shape in shapes() {
        let bytes = export_for(&shape);
        let hash = *blake3::hash(&bytes).as_bytes();
        println!("\n{} — body {} B", shape.label, bytes.len());

        measure("blake3 over the body", shape.label, shape.runs, || {
            black_box(blake3::hash(&bytes));
        });

        let mut decoded = Collect::default();
        measure(
            "read_body (decode+canonical)",
            shape.label,
            shape.runs,
            || {
                let mut collected = Collect::default();
                read_body::<_, RecoveryBodyRowV1>(&mut &bytes[..], &mut collected)
                    .expect("canonical");
                decoded = std::mem::take(&mut collected);
            },
        );
        black_box(&decoded.rows);

        let header = decoded.header.clone().expect("a header");
        let (_, identity) = capability("/unused".into(), hash);
        let mut built = None;
        measure(
            "transform (validate+build)",
            shape.label,
            shape.runs,
            || {
                built = Some(
                    super::import::transform(
                        &header,
                        &decoded.rows,
                        &identity,
                        &[[0x41u8; 32]],
                        &hash,
                    )
                    .expect("the transform succeeds"),
                );
                black_box(&built);
            },
        );

        let state = built.expect("a transformed state");
        measure("validate_before_commit", shape.label, shape.runs, || {
            super::rows::validate_before_commit(black_box(&state)).expect("loadable");
        });
    }
}

// ── The §11.4 snapshot: build and install ───────────────────────────────────

/// The leader a benchmark's blank entries are attributed to.
///
/// Any committed leader id works — the entries carry no command and the state
/// machine does not read it — so this is only here to keep the value out of the
/// call sites.
const BENCH_LEADER: [u8; 32] = [0x11; 32];

/// How far the sender is advanced past the receiver before the snapshot is
/// built.
///
/// The receiver is recovered from the **same** export as the sender, so the two
/// start at the same `last_applied`. An install between equals classifies
/// `Repair` — a marker rewrite, not a state replacement, and not the operation
/// this is here to price. Four blank entries put the sender ahead, which makes
/// the candidate strictly newer and the verdict `Replace`.
///
/// Blank entries rather than commands: they advance the applied index without
/// changing a single row, so the body the sender streams is the shape's body
/// and nothing else. A fixture that advanced by writing rows would be measuring
/// a shape it had quietly altered.
const AHEAD_BY: u64 = 4;

/// The async twin of [`measure_region`], with the same contract.
///
/// The snapshot path is `async` all the way down — the builder goes through the
/// state actor and the install runs *on* it — so the sync helper cannot time it
/// without a nested runtime. The reporting is identical, which is the point:
/// `n < 20` still prints `max` rather than a p95 that would be the maximum
/// wearing a percentile's name.
async fn measure_region_async<F, Fut>(
    what: &'static str,
    shape: impl Into<String>,
    runs: usize,
    mut body: F,
) -> Measured
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Duration>,
{
    let cold = body().await;
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        samples.push(body().await);
    }
    let measured = Measured {
        what,
        shape: shape.into(),
        cold,
        samples,
    };
    measured.report();
    measured
}

/// One state machine over a directory recovered from `sealed`, and the guards
/// that keep it alive.
///
/// The recovery import runs first and is dropped: `open_with_recovery` holds
/// the directory and both actors, and the state machine has to own the database
/// on its own. All of this is setup — no timed region contains it.
fn recovered_machine(
    family: &str,
    sealed: &super::import::RecoveryImport,
) -> (
    ScratchDir,
    super::machine::CoordinationStateMachine,
    super::machine::StateShutdown,
    super::reads::ReadDrain,
) {
    let dir = ScratchDir::new(family);
    drop(
        super::VoterStorage::open_with_recovery(
            super::schema::StorageLayout::new(dir.to_path_buf()),
            super::identity::OpenPolicy::production(),
            sealed,
        )
        .expect("the import succeeds"),
    );
    let database =
        redb::Database::create(dir.join("state.redb")).expect("open the recovered state database");
    let (machine, shutdown, drain) = super::machine::CoordinationStateMachine::new(
        database,
        super::schema::StorageLayout::new(dir.to_path_buf()),
    )
    .expect("open the state machine over the recovered store");
    (dir, machine, shutdown, drain)
}

/// The applied index a machine reports; 0 for a state that has applied nothing.
async fn applied_index(machine: &mut super::machine::CoordinationStateMachine) -> u64 {
    use openraft::storage::RaftStateMachine as _;
    machine
        .applied_state()
        .await
        .expect("the machine reports its applied state")
        .0
        .map(|id| id.index)
        .unwrap_or(0)
}

/// **What a §11.4 snapshot costs to build and to install, per §12.1 shape.**
///
/// Both halves run production code on the production path: `build_snapshot` is
/// the method OpenRaft calls when the snapshot policy fires, and
/// `prepare_install` followed by `install_snapshot` is the pair the snapshot
/// receiver runs when a peer's stream completes. The only thing the harness
/// supplies in place of the network is the body itself, handed over in-process
/// — see the module header for where the network half is measured.
///
/// ## The install is proved to have happened, not assumed
///
/// Every install run asserts, in order: the receiver's applied index is
/// **behind** the candidate before the call; the call succeeds; and the
/// snapshot's `last_log_id` is then reported by **two independent oracles** —
/// the state machine's own durable view, and the reopened `state.redb` file
/// itself after the machine has been stopped.
///
/// Nothing but adopting the body moves either of them. The receiver is a bare
/// `CoordinationStateMachine`: there is no `Raft`, no log store and no peer
/// anywhere in this test, so replay, append and catch-up are not merely
/// prevented, they do not exist.
///
/// **Only the first oracle is proved load-bearing.** Mutation M59 — an install
/// that drops the body and returns `Ok(())` — is killed by oracle 1, which
/// fires first and aborts the run before oracle 2 is reached. Oracle 2 is
/// therefore **additional defence, not demonstrated coverage**: it is there for
/// an install that adopts the body in memory and leaves `state.redb` unchanged,
/// and no mutation in this checkpoint produces that defect, so nothing here
/// shows it would catch one. Claiming otherwise would be exactly the "the call
/// site exists, therefore it works" reasoning `coordination_snapshot_stream`
/// was rewritten to stop making.
///
/// Both are checked *after* the clock stops, so neither is in the numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: run explicitly, see the module header"]
async fn the_snapshot_build_and_install_cost_what_they_cost() {
    use openraft::storage::RaftStateMachine as _;
    use openraft::RaftSnapshotBuilder as _;

    println!(
        "\n§11.4 snapshot — production build and production install\n{}",
        "-".repeat(112)
    );

    for shape in shapes() {
        let bytes = export_for(&shape);
        let hash = *blake3::hash(&bytes).as_bytes();
        let export_home = ScratchDir::new("bench-snapshot-export");
        let export_path = export_home.join("export.recovery");
        std::fs::write(&export_path, &bytes).expect("stage the export");
        let (sealed, _identity) = capability(export_path, hash);

        // The sender: recovered at this shape, then advanced past where any
        // receiver recovered from the same export will sit.
        let (sender_dir, mut sender, sender_shutdown, _sender_drain) =
            recovered_machine("bench-snapshot-sender", &sealed);
        let base = applied_index(&mut sender).await;
        let entries: Vec<_> = (1..=AHEAD_BY)
            .map(|n| super::machine::blank_entry(BENCH_LEADER, base + n))
            .collect();
        sender.apply(entries).await.expect("the blanks apply");
        let target = applied_index(&mut sender).await;
        assert_eq!(
            target,
            base + AHEAD_BY,
            "the sender must be ahead of any receiver recovered from the same export"
        );

        // The body size is what both halves scale with, so it is printed beside
        // the shape rather than inferred from the export size — the export is a
        // row stream and the body is a redb file, and those are not the same
        // number.
        let body_bytes = {
            let mut builder = sender.get_snapshot_builder().await;
            let built = builder.build_snapshot().await.expect("the first build");
            let size = super::snapshot::file_stem_for(&built.meta.snapshot_id)
                .map(|stem| {
                    sender_dir
                        .join("snapshots")
                        .join(format!("{stem}{}", super::snapshot::SNAPSHOT_FILE_SUFFIX))
                })
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|meta| meta.len());
            drop(built);
            size
        };
        println!(
            "\n{} — export {} B, snapshot body {} B, snapshot at index {}",
            shape.label,
            bytes.len(),
            body_bytes
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string()),
            target,
        );

        // ── Build ───────────────────────────────────────────────────────────
        //
        // `measure_region_async` rather than a whole-closure timer: the builder
        // handle is taken per run — OpenRaft takes a fresh one each time it
        // snapshots — and the built handle is dropped after the clock stops,
        // because releasing a lease is not part of building.
        {
            let sender = sender.clone();
            measure_region_async("snapshot build", shape.label, shape.runs, || {
                let mut sender = sender.clone();
                async move {
                    let mut builder = sender.get_snapshot_builder().await;
                    let at = Instant::now();
                    let built = builder.build_snapshot().await.expect("build");
                    let elapsed = at.elapsed();
                    drop(built);
                    elapsed
                }
            })
            .await;
        }

        // ── Install ─────────────────────────────────────────────────────────
        //
        // A fresh receiver per run, because an install is a once-per-catch-up
        // operation against a store that is behind: reusing one would classify
        // `Repair` from the second run onward and price a marker rewrite.
        // Recovering that receiver is setup and sits outside the timed region,
        // which is why this is a region timer and not a whole-closure one.
        {
            let sender = sender.clone();
            let sealed = &sealed;
            measure_region_async("snapshot install", shape.label, shape.runs, || {
                let mut sender = sender.clone();
                async move {
                    let (dir, mut receiver, shutdown, drain) =
                        recovered_machine("bench-snapshot-receiver", sealed);
                    let before = applied_index(&mut receiver).await;
                    let current = sender
                        .get_current_snapshot()
                        .await
                        .expect("read the published snapshot")
                        .expect("the sender published one");
                    let meta = current.meta.clone();
                    let candidate = meta.last_log_id.map(|id| id.index).unwrap_or(0);
                    assert!(
                        before < candidate,
                        "the receiver must be behind the candidate, or the install \
                         classifies Repair and prices a marker rewrite: receiver at \
                         {before}, candidate at {candidate}"
                    );

                    let at = Instant::now();
                    receiver
                        .prepare_then_install_(&meta, current.snapshot)
                        .await
                        .expect("the install succeeds");
                    let elapsed = at.elapsed();

                    // Oracle 1: the state machine's **own durable view**, not
                    // `RaftMetrics`. `applied_state` is served by the state
                    // actor out of the `DurableState` it loaded from redb, so
                    // an install that moved a metric without moving the state
                    // cannot satisfy it.
                    let after = applied_index(&mut receiver).await;
                    assert_eq!(
                        after, candidate,
                        "the receiver must land exactly on the snapshot's last log id; \
                         it has no log — no `Raft`, no log store, no peer — so nothing \
                         but adopting the body can move it from {before}"
                    );

                    // Oracle 2: the file. The machine is stopped and the
                    // database reopened from disk, so this is what a restarted
                    // process would find rather than anything the install left
                    // in memory. An install that replaced the in-memory state
                    // and not `state.redb` passes oracle 1 and fails here.
                    drop(receiver);
                    drop(shutdown);
                    drop(drain);
                    let reopened = redb::Database::create(dir.join("state.redb"))
                        .expect("reopen the receiver's state database");
                    let durable = super::rows::load(&reopened).expect("the replaced file loads");
                    assert_eq!(
                        durable.model.last_applied.as_ref().map(|id| id.index),
                        Some(candidate),
                        "the replaced `state.redb` must itself carry the snapshot's last \
                         log id; anything less means the body was adopted in memory only"
                    );
                    drop(reopened);
                    drop(dir);
                    elapsed
                }
            })
            .await;
        }

        drop(sender);
        drop(sender_shutdown);
        drop(sender_dir);
    }
}
