//! The replicated row set and its two byte counters.
//!
//! Every table here is a `BTreeMap`: apply must be deterministic across voters,
//! and iteration order is observable through snapshot row streams, audit
//! pruning, and intent eviction. A `HashMap` would give each voter its own
//! order and diverge them on the first eviction.

use std::collections::{BTreeMap, BTreeSet};

use super::limits::{self, MAX_ACTIVE_CLIENTS_PER_ACTOR, MAX_ACTIVE_CLIENTS_PER_GROUP};
use super::limits::{MAX_RETAINED_SESSIONS_PER_ACTOR, MAX_RETAINED_SESSIONS_PER_GROUP};
use crate::coordination::reason::{Outcome, Reason};

/// A voter or client node identity (32 bytes).
pub type NodeId = [u8; 32];
/// A client session identity (16 bytes).
pub type ClientId = [u8; 16];

/// The validated resource identity, re-exported so the model has one name for
/// it.
///
/// Deliberately **not** a model-local struct. An earlier revision had one with
/// public fields and maxima-only validation, which let empty and malformed
/// identities into state, saved replies and audit rows -- rows that
/// `coordination::ids::ResourceKey` would then refuse to reconstruct. Durable
/// state holds only values that passed its constructor.
pub use crate::coordination::ids::ResourceKey;

/// A resource row: the lease and the value it guards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceRow {
    pub epoch: u64,
    pub held: bool,
    /// `None` when unheld. Deliberately not a sentinel: 32 zero bytes would be
    /// indistinguishable from a real all-zero NodeId.
    pub holder: Option<NodeId>,
    pub ttl_ms: u64,
    pub lease_revision: u64,
    pub value_revision: u64,
    pub value: Vec<u8>,
    pub acl_revision: u64,
}

/// The lease/value snapshot saved with a consumed sequence, mirroring
/// §5.2.1's `SavedSnapshotV1` field for field.
///
/// **No `value`**, deliberately: saving values here would duplicate every
/// resource value into the reserve-funded session table, and the reserve is
/// sized at `256 + KEY` a row on the assumption that it does not.
///
/// Everything *else* the reply reports is here, and that is the point. An
/// earlier revision stored only the revisions and rebuilt `holder`, `held`,
/// `ttl_ms` and `lease_revision` from **current** state at projection time, so
/// a replay after a later mutation returned the saved revisions stapled to a
/// different holder -- a reply that never existed at any point in the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedSnapshot {
    pub resource: ResourceKey,
    pub epoch: u64,
    pub holder: Option<NodeId>,
    pub held: bool,
    pub ttl_ms: u64,
    pub lease_revision: u64,
    pub value_revision: u64,
    pub acl_revision: u64,
}

impl SavedSnapshot {
    /// The snapshot a row produces at the moment it is written.
    pub fn of(resource: ResourceKey, row: &ResourceRow) -> Self {
        Self {
            resource,
            epoch: row.epoch,
            holder: row.holder,
            held: row.held,
            ttl_ms: row.ttl_ms,
            lease_revision: row.lease_revision,
            value_revision: row.value_revision,
            acl_revision: row.acl_revision,
        }
    }
}

/// The result saved for a consumed sequence (§5.2.1 `SavedReplyV1`).
///
/// Structurally sufficient for an **exact** replay: every field the public
/// reply reports is either here or is the redacted value. Projection reads no
/// business state at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedReply {
    pub outcome: Outcome,
    pub reason: Reason,
    /// The resource snapshot, when the command named one.
    pub snapshot: Option<SavedSnapshot>,
    pub acl_revision: u64,
    pub admin_revision: u64,
    /// The membership operation this reply concerns, if any.
    pub operation_id: Option<OperationId>,
    /// The membership log id this reply observed, if any.
    pub membership_log_id: Option<LogId>,
}

impl SavedReply {
    /// The resource this reply names, if any.
    pub fn resource(&self) -> Option<&ResourceKey> {
        self.snapshot.as_ref().map(|s| &s.resource)
    }

    /// The charge parts of the embedded resource key, if any.
    fn key_parts(&self) -> Option<(&str, &[u8])> {
        self.resource().map(key_parts)
    }
}

/// The `(namespace, key)` pair as the charge formulas take it.
pub fn key_parts(key: &ResourceKey) -> (&str, &[u8]) {
    (key.namespace(), key.key())
}

/// One client session row. Tombstones are session rows with `retired = true`;
/// there is no delete path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub last_sequence: u64,
    pub request_hash: [u8; 32],
    pub last_result: Option<SavedReply>,
    pub retired: bool,
}

/// What an applied entry produced, as the audit table records it.
///
/// §5.2: "the result code is the public outcome code for client commands and a
/// separately declared system phase code for system commands." Those are two
/// disjoint code spaces -- `1 Expired` is not `1 Granted` -- so storing them in
/// one `i32` field without a tag makes an audit row ambiguous about which space
/// it is in. An earlier revision coerced every system result to
/// `Outcome::Rejected`, which recorded a successful expiry as a rejection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditResult {
    /// A client command's §5.2 outcome code.
    Client(Outcome),
    /// A system command's phase code (`SYS_*` in `contract`).
    System(i32),
}

impl AuditResult {
    /// The stored `result_code`.
    pub fn code(self) -> i32 {
        match self {
            AuditResult::Client(outcome) => outcome.code(),
            AuditResult::System(code) => code,
        }
    }

    /// Was this a client command?
    pub fn is_client(self) -> bool {
        matches!(self, AuditResult::Client(_))
    }
}

/// One authoritative audit row, mirroring
/// [`AuditRowV1`](crate::coordination::rowstream::AuditRowV1).
///
/// The conversions in [`super::wire`] are typed both ways, so a field added to
/// either side is a compile error rather than a silent divergence.
///
/// Carries `result` -- the outcome -- and no reason code (§5.2). That is why a
/// reserved reason number could not be disambiguated from audit even in
/// principle, and why the reserved-code argument rests on saved replies and
/// client journals instead.
///
/// `leader_node_id` is stored even though term and index are the table key:
/// together they reconstruct the entry's full [`LogId`] after the log itself has
/// been purged, which is the only place that information then survives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    pub leader_node_id: NodeId,
    /// The actor, when one could be validated.
    ///
    /// `None` **only** for a committed entry whose actor id itself would not
    /// narrow. §11.3 requires one audit row for every committed command, and a
    /// hostile malformed entry is exactly the one that must not vanish from the
    /// record -- so the row is written either way, with attribution marked
    /// absent rather than faked. A zeroed NodeId would have recorded an action
    /// by a node that does not exist.
    pub actor: Option<NodeId>,
    pub operation_code: i32,
    pub resource: Option<ResourceKey>,
    pub result: AuditResult,
    /// The resulting epoch, copied from the saved reply.
    pub epoch: u64,
    /// The resulting value revision, copied from the saved reply.
    pub value_revision: u64,
}

/// A 16-byte membership operation identity (§4.1).
pub type OperationId = [u8; 16];

/// A membership plan: the normative `ChangeMembershipActionV1` payload.
///
/// Stored **typed**, not as a hash and a length. A failover controller has to
/// re-run the plan after a restart, and it cannot reconstruct desired voters,
/// retained learners or member seeds from a digest -- which is what an earlier
/// revision stored.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MembershipPlan {
    pub desired_voters: Vec<NodeId>,
    pub retained_learners: Vec<NodeId>,
    pub member_seeds: Vec<MemberSeed>,
}

impl MembershipPlan {
    /// The encoded length used for the §12.1 intent charge.
    ///
    /// **Measured, not estimated.** This value feeds the replicated growth
    /// counter, so an approximation over field lengths is not merely imprecise:
    /// it undercounts Fory's per-field and per-wrapper framing, which means two
    /// voters agree on the number while the store holds more than the counter
    /// says.
    ///
    /// It measures the **stored** value, so it takes the operation id and
    /// expected membership that go into the row alongside the plan. An earlier
    /// revision encoded a synthetic action with a zeroed operation id and
    /// `expected_membership_log_id: None`, which is not what §11's
    /// `MembershipIntentV1.desired_plan` holds -- so the charge described a
    /// value that is never written.
    ///
    /// Encoding is fallible; a plan that will not encode is charged its declared
    /// maximum rather than zero, because charging zero for something
    /// unmeasurable is the one answer guaranteed to be wrong in the unsafe
    /// direction.
    pub fn encoded_len(
        &self,
        operation_id: &OperationId,
        expected_membership: Option<LogId>,
    ) -> usize {
        use crate::coordination::contract::{
            payload_fory, ChangeMembershipActionV1, LogIdV1, MemberSeedV1, NodeIdV1,
        };

        let wire = ChangeMembershipActionV1 {
            operation_id: operation_id.to_vec(),
            expected_membership_log_id: expected_membership.map(|id| LogIdV1::from(&id)),
            desired_voters: self
                .desired_voters
                .iter()
                .map(|n| NodeIdV1 {
                    node_id: n.to_vec(),
                })
                .collect(),
            retained_learners: self
                .retained_learners
                .iter()
                .map(|n| NodeIdV1 {
                    node_id: n.to_vec(),
                })
                .collect(),
            member_seeds: self
                .member_seeds
                .iter()
                .map(|s| MemberSeedV1 {
                    node_id: s.node_id.to_vec(),
                    ticket: String::from_utf8_lossy(&s.ticket).into_owned(),
                    signed_join_permit: s.join_permit.clone(),
                })
                .collect(),
        };
        payload_fory::<ChangeMembershipActionV1>()
            .serialize(&wire)
            .map_or(limits::MAX_ENCODED_COMMAND, |bytes| bytes.len())
    }
}

/// One member seed inside a plan (§5.2.1 `MemberSeedV1`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberSeed {
    pub node_id: NodeId,
    pub ticket: Vec<u8>,
    pub join_permit: Option<Vec<u8>>,
}

/// A membership intent, mirroring §11.3's `IntentRowV1` field for field.
///
/// Terminal intents are evicted deterministically when a 65th is produced,
/// which is why reason `20` is reserved and never emitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    /// The administrator that submitted it.
    pub actor: NodeId,
    /// The canonical request hash of the submitting command, which is what
    /// §9's "identical plan" comparison is against.
    pub request_hash: [u8; 32],
    /// The membership log id the plan expected. `None` means "no membership
    /// has been committed yet", which is distinct from any particular id.
    pub expected_membership: Option<LogId>,
    pub plan: MembershipPlan,
    /// One of the §5.2 phase codes; see `membership::is_terminal_phase`.
    pub status_code: i32,
    /// The system result code recorded when the intent reached a terminal
    /// phase; `0` while it is still active.
    pub result_code: i32,
    /// The log id at which the intent was accepted.
    pub accepted_log_id: LogId,
    /// The log id at which it reached a terminal phase.
    ///
    /// **Eviction orders by this**, not by acceptance: §9 says terminal intents
    /// are retained newest-first *by their completion log id*, and two intents
    /// accepted in one order can complete in the other.
    pub completed_log_id: Option<LogId>,
    /// The committed membership the controller observed.
    pub observed_membership_log_id: Option<LogId>,
}

impl Intent {
    /// The encoded length this intent is charged for under §12.1.
    ///
    /// Takes the operation id because the row is keyed by it and stores it, so
    /// the charge and the stored value are measured from the same thing.
    pub fn charge_len(&self, operation_id: &OperationId) -> usize {
        self.plan
            .encoded_len(operation_id, self.expected_membership)
    }
}

/// A retained node route (§11.3 `RouteRowV1`), an advisory dialling hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub ticket: Vec<u8>,
    /// The intent that introduced this route.
    pub source_operation_id: OperationId,
    /// Eviction orders oldest-first by this.
    pub introduced_log_id: LogId,
}

/// A grant: the rights one subject holds on one resource.
pub type GrantKey = (ResourceKey, NodeId);

/// A Raft log entry identity: `(term, leader_node_id, index)`.
///
/// All three, because that is what the compiled contract declares
/// ([`LogIdV1`](crate::coordination::contract::LogIdV1)) and what OpenRaft
/// retains under this type config -- its `LogId` is
/// `{ leader_id: { term, node_id }, index }`. An earlier revision of this model
/// carried `(term, index)` only, which cannot round-trip the wire type and
/// cannot reconstruct a full log id from a stored audit row after the log is
/// purged.
///
/// Deliberately **no** `Ord`. Log ordering is by index; deriving a
/// lexicographic comparison over the fields would silently order two entries by
/// term first, which is not what any caller means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LogId {
    pub term: u64,
    /// The leader that created the entry. Part of the identity: two leaders can
    /// each write an entry at the same term and index, and they are different
    /// entries.
    pub leader_node_id: NodeId,
    pub index: u64,
}

impl LogId {
    pub fn new(term: u64, leader_node_id: NodeId, index: u64) -> Self {
        Self {
            term,
            leader_node_id,
            index,
        }
    }
}

/// The complete replicated state.
///
/// `PartialEq` is derived deliberately: tests that assert "nothing changed"
/// compare the whole value, so a field added later is covered without anyone
/// remembering to extend an assertion list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub resources: BTreeMap<ResourceKey, ResourceRow>,
    pub acl: BTreeMap<GrantKey, u64>,
    pub admins: BTreeSet<NodeId>,
    pub sessions: BTreeMap<(NodeId, ClientId), Session>,
    pub intents: BTreeMap<OperationId, Intent>,
    pub routes: BTreeMap<NodeId, Route>,
    /// The full [`LogId`] of the newest audit row, maintained on write.
    ///
    /// An O(1) tail rather than a scan: `apply` consults it on every entry, and
    /// scanning up to 100,000 audit rows each time would make replaying a full
    /// log quadratic. Carrying the whole log id also catches a row at the same
    /// index as `last_applied` but from a different term or leader, which a
    /// maximum-index scan cannot see.
    ///
    /// **Rebuilt and validated from the retained audit rows at database open
    /// and at snapshot install**, not trusted from whatever was last persisted.
    /// That is where a full cross-table consistency check belongs -- it is paid
    /// once per open rather than once per entry -- and it is what makes the
    /// cheap per-apply check above sound: the tip it reads was verified against
    /// the rows it summarises.
    pub audit_tip: Option<LogId>,
    /// Keyed by the normative entry identity `(log_term, log_index)` (§11.2),
    /// which orders pruning deterministically.
    ///
    /// **Not** an oracle for "has this entry been applied?". Audit rows are
    /// prunable, are absent for non-command entries, and are a lossy projection
    /// of the command -- two different `compare_and_set` commands produce
    /// identical rows, because the row carries no value. `last_applied` is the
    /// authority; see [`State::highest_audited_index`].
    pub audit: BTreeMap<(u64, u64), AuditRow>,
    /// Maintained incrementally. MUST equal [`State::recompute_growth`].
    pub growth_bytes: u64,
    /// Maintained incrementally. MUST equal [`State::recompute_reserve`].
    pub reserve_bytes: u64,
    pub admin_revision: u64,
    /// The current committed voter and learner set.
    ///
    /// Kept beside the log id because §9 protects a route for a node **in the
    /// current committed membership** from eviction, and a log id alone cannot
    /// answer "is this node a member?". Storing only the id made that rule
    /// unimplementable.
    pub members: BTreeSet<NodeId>,
    /// The committed membership's log id, or `None` before the first change.
    ///
    /// `change_membership` carries an `expected_membership_log_id`, not a
    /// revision counter -- an earlier revision of this model substituted a
    /// synthetic `u64`, which cannot express "no membership committed yet" and
    /// does not match what the contract compares.
    pub membership_log_id: Option<LogId>,
    /// The complete [`LogId`] of the oldest audit row still retained, or
    /// `None` while nothing has been pruned.
    ///
    /// The frozen type is `Option<LogIdV1>`, so an index alone loses the term
    /// and leader — and `audit_stream` compares a caller's `after_log_id`
    /// against this floor, which means a floor that cannot express a full log
    /// id cannot answer the comparison the contract specifies.
    pub audit_floor: Option<LogId>,
    /// The complete [`LogId`] of the last applied entry, or `None` before the
    /// first.
    ///
    /// Advances on **every** applied entry, including a rejection: a committed
    /// entry that left no trace would be indistinguishable from one that was
    /// skipped. It is written in the same transaction as that entry's effects
    /// and audit row (§11.3), which is what makes it the authoritative answer
    /// to "has this entry been applied?".
    pub last_applied: Option<LogId>,
}

impl State {
    /// Would `added` growth bytes fit the §12.1 budget?
    ///
    /// One place, so every growth-funded table is checked the same way. An
    /// earlier revision checked only resource values, so `set_admin`,
    /// membership intents and routes wrote past a full budget.
    pub fn growth_fits(&self, added: u64) -> bool {
        self.growth_bytes.saturating_add(added) <= limits::GROWTH_BUDGET
    }

    /// `Σ charge` over the growth-funded tables: resources, ACL, admins,
    /// intents, routes.
    ///
    /// Partitioned by **table**, never by the command that wrote the row -- a
    /// row on disk does not record what created it, so any command-dependent
    /// split is unrecomputable and a snapshot could not reproduce the counter.
    pub fn recompute_growth(&self) -> u64 {
        let resources: u64 = self
            .resources
            .iter()
            .map(|(k, r)| limits::charge_resource(k.namespace(), k.key(), r.value.len()))
            .sum();
        let acl: u64 = self
            .acl
            .keys()
            .map(|(k, _)| limits::charge_acl(k.namespace(), k.key()))
            .sum();
        let admins = self.admins.len() as u64 * limits::charge_admin();
        let intents: u64 = self
            .intents
            .iter()
            .map(|(id, i)| limits::charge_intent(i.charge_len(id)))
            .sum();
        let routes: u64 = self
            .routes
            .values()
            .map(|r| limits::charge_route(r.ticket.len()))
            .sum();
        resources + acl + admins + intents + routes
    }

    /// `Σ charge` over the reserve-funded tables: sessions and audit.
    pub fn recompute_reserve(&self) -> u64 {
        let sessions: u64 = self
            .sessions
            .values()
            .map(|s| limits::charge_session(s.last_result.as_ref().and_then(SavedReply::key_parts)))
            .sum();
        let audit: u64 = self
            .audit
            .values()
            .map(|a| limits::charge_audit(a.resource.as_ref().map(key_parts)))
            .sum();
        sessions + audit
    }

    /// Do the incrementally maintained counters equal a full recomputation?
    ///
    /// This is the property snapshot install and log replay depend on. It is
    /// checked in tests after arbitrary histories rather than asserted in
    /// apply, where recomputing on every transition would be O(state).
    pub fn counters_match_recomputation(&self) -> bool {
        self.growth_bytes == self.recompute_growth()
            && self.reserve_bytes == self.recompute_reserve()
    }

    /// Session capacity for `actor`, at both scopes.
    pub fn capacity_for(&self, actor: &NodeId) -> Capacity {
        let retained_group = self.sessions.len() as u64;
        let retained_actor = self.sessions.keys().filter(|(a, _)| a == actor).count() as u64;
        let active_group = self.sessions.values().filter(|s| !s.retired).count() as u64;
        let active_actor = self
            .sessions
            .iter()
            .filter(|((a, _), s)| a == actor && !s.retired)
            .count() as u64;
        Capacity {
            retained_group,
            retained_actor,
            active_group,
            active_actor,
        }
    }
}

/// Session-row capacity at both scopes, as §8.2.1 rows 9 and 10 need it.
///
/// Retained counts **every** session row including tombstones; active counts
/// only non-retired ones. A new row is refused if *either* scope is exhausted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub retained_group: u64,
    pub retained_actor: u64,
    pub active_group: u64,
    pub active_actor: u64,
}

impl Capacity {
    /// Is there no room for another **retained** row, at either scope?
    ///
    /// Retained never decreases in v1 -- no path frees it -- which is what makes
    /// reason `18` terminal.
    pub fn retained_exhausted(&self) -> bool {
        self.retained_group >= MAX_RETAINED_SESSIONS_PER_GROUP
            || self.retained_actor >= MAX_RETAINED_SESSIONS_PER_ACTOR
    }

    /// Is there no room for another **active** row, at either scope?
    ///
    /// Retiring a session frees active capacity without freeing retained, which
    /// is what makes reason `21` retryable where `18` is not.
    pub fn active_exhausted(&self) -> bool {
        self.active_group >= MAX_ACTIVE_CLIENTS_PER_GROUP
            || self.active_actor >= MAX_ACTIVE_CLIENTS_PER_ACTOR
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ns: &str, k: &[u8]) -> ResourceKey {
        ResourceKey::new(ns, k).expect("a valid fixture identity")
    }

    /// A named mutation used to probe which counter a table charges to.
    type TableProbe = (&'static str, fn(&mut State));

    /// The ordering the canonical row stream and redb's tuple comparator both
    /// require. The spike shipped a flattened `len || bytes` key under which
    /// `"b"` sorts before `"aa"`, and its test only asserted non-collision, so
    /// it passed.
    #[test]
    fn resource_keys_order_as_a_tuple_not_as_a_flattened_buffer() {
        let mut keys = vec![
            key("ns", b"b"),
            key("ns", b"aa"),
            key("ns", b"a"),
            key("mz", b"zzzz"),
        ];
        keys.sort();

        let expected = vec![
            key("mz", b"zzzz"),
            key("ns", b"a"),
            key("ns", b"aa"),
            key("ns", b"b"),
        ];
        assert_eq!(keys, expected);

        // The specific inversion a length-prefixed flattening produces.
        assert!(
            key("ns", b"aa") < key("ns", b"b"),
            "\"aa\" must sort before \"b\"; a length-prefixed key inverts this"
        );
        // And the namespace dominates, whatever the key lengths.
        assert!(key("a", b"zzzzzzzz") < key("b", b"a"));
    }

    /// A one-zero-byte key is a real key, not a sentinel -- and an *empty* key
    /// is not constructible at all, which is why no row can carry one.
    #[test]
    fn a_zero_byte_key_is_a_real_key_and_an_empty_one_is_unconstructible() {
        assert_ne!(key("ns", b"\0"), key("ns", b"\0\0"));
        assert!(key("ns", b"\0") < key("ns", b"\0\0"));
        assert!(
            ResourceKey::new("ns", Vec::new()).is_err(),
            "an empty key must never reach a durable row"
        );
    }

    /// Every field is load-bearing: changing any one of the three changes the
    /// identity, so none can be dropped from the model without losing entries.
    #[test]
    fn every_log_id_field_distinguishes_an_entry() {
        let base = LogId::new(1, [1u8; 32], 1);
        assert_ne!(base, LogId::new(2, [1u8; 32], 1), "term");
        assert_ne!(base, LogId::new(1, [2u8; 32], 1), "leader");
        assert_ne!(base, LogId::new(1, [1u8; 32], 2), "index");
    }

    #[test]
    fn an_empty_state_has_zero_counters_and_they_match_recomputation() {
        let s = State::default();
        assert_eq!(s.recompute_growth(), 0);
        assert_eq!(s.recompute_reserve(), 0);
        assert!(s.counters_match_recomputation());
    }

    /// Growth covers exactly the five growth-funded tables, and reserve exactly
    /// the two reserve-funded ones. Asserted by adding one row to each table and
    /// checking which counter moved -- a partition stated in a comment is not
    /// enforced by anything.
    #[test]
    fn each_table_charges_to_the_counter_section_12_1_assigns_it() {
        let growth_tables: Vec<TableProbe> = vec![
            ("resources", |s| {
                s.resources.insert(key("ns", b"k"), ResourceRow::default());
            }),
            ("acl", |s| {
                s.acl.insert((key("ns", b"k"), [1u8; 32]), 1);
            }),
            ("admins", |s| {
                s.admins.insert([2u8; 32]);
            }),
            ("intents", |s| {
                s.intents.insert(
                    [3u8; 16],
                    Intent {
                        actor: [1u8; 32],
                        request_hash: [0u8; 32],
                        expected_membership: None,
                        plan: MembershipPlan {
                            desired_voters: vec![[1u8; 32]],
                            retained_learners: Vec::new(),
                            member_seeds: Vec::new(),
                        },
                        status_code: 1,
                        result_code: 0,
                        accepted_log_id: LogId::new(1, [0u8; 32], 1),
                        completed_log_id: None,
                        observed_membership_log_id: None,
                    },
                );
            }),
            ("routes", |s| {
                s.routes.insert(
                    [4u8; 32],
                    Route {
                        ticket: vec![0u8; 20],
                        source_operation_id: [0u8; 16],
                        introduced_log_id: LogId::new(1, [0u8; 32], 1),
                    },
                );
            }),
        ];

        for (name, insert) in growth_tables {
            let mut s = State::default();
            insert(&mut s);
            assert!(s.recompute_growth() > 0, "{name} must charge to growth");
            assert_eq!(
                s.recompute_reserve(),
                0,
                "{name} must not charge to the reserve"
            );
        }

        let reserve_tables: Vec<TableProbe> = vec![
            ("sessions", |s| {
                s.sessions.insert(
                    ([5u8; 32], [6u8; 16]),
                    Session {
                        last_sequence: 1,
                        request_hash: [0u8; 32],
                        last_result: None,
                        retired: false,
                    },
                );
            }),
            ("audit", |s| {
                s.audit.insert(
                    (1, 0),
                    AuditRow {
                        leader_node_id: [8u8; 32],
                        actor: Some([7u8; 32]),
                        operation_code: 1,
                        resource: None,
                        result: AuditResult::Client(Outcome::Granted),
                        epoch: 1,
                        value_revision: 0,
                    },
                );
            }),
        ];

        for (name, insert) in reserve_tables {
            let mut s = State::default();
            insert(&mut s);
            assert!(
                s.recompute_reserve() > 0,
                "{name} must charge to the reserve"
            );
            assert_eq!(s.recompute_growth(), 0, "{name} must not charge to growth");
        }
    }

    /// A session row's charge follows the key stored in its saved reply, not the
    /// command that wrote it -- the property that makes the counter recomputable.
    #[test]
    fn a_sessions_charge_follows_the_key_in_its_stored_row() {
        let mut bare = State::default();
        bare.sessions.insert(
            ([1u8; 32], [1u8; 16]),
            Session {
                last_sequence: 1,
                request_hash: [0u8; 32],
                last_result: None,
                retired: false,
            },
        );

        let mut with_key = State::default();
        with_key.sessions.insert(
            ([1u8; 32], [1u8; 16]),
            Session {
                last_sequence: 1,
                request_hash: [0u8; 32],
                last_result: Some(SavedReply {
                    outcome: Outcome::Granted,
                    reason: Reason::None,
                    snapshot: Some(SavedSnapshot::of(
                        key("namespace", b"a-longer-key"),
                        &ResourceRow::default(),
                    )),
                    acl_revision: 0,
                    admin_revision: 0,
                    operation_id: None,
                    membership_log_id: None,
                }),
                retired: false,
            },
        );

        assert_eq!(bare.recompute_reserve(), 256);
        assert_eq!(
            with_key.recompute_reserve(),
            256 + "namespace".len() as u64 + "a-longer-key".len() as u64
        );
    }

    /// Retired rows still count against retained; they do not count against
    /// active. Conflating the two is what let create/retire/repeat grow
    /// permanent rows without tripping either cap.
    #[test]
    fn a_retired_session_consumes_retained_capacity_but_not_active() {
        let actor = [1u8; 32];
        let mut s = State::default();
        for i in 0..5u8 {
            s.sessions.insert(
                (actor, [i; 16]),
                Session {
                    last_sequence: 1,
                    request_hash: [0u8; 32],
                    last_result: None,
                    retired: i % 2 == 0,
                },
            );
        }

        let cap = s.capacity_for(&actor);
        assert_eq!(cap.retained_actor, 5, "tombstones still occupy retained");
        assert_eq!(cap.active_actor, 2, "only non-retired rows are active");
        assert_eq!(cap.retained_group, 5);
        assert_eq!(cap.active_group, 2);
    }

    /// Capacity is per actor as well as per group: one actor must not be able to
    /// consume the whole group allowance.
    #[test]
    fn capacity_is_counted_per_actor_as_well_as_per_group() {
        let mine = [1u8; 32];
        let theirs = [2u8; 32];
        let mut s = State::default();
        for i in 0..3u8 {
            s.sessions.insert(
                (mine, [i; 16]),
                Session {
                    last_sequence: 1,
                    request_hash: [0u8; 32],
                    last_result: None,
                    retired: false,
                },
            );
        }
        s.sessions.insert(
            (theirs, [9u8; 16]),
            Session {
                last_sequence: 1,
                request_hash: [0u8; 32],
                last_result: None,
                retired: false,
            },
        );

        let cap = s.capacity_for(&mine);
        assert_eq!((cap.retained_actor, cap.retained_group), (3, 4));
        assert_eq!((cap.active_actor, cap.active_group), (3, 4));
    }

    /// Either scope alone exhausts capacity.
    #[test]
    fn either_scope_alone_exhausts_capacity() {
        let at_group = Capacity {
            retained_group: MAX_RETAINED_SESSIONS_PER_GROUP,
            retained_actor: 0,
            active_group: MAX_ACTIVE_CLIENTS_PER_GROUP,
            active_actor: 0,
        };
        assert!(at_group.retained_exhausted());
        assert!(at_group.active_exhausted());

        let at_actor = Capacity {
            retained_group: 0,
            retained_actor: MAX_RETAINED_SESSIONS_PER_ACTOR,
            active_group: 0,
            active_actor: MAX_ACTIVE_CLIENTS_PER_ACTOR,
        };
        assert!(at_actor.retained_exhausted());
        assert!(at_actor.active_exhausted());

        let room = Capacity {
            retained_group: MAX_RETAINED_SESSIONS_PER_GROUP - 1,
            retained_actor: MAX_RETAINED_SESSIONS_PER_ACTOR - 1,
            active_group: MAX_ACTIVE_CLIENTS_PER_GROUP - 1,
            active_actor: MAX_ACTIVE_CLIENTS_PER_ACTOR - 1,
        };
        assert!(!room.retained_exhausted());
        assert!(!room.active_exhausted());
    }
}
