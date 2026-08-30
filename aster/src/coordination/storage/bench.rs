//! **Measured cost of the §13 recovery import** (S5-E).
//!
//! ## What is measured
//!
//! The **whole production import**: a fresh directory, a sealed capability, and
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

/// **The import's components**, for localising a regression the whole-path
/// number only reveals the existence of.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn the_import_components_cost_what_they_cost() {
    use crate::coordination::rowstream::{read_body, BodyError, BodyVisitor, RecoveryBodyRowV1};

    #[derive(Default)]
    struct Collect {
        header: Option<crate::coordination::rowstream::RecoveryHeaderV1>,
        rows: Vec<RecoveryBodyRowV1>,
    }
    impl BodyVisitor<RecoveryBodyRowV1> for Collect {
        fn header(
            &mut self,
            header: &crate::coordination::rowstream::RecoveryHeaderV1,
        ) -> Result<(), BodyError> {
            self.header = Some(header.clone());
            Ok(())
        }
        fn row(&mut self, _section: u64, row: RecoveryBodyRowV1) -> Result<(), BodyError> {
            self.rows.push(row);
            Ok(())
        }
    }

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
