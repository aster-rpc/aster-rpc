//! The Rust in `docs/aster-coordination-getstarted.md`, compiled.
//!
//! Every fenced block the guide presents as real Rust (` ```rust `) appears
//! here verbatim, inside a function, and [`every_rust_block_in_the_guide_is_compiled_here`]
//! reads the guide and refuses a block it cannot find in this file. So the
//! guide cannot drift from the released API without this target failing to
//! compile, and it cannot present prose as code without this test failing.
//! Blocks the guide marks ` ```text ` or `pseudocode` are schematic on purpose
//! and are not checked.
//!
//! The functions are never called: this is a compilation gate, not a
//! scenario. The scenarios are the other `coordination_*` targets.

#![cfg(feature = "coordination")]
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "the guide's blocks are compiled, not run; a test setup failure is the test failing"
)]

// ── §5.3 Verified producer admission ──────────────────────────────────────
mod admission {
    use std::sync::Arc;

    use aster::coordination::admission::{SignedAdmissionSet, VerifiedAdmissionSource};

    /// The enrollment layer already holds a ROOT-signed admission set; Aster
    /// verifies its signature and authority domain before answering a single
    /// lookup from it.
    fn admission_source(
        root_public_key: [u8; 32],
        authority_domain: &str,
        signed_admission_set: &[u8],
    ) -> Result<Arc<dyn VerifiedAdmissionSource>, Box<dyn std::error::Error>> {
        let verified =
            SignedAdmissionSet::verify(&root_public_key, authority_domain, signed_admission_set)?;
        Ok(Arc::new(verified))
    }
}

// ── §6 Bootstrap the initial three voters ─────────────────────────────────
mod bootstrap {
    use std::sync::Arc;

    use aster::coordination::admission::VerifiedAdmissionSource;
    use aster::coordination::host::{CoordinationConfig, CoordinationStart};
    use aster::rpc::AsterServer;
    use aster::AsterConfig;

    async fn start_initial_voter(
        signed_genesis: Vec<u8>,
        admission_source: Arc<dyn VerifiedAdmissionSource>,
    ) -> aster::Result<AsterServer> {
        // ASTER_ROOT_PUBKEY / ASTER_ROOT_PUBKEY_FILE name the deployment ROOT.
        // Coordination consumes that key; it has no second authority key to
        // configure.
        let aster_config = AsterConfig::from_env()?;

        let coordination = CoordinationConfig::builder()
            .storage_dir("/var/lib/my-app/coordination")
            .authority_domain("example.my-app.production")
            .start_mode(CoordinationStart::Bootstrap {
                signed_genesis,
                recovery_export_path: None,
            })
            .verified_admission(admission_source)
            .build()?;

        let server = AsterServer::builder()
            .config(aster_config)
            .identity("/var/lib/my-app/node.secret")
            .builtin_coordination(coordination)
            .start()
            .await?;

        // start() returns once storage is open and the runtime is spawned;
        // the voters may still be finding one another.
        let coordination = server
            .coordination()
            .expect("a server built with builtin_coordination hosts it");
        coordination.wait_ready().await?;
        Ok(server)
    }
}

// ── §7 Open a durable client ──────────────────────────────────────────────
mod client {
    use std::path::Path;

    use aster::coordination::client::{CoordinationClient, CoordinationReadClient};
    use aster::coordination::ids::CoordinationGroupId;
    use aster::Node;

    fn open_durable_client(
        node: Node,
        group_id: CoordinationGroupId,
        initial_member_tickets: Vec<String>,
        app_state: &Path,
    ) -> aster::Result<CoordinationClient> {
        CoordinationClient::builder(node, group_id)
            .seed_tickets(initial_member_tickets)
            .journal_dir(app_state.join("coordination-client"))
            .build()
    }

    fn open_read_only_client(
        node: Node,
        group_id: CoordinationGroupId,
        initial_member_tickets: Vec<String>,
    ) -> aster::Result<CoordinationReadClient> {
        CoordinationClient::builder(node, group_id)
            .seed_tickets(initial_member_tickets)
            .build_read_only()
    }
}

// ── §8 Grant exact rights to an exact resource ────────────────────────────
mod grant {
    use aster::coordination::client::CoordinationClient;
    use aster::coordination::contract::ResourceKeyV1;
    use aster::coordination::model::limits::{
        RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW,
    };

    fn active_head_resource(workload_id: &[u8]) -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "my-app.active-head".to_string(),
            key: workload_id.to_vec(),
        }
    }

    async fn grant_worker(
        admin_client: &CoordinationClient,
        resource: ResourceKeyV1,
        worker_node_id: [u8; 32],
        expected_acl_revision: u64,
    ) -> aster::Result<u64> {
        let resolved = admin_client
            .set_grant(
                resource,
                worker_node_id,
                RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
                expected_acl_revision,
            )
            .await?;
        let acl_revision = resolved.reply().acl_revision;
        // The result was fsynced `ResolvedAwaitingAck` before it was returned.
        // Acknowledge once the application has durably handled it; until then
        // the journal permits no new sequence.
        resolved.acknowledge().await?;
        Ok(acl_revision)
    }
}

// ── §9 Acquire, mutate, and release a fenced register ─────────────────────
mod register {
    use aster::coordination::client::lease::{CoordinationLease, LeaseAcquireOutcome};
    use aster::coordination::client::CoordinationClient;
    use aster::coordination::contract::ResourceKeyV1;
    use aster::lease::LeaseOptions;

    async fn advance_head_then_hand_off(
        client: CoordinationClient,
        resource: ResourceKeyV1,
        next_head: Vec<u8>,
        final_head: Vec<u8>,
    ) -> aster::Result<()> {
        let lease = match CoordinationLease::acquire(
            client.clone(),
            resource.clone(),
            LeaseOptions::default(),
        )
        .await?
        {
            LeaseAcquireOutcome::Granted(lease) => lease,
            LeaseAcquireOutcome::Denied(reason, _current) => {
                return Err(aster::Error::InvalidArgument(format!(
                    "the register is held elsewhere: {reason:?}"
                )));
            }
        };

        // Ask for the fence immediately before protected work; `None` means
        // the self-limit has passed and this process must stop.
        if lease.fence().is_none() {
            return Err(aster::Error::InvalidArgument("the lease was lost".into()));
        }
        let current = client.reads().read_linearizable(resource.clone()).await?;

        // The lease presents its current fence with the mutation; the group
        // checks epoch, holder, held, and expected value revision together.
        let updated = lease
            .compare_and_set(current.snapshot.value_revision, next_head)
            .await?;
        let committed_revision = updated
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.value_revision)
            .unwrap_or(current.snapshot.value_revision);

        // A graceful handoff commits the final value and releases atomically:
        // both, or neither.
        lease
            .release_with_value(committed_revision, final_head)
            .await?;
        Ok(())
    }
}

// ── §10 Maintain three voters with topology-aware placement ───────────────
mod placement {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use aster::coordination::client::manager::{
        CoordinationPlacementManager, PlacementPolicySource, PlacementProvisioner,
    };
    use aster::coordination::client::placement::{
        PlacementCandidate, PlacementPolicy, PlacementReport,
    };
    use aster::coordination::client::CoordinationClient;
    use aster::coordination::ids::CoordinationNodeId;
    use aster::rpc::AsterServer;

    fn ten_node_policy(
        policy_revision: u64,
        verified_policy_digest: [u8; 32],
        root_public_key: [u8; 32],
        eligible_hosts_a_through_j: &[CoordinationNodeId],
        maintenance_exclusions: BTreeSet<CoordinationNodeId>,
    ) -> PlacementPolicy {
        PlacementPolicy {
            revision: policy_revision,
            digest: verified_policy_digest,
            authority_public_key: root_public_key,
            authority_domain: "example.my-app.production".to_string(),
            target_voters: 3,
            target_retained_learners: 0,
            eligible: eligible_hosts_a_through_j
                .iter()
                .map(|id| {
                    (
                        *id,
                        PlacementCandidate {
                            node_id: *id,
                            member_seed: None,
                        },
                    )
                })
                .collect(),
            pinned_voters: BTreeSet::new(),
            excluded: maintenance_exclusions,
            // Three voters make three pairs. This policy requires positive
            // separation evidence for all three; it is not a geography claim.
            minimum_separated_voter_pairs: 3,
            replacement_grace: Duration::from_secs(300),
            allow_healthy_rebalance: false,
        }
    }

    async fn reconcile_placement<P, V>(
        admin_client: CoordinationClient,
        server: &AsterServer,
        policy_source: P,
        provisioner: V,
    ) -> aster::Result<PlacementReport>
    where
        P: PlacementPolicySource,
        V: PlacementProvisioner,
    {
        // The server's topology handle must have joined the producer-only
        // shared namespace; that gives the manager a view, not a permission.
        let mut manager = CoordinationPlacementManager::new(
            admin_client,
            server.topology(),
            policy_source,
            provisioner,
        );
        manager.reconcile().await
    }
}

// ── §12 Restart, join, remove, and recover ────────────────────────────────
mod lifecycle {
    use std::path::Path;
    use std::sync::Arc;

    use aster::coordination::admission::VerifiedAdmissionSource;
    use aster::coordination::client::CoordinationClient;
    use aster::coordination::host::{CoordinationConfig, CoordinationStart};
    use aster::coordination::ids::CoordinationGroupId;

    fn restart_existing_voter(
        signed_genesis: Vec<u8>,
        expected_group_id: CoordinationGroupId,
        expected_genesis_hash: [u8; 32],
        expected_deployment_profile_code: i32,
        admission_source: Arc<dyn VerifiedAdmissionSource>,
    ) -> aster::Result<CoordinationConfig> {
        CoordinationConfig::builder()
            .storage_dir("/var/lib/my-app/coordination")
            .authority_domain("example.my-app.production")
            // Independently expected identity: never inferred from the
            // database being checked.
            .start_mode(CoordinationStart::Existing {
                signed_genesis,
                expected_group_id,
                expected_genesis_hash,
                expected_deployment_profile_code,
            })
            .verified_admission(admission_source)
            .build()
    }

    /// While the source group still has quorum: one linearizable, canonical
    /// recovery point, streamed to a file outside the live voter set.
    async fn take_recovery_point(
        admin_client: &CoordinationClient,
        outside_the_voters: &Path,
    ) -> Result<[u8; 32], Box<dyn std::error::Error>> {
        let file = std::fs::File::create(outside_the_voters)?;
        let (manifest, file) = admin_client.reads().recovery_export_to(file).await?;
        file.sync_all()?;
        let mut export_hash = [0u8; 32];
        export_hash.copy_from_slice(&manifest.blake3_checksum);
        Ok(export_hash)
    }

    /// After an unrecoverable quorum loss: a **new** group whose signed
    /// genesis binds the export's hash. The group id changes, so every old
    /// fence fails against it.
    fn bootstrap_recovered_group(
        signed_recovery_genesis: Vec<u8>,
        saved_export: &Path,
        admission_source: Arc<dyn VerifiedAdmissionSource>,
    ) -> aster::Result<CoordinationConfig> {
        CoordinationConfig::builder()
            .storage_dir("/var/lib/my-app/coordination-recovered")
            .authority_domain("example.my-app.production")
            .start_mode(CoordinationStart::Bootstrap {
                signed_genesis: signed_recovery_genesis,
                recovery_export_path: Some(saved_export.to_path_buf()),
            })
            .verified_admission(admission_source)
            .build()
    }
}

// ── §13 Observe the group honestly ────────────────────────────────────────
mod observe {
    use aster::coordination::Coordination;

    fn report_startup(coordination: &Coordination) {
        let timings = coordination.startup_timings();
        // Both are `None` until measured: a start that never reached Ready
        // says so rather than reporting zero.
        match (timings.storage_open_micros, timings.start_to_ready_micros) {
            (Some(open), Some(ready)) => {
                println!("storage open {open} µs, start to Ready {ready} µs");
            }
            (Some(open), None) => println!("storage open {open} µs; not Ready yet"),
            _ => println!("still opening storage"),
        }
    }
}

// ── The gate ──────────────────────────────────────────────────────────────

const GUIDE: &str = include_str!("../../docs/aster-coordination-getstarted.md");
const THIS_FILE: &str = include_str!("coordination_guide.rs");

fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every ` ```rust ` block in the guide, in order, with its fence stripped.
fn rust_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in markdown.lines() {
        match &mut current {
            None if line.trim_end() == "```rust" => current = Some(Vec::new()),
            None => {}
            Some(lines) if line.trim_end() == "```" => {
                blocks.push(lines.join("\n"));
                current = None;
            }
            Some(lines) => lines.push(line),
        }
    }
    assert!(
        current.is_none(),
        "the guide ends inside an unterminated ```rust block"
    );
    blocks
}

#[test]
fn every_rust_block_in_the_guide_is_compiled_here() {
    let blocks = rust_blocks(GUIDE);
    assert!(
        blocks.len() >= 8,
        "the guide should carry its worked examples as real Rust; found {} blocks",
        blocks.len()
    );
    let compiled = normalise(THIS_FILE);
    for (index, block) in blocks.iter().enumerate() {
        let wanted = normalise(block);
        assert!(
            compiled.contains(&wanted),
            "```rust block #{} of the guide is not compiled by this file; either copy it \
             here verbatim (inside a function) or mark it ```pseudocode if it is schematic:\n{block}",
            index + 1
        );
    }
}

/// The guide must not hide real-looking Rust behind `rust,ignore`: a block is
/// either compiled (```rust) or declared schematic (```pseudocode / ```text).
#[test]
fn the_guide_hides_nothing_behind_rust_ignore() {
    let offenders: Vec<usize> = GUIDE
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_end().starts_with("```rust,"))
        .map(|(number, _)| number + 1)
        .collect();
    assert!(
        offenders.is_empty(),
        "the guide uses a qualified rust fence at lines {offenders:?}; use ```rust for \
         compiled examples and ```pseudocode for schematic ones"
    );
}
