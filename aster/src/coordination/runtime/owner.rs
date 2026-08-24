//! `CoordinationRuntime`: the one owner of the Raft instance, its storage, its
//! controllers, and their shutdown (§7.6).
//!
//! ## Why one owner
//!
//! §7.6 fixes cancellation as the strict reverse of construction:
//!
//! ```text
//! construct: storage → runtime → Raft → RPC registration → controllers
//! shutdown:  controllers → deregister RPC → Raft::shutdown() → state and log
//!            actors and read leases → directory lock
//! ```
//!
//! That ordering is only enforceable if one value owns all of it. A runtime
//! assembled from independently-dropped handles has whatever order the compiler
//! picks, and **releasing the directory lock while an actor still holds a
//! database open is how a second voter is admitted to a directory the first has
//! not finished with**. Rust drops fields in declaration order, so the field
//! order below is the shutdown order and is load-bearing — the same rule
//! `VoterStorage` already documents one layer down.
//!
//! ## The metrics loop is the only writer of the lifecycle view
//!
//! It watches `Raft::metrics()`, classifies through
//! [`lifecycle::classify`](super::lifecycle::classify), and publishes. Nothing
//! else writes: two writers would let a node disagree with itself about its own
//! state, which is the failure `classify` already exists to prevent one level
//! down.
//!
//! It also carries the two facts OpenRaft does not have — which start corridor
//! is still running, and how long since a follower last accepted an append from
//! its leader — because neither is a Raft concept.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openraft::Raft;

use super::config::leader_liveness_timeout;
use super::lifecycle::{
    classify, Corridor, FatalCause, LifecycleCode, LifecycleView, LocalFacts, LocalRole,
};
use super::readiness::{channel, LifecyclePublisher, Readiness};
use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::network::admit::SenderAllowlist;
use crate::coordination::network::service::{AttachedRuntime, RaftEndpoint};
use crate::coordination::typeconfig::CoordinationTypeConfig;

/// The local facts the metrics loop maintains, shared with whatever updates
/// them.
///
/// Atomics rather than a mutex: every one is a single value written by one
/// place and read by the metrics loop, and a lock here would put the Raft
/// append path behind the classification loop.
#[derive(Debug)]
pub(crate) struct RuntimeFacts {
    /// Which corridor is running, as a `LifecycleCode` discriminant so it fits
    /// an atomic. `0` means `Running`.
    corridor: AtomicU64,
    draining: AtomicBool,
    /// Milliseconds since the process start reference at the last accepted
    /// append from the recognised leader. `u64::MAX` means "never".
    ///
    /// **Advisory** — see [`lifecycle`](super::lifecycle) for why a follower's
    /// leader contact is not quorum evidence, and why v1 says so rather than
    /// pretending otherwise.
    last_leader_contact_ms: AtomicU64,
    /// The reference point the value above is measured from. A monotonic
    /// `Instant` cannot live in an atomic, so the runtime holds one origin and
    /// stores offsets from it.
    origin: Instant,
    /// Latched. A `Fatal` that a later observation could clear would not be
    /// `Fatal`.
    fatal: std::sync::Mutex<Option<FatalCause>>,
    /// Whether this node has applied the group's signed genesis (§7.6).
    ///
    /// Latched once true: the genesis is installed by a committed entry and
    /// nothing un-installs it, so an observation that went back to false would
    /// be a read failure rather than a state change — and it must not be able
    /// to withdraw readiness.
    genesis_installed: AtomicBool,
}

impl RuntimeFacts {
    /// Start in the corridor this start mode begins in.
    pub(crate) fn new(corridor: Corridor) -> Self {
        Self {
            corridor: AtomicU64::new(corridor_code(corridor)),
            draining: AtomicBool::new(false),
            last_leader_contact_ms: AtomicU64::new(u64::MAX),
            origin: Instant::now(),
            fatal: std::sync::Mutex::new(None),
            genesis_installed: AtomicBool::new(false),
        }
    }

    /// Record an accepted `append_entries` from the recognised leader.
    ///
    /// Called by the Raft service handler. Advisory: it proves one node
    /// believes itself leader and is reaching us, and nothing about quorum.
    pub(crate) fn note_leader_contact(&self) {
        let elapsed = self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.last_leader_contact_ms.store(elapsed, Ordering::SeqCst);
    }

    pub(crate) fn enter(&self, corridor: Corridor) {
        self.corridor
            .store(corridor_code(corridor), Ordering::SeqCst);
    }

    pub(crate) fn corridor(&self) -> Corridor {
        corridor_of(self.corridor.load(Ordering::SeqCst))
    }

    pub(crate) fn begin_draining(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// Record that this node has applied the group's genesis.
    ///
    /// One-way. The caller polls a state read that can legitimately fail —
    /// during shutdown, or while the actor is fatal — and a poll that answered
    /// "not installed" for either reason must not take readiness away from a
    /// node that had it.
    pub(crate) fn note_genesis_installed(&self) {
        self.genesis_installed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn genesis_installed(&self) -> bool {
        self.genesis_installed.load(Ordering::SeqCst)
    }

    /// Latch a fatal cause. The **first** one wins: it is the one that
    /// explains the others.
    pub(crate) fn latch_fatal(&self, cause: FatalCause) {
        let mut held = self.fatal.lock().expect("the fatal latch");
        if held.is_none() {
            *held = Some(cause);
        }
    }

    /// `committed` is passed in rather than held here: the state actor already
    /// publishes it, and a second copy on these facts would be a second answer
    /// that goes stale between the two.
    fn snapshot(
        &self,
        timeout: Duration,
        committed: std::sync::Arc<
            openraft::StoredMembership<
                crate::coordination::ids::CoordinationNodeId,
                crate::coordination::typeconfig::CoordinationNode,
            >,
        >,
    ) -> LocalFacts {
        let contact = self.last_leader_contact_ms.load(Ordering::SeqCst);
        LocalFacts {
            committed,
            corridor: self.corridor(),
            draining: self.draining.load(Ordering::SeqCst),
            fatal: self.fatal.lock().expect("the fatal latch").clone(),
            follower_contact_age: (contact != u64::MAX).then(|| {
                let now = self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64;
                Duration::from_millis(now.saturating_sub(contact))
            }),
            leader_liveness_timeout: timeout,
            genesis_installed: self.genesis_installed.load(Ordering::SeqCst),
        }
    }
}

fn corridor_code(corridor: Corridor) -> u64 {
    match corridor {
        Corridor::Running => 0,
        Corridor::Bootstrapping => 1,
        Corridor::Joining => 2,
    }
}

fn corridor_of(code: u64) -> Corridor {
    match code {
        1 => Corridor::Bootstrapping,
        2 => Corridor::Joining,
        // Anything else is `Running`. Total rather than panicking: the value is
        // written only by `enter`, so an unrecognised one is unreachable, and a
        // panic in the metrics loop would take the lifecycle surface with it.
        _ => Corridor::Running,
    }
}

/// The lifecycle loop: the only writer of the published view.
///
/// Returns when the Raft metrics channel closes, which happens on
/// `Raft::shutdown()`. Dropping the publisher then releases every outstanding
/// `wait_ready()` with `ShutDown` rather than stranding it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_lifecycle(
    raft: Raft<CoordinationTypeConfig>,
    facts: Arc<RuntimeFacts>,
    publisher: LifecyclePublisher,
    allowlist: Arc<SenderAllowlist>,
    local: CoordinationNodeId,
    timeout: Duration,
    state: crate::coordination::storage::machine::StateHandle,
    generation: Arc<AtomicU64>,
    genesis: Option<Arc<Vec<u8>>>,
    store_metrics: std::sync::Arc<crate::coordination::store::host::StoreMetrics>,
) {
    let mut metrics = raft.metrics();
    // §14 transition counters: observed here because this loop is the one
    // reader of every metrics change.
    let mut last_leader: Option<CoordinationNodeId> = None;
    let mut last_quorum: Option<bool> = None;
    // The controller task for the generation this node currently holds.
    // Aborted and replaced on every leadership change, which is what makes
    // "one leader-generation task" true rather than aspirational.
    let mut controller: Option<tokio::task::JoinHandle<()>> = None;
    // The §9.2 expiry loop, likewise per leader generation.
    let mut expiry: Option<tokio::task::JoinHandle<()>> = None;
    // The in-flight genesis proposal, if any. Held so a second is not spawned
    // beside it: a proposal into a group without a quorum does not fail, it
    // waits, and one metrics change per heartbeat would otherwise queue an
    // unbounded number of identical entries behind it.
    let mut proposing: Option<tokio::task::JoinHandle<()>> = None;
    let mut leading = false;
    loop {
        // Observe the genesis before classifying. Applying an entry moves
        // `last_applied`, which is what wakes this loop, so this needs no timer
        // of its own — and the read is skipped entirely once latched.
        //
        // Before the classification rather than after it, so the loop keeps
        // exactly one publisher: a second `publish` for the genesis would make
        // the view have two writers, which is the thing this loop exists to
        // prevent. A genesis observed a tick late costs one heartbeat.
        if !facts.genesis_installed() {
            match state.genesis_installed().await {
                Ok(true) => facts.note_genesis_installed(),
                Ok(false) => {}
                // **A failed read is not "not installed".**
                //
                // Swallowing it as `false` would hold this node at `NoQuorum`
                // for as long as the store stayed broken — reporting a
                // condition a later message repairs, for one that no message
                // does. The state actor latches fatal on its own failures and
                // answers `ActorStopped` afterwards, so this is the same
                // failure arriving at the one loop that can publish it.
                Err(err) => facts.latch_fatal(FatalCause::Storage(format!(
                    "the genesis state could not be read: {err}"
                ))),
            }
        }

        {
            let current = metrics.borrow_and_update().clone();
            if current.current_leader.is_some() && current.current_leader != last_leader {
                crate::coordination::store::host::StoreMetrics::bump(&store_metrics.leader_changes);
            }
            last_leader = current.current_leader;

            // §7.2's graduation: the allowlist stops reading the signed seeds
            // once a committed membership contains this node.
            //
            // **From the state actor**, which applies committed entries and
            // nothing else. This read was `current.membership_config`, which
            // OpenRaft populates from `membership_state.effective()` — the
            // newest membership entry seen, committed or not. Graduation is
            // permanent, so an entry a leader appended and a quorum never
            // accepted could retire this node's signed-seed check for good, and
            // rolling that entry back would not bring it back. The corridor
            // transitions below read the same value for the same reason: §7.2
            // says "a *committed* membership contains us", and an uncommitted
            // one saying so is not that.
            let committed = state.committed_membership();
            // **Every node, not every voter.** §7.2 makes the event "a committed
            // membership contains this node", and §7.3 adds a candidate as a
            // *learner* first and promotes it only after catch-up. Reading
            // `voter_ids()` meant a joining node never graduated during the one
            // interval where graduation matters: it stayed on the signed seed
            // allowlist, which admits only its `bootstrap_senders`, so a
            // leadership change to a voter outside that list left the joiner
            // unable to be spoken to by the leader responsible for catching it
            // up — wedged, with nothing in its own state saying why.
            //
            // The same set drives the corridor exit below for the same reason:
            // a `Joining` node becomes `Running` when the group has committed
            // its membership, in whichever role.
            let members: BTreeSet<CoordinationNodeId> =
                committed.membership().nodes().map(|(id, _)| *id).collect();
            allowlist.observe(&local, &members);

            // A corridor ends on an observable durable fact, never on a timer.
            // `Bootstrapping` ends when initialization commits — that is,
            // when a membership exists at all. `Joining` ends when a committed
            // membership contains us.
            match facts.corridor() {
                Corridor::Bootstrapping if !members.is_empty() => facts.enter(Corridor::Running),
                Corridor::Joining if members.contains(&local) => facts.enter(Corridor::Running),
                _ => {}
            }

            // §7.3's controller is per leader generation. Acquiring leadership
            // starts one; losing it aborts the current one **before** the next
            // begins, so two generations never run side by side — the failure
            // that makes "the controller compares state after every retry"
            // false, because two of them would be comparing against each
            // other's half-finished work.
            let is_leader =
                matches!(current.state, openraft::ServerState::Leader) && current.id == local;
            if is_leader != leading {
                if let Some(task) = controller.take() {
                    task.abort();
                }
                if let Some(task) = expiry.take() {
                    task.abort();
                }
                // Superseding the generation stops the old task even if its
                // abort has not landed yet: it rechecks this counter before
                // every side effect.
                let next = generation.fetch_add(1, Ordering::SeqCst) + 1;
                if is_leader {
                    controller = Some(tokio::spawn(super::membership::run_generation(
                        raft.clone(),
                        state.clone(),
                        local,
                        super::membership::Generation(next),
                        Arc::clone(&generation),
                        Arc::clone(&facts),
                    )));
                    // A fresh generation starts with no timers, so every held
                    // row gets a fresh full TTL (§9.2): failover may delay a
                    // takeover, never overlap one.
                    expiry = Some(tokio::spawn(super::expiry::run_generation(
                        raft.clone(),
                        state.clone(),
                        next,
                        Arc::clone(&generation),
                        std::sync::Arc::clone(&store_metrics),
                    )));
                }
                leading = is_leader;
            }

            // The bootstrap proposal (§7.1). Only a bootstrapping voter holds
            // the document, only a leader can propose, and applying it is what
            // makes `facts.genesis_installed()` true — on this node and, by
            // replication, on every other. Exact replay is idempotent, so a
            // second leader proposing it after a leadership change is safe;
            // that is why this can simply retry rather than having to know
            // whether an earlier attempt survived.
            // `!facts.genesis_installed()` **is** the revision-0 gate: the fact
            // is set from `admin_revision > 0`, so a healthy restart into a
            // long-running group proposes nothing, and only a voter that came
            // back before the entry applied still has work to do.
            if is_leader
                && !facts.genesis_installed()
                && proposing.as_ref().is_none_or(|task| task.is_finished())
            {
                if let Some(document) = genesis.clone() {
                    let raft = raft.clone();
                    proposing = Some(tokio::spawn(async move {
                        let command = crate::coordination::contract::CommandV1::Genesis(
                            crate::coordination::contract::GenesisCommandV1 {
                                signed_genesis: document.as_ref().clone(),
                            },
                        );
                        // The outcome is not inspected. A refusal is fatal on
                        // every voter including this one, and would arrive as a
                        // `Fatal` through the same latch every other storage
                        // failure uses; a timeout means no quorum yet, and the
                        // next metrics change tries again.
                        let _ = raft.client_write(command).await;
                    }));
                }
            }

            let view = classify(
                &current,
                &facts.snapshot(timeout, std::sync::Arc::clone(&committed)),
            );
            if last_quorum == Some(true) && view.quorum_available == Some(false) {
                crate::coordination::store::host::StoreMetrics::bump(&store_metrics.quorum_losses);
            }
            if let Some(quorum) = view.quorum_available {
                last_quorum = Some(quorum);
            }
            publisher.publish(view);
        }

        if metrics.changed().await.is_err() {
            for task in [controller.take(), proposing.take()].into_iter().flatten() {
                task.abort();
            }
            return;
        }
    }
}

/// The running voter: one owner for everything Slice 3 starts.
///
/// **Field order is shutdown order.** See the module docs.
pub(crate) struct CoordinationRuntime {
    raft: Raft<CoordinationTypeConfig>,
    facts: Arc<RuntimeFacts>,
    readiness: Readiness,
    endpoint: RaftEndpoint,
    store_endpoint: crate::coordination::store::host::StoreEndpoint,
    store_metrics: std::sync::Arc<crate::coordination::store::host::StoreMetrics>,
    pressure: std::sync::Arc<crate::coordination::store::host::PressureState>,
    local: CoordinationNodeId,
    /// The leader generation counter. Bumping it is what makes a superseded
    /// controller stop at its next side effect, whether or not its abort has
    /// landed.
    generation: Arc<AtomicU64>,

    /// 1. Controllers stop first. A controller that outlived the Raft would
    ///    submit a membership change into a shut-down instance and report a
    ///    failure that is really our own shutdown.
    controllers: Vec<tokio::task::JoinHandle<()>>,
    /// 2. Then the lifecycle loop, which holds the publisher: dropping it is
    ///    what releases outstanding `wait_ready()` calls.
    lifecycle: Option<tokio::task::JoinHandle<()>>,
    /// 3. Then storage — its own field order stops both actors and drains the
    ///    read leases before releasing the directory lock.
    storage: crate::coordination::storage::VoterStorage,
}

impl CoordinationRuntime {
    /// Assemble a runtime around an already-open store and an already-created
    /// `Raft`.
    ///
    /// Takes both by value rather than creating them, because §3.3's order puts
    /// service registration between them: the endpoint must be attachable
    /// before the `Raft` exists, and this is the step that attaches it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn attach(
        raft: Raft<CoordinationTypeConfig>,
        storage: crate::coordination::storage::VoterStorage,
        endpoint: RaftEndpoint,
        store_endpoint: crate::coordination::store::host::StoreEndpoint,
        attached: AttachedRuntime,
        facts: Arc<RuntimeFacts>,
        allowlist: Arc<SenderAllowlist>,
        local: CoordinationNodeId,
        config: &openraft::Config,
        // The signed genesis, for a voter that is bootstrapping. `None` for
        // `Join` and `Existing`: neither founds a group, so neither proposes
        // the entry — they receive it like any other committed state.
        genesis: Option<Arc<Vec<u8>>>,
    ) -> Self {
        let state = attached.state.clone();
        let group = attached.group;
        let state_db = attached
            .layout
            .database(crate::coordination::storage::error::Db::State);
        let store_metrics =
            std::sync::Arc::new(crate::coordination::store::host::StoreMetrics::default());
        let pressure =
            std::sync::Arc::new(crate::coordination::store::host::PressureState::default());
        let buckets = std::sync::Arc::new(crate::coordination::store::host::Buckets::default());
        let snapshots = attached.layout.snapshots();
        let raft_db = attached
            .layout
            .database(crate::coordination::storage::error::Db::Raft);
        let generation = Arc::new(AtomicU64::new(0));
        let timeout = leader_liveness_timeout(config);

        // The initial view is the state `start()` may legitimately return in.
        // Never `Ready`: readiness is observed, and `channel` asserts it.
        let initial = LifecycleView {
            code: match facts.corridor() {
                Corridor::Bootstrapping => LifecycleCode::Bootstrapping,
                Corridor::Joining => LifecycleCode::Joining,
                Corridor::Running => LifecycleCode::NoQuorum,
            },
            local_role: LocalRole::None,
            quorum_available: None,
            fatal: None,
        };
        let (publisher, readiness) = channel(initial);

        endpoint.attach(attached).await;
        store_endpoint
            .attach(crate::coordination::store::host::AttachedStore {
                raft: raft.clone(),
                state: state.clone(),
                group,
                local,
                readiness: readiness.clone(),
                metrics: std::sync::Arc::clone(&store_metrics),
                pressure: std::sync::Arc::clone(&pressure),
                buckets,
            })
            .await;

        // The §11.5 pressure monitor. A controller in the ownership sense:
        // stopped at shutdown step 3, harmless during the drain window
        // because all it does is publish two atomics.
        let monitor = tokio::spawn(super::pressure::run(
            state_db,
            raft_db,
            snapshots,
            std::sync::Arc::clone(&pressure),
            Arc::clone(&facts),
            std::sync::Arc::clone(&store_metrics),
        ));

        let lifecycle = tokio::spawn(run_lifecycle(
            raft.clone(),
            Arc::clone(&facts),
            publisher,
            allowlist,
            local,
            timeout,
            state,
            Arc::clone(&generation),
            genesis,
            std::sync::Arc::clone(&store_metrics),
        ));

        Self {
            raft,
            facts,
            readiness,
            endpoint,
            store_endpoint,
            store_metrics,
            pressure,
            local,
            generation,
            controllers: vec![monitor],
            lifecycle: Some(lifecycle),
            storage,
        }
    }

    /// Register a controller task the runtime owns and stops first.
    pub(crate) fn spawn_controller(&mut self, task: tokio::task::JoinHandle<()>) {
        self.controllers.push(task);
    }

    pub(crate) fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    pub(crate) fn store_metrics(
        &self,
    ) -> std::sync::Arc<crate::coordination::store::host::StoreMetrics> {
        std::sync::Arc::clone(&self.store_metrics)
    }

    /// The §11.5 tiers, shared with the Store admission path.
    pub(crate) fn pressure(
        &self,
    ) -> std::sync::Arc<crate::coordination::store::host::PressureState> {
        std::sync::Arc::clone(&self.pressure)
    }

    pub(crate) fn facts(&self) -> &Arc<RuntimeFacts> {
        &self.facts
    }

    pub(crate) fn raft(&self) -> &Raft<CoordinationTypeConfig> {
        &self.raft
    }

    pub(crate) fn local(&self) -> &CoordinationNodeId {
        &self.local
    }

    /// The state actor's handle.
    ///
    /// Reads the Raft metrics cannot answer — durable intent phase, the
    /// installed genesis — come from here, because they are state-machine facts
    /// rather than consensus facts.
    pub(crate) fn state(&self) -> &crate::coordination::storage::machine::StateHandle {
        self.storage.state().handle()
    }

    pub(crate) fn storage(&self) -> &crate::coordination::storage::VoterStorage {
        &self.storage
    }

    /// §7.6's cancellation, in the strict reverse of construction.
    ///
    /// Explicit and `async` rather than left to `Drop`: three of the five steps
    /// need to await, and a `Drop` that blocked on a runtime would deadlock the
    /// executor it is running on. `Drop` still stops the storage actors in the
    /// right order if this is never called — the field order guarantees that —
    /// but a runtime dropped without `shutdown` skips the RPC deregistration,
    /// so a call can reach a `Raft` that is going away.
    pub(crate) async fn shutdown(mut self) {
        // 1. §11.5 step 1: mark draining and stop accepting new Store
        //    mutations — the Store endpoint detaches before anything the
        //    in-flight calls depend on is taken down.
        self.facts.begin_draining();
        self.store_endpoint.detach().await;
        // 2. §11.5 step 2: wait up to five bounded seconds for the Store
        //    calls that were already inside a handler, while the Raft and
        //    controllers they may be awaiting still run.
        self.store_endpoint.drain().await;

        // 3. §11.5 step 3 begins: controllers. Bumping the generation first
        //    stops the membership controller at its next leadership check even
        //    if it is mid-await, which an abort alone cannot guarantee.
        self.generation.fetch_add(1, Ordering::SeqCst);
        for task in std::mem::take(&mut self.controllers) {
            task.abort();
            let _ = task.await;
        }

        //    Deregister the Raft RPC. After this a call is answered
        //    `FailedPrecondition` — the same status a node that has not yet
        //    attached answers, which is right: in both cases the node is up and
        //    not serving Raft.
        self.endpoint.detach().await;

        // 3. The Raft itself.
        let _ = self.raft.shutdown().await;

        // 4. The lifecycle loop, which the metrics channel closing has already
        //    ended. Awaiting it drops the publisher, releasing every
        //    outstanding `wait_ready()` with `ShutDown`.
        if let Some(lifecycle) = self.lifecycle.take() {
            let _ = lifecycle.await;
        }

        // 5. Storage: dropping `self` runs its field order, which stops both
        //    actors and drains the read leases *before* the directory lock.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corridor round-trips through its atomic encoding, for every value.
    ///
    /// The encoding exists so the corridor fits an atomic; a mapping that lost
    /// a variant would silently report `Running` for a node that is still
    /// joining, which precedence says must win over every health question.
    #[test]
    fn every_corridor_survives_its_atomic_encoding() {
        for corridor in [
            Corridor::Running,
            Corridor::Bootstrapping,
            Corridor::Joining,
        ] {
            assert_eq!(corridor_of(corridor_code(corridor)), corridor);
        }
        // Distinct codes, or two corridors would collide.
        let codes: BTreeSet<u64> = [
            Corridor::Running,
            Corridor::Bootstrapping,
            Corridor::Joining,
        ]
        .into_iter()
        .map(corridor_code)
        .collect();
        assert_eq!(codes.len(), 3);
    }

    /// The facts start where the start mode says and move only when told.
    #[test]
    fn the_facts_begin_in_their_start_corridor() {
        let facts = RuntimeFacts::new(Corridor::Bootstrapping);
        assert_eq!(facts.corridor(), Corridor::Bootstrapping);
        facts.enter(Corridor::Running);
        assert_eq!(facts.corridor(), Corridor::Running);
    }

    /// A follower with no contact reports `None`, and one with contact reports
    /// an age that is not in the future.
    #[test]
    fn leader_contact_is_absent_until_it_happens() {
        let facts = RuntimeFacts::new(Corridor::Running);
        let timeout = Duration::from_millis(3_000);
        assert_eq!(
            facts
                .snapshot(timeout, std::sync::Arc::new(Default::default()))
                .follower_contact_age,
            None
        );

        facts.note_leader_contact();
        let age = facts
            .snapshot(timeout, std::sync::Arc::new(Default::default()))
            .follower_contact_age
            .expect("contact was recorded");
        assert!(
            age <= Duration::from_secs(1),
            "a just-recorded contact must be fresh, got {age:?}"
        );
    }

    /// The **first** fatal cause wins: it is the one that explains the others.
    ///
    /// A latch that kept the most recent would report a downstream consequence
    /// — the Raft stopping — instead of the storage failure that stopped it.
    #[test]
    fn the_first_fatal_cause_is_the_one_that_is_kept() {
        let facts = RuntimeFacts::new(Corridor::Running);
        assert_eq!(
            facts
                .snapshot(Duration::ZERO, std::sync::Arc::new(Default::default()))
                .fatal,
            None
        );

        facts.latch_fatal(FatalCause::Storage("out of disk".into()));
        facts.latch_fatal(FatalCause::Raft("stopped".into()));
        assert_eq!(
            facts
                .snapshot(Duration::ZERO, std::sync::Arc::new(Default::default()))
                .fatal,
            Some(FatalCause::Storage("out of disk".into())),
            "the cause that explains the others must survive"
        );
    }

    /// Draining is one-way and visible to the classifier.
    #[test]
    fn draining_is_one_way() {
        let facts = RuntimeFacts::new(Corridor::Running);
        assert!(
            !facts
                .snapshot(Duration::ZERO, std::sync::Arc::new(Default::default()))
                .draining
        );
        facts.begin_draining();
        assert!(
            facts
                .snapshot(Duration::ZERO, std::sync::Arc::new(Default::default()))
                .draining
        );
    }

    /// **Field order is shutdown order**, and the order is §7.6's.
    ///
    /// A statement about the source, because Rust's drop order is a property of
    /// the declaration rather than of any behaviour a test can observe: a
    /// reordered struct compiles, passes every functional test, and releases
    /// the directory lock while an actor still holds a database open. The
    /// storage layer guards its own field order the same way and for the same
    /// reason.
    #[test]
    fn the_field_order_is_the_shutdown_order() {
        let source = include_str!("owner.rs");
        let at = source
            .find("pub(crate) struct CoordinationRuntime {")
            .expect("the runtime struct exists");
        let body = &source[at..at + source[at..].find("\n}").expect("a close")];

        let positions: Vec<(&str, usize)> = ["controllers:", "lifecycle:", "storage:"]
            .into_iter()
            .map(|field| {
                (
                    field,
                    body.find(field)
                        .unwrap_or_else(|| panic!("{field} must be a field")),
                )
            })
            .collect();

        for pair in positions.windows(2) {
            assert!(
                pair[0].1 < pair[1].1,
                "{} must be declared before {}: §7.6 stops controllers, then the \
                 lifecycle loop, then storage — and storage releases the directory lock last",
                pair[0].0,
                pair[1].0
            );
        }
    }

    /// The shutdown body performs §7.6's five steps in order.
    ///
    /// The same instrument, for the same reason: the ordering is not observable
    /// from outside without a real cluster, and a reordered body deadlocks or
    /// races rather than failing an assertion.
    #[test]
    fn the_shutdown_body_follows_the_reverse_of_construction() {
        let source = include_str!("owner.rs");
        let at = source
            .find("pub(crate) async fn shutdown(")
            .expect("shutdown exists");
        let body = &source[at..];

        let steps = [
            "begin_draining()",
            "endpoint.detach()",
            "raft.shutdown()",
            "lifecycle.take()",
        ];
        let mut last = 0usize;
        for step in steps {
            let at = body
                .find(step)
                .unwrap_or_else(|| panic!("shutdown must {step}"));
            assert!(
                at > last,
                "{step} is out of §7.6's order: controllers, RPC, Raft, lifecycle, storage"
            );
            last = at;
        }
    }

    /// The lifecycle loop is the only writer of the published view.
    ///
    /// Two writers would let a node disagree with itself about its own state,
    /// which is the failure `classify` exists to prevent one level down.
    #[test]
    fn only_the_lifecycle_loop_publishes() {
        let source = include_str!("owner.rs");
        let end = source
            .rfind("\n#[cfg(test)]\nmod ")
            .map(|at| at + 1)
            .unwrap_or(source.len());
        let sites: Vec<usize> = source[..end]
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("publisher.publish(")
            })
            .map(|(n, _)| n + 1)
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "the lifecycle view must have exactly one writer; found {sites:?}"
        );
    }
}
