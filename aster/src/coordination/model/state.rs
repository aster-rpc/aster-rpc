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

/// A `(namespace, key)` resource identity.
///
/// Ordering is `(namespace, key)` lexicographic, which is what redb's tuple
/// comparator gives and what the canonical row stream's ordering key must
/// match. Deriving `Ord` on the struct in field order produces exactly that;
/// flattening the two into one buffer does **not** -- the spike's round-2 defect
/// was a length-prefixed flattening under which `"b"` sorts before `"aa"`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ResourceKey {
    pub namespace: String,
    pub key: Vec<u8>,
}

impl ResourceKey {
    pub fn new(namespace: impl Into<String>, key: impl Into<Vec<u8>>) -> Self {
        Self {
            namespace: namespace.into(),
            key: key.into(),
        }
    }

    /// The `(namespace, key)` pair as the charge formulas take it.
    pub fn as_charge_parts(&self) -> (&str, &[u8]) {
        (&self.namespace, &self.key)
    }
}

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

/// The result saved for a consumed sequence (§5.2.1 `SavedSnapshotV1`).
///
/// There is deliberately **no value field**. A replay carries
/// `value_redacted = true` and the revisions, never the bytes: saving values
/// here would duplicate every resource value into the reserve-funded session
/// table, and the reserve is sized on the assumption that it does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedReply {
    pub outcome: Outcome,
    pub reason: Reason,
    /// The resource the command named, if any. This is what makes the session
    /// charge include `KEY`.
    pub resource: Option<ResourceKey>,
    pub epoch: u64,
    pub value_revision: u64,
    /// The ACL revision a `set_grant` produced, so a client that lost its
    /// response can resync `expected_acl_revision` rather than guess. Without
    /// it every subsequent grant from that client would mismatch.
    pub acl_revision: u64,
}

impl SavedReply {
    /// The charge parts of the embedded resource key, if any.
    fn key_parts(&self) -> Option<(&str, &[u8])> {
        self.resource.as_ref().map(ResourceKey::as_charge_parts)
    }
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

/// One authoritative audit row.
///
/// Carries `result_code` -- the outcome -- and no reason code (§5.2). That is
/// why a reserved reason number could not be disambiguated from audit even in
/// principle, and why the reserved-code argument rests on saved replies and
/// client journals instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    pub actor: NodeId,
    pub operation_code: i32,
    pub resource: Option<ResourceKey>,
    pub result: Outcome,
}

/// A membership intent. Terminal intents are evicted deterministically when a
/// 65th is produced, which is why reason `20` is reserved and never emitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub encoded_plan_len: usize,
    pub terminal: bool,
}

/// A retained node route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub ticket_len: usize,
}

/// A grant: the rights one subject holds on one resource.
pub type GrantKey = (ResourceKey, NodeId);

/// The complete replicated state.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub resources: BTreeMap<ResourceKey, ResourceRow>,
    pub acl: BTreeMap<GrantKey, u64>,
    pub admins: BTreeSet<NodeId>,
    pub sessions: BTreeMap<(NodeId, ClientId), Session>,
    pub intents: BTreeMap<[u8; 16], Intent>,
    pub routes: BTreeMap<NodeId, Route>,
    /// Keyed `(log_index, ordinal)` so pruning the oldest is deterministic.
    pub audit: BTreeMap<(u64, u64), AuditRow>,
    /// Maintained incrementally. MUST equal [`State::recompute_growth`].
    pub growth_bytes: u64,
    /// Maintained incrementally. MUST equal [`State::recompute_reserve`].
    pub reserve_bytes: u64,
    pub admin_revision: u64,
    /// The highest applied log index. Advances on **every** applied entry,
    /// including a rejection: a committed entry that left no trace would be
    /// indistinguishable from one that was skipped.
    pub last_applied: u64,
}

impl State {
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
            .map(|(k, r)| limits::charge_resource(&k.namespace, &k.key, r.value.len()))
            .sum();
        let acl: u64 = self
            .acl
            .keys()
            .map(|(k, _)| limits::charge_acl(&k.namespace, &k.key))
            .sum();
        let admins = self.admins.len() as u64 * limits::charge_admin();
        let intents: u64 = self
            .intents
            .values()
            .map(|i| limits::charge_intent(i.encoded_plan_len))
            .sum();
        let routes: u64 = self
            .routes
            .values()
            .map(|r| limits::charge_route(r.ticket_len))
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
            .map(|a| limits::charge_audit(a.resource.as_ref().map(ResourceKey::as_charge_parts)))
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
        ResourceKey::new(ns, k)
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
        assert!(key("a", b"zzzzzzzz") < key("b", b""));
    }

    /// Empty and one-zero-byte keys are distinct, and neither is a sentinel.
    #[test]
    fn an_empty_key_is_distinct_from_a_zero_byte_key() {
        assert_ne!(key("ns", b""), key("ns", b"\0"));
        assert!(key("ns", b"") < key("ns", b"\0"));
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
                        encoded_plan_len: 10,
                        terminal: false,
                    },
                );
            }),
            ("routes", |s| {
                s.routes.insert([4u8; 32], Route { ticket_len: 20 });
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
                        actor: [7u8; 32],
                        operation_code: 1,
                        resource: None,
                        result: Outcome::Granted,
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
                    resource: Some(key("namespace", b"a-longer-key")),
                    epoch: 1,
                    value_revision: 0,
                    acl_revision: 0,
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
