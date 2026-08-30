//! The three start corridors (§7.2), and the §3.3 startup order they run in.
//!
//! ## The order is fixed, and each step is where it is for a reason
//!
//! ```text
//! validate static config          (CoordinationConfig::build)
//! start the Aster node            (AsterServer::start)
//! resolve the start mode          start::resolve — needs the stable NodeId
//! open and validate storage       against that NodeId
//! register the Iroh-only services before the acceptor, before any peer dials
//! start the RPC acceptor          (AsterServer::start)
//! create the Raft, attach it      the service is late-bound for exactly this
//! run the corridor                bootstrap / join / nothing
//! spawn controllers               membership, expiry
//! ```
//!
//! Two steps have to be in that order and are easy to get wrong. The signature
//! checks in `start::resolve` bind the **local NodeId**, which does not exist
//! before the endpoint does — so they cannot move earlier. And
//! `initialize` cannot run before peers can dial this node, because every other
//! initial voter is doing the same thing and they have to reach each other — so
//! it cannot move earlier either.
//!
//! ## What each corridor actually does
//!
//! - **Bootstrap** calls `Raft::initialize` with the membership the *signed
//!   genesis* names. Every initial voter calls it with the same set, and
//!   identical concurrent initialization is tolerated: OpenRaft answers
//!   `NotAllowed` to the losers, which is success here, not failure.
//! - **Join** calls nothing, and is the one mode that never may. A permit
//!   grants no membership; an existing administrator must commit an intent
//!   through the current quorum, and a joining node that initialized would
//!   found a second group beside the one it is asking to join. The corridor's
//!   whole job is to wait until a committed membership contains this node,
//!   which the lifecycle loop observes. It restarts freely: its first start
//!   binds the directory and every later one opens a bound directory, so
//!   `Join` admits both.
//! - **Existing** initializes **only when OpenRaft says the node is not
//!   initialized**. Ordinarily that never happens — its storage is bound and
//!   its group formed long ago. It matters for one window: a fresh open
//!   commits both identity bindings *before* `Raft::initialize` runs, so a
//!   crash in between leaves a directory that is bound with no membership,
//!   which `Bootstrap` refuses as not `Fresh`. Without this the group could
//!   never elect, never install its genesis, and never leave `NoQuorum`.
//!   Completing that founding is not founding a second group.

use std::collections::BTreeMap;
use std::sync::Arc;

use openraft::error::{InitializeError, RaftError};
use openraft::Raft;

use super::host::{CoordinationConfig, CoordinationStart};
use super::ids::CoordinationNodeId;
use super::network::admit::SenderAllowlist;
use super::network::client::{CoordinationNetworkFactory, FaultLatch, NetworkContext, RouteTable};
use super::network::service::{AttachedRuntime, RaftEndpoint};
use super::runtime::config::validated;
use super::runtime::lifecycle::Corridor;
use super::runtime::owner::{CoordinationRuntime, RuntimeFacts};
use super::start::{resolve, ResolvedStart, StartPlan};
use super::storage::VoterStorage;
use super::typeconfig::{CoordinationNode, CoordinationTypeConfig};
use crate::error::{Error, Result};
use crate::Node;

/// Everything the corridor decides before storage is opened.
///
/// Kept as one value so the two facts that must agree — which allowlist this
/// node starts with, and which corridor it reports — are produced together.
/// Deriving them separately is how a `Join` ends up reporting `Bootstrapping`.
struct CorridorPlan {
    corridor: Corridor,
    allowlist: SenderAllowlist,
    /// Advisory dialling hints from the signed start material. §7.2's seeds and
    /// a permit's own ticket are the only routes a node has before any
    /// membership commits.
    routes: Vec<(CoordinationNodeId, String)>,
}

impl CorridorPlan {
    fn of(plan: &StartPlan, local: &CoordinationNodeId) -> Self {
        match plan.resolved() {
            ResolvedStart::Bootstrap { genesis, .. } => Self {
                corridor: Corridor::Bootstrapping,
                // §7.2: before membership lands, only the genesis's signed
                // initial voters may speak to this node.
                allowlist: SenderAllowlist::seeded(
                    genesis
                        .initial_voters()
                        .iter()
                        .map(|seed| CoordinationNodeId::from_array(*seed.node_id())),
                ),
                routes: genesis
                    .initial_voters()
                    .iter()
                    .map(|seed| {
                        (
                            CoordinationNodeId::from_array(*seed.node_id()),
                            seed.ticket().to_string(),
                        )
                    })
                    .collect(),
            },
            ResolvedStart::Join { permit } => Self {
                corridor: Corridor::Joining,
                // §7.2: the permit's `bootstrap_senders`, and nothing else,
                // until a committed membership contains this node.
                allowlist: SenderAllowlist::seeded(
                    permit
                        .bootstrap_senders()
                        .iter()
                        .map(|node| CoordinationNodeId::from_array(*node)),
                ),
                // A permit carries no routes for its senders — only the
                // candidate's own ticket, which this node does not need to dial
                // itself. Discovery finds them, or the sender dials us first.
                routes: Vec::new(),
            },
            ResolvedStart::Existing { .. } => Self {
                corridor: Corridor::Running,
                // Storage is already bound, so a committed membership exists
                // and a seed list would be a second, staler answer.
                allowlist: SenderAllowlist::from_membership(),
                routes: Vec::new(),
            },
        }
        .without_self(local)
    }

    /// Drop this node's own route. Dialling ourselves is never useful and a
    /// self-entry in the address book is a lookup that resolves to a loop.
    fn without_self(mut self, local: &CoordinationNodeId) -> Self {
        self.routes.retain(|(node, _)| node != local);
        self
    }
}

/// Open storage, create the Raft, attach the service, and run the corridor.
///
/// Takes the `RaftEndpoint` the caller **already registered**: §3.3 puts
/// registration before the acceptor and the acceptor before this, so by the
/// time this runs a peer can already dial the service and be answered
/// `FailedPrecondition` until the attach below.
pub(crate) async fn start(
    config: &CoordinationConfig,
    root: &[u8; 32],
    node: &Node,
    endpoint: RaftEndpoint,
    store_endpoint: crate::coordination::store::host::StoreEndpoint,
    local: CoordinationNodeId,
) -> Result<CoordinationRuntime> {
    // The §S5-E startup gauge starts before anything is resolved or opened, so
    // start-to-`Ready` measures the whole start rather than the part after the
    // expensive one.
    let startup =
        std::sync::Arc::new(crate::coordination::runtime::readiness::StartupClock::started());

    // 3. Resolve the start mode against this node's verified identity.
    let plan = resolve(config, root, local).map_err(Error::from)?;
    let corridor = CorridorPlan::of(&plan, &local);

    // 4. Open storage against that identity, and check the directory's state
    //    matches the mode. The check is after the open because "is this
    //    directory initialized?" is a question only the open coordinator can
    //    answer without racing another opener.
    //
    //    A recovery bootstrap runs the §13 import inside the open, under the
    //    directory lock, before any actor exists: verify the bound export,
    //    transform it, and durably commit the founding state — or refuse and
    //    leave the store untouched. `Raft::initialize` below then only ever
    //    runs over a store whose import has already succeeded.
    let recovery = plan.recovery_import().map_err(Error::from)?;
    let opening = std::time::Instant::now();
    let storage = match &recovery {
        // The capability carries the identity its own genesis produced, so the
        // recovery open is not given one — there is no pair here to cross.
        Some(import) => {
            VoterStorage::open_with_recovery(plan.layout().clone(), plan.policy(), import)
        }
        None => VoterStorage::open(
            plan.layout().clone(),
            plan.identity().clone(),
            plan.policy(),
        ),
    }
    .map_err(|err| Error::Transport(anyhow::anyhow!("coordination storage: {err}")))?;
    startup.record_storage_open(opening.elapsed());
    plan.check_directory_state(storage.outcome())
        .map_err(Error::from)?;

    // 7. The Raft. Its network reads the advisory routes the signed start
    //    material supplied.
    let raft_config = validated(plan.identity().group_id()).map_err(Error::from)?;
    let routes = RouteTable::new();
    for (peer, ticket) in corridor.routes {
        routes.insert(peer, ticket);
    }
    let faults = FaultLatch::new();
    let factory = CoordinationNetworkFactory::new(NetworkContext {
        node: node.clone(),
        group: *plan.identity().group_id(),
        local,
        routes,
        faults,
        // A shipping build has no field to read: `CoordinationConfig::chaos` is
        // gated on `coordination-testing`, so this arm is the only one that
        // compiles into a release and it is a literal `None`.
        #[cfg(feature = "coordination-testing")]
        chaos: config.chaos(),
        #[cfg(not(feature = "coordination-testing"))]
        chaos: None,
    });

    let raft = Raft::new(
        local,
        Arc::clone(&raft_config),
        factory,
        storage.log().clone(),
        storage.state().clone(),
    )
    .await
    .map_err(|err| Error::Transport(anyhow::anyhow!("coordination raft: {err}")))?;

    // 8. Attach, so the already-registered service starts answering.
    let allowlist = Arc::new(corridor.allowlist);
    let facts = Arc::new(RuntimeFacts::new(corridor.corridor));
    let attached = AttachedRuntime {
        raft: raft.clone(),
        state: storage.state().handle().clone(),
        group: *plan.identity().group_id(),
        local,
        admission: Arc::clone(config.verified_admission()),
        allowlist: Arc::clone(&allowlist),
        layout: plan.layout().clone(),
        facts: Arc::clone(&facts),
    };

    let runtime = CoordinationRuntime::attach(
        raft.clone(),
        storage,
        endpoint,
        store_endpoint,
        attached,
        Arc::clone(&facts),
        allowlist,
        local,
        &raft_config,
        // §7.1's document, for the two modes that hold it.
        //
        // **`Existing` too**, and that is the whole point of requiring it there.
        // The genesis is installed by a committed entry, so a voter that
        // crashed after `Raft::initialize` committed and before that entry
        // applied comes back at `admin_revision = 0` — no administrators, and
        // no administrator able to appoint one. Without the document a restart
        // could never finish the bootstrap it interrupted, and the group would
        // sit at `NoQuorum` forever with nothing to explain it.
        //
        // Proposing is still gated on revision 0, so a healthy restart into a
        // long-running group proposes nothing.
        //
        // `Join` holds no genesis and needs none: it receives the entry by
        // replay or in the snapshot that subsumes it.
        // The **signed envelope**, from the configuration: `VerifiedGenesis`
        // seals the inner document, and the command carries the envelope the
        // operator signed. `resolve` has already verified these exact bytes and
        // bound them to this directory's hash, so passing them on is not a
        // second decode of anything.
        match config.start_mode() {
            CoordinationStart::Bootstrap { signed_genesis, .. }
            | CoordinationStart::Existing { signed_genesis, .. } => {
                Some(Arc::new(signed_genesis.clone()))
            }
            CoordinationStart::Join { .. } => None,
        },
        std::sync::Arc::clone(&startup),
    )
    .await;

    // 9. The corridor itself.
    run_corridor(&raft, plan.resolved(), local).await?;

    Ok(runtime)
}

/// Bootstrap initializes, and so does an `Existing` whose Raft was never
/// initialized — the interrupted-bootstrap recovery below. `Join` never does.
async fn run_corridor(
    raft: &Raft<CoordinationTypeConfig>,
    resolved: &ResolvedStart,
    local: CoordinationNodeId,
) -> Result<()> {
    match resolved {
        ResolvedStart::Bootstrap { genesis, .. } => {
            let members = initial_members(genesis);
            debug_assert!(
                members.contains_key(&local),
                "start::resolve refuses a bootstrap by a node outside the initial voters"
            );
            initialize(raft, members).await
        }
        // **An interrupted bootstrap finishes here.**
        //
        // A fresh open commits both identity bindings *before* `initialize`
        // runs, so a crash in between leaves a directory that is bound and has
        // no membership. `Bootstrap` then refuses it — the directory is no
        // longer `Fresh` — and if `Existing` also did nothing, the group could
        // never elect a leader, never propose its genesis, and never leave
        // `NoQuorum`. Nothing would say why.
        //
        // This is not "Existing founds a group". It initializes only when there
        // is **no committed membership at all**, with the membership named by
        // the genesis this directory is already bound to — completing the
        // founding of *this* group rather than starting a second one. Once a
        // membership exists this is a no-op, and `initialize`'s own
        // `NotAllowed` is the backstop if one commits between the check and the
        // call.
        ResolvedStart::Existing { genesis } => {
            // `Raft::is_initialized()`, not "does the membership have voters".
            // OpenRaft's own predicate also counts an existing log or a
            // non-default vote, so the two disagree on a store that crashed
            // *after* appending but before any membership committed — and this
            // arm would then call `initialize` on a node OpenRaft considers
            // initialized. Using its definition means the code and the claim
            // cannot drift apart.
            if raft
                .is_initialized()
                .await
                .map_err(|err| Error::Transport(anyhow::anyhow!("coordination raft: {err}")))?
            {
                return Ok(());
            }
            initialize(raft, initial_members(genesis)).await
        }
        // A permit grants no membership, and a joining node must never
        // initialize: it would found a second group beside the one it is
        // asking to join.
        ResolvedStart::Join { .. } => Ok(()),
    }
}

/// The membership a genesis names, as `Raft::initialize` takes it.
fn initial_members(
    genesis: &super::model::authority::VerifiedGenesis,
) -> BTreeMap<CoordinationNodeId, CoordinationNode> {
    genesis
        .initial_voters()
        .iter()
        .map(|seed| {
            (
                CoordinationNodeId::from_array(*seed.node_id()),
                CoordinationNode,
            )
        })
        .collect()
}

/// `Raft::initialize`, with concurrent initialization treated as success.
///
/// §7.2: "Every initial voter may call `Raft::initialize` with the same signed
/// membership; identical concurrent initialization is tolerated." OpenRaft
/// answers `NotAllowed` to whoever arrives after the first — that is the *only*
/// way a group of three ever starts, since all three call it, so treating it as
/// a failure would make bootstrap succeed on exactly one node and fail on the
/// rest.
///
/// The set is not re-derived here: it is the signed genesis's, already verified,
/// and every voter computes it from the same document. A group whose members
/// disagreed about the initial set would be two groups.
async fn initialize(
    raft: &Raft<CoordinationTypeConfig>,
    members: BTreeMap<CoordinationNodeId, CoordinationNode>,
) -> Result<()> {
    match raft.initialize(members).await {
        Ok(()) => Ok(()),
        // Already initialized, by a peer or by a previous attempt.
        Err(RaftError::APIError(InitializeError::NotAllowed(_))) => Ok(()),
        // The membership we were given does not contain us. `start::resolve`
        // already refuses that, so reaching here is a broken invariant rather
        // than a configuration mistake — and it must not be swallowed.
        Err(RaftError::APIError(InitializeError::NotInMembers(e))) => Err(Error::Transport(
            anyhow::anyhow!("coordination bootstrap: {e}"),
        )),
        Err(err) => Err(Error::Transport(anyhow::anyhow!(
            "coordination bootstrap: {err}"
        ))),
    }
}

/// The membership a bootstrap would install, for tests and for the corridor.
///
/// Exposed so a test can assert the set without reaching into `initialize`,
/// and so the assertion has an oracle that is not the call itself.
#[cfg(test)]
pub(crate) fn bootstrap_members(
    genesis: &super::model::authority::VerifiedGenesis,
) -> std::collections::BTreeSet<CoordinationNodeId> {
    genesis
        .initial_voters()
        .iter()
        .map(|seed| CoordinationNodeId::from_array(*seed.node_id()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::admission::testing::source_admitting;
    use crate::coordination::host::CoordinationStart;
    use crate::coordination::model::authority::testing::{
        authority, genesis_for, permit_for, sign, sign_genesis,
    };
    use crate::coordination::model::membership::test_ticket;
    use crate::coordination::model::shape;
    use std::collections::BTreeSet;

    const LOCAL: [u8; 32] = [1u8; 32];
    const PEER: [u8; 32] = [2u8; 32];
    const THIRD: [u8; 32] = [3u8; 32];

    fn node_id(n: [u8; 32]) -> CoordinationNodeId {
        CoordinationNodeId::from_array(n)
    }

    fn config(start: CoordinationStart) -> CoordinationConfig {
        let a = authority();
        CoordinationConfig::builder()
            .storage_dir("/tmp/unused-in-these-tests")
            .authority_domain(a.domain)
            .start_mode(start)
            .verified_admission(Arc::new(source_admitting(&[LOCAL, PEER, THIRD])))
            .build()
            .expect("a complete config")
    }

    /// The `Existing` mode for this fixture's own group.
    ///
    /// The hash is derived from the document rather than taken from
    /// `authority().genesis_hash`, which is a fixed constant unrelated to any
    /// genesis these tests build. `Existing` now verifies the two against each
    /// other, so a fixture pairing a real document with an unrelated hash would
    /// fail on the mismatch rather than exercising the corridor.
    fn existing_of() -> CoordinationStart {
        use crate::coordination::raft::SignedCoordinationDocumentV1;

        let signed = sign_genesis(&genesis_for(&[LOCAL, PEER, THIRD], shape::PROFILE_PROD));
        let envelope: SignedCoordinationDocumentV1 =
            crate::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&signed)
                .expect("the fixture's own genesis decodes");
        let expected_genesis_hash =
            crate::coordination::model::authority::genesis_hash(&envelope.document_bytes);
        CoordinationStart::Existing {
            signed_genesis: signed,
            expected_group_id: crate::coordination::ids::CoordinationGroupId::from_array(
                authority().group_id,
            ),
            expected_genesis_hash,
            expected_deployment_profile_code: shape::PROFILE_PROD,
        }
    }

    fn plan_for(start: CoordinationStart) -> StartPlan {
        resolve(&config(start), &authority().public_key, node_id(LOCAL)).expect("resolve")
    }

    fn bootstrap_start() -> CoordinationStart {
        CoordinationStart::Bootstrap {
            signed_genesis: sign_genesis(&genesis_for(&[LOCAL, PEER, THIRD], shape::PROFILE_PROD)),
            recovery_export_path: None,
        }
    }

    /// Bootstrap seeds its allowlist from the **signed** initial voters, and
    /// imports their tickets as advisory routes.
    ///
    /// Both halves matter and they are different things: the allowlist decides
    /// who may speak to us, and the routes decide how we find them. §2.1 lists
    /// substituting one for the other as forbidden.
    #[test]
    fn bootstrap_seeds_its_allowlist_and_routes_from_the_genesis() {
        let plan = plan_for(bootstrap_start());
        let corridor = CorridorPlan::of(&plan, &node_id(LOCAL));

        assert_eq!(corridor.corridor, Corridor::Bootstrapping);
        assert!(
            !corridor.allowlist.is_graduated(),
            "a bootstrapping node reads its seeds, not a membership it has not committed"
        );

        let members: BTreeSet<CoordinationNodeId> =
            [LOCAL, PEER, THIRD].into_iter().map(node_id).collect();
        for peer in &members {
            assert!(
                corridor.allowlist.permits(peer, &BTreeSet::new()),
                "every signed initial voter must be permitted before membership lands"
            );
        }
        assert!(
            !corridor
                .allowlist
                .permits(&node_id([9u8; 32]), &BTreeSet::new()),
            "and nobody else"
        );

        // Routes for the peers, and not for ourselves.
        let routed: BTreeSet<CoordinationNodeId> =
            corridor.routes.iter().map(|(node, _)| *node).collect();
        assert_eq!(
            routed,
            [PEER, THIRD]
                .into_iter()
                .map(node_id)
                .collect::<BTreeSet<_>>(),
            "a self-route is a lookup that resolves to a loop"
        );
        for (_, ticket) in &corridor.routes {
            assert!(
                !ticket.is_empty(),
                "a seed's ticket is what makes it reachable before any membership"
            );
        }
    }

    /// Join seeds from the **permit's** senders, which is a different list from
    /// a genesis's voters and must not be confused with one.
    #[test]
    fn join_seeds_its_allowlist_from_the_permits_senders() {
        let ticket = test_ticket(&LOCAL);
        let plan = plan_for(CoordinationStart::Join {
            signed_join_permit: sign(&permit_for(&LOCAL, &ticket)),
            expected_deployment_profile_code: shape::PROFILE_PROD,
        });
        let corridor = CorridorPlan::of(&plan, &node_id(LOCAL));

        assert_eq!(corridor.corridor, Corridor::Joining);
        assert!(!corridor.allowlist.is_graduated());
        // The fixture permit names exactly one bootstrap sender.
        assert!(corridor
            .allowlist
            .permits(&node_id([1u8; 32]), &BTreeSet::new()));
        assert!(!corridor
            .allowlist
            .permits(&node_id([7u8; 32]), &BTreeSet::new()));
        assert!(
            corridor.routes.is_empty(),
            "a permit carries the candidate's own ticket, not its senders'"
        );
    }

    /// Existing starts already graduated: its storage holds a committed
    /// membership, and a seed list would be a second, staler answer.
    #[test]
    fn existing_reads_the_committed_membership_from_the_first_call() {
        let plan = plan_for(existing_of());
        let corridor = CorridorPlan::of(&plan, &node_id(LOCAL));

        assert_eq!(corridor.corridor, Corridor::Running);
        assert!(corridor.allowlist.is_graduated());
        assert!(corridor.routes.is_empty());
    }

    /// The membership a bootstrap installs is exactly the genesis's initial
    /// voters — no more, no fewer, and derived from the signed document rather
    /// than from anything local.
    ///
    /// The oracle is the fixture's own voter list, so a corridor that added the
    /// local node to a set it was already in, or dropped a peer, fails.
    #[test]
    fn a_bootstrap_installs_exactly_the_signed_initial_voters() {
        let plan = plan_for(bootstrap_start());
        let ResolvedStart::Bootstrap { genesis, .. } = plan.resolved() else {
            panic!("bootstrap resolves to Bootstrap");
        };
        assert_eq!(
            bootstrap_members(genesis),
            [LOCAL, PEER, THIRD].into_iter().map(node_id).collect(),
        );
    }

    /// Each corridor's reported state and its allowlist are produced together,
    /// so a `Join` cannot report `Bootstrapping`.
    ///
    /// Stated as a property over all three rather than as three assertions: the
    /// failure this prevents is a fourth mode added later that derives one and
    /// forgets the other.
    #[test]
    fn every_corridor_agrees_with_its_own_allowlist() {
        let ticket = test_ticket(&LOCAL);
        let cases = [
            (bootstrap_start(), Corridor::Bootstrapping, false),
            (
                CoordinationStart::Join {
                    signed_join_permit: sign(&permit_for(&LOCAL, &ticket)),
                    expected_deployment_profile_code: shape::PROFILE_PROD,
                },
                Corridor::Joining,
                false,
            ),
            (existing_of(), Corridor::Running, true),
        ];
        for (start, expected_corridor, graduated) in cases {
            let plan = plan_for(start);
            let corridor = CorridorPlan::of(&plan, &node_id(LOCAL));
            assert_eq!(corridor.corridor, expected_corridor);
            assert_eq!(
                corridor.allowlist.is_graduated(),
                graduated,
                "{expected_corridor:?} must start {} committed membership",
                if graduated { "reading" } else { "without" }
            );
        }
    }

    /// **A shipping voter has no fault injector.**
    ///
    /// The §6.2 chaos handle is a deliberately unreliable network, and one that
    /// reached production would be a group quietly dropping its own Raft
    /// traffic with nothing to attribute it to. The guarantee is structural
    /// rather than a runtime default: `CoordinationConfig`'s field is gated on
    /// `coordination-testing`, so a release build has nothing to read and this
    /// arm is the only one that compiles.
    ///
    /// Asserted on the source because the property is about the *absence* of a
    /// configuration path — a build without the feature cannot express the
    /// mistake, so no run of it can observe the check working.
    #[test]
    fn a_production_voter_installs_no_fault_injector() {
        let source = include_str!("boot.rs");
        let end = crate::coordination::guards::production_end(source);
        let production = &source[..end];

        assert!(
            production.contains("#[cfg(not(feature = \"coordination-testing\"))]")
                && production.contains("chaos: None,"),
            "a build without `coordination-testing` must set the injector to None"
        );
        // And the only other arm is gated on the testing feature, so there is
        // no third path that could supply one.
        let sites = production.matches("chaos:").count();
        assert_eq!(
            sites, 2,
            "the injector must be set in exactly the two cfg arms; found {sites}"
        );
    }

    /// **`Join` never initializes, and `Existing` only recovers.**
    ///
    /// Source-level, because the harm is a call that must not happen: a `Join`
    /// that initialized would found a second group over an existing one, and no
    /// test of a *correct* run observes a call that was never made. There is one
    /// `initialize` call site, reachable from `Bootstrap` and from `Existing`'s
    /// interrupted-bootstrap arm, which is guarded on `is_initialized()`.
    #[test]
    fn initialize_is_reachable_only_from_bootstrap_and_the_recovery_arm() {
        let source = include_str!("boot.rs");
        let end = crate::coordination::guards::production_end(source);
        let production = &source[..end];

        let sites: Vec<usize> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("raft.initialize(")
            })
            .map(|(n, _)| n + 1)
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "Raft::initialize must be called from exactly one place; found {sites:?}"
        );

        // And **`Join` never initializes**, which is the arm where founding a
        // second group would actually be possible: a joining node is asking to
        // be admitted to a group that already exists.
        //
        // `Existing` may initialize, and that is a deliberate change. A fresh
        // open commits both identity bindings before `Raft::initialize` runs,
        // so a crash in between leaves a directory that is bound and has no
        // membership — `Bootstrap` then refuses it as not `Fresh`, and if
        // `Existing` also did nothing the group could never elect, never
        // propose its genesis, and never leave `NoQuorum`. It initializes only
        // when there is no committed membership at all, with the set named by
        // the genesis the directory is already bound to.
        let corridor_at = production
            .find("async fn run_corridor(")
            .expect("the corridor exists");
        let arm = &production[corridor_at..];
        assert!(
            arm.contains("ResolvedStart::Join { .. } => Ok(())"),
            "the join arm must do nothing: a permit grants no membership"
        );

        // The `Existing` arm's initialize is gated on OpenRaft's own
        // `is_initialized()`, not unconditional. Asserted on the source because
        // the alternative — an `Existing` start that initializes over a live
        // group — is not something a test can safely provoke.
        let existing_at = arm
            .find("ResolvedStart::Existing { genesis }")
            .expect("an existing arm");
        let existing = &arm[existing_at..];
        let init_at = existing
            .find("initialize(raft, initial_members(genesis))")
            .expect("the existing arm may initialize");
        assert!(
            existing[..init_at].contains("is_initialized()"),
            "the existing arm must consult OpenRaft's own initialization \
             predicate before initializing"
        );
    }
}
