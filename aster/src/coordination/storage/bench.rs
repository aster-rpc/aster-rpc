//! **Measured cost of the durable §11/§13 operations** (S5-E).
//!
//! ## What a number here is, and what it is not
//!
//! These are `#[ignore]`d by default and run on demand. They are *not* a
//! regression gate: a debug build on a loaded developer machine is not a
//! production measurement, and asserting a millisecond bound in CI would either
//! be so loose it catches nothing or so tight it fails on a busy runner. What
//! they produce is a **distribution** — the input to choosing alert thresholds,
//! which is what S5-E actually asks for.
//!
//! The one thing they do assert is the rule S5-E states directly: *do not
//! loosen a logical or durability bound to meet a benchmark*. Every shape here
//! is a legal §12.1 shape, every write goes through the same
//! `Durability::Immediate` path production uses, and nothing is measured with a
//! check disabled. A benchmark that had to weaken a bound to look good would be
//! measuring a build nobody ships.
//!
//! ## Default and maximum legal shapes
//!
//! Both, because they answer different questions. The default shape is what an
//! operator sees on an ordinary day; the maximum legal shape is what §12.1
//! permits, and it is the one that decides whether a bound is survivable at
//! all — an export that is comfortable at ten resources and unusable at the
//! hundred thousand §12.1 allows would pass a default-shape benchmark and fail
//! the first large group.
//!
//! Run with:
//!
//! ```text
//! cargo test -p aster --lib --no-default-features --features coordination \
//!     -- --ignored --nocapture coordination::storage::bench
//! ```

#![cfg(test)]

use std::time::{Duration, Instant};

use super::fixture::{encode_recovery_export, recovery_export_parts};
use super::rows;
use crate::coordination::model::limits;

/// One measured operation at one shape.
struct Measured {
    what: &'static str,
    shape: String,
    samples: Vec<Duration>,
}

impl Measured {
    fn percentile(&self, p: f64) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        // Nearest-rank, which needs no interpolation and cannot invent a value
        // between two observations that were never seen.
        let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
        sorted[rank.min(sorted.len()) - 1]
    }

    fn report(&self) {
        println!(
            "{:<28} {:<22} n={:<4} p50={:>9.3?} p95={:>9.3?} max={:>9.3?}",
            self.what,
            self.shape,
            self.samples.len(),
            self.percentile(50.0),
            self.percentile(95.0),
            self.percentile(100.0),
        );
    }
}

fn measure(
    what: &'static str,
    shape: impl Into<String>,
    runs: usize,
    mut body: impl FnMut(),
) -> Measured {
    // One untimed pass first: the first call pays for lazily-built Fory
    // registries and page-cache warming, which are startup costs rather than
    // per-operation costs and would otherwise land entirely in the maximum.
    body();
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let at = Instant::now();
        body();
        samples.push(at.elapsed());
    }
    let measured = Measured {
        what,
        shape: shape.into(),
        samples,
    };
    measured.report();
    measured
}

/// A recovery export carrying `resources` resources and one grant each, which
/// is a legal §12.1 shape for any count up to the caps.
fn export_of(resources: u64) -> Vec<u8> {
    use crate::coordination::contract::ResourceKeyV1;
    use crate::coordination::rowstream::{GrantRowV1, RecoveryBodyRowV1, RecoveryResourceRowV1};

    let (mut header, mut rows) = recovery_export_parts();
    rows.retain(|row| matches!(row, RecoveryBodyRowV1::Admin(_)));
    let key = |n: u64| ResourceKeyV1 {
        namespace: "portal.roaming".into(),
        key: n.to_be_bytes().to_vec(),
    };
    let mut resource_rows = Vec::with_capacity(resources as usize);
    let mut grant_rows = Vec::with_capacity(resources as usize);
    for n in 0..resources {
        resource_rows.push(RecoveryBodyRowV1::Resource(RecoveryResourceRowV1 {
            key: key(n),
            source_epoch: 9,
            ttl_ms: 30_000,
            lease_revision: 10,
            value_revision: 7,
            value: vec![0x5A; 64],
            acl_revision: 2,
        }));
        grant_rows.push(RecoveryBodyRowV1::Grant(GrantRowV1 {
            key: key(n),
            subject_node_id: [0x21u8; 32].to_vec(),
            rights: 0x3f,
        }));
    }
    header.row_counts.resources = resources;
    header.row_counts.grants = resources;
    let mut all = resource_rows;
    all.append(&mut grant_rows);
    all.extend(rows);
    encode_recovery_export(&header, &all)
}

/// **§13 export decode and transform, at both shapes.**
///
/// The import's two costs are separable and worth separating: reading the
/// canonical body (framing, limits, widths, inner canonicality) and
/// transforming it (checked arithmetic, cross-row consistency, the ordered
/// per-resource pass). The second is where the S5-B review found an accidental
/// `O(R × A)` scan, so it is the one a threshold has to watch.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn the_recovery_import_costs_what_it_costs() {
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

    println!("\n§13 recovery import\n{}", "-".repeat(88));
    for (label, resources, runs) in [
        ("default (2 resources)", 2u64, 50usize),
        ("large (10,000)", 10_000, 5),
        // The §12.1 ceiling. Slow by construction and that is the point: this
        // is the shape a threshold must survive.
        (
            "maximum legal (100,000)",
            limits::MAX_RESOURCES_PER_GROUP,
            2,
        ),
    ] {
        let bytes = export_of(resources);
        println!("  body: {} bytes", bytes.len());

        let mut decoded = Collect::default();
        measure("read_body (decode+canonical)", label, runs, || {
            let mut collected = Collect::default();
            read_body::<_, RecoveryBodyRowV1>(&mut &bytes[..], &mut collected)
                .expect("the body is canonical");
            decoded = std::mem::take(&mut collected);
        });

        let header = decoded.header.clone().expect("a header");
        let identity = bench_identity();
        let hash = *blake3::hash(&bytes).as_bytes();
        measure("transform (validate+build)", label, runs, || {
            super::import::transform(&header, &decoded.rows, &identity, &[[0x41u8; 32]], &hash)
                .expect("the transform succeeds");
        });
    }
}

/// **Whole-state validation, which every open pays.**
///
/// `rows::validate` runs on every load and now also before every §13 commit
/// (S5-B). It is the cost an operator pays on each restart, so its maximum-shape
/// number is what a startup-latency alert is set from.
#[test]
#[ignore = "benchmark: run explicitly, see the module header"]
fn whole_state_validation_costs_what_it_costs() {
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

    println!("\n§11.3 whole-state validation\n{}", "-".repeat(88));
    for (label, resources, runs) in [
        ("default (2 resources)", 2u64, 50usize),
        (
            "maximum legal (100,000)",
            limits::MAX_RESOURCES_PER_GROUP,
            3,
        ),
    ] {
        let bytes = export_of(resources);
        let mut collected = Collect::default();
        read_body::<_, RecoveryBodyRowV1>(&mut &bytes[..], &mut collected).expect("canonical");
        let header = collected.header.expect("a header");
        let state = super::import::transform(
            &header,
            &collected.rows,
            &bench_identity(),
            &[[0x41u8; 32]],
            blake3::hash(&bytes).as_bytes(),
        )
        .expect("the transform succeeds");

        measure("validate_before_commit", label, runs, || {
            rows::validate_before_commit(&state).expect("the founding state is loadable");
        });
    }
}

fn bench_identity() -> super::identity::StorageIdentity {
    let authority = crate::coordination::model::authority::testing::authority();
    super::identity::StorageIdentity::new(
        crate::coordination::ids::CoordinationGroupId::from_array([0xEE; 32]),
        [0xE1; 32],
        authority.public_key,
        authority.domain.clone(),
        crate::coordination::ids::CoordinationNodeId::from_array([0xE5; 32]),
        crate::coordination::model::shape::PROFILE_PROD,
    )
}
