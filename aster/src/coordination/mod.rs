//! Coordination: the replicated register/lease group.
//!
//! `coordination-client` compiles the wire contract, the validated identifier
//! types, and (later) the client stub. `coordination` adds the voter — the
//! replicated state machine and its redb storage.

/// Source-level guards over the whole subsystem: the rules that cannot be
/// checked at runtime, with one inventory of every file they read.
#[cfg(all(test, feature = "coordination"))]
pub(crate) mod guards;

/// The public durable client (§3.4). Client-graph only.
pub mod client;
pub mod contract;
pub mod hash;
pub mod ids;
pub mod reason;
pub mod rowstream;
pub mod service;
pub mod store;

/// The §8.4 verified admission source. Voter-only: a client is admitted *by* a
/// voter and consults no directory itself.
#[cfg(feature = "coordination")]
pub mod admission;

/// Host-side configuration for a voter. Needs `coordination`: it names a
/// storage directory and is accepted only by
/// [`builtin_coordination`](crate::rpc::AsterServerBuilder::builtin_coordination).
#[cfg(feature = "coordination")]
pub mod host;

/// The replicated state machine. A client evaluates no transition, so
/// `coordination-client` compiles none of them — but §12.1's logical limits
/// and the §8.1 rights bits are contract facts, and `model::limits` compiles
/// in both graphs. Every other submodule stays voter-only; see `model/mod.rs`.
pub mod model;

/// The OpenRaft type configuration. Voter-only, so it needs `coordination`
/// rather than `coordination-client`.
/// §5.3.1 OpenRaft mirrors, the §5.3 wire messages, and the §11 storage roots.
///
/// Compiled by `coordination-client` as well as `coordination`: these are plain
/// Fory declarations with no `openraft` dependency, and `rowstream`'s snapshot
/// and recovery bodies nest the storage roots. Only the conversions to and from
/// real OpenRaft values need the voter feature, and they live in
/// [`super::convert`].
pub mod raft;

/// Conversions between OpenRaft values and the §5.3.1 mirrors. Voter-only: it
/// is the one place `openraft` types appear outside the type config.
#[cfg(feature = "coordination")]
pub mod convert;

/// The three start corridors and the §3.3 startup order (§7.2). Voter-only
/// and crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod boot;

/// The authenticated Raft transport over Aster RPC (§5.3, §5.4). Voter-only
/// and crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod network;

/// The voter runtime: pinned OpenRaft configuration, lifecycle and readiness
/// (§7.5, §7.6). Voter-only and crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod runtime;

/// Resolving a configured start mode into a verified storage binding (§7.2).
/// Crate-private: it produces a `StorageIdentity`, which is itself crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod start;

#[cfg(feature = "coordination")]
pub mod typeconfig;

/// Durable redb storage: schema, log, state machine, snapshots (§11).
/// Voter-only and crate-private — Slice 3 owns the public start
/// configuration, so nothing here is nameable by a consumer yet.
#[cfg(feature = "coordination")]
pub(crate) mod storage;

/// The Slice 3 internal test harness (§S3-E, §S3-F). Never enabled by a
/// shipping profile; see the module docs for why it is one method.
#[cfg(feature = "coordination-testing")]
pub mod testing;

/// A handle on a running coordination voter (§3.3).
///
/// The public surface Slice 3 owns: readiness, and the lifecycle a caller
/// needs to interpret it. Obtained from
/// [`AsterServer::coordination`](crate::rpc::AsterServer::coordination); the
/// runtime itself stays owned by the server, so dropping this handle never
/// orphans a voter task.
#[cfg(feature = "coordination")]
#[derive(Clone)]
pub struct Coordination {
    readiness: runtime::readiness::Readiness,
    store_metrics: std::sync::Arc<store::host::StoreMetrics>,
    /// The handle onto the running `Raft` — read here **only** for the §14
    /// consensus snapshot; every mutation path stays behind the RPC service.
    /// Cloning a `Raft` is an `Arc` bump.
    raft: openraft::Raft<typeconfig::CoordinationTypeConfig>,
    /// The state actor, for reads the Raft metrics cannot answer — the §14
    /// genesis hash and audit floor, and the testing seams.
    state: storage::machine::StateHandle,
    /// The §11.5 tiers and §14 disk gauges.
    pressure: std::sync::Arc<store::host::PressureState>,
}

#[cfg(feature = "coordination")]
impl Coordination {
    pub(crate) fn of(rt: &runtime::owner::CoordinationRuntime) -> Self {
        Self {
            readiness: rt.readiness(),
            store_metrics: rt.store_metrics(),
            raft: rt.raft().clone(),
            state: rt.state().clone(),
            pressure: rt.pressure(),
        }
    }

    /// Force the §11.5 tiers, overriding what the monitor measured.
    ///
    /// The override is OR'd with the measured tiers, so forcing `false`
    /// releases the override rather than masking a genuinely full disk.
    /// Forcing the **hard** tier is one-way in effect: the monitor's next
    /// pass latches it and evacuates the runtime, exactly as a genuinely
    /// full disk would, and withdrawing the force afterwards un-drains
    /// nothing.
    #[cfg(feature = "coordination-testing")]
    pub fn set_pressure_for_testing(&self, growth: bool, hard: bool) {
        self.pressure
            .force_growth
            .store(growth, std::sync::atomic::Ordering::Relaxed);
        self.pressure
            .force_hard
            .store(hard, std::sync::atomic::Ordering::Relaxed);
    }

    /// Wait until this voter is `Ready`.
    ///
    /// **Level-triggered, and not a latch** (§7.6). It returns immediately if
    /// the node is already `Ready`, and otherwise waits for the next transition
    /// into `Ready`. It promises nothing about the future: a node may be
    /// `Ready` when this returns and `NoQuorum` a moment later. A caller
    /// needing a durable guarantee holds a fence.
    ///
    /// Returns an error if the node latches `Fatal` — no later message repairs
    /// that, so waiting would hang with the cause recorded nowhere — or if the
    /// runtime shuts down.
    pub async fn wait_ready(&self) -> crate::Result<()> {
        let mut readiness = self.readiness.clone();
        readiness.wait_ready().await.map_err(Into::into)
    }

    /// The §5.2 lifecycle code this node currently reports, without waiting.
    ///
    /// `1 Bootstrapping`, `2 Joining`, `3 Ready`, `4 NoQuorum`, `5 Draining`,
    /// `6 Fatal`.
    pub fn lifecycle_code(&self) -> i32 {
        self.readiness.code().code()
    }

    /// Whether this node holds **direct evidence** that a quorum is reachable
    /// (§7.6).
    ///
    /// `Some` on a leader, from OpenRaft's own `millis_since_quorum_ack`;
    /// `None` on a follower, which holds no such evidence. A plain `bool` would
    /// force a follower stranded in a minority partition to claim a quorum it
    /// cannot observe.
    pub fn quorum_available(&self) -> Option<bool> {
        self.readiness.view().quorum_available
    }

    /// A snapshot of the bounded Store counters (§14).
    ///
    /// Monotonic operation counts only — no value, resource key, or actor is
    /// ever recorded, so metrics can be exported without touching the audit
    /// boundary.
    pub fn store_metrics(&self) -> StoreMetricsSnapshot {
        use std::sync::atomic::Ordering;
        let m = &self.store_metrics;
        StoreMetricsSnapshot {
            proposed: m.proposed.load(Ordering::Relaxed),
            replayed: m.replayed.load(Ordering::Relaxed),
            denied_typed: m.denied_typed.load(Ordering::Relaxed),
            denied_status: m.denied_status.load(Ordering::Relaxed),
            redirected: m.redirected.load(Ordering::Relaxed),
            reads_served: m.reads_served.load(Ordering::Relaxed),
            pressure_refusals: m.pressure_refusals.load(Ordering::Relaxed),
            granted: m.granted.load(Ordering::Relaxed),
            renewed: m.renewed.load(Ordering::Relaxed),
            mutated: m.mutated.load(Ordering::Relaxed),
            released: m.released.load(Ordering::Relaxed),
            revoked: m.revoked.load(Ordering::Relaxed),
            denied_auth: m.denied_auth.load(Ordering::Relaxed),
            denied_stale_fence: m.denied_stale_fence.load(Ordering::Relaxed),
            denied_revision: m.denied_revision.load(Ordering::Relaxed),
            denied_membership: m.denied_membership.load(Ordering::Relaxed),
            denied_admission: m.denied_admission.load(Ordering::Relaxed),
            denied_rate_limited: m.denied_rate_limited.load(Ordering::Relaxed),
            expire_proposed: m.expire_proposed.load(Ordering::Relaxed),
            expiry_rearms: m.expiry_rearms.load(Ordering::Relaxed),
            self_limit_expirations: m.self_limit_expirations.load(Ordering::Relaxed),
            leader_changes: m.leader_changes.load(Ordering::Relaxed),
            quorum_losses: m.quorum_losses.load(Ordering::Relaxed),
            command_latency_micros: m.command_latency_micros.load(Ordering::Relaxed),
            commands_timed: m.commands_timed.load(Ordering::Relaxed),
            read_latency_micros: m.read_latency_micros.load(Ordering::Relaxed),
            reads_timed: m.reads_timed.load(Ordering::Relaxed),
        }
    }

    /// The §14 disk gauges and pressure tiers, as the monitor last sampled
    /// them: byte sizes and free-space state, never file contents.
    pub fn disk_status(&self) -> DiskStatusSnapshot {
        use std::sync::atomic::Ordering;
        let p = &self.pressure;
        DiskStatusSnapshot {
            state_db_bytes: p.state_db_bytes.load(Ordering::Relaxed),
            raft_db_bytes: p.raft_db_bytes.load(Ordering::Relaxed),
            snapshot_bytes: p.snapshot_bytes.load(Ordering::Relaxed),
            free_space_bytes: p.free_space_bytes.load(Ordering::Relaxed),
            growth_pressure: p.growth.load(Ordering::Relaxed)
                || p.force_growth.load(Ordering::Relaxed),
            free_space_pressure: p.hard.load(Ordering::Relaxed)
                || p.force_hard.load(Ordering::Relaxed),
        }
    }

    /// The §14 consensus snapshot: the group's genesis identity, full log
    /// positions (last/committed/applied), the audit floor, leadership,
    /// quorum age, learner membership and per-peer replication progress —
    /// identifiers and indexes only, advisory like every local field.
    ///
    /// `async` because two of the §14 facts do not live in `RaftMetrics`:
    /// the last/committed log ids come from the Raft's own state, and the
    /// genesis hash and audit floor from the state machine.
    pub async fn consensus_status(&self) -> ConsensusSnapshot {
        let metrics = self.raft.metrics().borrow().clone();
        let of = |id: openraft::LogId<ids::CoordinationNodeId>| LogPosition {
            term: id.leader_id.term,
            index: id.index,
        };
        let last_log = self.raft.data_metrics().borrow().last_log;
        let committed = self
            .raft
            .with_raft_state(|state| state.committed)
            .await
            .unwrap_or(None);
        let voters: std::collections::BTreeSet<ids::CoordinationNodeId> =
            metrics.membership_config.membership().voter_ids().collect();
        let learners: Vec<[u8; 32]> = metrics
            .membership_config
            .membership()
            .nodes()
            .filter(|(id, _)| !voters.contains(id))
            .map(|(id, _)| *id.as_bytes())
            .collect();
        let observability = self.state.observability().await.ok();
        ConsensusSnapshot {
            leader_node_id: metrics.current_leader.map(|n| *n.as_bytes()),
            term: metrics.vote.leader_id().get_term(),
            genesis_hash: observability.map(|o| o.genesis_hash),
            genesis_installed: observability.is_some_and(|o| o.genesis_installed),
            audit_floor_index: observability.and_then(|o| o.audit_floor_index),
            last_log: last_log.map(of),
            committed: committed.map(of),
            last_applied: metrics.last_applied.map(of),
            last_log_index: metrics.last_log_index,
            last_applied_index: metrics.last_applied.map(|id| id.index),
            snapshot_index: metrics.snapshot.map(|id| id.index),
            millis_since_quorum_ack: metrics.millis_since_quorum_ack,
            learners,
            replication: metrics
                .replication
                .map(|peers| {
                    peers
                        .into_iter()
                        .map(|(node, matched)| (*node.as_bytes(), matched.map(|id| id.index)))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// One sample of the §14 disk gauges.
#[cfg(feature = "coordination")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskStatusSnapshot {
    pub state_db_bytes: u64,
    pub raft_db_bytes: u64,
    pub snapshot_bytes: u64,
    pub free_space_bytes: u64,
    /// §11.5 tier one: the state database reached its high-water mark.
    pub growth_pressure: bool,
    /// §11.5 tier two: free space fell below the snapshot-replacement floor.
    pub free_space_pressure: bool,
}

/// One log position: the §14 "id" spelling — term and index — without the
/// leader NodeId (which `leader_node_id` and `replication` already carry).
#[cfg(feature = "coordination")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogPosition {
    pub term: u64,
    pub index: u64,
}

/// One sample of the §14 consensus facts (advisory; from `RaftMetrics`, the
/// Raft state, and the state machine's own bounded facts).
#[cfg(feature = "coordination")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsensusSnapshot {
    pub leader_node_id: Option<[u8; 32]>,
    pub term: u64,
    /// The group's genesis hash, from the bound authority; `None` only when
    /// the state actor is unreachable (fatal or shutting down).
    pub genesis_hash: Option<[u8; 32]>,
    /// Whether the committed genesis has been applied on this voter.
    pub genesis_installed: bool,
    /// The lowest retained audit row's log index; `None` before any pruning
    /// (the full history is retained) or when the state actor is unreachable.
    pub audit_floor_index: Option<u64>,
    /// The last appended, committed and applied log ids in full.
    pub last_log: Option<LogPosition>,
    pub committed: Option<LogPosition>,
    pub last_applied: Option<LogPosition>,
    pub last_log_index: Option<u64>,
    pub last_applied_index: Option<u64>,
    /// The last snapshot's log index — the floor a lagging learner restores
    /// from.
    pub snapshot_index: Option<u64>,
    pub millis_since_quorum_ack: Option<u64>,
    /// The committed membership's learners. Paired with `replication`, this
    /// is the §14 learner-progress witness: a learner's matched index
    /// against `last_log_index`.
    pub learners: Vec<[u8; 32]>,
    /// Leader only: per-peer matched log index — the replication lag
    /// witness for voters and learners alike. Empty on followers.
    pub replication: Vec<([u8; 32], Option<u64>)>,
}

/// One sample of the §14 Store counters.
#[cfg(feature = "coordination")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreMetricsSnapshot {
    /// Mutations admitted and proposed through consensus.
    pub proposed: u64,
    /// Requests answered from the saved session result.
    pub replayed: u64,
    /// Typed reason-coded denials.
    pub denied_typed: u64,
    /// Status-only denials, including the §11.5 pressure gate.
    pub denied_status: u64,
    /// Follower answers carrying the redirect hint.
    pub redirected: u64,
    /// Linearizable read/status/stream calls served.
    pub reads_served: u64,
    /// Growth-class mutations refused at the state-database high-water mark.
    pub pressure_refusals: u64,
    // §14: applied register outcomes.
    pub granted: u64,
    pub renewed: u64,
    pub mutated: u64,
    pub released: u64,
    pub revoked: u64,
    // §14: denial families (each also counted in a total above).
    pub denied_auth: u64,
    pub denied_stale_fence: u64,
    pub denied_revision: u64,
    pub denied_membership: u64,
    pub denied_admission: u64,
    /// §14's rate-limit split of `denied_admission`: the two token buckets
    /// alone.
    pub denied_rate_limited: u64,
    // §14: the §9.2 expiry loop.
    pub expire_proposed: u64,
    pub expiry_rearms: u64,
    /// Expiries that applied: leases that reached their holder's own TTL.
    pub self_limit_expirations: u64,
    // §14: consensus transitions.
    pub leader_changes: u64,
    pub quorum_losses: u64,
    // §14: command/read latency, cumulative micros over a count.
    pub command_latency_micros: u64,
    pub commands_timed: u64,
    pub read_latency_micros: u64,
    pub reads_timed: u64,
}
