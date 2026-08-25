//! An interval-based linearizability checker for the §5.2 register.
//!
//! ## What this is
//!
//! The Wing–Gong search, specialized to one register: every operation is
//! recorded as a **real-time interval** — invocation instant to response
//! instant — together with what the caller asked and what it observed,
//! denials included. The checker then searches for a total order that
//!
//! * respects real time — an operation that responded before another was
//!   invoked must precede it — and
//! * is accepted by a **sequential model** of the register: an independent
//!   transcription of §5.2/§9.1's transitions (acquire, renew,
//!   compare_and_set, release, revoke, read), checked against the full
//!   observation: outcome code, reason code, and **every** snapshot fact —
//!   epoch, lease revision, value revision, holdership, holder identity,
//!   the acquisition TTL, the group and resource identity, the ACL
//!   revision, and the value as an explicit present-or-redacted fact. A
//!   successful reply must carry a snapshot and a zero reason; a refusal
//!   must carry the model's reason, and any snapshot it does carry must
//!   state the unchanged pre-state.
//!
//! If no such order exists the history is not linearizable and the checker
//! says which operations could not be placed.
//!
//! ## Independent identity, ACL and visibility oracles
//!
//! The group id, resource key and ACL revision are **supplied by the
//! harness** ([`Expectations`]), never learned from a reply: a checker that
//! pinned them from the first production snapshot would accept a defect
//! that returns the same wrong identity in every reply. Likewise each
//! recorded operation carries `may_read_value`, the harness's independent
//! knowledge of the caller's `READ` grant: an authorized caller receiving a
//! withheld value — or an unauthorized one receiving bytes — is refuted,
//! not absorbed as "unknown".
//!
//! ## Ambiguous operations
//!
//! A client that gave up on a journaled mutation has an operation with no
//! response: because the driver resends exact bytes, the effect may land at
//! any later time — or never. Such operations are recorded with no
//! observation; the search may linearize them anywhere after invocation
//! (applying the success effect) or omit them entirely, which is exactly the
//! standard treatment of information-incomplete operations. They never
//! constrain other operations' order.
//!
//! ## What is deliberately absent
//!
//! There is no hidden `Expire` transition. The workloads that feed this
//! checker acquire with TTLs longer than the whole run, so a §9.2 expiry
//! cannot legitimately fire; leaving it out keeps the model deterministic
//! and means a lost release cannot be silently "explained" as an expiry.
//! (§9.2 expiry has its own gate.)

#![allow(dead_code)]

use std::collections::HashSet;

/// One §5.2 register call, as the caller framed it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RegisterCall {
    Acquire {
        /// The TTL the caller asked for — the row's committed TTL on grant,
        /// which the reply's snapshot must echo.
        ttl_ms: u64,
    },
    Renew {
        fence_epoch: u64,
    },
    CompareAndSet {
        fence_epoch: u64,
        expected_value_revision: u64,
        new_value: Vec<u8>,
    },
    Release {
        fence_epoch: u64,
    },
    Revoke {
        expected_epoch: Option<u64>,
    },
    Read,
}

/// The value a snapshot stated, explicitly — the three §5.2 wire states,
/// never a wildcard. `Present` compares byte-for-byte, **empty included**:
/// the wire distinguishes `Some(empty)` ("the resource has no value") from
/// `None`, so a reply that lost an unredacted value claims `Present(empty)`
/// — a different value from the model's, not a missing one. `Redacted` is a
/// replay whose bytes were never stored; `Withheld` is `None` without the
/// redaction flag — the caller lacks `RIGHT_READ`, an authorization fact
/// that states no value fact at all.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueFact {
    Present(Vec<u8>),
    Redacted,
    Withheld,
}

/// The snapshot a reply carried, in full: every fact the wire snapshot
/// states is a fact the model must agree with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotFacts {
    /// The group the snapshot claims to be about.
    pub group_id: [u8; 32],
    /// The resource the snapshot claims to be about.
    pub resource: (String, Vec<u8>),
    pub epoch: u64,
    pub lease_revision: u64,
    pub value_revision: u64,
    pub held: bool,
    /// The holder the reply named; `None` when the row is free.
    pub holder: Option<[u8; 32]>,
    /// The row's committed acquisition TTL.
    pub ttl_ms: u64,
    pub acl_revision: u64,
    pub value: ValueFact,
}

/// What the caller observed when the operation responded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Observed {
    pub outcome_code: i32,
    pub reason_code: i32,
    /// `None` when the reply carried no snapshot — which the model permits
    /// only for refusals.
    pub snapshot: Option<SnapshotFacts>,
}

/// One recorded operation: the interval, the call, and the observation.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// Who called, as the transport authenticated them.
    pub actor: [u8; 32],
    /// The harness's **independent** knowledge of whether this caller holds
    /// `RIGHT_READ` on the register — from the grants it made, never from
    /// any reply.
    pub may_read_value: bool,
    /// Nanoseconds since the history's origin at invocation.
    pub invoked: u64,
    /// Nanoseconds at response; `None` for an ambiguous operation.
    pub returned: Option<u64>,
    pub call: RegisterCall,
    /// `None` for an ambiguous operation.
    pub observed: Option<Observed>,
}

// §5.2 outcome codes.
const GRANTED: i32 = 1;
const RENEWED: i32 = 2;
const MUTATED: i32 = 3;
const RELEASED: i32 = 4;
const REVOKED: i32 = 5;
const REJECTED: i32 = 100;

// §5.2 reason codes this model can produce.
const HELD_BY_OTHER: i32 = 1;
const NOT_HELD: i32 = 2;
const NOT_HOLDER: i32 = 3;
const EPOCH_MISMATCH: i32 = 4;
const VALUE_REVISION_MISMATCH: i32 = 5;

/// The harness-supplied identity oracle: the group, resource and ACL
/// revision every snapshot in this history must state. Independent of the
/// replies by construction — the harness knows which group it built, which
/// register it drove, and how many ACL operations it performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expectations {
    pub group_id: [u8; 32],
    pub resource: (String, Vec<u8>),
    pub acl_revision: u64,
}

/// The sequential register: an independent transcription of the §9.1 rows,
/// deliberately *not* the production `resource::decide` so the two are
/// separate witnesses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct Register {
    exists: bool,
    epoch: u64,
    held: bool,
    holder: Option<[u8; 32]>,
    lease_revision: u64,
    value_revision: u64,
    value: Vec<u8>,
    /// The committed acquisition TTL: written by `Acquire`, retained by
    /// every other transition, exactly as `resource::decide` keeps the row's
    /// `ttl_ms`.
    ttl_ms: u64,
}

/// What one sequential step must have produced. The state facts live on the
/// registers themselves: an applied observation is compared against the
/// post-state, a refused one against the unchanged pre-state.
#[derive(Debug, PartialEq)]
enum Expected {
    Applied { outcome_code: i32 },
    Refused { reason_code: i32 },
}

impl Register {
    /// §9.1's fence predicate, most specific reason first.
    fn fence_refusal(&self, actor: &[u8; 32], fence_epoch: u64) -> Option<i32> {
        if !self.exists || !self.held {
            return Some(NOT_HELD);
        }
        if self.holder != Some(*actor) {
            return Some(NOT_HOLDER);
        }
        if self.epoch != fence_epoch {
            return Some(EPOCH_MISMATCH);
        }
        None
    }

    /// Apply one call sequentially: the next state and the observation §5.2
    /// requires of it. Reads leave the state alone by construction.
    fn step(&self, actor: &[u8; 32], call: &RegisterCall) -> (Register, Expected) {
        let mut next = self.clone();
        let refuse = |reason_code| Expected::Refused { reason_code };
        match call {
            RegisterCall::Acquire { ttl_ms } => {
                if self.exists && self.held {
                    return (next, refuse(HELD_BY_OTHER));
                }
                next.exists = true;
                next.epoch += 1;
                next.lease_revision += 1;
                next.held = true;
                next.holder = Some(*actor);
                next.ttl_ms = *ttl_ms;
                (
                    next,
                    Expected::Applied {
                        outcome_code: GRANTED,
                    },
                )
            }
            RegisterCall::Renew { fence_epoch } => {
                if let Some(reason) = self.fence_refusal(actor, *fence_epoch) {
                    return (next, refuse(reason));
                }
                next.lease_revision += 1;
                (
                    next,
                    Expected::Applied {
                        outcome_code: RENEWED,
                    },
                )
            }
            RegisterCall::CompareAndSet {
                fence_epoch,
                expected_value_revision,
                new_value,
            } => {
                if let Some(reason) = self.fence_refusal(actor, *fence_epoch) {
                    return (next, refuse(reason));
                }
                if self.value_revision != *expected_value_revision {
                    return (next, refuse(VALUE_REVISION_MISMATCH));
                }
                next.value = new_value.clone();
                next.value_revision += 1;
                (
                    next,
                    Expected::Applied {
                        outcome_code: MUTATED,
                    },
                )
            }
            RegisterCall::Release { fence_epoch } => {
                if let Some(reason) = self.fence_refusal(actor, *fence_epoch) {
                    return (next, refuse(reason));
                }
                next.held = false;
                next.holder = None;
                next.epoch += 1;
                next.lease_revision += 1;
                (
                    next,
                    Expected::Applied {
                        outcome_code: RELEASED,
                    },
                )
            }
            RegisterCall::Revoke { expected_epoch } => {
                // §9.1: revoke is unconditional fencing — no `held`
                // precondition — but there is nothing to fence on a resource
                // that does not exist.
                if !self.exists {
                    return (next, refuse(NOT_HELD));
                }
                if let Some(expected) = expected_epoch {
                    if self.epoch != *expected {
                        return (next, refuse(EPOCH_MISMATCH));
                    }
                }
                next.held = false;
                next.holder = None;
                next.epoch += 1;
                next.lease_revision += 1;
                (
                    next,
                    Expected::Applied {
                        outcome_code: REVOKED,
                    },
                )
            }
            RegisterCall::Read => (next, Expected::Applied { outcome_code: 0 }),
        }
    }
}

/// Every fact the snapshot states must be the register's fact — with the
/// identity judged against the harness's independent [`Expectations`] and
/// the value judged against the caller's independent `READ` visibility.
fn facts_match(
    expected: &Expectations,
    snapshot: &SnapshotFacts,
    register: &Register,
    may_read_value: bool,
) -> bool {
    let identity = snapshot.group_id == expected.group_id
        && snapshot.resource == expected.resource
        && snapshot.acl_revision == expected.acl_revision;
    let value = match &snapshot.value {
        // Bytes reach only a caller entitled to them, and must be the
        // model's bytes exactly — empty included.
        ValueFact::Present(bytes) => may_read_value && *bytes == register.value,
        // A replay's bytes were never stored; legitimate for any caller.
        ValueFact::Redacted => true,
        // An authorization outcome: legitimate **only** for a caller the
        // harness independently knows lacks `READ`. For an authorized
        // caller a withheld value is a lost value, not an unknown one.
        ValueFact::Withheld => !may_read_value,
    };
    identity
        && value
        && snapshot.epoch == register.epoch
        && snapshot.lease_revision == register.lease_revision
        && snapshot.value_revision == register.value_revision
        && snapshot.held == register.held
        && snapshot.holder == register.holder
        && snapshot.ttl_ms == register.ttl_ms
}

/// Does `observed` match what the model required of this step?
///
/// `pre` is the register the step ran on and `post` the register it
/// produced; a refusal changes nothing, so any snapshot a refusal carries
/// must state `pre`, while an applied reply must carry a snapshot stating
/// `post`.
fn matches(
    oracle: &Expectations,
    expected: &Expected,
    op: &Recorded,
    observed: &Observed,
    pre: &Register,
    post: &Register,
) -> bool {
    match expected {
        Expected::Applied { outcome_code } => {
            let outcome_matches = if matches!(op.call, RegisterCall::Read) {
                // A read reply has no outcome code; it is all snapshot.
                true
            } else {
                observed.outcome_code == *outcome_code
            };
            // A success carrying a refusal reason is a malformed reply, not
            // a permissible spelling of success.
            outcome_matches
                && observed.reason_code == 0
                && observed
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| facts_match(oracle, snapshot, post, op.may_read_value))
        }
        Expected::Refused { reason_code } => {
            observed.outcome_code == REJECTED
                && observed.reason_code == *reason_code
                && observed
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| facts_match(oracle, snapshot, pre, op.may_read_value))
        }
    }
}

/// Search for a linearization of `history`. `Ok` carries the witness order
/// (indices into `history`); `Err` carries a diagnosis.
pub fn check(oracle: &Expectations, history: &[Recorded]) -> Result<Vec<usize>, String> {
    assert!(
        history.len() <= 128,
        "the bitmask search handles at most 128 operations"
    );
    let n = history.len();
    let returned: Vec<u64> = history
        .iter()
        .map(|op| op.returned.unwrap_or(u64::MAX))
        .collect();

    // An operation can be chosen next iff every *other* pending operation's
    // response did not precede its invocation.
    let candidate = |mask: u128, i: usize| -> bool {
        (0..n).all(|j| j == i || mask & (1u128 << j) != 0 || returned[j] >= history[i].invoked)
    };

    let complete = |mask: u128| -> bool {
        (0..n).all(|i| mask & (1u128 << i) != 0 || history[i].observed.is_none())
    };

    let mut seen: HashSet<(u128, Register)> = HashSet::new();
    let mut order: Vec<usize> = Vec::new();
    let mut best: Vec<usize> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn dfs(
        oracle: &Expectations,
        history: &[Recorded],
        candidate: &dyn Fn(u128, usize) -> bool,
        complete: &dyn Fn(u128) -> bool,
        seen: &mut HashSet<(u128, Register)>,
        order: &mut Vec<usize>,
        best: &mut Vec<usize>,
        mask: u128,
        register: Register,
    ) -> bool {
        if complete(mask) {
            return true;
        }
        if !seen.insert((mask, register.clone())) {
            return false;
        }
        if order.len() > best.len() {
            best.clone_from(order);
        }
        for i in 0..history.len() {
            if mask & (1u128 << i) != 0 || !candidate(mask, i) {
                continue;
            }
            let op = &history[i];
            let (next, expected) = register.step(&op.actor, &op.call);
            let next = match &op.observed {
                Some(observed) => {
                    if !matches(oracle, &expected, op, observed, &register, &next) {
                        continue;
                    }
                    // A refusal leaves the state untouched.
                    if matches!(expected, Expected::Refused { .. }) {
                        register.clone()
                    } else {
                        next
                    }
                }
                // Ambiguous: only the applied branch matters — a refusal has
                // no effect, which the "never linearized" branch covers.
                None => {
                    if matches!(expected, Expected::Refused { .. }) {
                        continue;
                    }
                    next
                }
            };
            order.push(i);
            if dfs(
                oracle,
                history,
                candidate,
                complete,
                seen,
                order,
                best,
                mask | (1u128 << i),
                next,
            ) {
                return true;
            }
            order.pop();
        }
        false
    }

    if dfs(
        oracle,
        history,
        &candidate,
        &complete,
        &mut seen,
        &mut order,
        &mut best,
        0,
        Register::default(),
    ) {
        Ok(order)
    } else {
        let placed: HashSet<usize> = best.iter().copied().collect();
        let stuck: Vec<String> = (0..n)
            .filter(|i| !placed.contains(i) && history[*i].observed.is_some())
            .map(|i| format!("  [{i}] {:?} -> {:?}", history[i].call, history[i].observed))
            .collect();
        Err(format!(
            "no linearization exists: {} of {} operations placed in the best prefix; \
             unplaced observed operations:\n{}",
            best.len(),
            n,
            stuck.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: u64 = 120_000;
    const GROUP: [u8; 32] = [0xAA; 32];

    fn resource() -> (String, Vec<u8>) {
        ("history.gate".to_string(), b"register".to_vec())
    }

    fn actor(tag: u8) -> [u8; 32] {
        [tag; 32]
    }

    fn oracle() -> Expectations {
        Expectations {
            group_id: GROUP,
            resource: resource(),
            acl_revision: 1,
        }
    }

    fn acquire() -> RegisterCall {
        RegisterCall::Acquire { ttl_ms: TTL }
    }

    /// A snapshot with the healthy identity facts and the given register
    /// facts.
    #[allow(clippy::too_many_arguments)]
    fn facts(
        holder_tag: Option<u8>,
        epoch: u64,
        lease_revision: u64,
        value_revision: u64,
        ttl_ms: u64,
        value: ValueFact,
    ) -> SnapshotFacts {
        SnapshotFacts {
            group_id: GROUP,
            resource: resource(),
            epoch,
            lease_revision,
            value_revision,
            held: holder_tag.is_some(),
            holder: holder_tag.map(actor),
            ttl_ms,
            acl_revision: 1,
            value,
        }
    }

    fn granted(holder_tag: u8, epoch: u64, lease_revision: u64, value_revision: u64) -> Observed {
        Observed {
            outcome_code: GRANTED,
            reason_code: 0,
            snapshot: Some(facts(
                Some(holder_tag),
                epoch,
                lease_revision,
                value_revision,
                TTL,
                ValueFact::Redacted,
            )),
        }
    }

    fn refused(reason_code: i32) -> Observed {
        Observed {
            outcome_code: REJECTED,
            reason_code,
            snapshot: None,
        }
    }

    fn applied(
        outcome_code: i32,
        holder_tag: Option<u8>,
        epoch: u64,
        lease_revision: u64,
        value_revision: u64,
        value: ValueFact,
    ) -> Observed {
        Observed {
            outcome_code,
            reason_code: 0,
            snapshot: Some(facts(
                holder_tag,
                epoch,
                lease_revision,
                value_revision,
                TTL,
                value,
            )),
        }
    }

    fn op(
        actor_tag: u8,
        invoked: u64,
        returned: u64,
        call: RegisterCall,
        observed: Observed,
    ) -> Recorded {
        Recorded {
            actor: actor(actor_tag),
            may_read_value: true,
            invoked,
            returned: Some(returned),
            call,
            observed: Some(observed),
        }
    }

    /// A sequential grant/CAS/release chain with a justified denial in the
    /// middle linearizes, and the witness order respects real time.
    #[test]
    fn a_clean_history_linearizes() {
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            // Concurrent with the release below: the denial is legal before
            // it, the second grant after it.
            op(2, 5, 40, acquire(), refused(HELD_BY_OTHER)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![7],
                },
                applied(MUTATED, Some(1), 1, 1, 1, ValueFact::Present(vec![7])),
            ),
            op(
                1,
                21,
                30,
                RegisterCall::Release { fence_epoch: 1 },
                applied(RELEASED, None, 2, 2, 1, ValueFact::Redacted),
            ),
            op(2, 35, 45, acquire(), granted(2, 3, 3, 1)),
        ];
        check(&oracle(), &history).expect("linearizable");
    }

    /// Two grants at the same epoch — two holders at once — cannot linearize.
    #[test]
    fn a_shared_epoch_is_refuted() {
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(2, 2, 12, acquire(), granted(2, 1, 1, 0)),
        ];
        check(&oracle(), &history).expect_err("two holders at one epoch");
    }

    /// A denial that no interleaving justifies — the register was free for
    /// the denied operation's whole interval — cannot linearize.
    #[test]
    fn an_unjustified_denial_is_refuted() {
        let history = vec![
            op(1, 0, 10, acquire(), refused(HELD_BY_OTHER)),
            op(1, 20, 30, acquire(), granted(1, 1, 1, 0)),
        ];
        check(&oracle(), &history).expect_err("nobody held the register");
    }

    /// Real-time order binds: a write that responded before a read began
    /// must be visible to that read.
    #[test]
    fn a_stale_read_is_refuted() {
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                applied(MUTATED, Some(1), 1, 1, 1, ValueFact::Present(vec![9])),
            ),
            // Invoked after the CAS responded, yet reads revision 0.
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                applied(0, Some(1), 1, 1, 0, ValueFact::Present(Vec::new())),
            ),
        ];
        check(&oracle(), &history).expect_err("the read is stale past a completed write");
    }

    /// A lost effect: a CAS the client saw commit whose value a later read
    /// never observes (and no later write explains) cannot linearize.
    #[test]
    fn a_lost_write_is_refuted() {
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                applied(MUTATED, Some(1), 1, 1, 1, ValueFact::Present(vec![9])),
            ),
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                // The revision advanced but the value is not the write's.
                applied(0, Some(1), 1, 1, 1, ValueFact::Present(vec![4])),
            ),
        ];
        check(&oracle(), &history).expect_err("the committed value vanished");
    }

    /// A **cleared unredacted value** is a lost value, not an unknown one:
    /// the CAS committed `[9]`, the later read reports an empty present
    /// value with `value_redacted = false`. The explicit `Present`/`Redacted`
    /// split is what refutes it — a wildcard "absent" would have absorbed it.
    #[test]
    fn a_missing_unredacted_value_is_refuted() {
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                applied(MUTATED, Some(1), 1, 1, 1, ValueFact::Present(vec![9])),
            ),
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                applied(0, Some(1), 1, 1, 1, ValueFact::Present(Vec::new())),
            ),
        ];
        check(&oracle(), &history)
            .expect_err("an unredacted empty value is not the committed value");

        // The honestly redacted twin linearizes: redaction is policy.
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                applied(MUTATED, Some(1), 1, 1, 1, ValueFact::Present(vec![9])),
            ),
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                applied(0, Some(1), 1, 1, 1, ValueFact::Redacted),
            ),
        ];
        check(&oracle(), &history).expect("a redacted value states no value fact");
    }

    /// A reply naming the wrong holder — actor 1 acquired, the snapshot says
    /// actor 2 holds — cannot linearize. This is the fact the holder field
    /// exists to pin: without it a grant handed to the wrong identity reads
    /// as healthy.
    #[test]
    fn a_wrong_holder_reply_is_refuted() {
        let history = vec![op(1, 0, 10, acquire(), granted(2, 1, 1, 0))];
        check(&oracle(), &history)
            .expect_err("the snapshot names a holder that is not the acquirer");
    }

    /// A success carrying a refusal reason is malformed: outcome `GRANTED`
    /// with reason `HeldByOther` is neither a grant nor a refusal, and no
    /// interleaving may absorb it.
    #[test]
    fn a_success_with_a_refusal_reason_is_refuted() {
        let mut observed = granted(1, 1, 1, 0);
        observed.reason_code = HELD_BY_OTHER;
        let history = vec![op(1, 0, 10, acquire(), observed)];
        check(&oracle(), &history).expect_err("a success must carry reason 0");
    }

    /// A renewal that does not advance the lease revision did not renew
    /// anything: the §9.2 timer identity is `(epoch, lease_revision)`, and a
    /// "renewal" that leaves it unchanged would never re-arm the expiry.
    #[test]
    fn a_renewal_that_freezes_the_lease_revision_is_refuted() {
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::Renew { fence_epoch: 1 },
                // Same lease revision as the grant: nothing was renewed.
                applied(RENEWED, Some(1), 1, 1, 0, ValueFact::Redacted),
            ),
        ];
        check(&oracle(), &history).expect_err("a renewal must advance the lease revision");

        // The same history with the advanced revision linearizes, so the
        // refutation above is the lease-revision check and nothing else.
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::Renew { fence_epoch: 1 },
                applied(RENEWED, Some(1), 1, 2, 0, ValueFact::Redacted),
            ),
        ];
        check(&oracle(), &history).expect("the honest renewal linearizes");
    }

    /// A grant that reports a different TTL than the acquire asked for is a
    /// row the run never created: §9.2 arms timers from the committed TTL,
    /// so a drifted TTL is an expiry schedule the holder never chose.
    #[test]
    fn a_wrong_ttl_is_refuted() {
        let mut observed = granted(1, 1, 1, 0);
        observed.snapshot.as_mut().expect("a snapshot").ttl_ms = TTL / 2;
        let history = vec![op(1, 0, 10, acquire(), observed)];
        check(&oracle(), &history).expect_err("the snapshot's TTL is not the acquire's");
    }

    /// The ACL revision is pinned: no recorded operation changes it, so two
    /// snapshots that disagree describe an ACL history the run never made.
    #[test]
    fn a_shifted_acl_revision_is_refuted() {
        let mut renewed = applied(RENEWED, Some(1), 1, 2, 0, ValueFact::Redacted);
        renewed.snapshot.as_mut().expect("a snapshot").acl_revision = 7;
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(1, 11, 20, RegisterCall::Renew { fence_epoch: 1 }, renewed),
        ];
        check(&oracle(), &history).expect_err("the ACL revision moved without an ACL operation");
    }

    /// The resource identity is pinned: a snapshot about another resource —
    /// or another group — is a reply to some other register and cannot be
    /// this history's.
    #[test]
    fn a_wrong_resource_identity_is_refuted() {
        let mut renewed = applied(RENEWED, Some(1), 1, 2, 0, ValueFact::Redacted);
        renewed.snapshot.as_mut().expect("a snapshot").resource =
            ("history.gate".to_string(), b"other-register".to_vec());
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(1, 11, 20, RegisterCall::Renew { fence_epoch: 1 }, renewed),
        ];
        check(&oracle(), &history).expect_err("the snapshot names another resource");

        let mut foreign = applied(RENEWED, Some(1), 1, 2, 0, ValueFact::Redacted);
        foreign.snapshot.as_mut().expect("a snapshot").group_id = [0xBB; 32];
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            op(1, 11, 20, RegisterCall::Renew { fence_epoch: 1 }, foreign),
        ];
        check(&oracle(), &history).expect_err("the snapshot names another group");
    }

    /// A refusal that carries a snapshot must state the unchanged pre-state;
    /// one that contradicts it — claiming the register free while the model
    /// holds it — is refuted.
    #[test]
    fn a_refusal_contradicting_the_state_is_refuted() {
        let mut denial = refused(HELD_BY_OTHER);
        denial.snapshot = Some(facts(None, 0, 0, 0, 0, ValueFact::Redacted));
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            // Denied because held — but the snapshot claims nobody holds it.
            op(2, 11, 20, acquire(), denial),
        ];
        check(&oracle(), &history).expect_err("a refusal's snapshot must state the pre-state");
    }

    /// **An authorized caller receiving a withheld value is refuted** (the
    /// fourth review's escape): `None, value_redacted = false` maps to
    /// `Withheld`, and for a caller the harness independently knows holds
    /// `READ`, that is a lost value — production only withholds for want of
    /// the right. The same observation from a caller *without* `READ`
    /// linearizes, pinning the refutation to the visibility oracle.
    #[test]
    fn a_withheld_value_for_an_authorized_reader_is_refuted() {
        let withheld = applied(GRANTED, Some(1), 1, 1, 0, ValueFact::Withheld);
        let history = vec![op(1, 0, 10, acquire(), withheld.clone())];
        check(&oracle(), &history).expect_err("an authorized reader must receive the value");

        let mut unauthorized = op(1, 0, 10, acquire(), withheld);
        unauthorized.may_read_value = false;
        check(&oracle(), &[unauthorized]).expect("withheld is exactly the unauthorized outcome");
    }

    /// **An unauthorized caller receiving value bytes is refuted** — even
    /// the *correct* bytes: the server never hands a value to a caller
    /// without `READ`, so a reply that does is not this group's reply. The
    /// bytes here equal the model's value exactly (the mutation campaign
    /// proved a mismatched-bytes spelling vacuous — the value comparison
    /// refuted it before the authorization gate could).
    #[test]
    fn a_present_value_for_an_unauthorized_reader_is_refuted() {
        let mut with_bytes = op(1, 0, 10, acquire(), granted(1, 1, 1, 0));
        with_bytes
            .observed
            .as_mut()
            .expect("observed")
            .snapshot
            .as_mut()
            .expect("snapshot")
            .value = ValueFact::Present(Vec::new());
        with_bytes.may_read_value = false;
        check(&oracle(), &[with_bytes.clone()])
            .expect_err("no bytes for a caller without READ, correct or not");

        // The same observation from an authorized caller linearizes: the
        // refutation above is the authorization gate and nothing else.
        with_bytes.may_read_value = true;
        check(&oracle(), &[with_bytes]).expect("the authorized twin is healthy");
    }

    /// **The identity oracle is the harness's, not the replies'**: a history
    /// whose *every* snapshot consistently names a wrong group — the defect
    /// a first-reply pin would normalize — is refuted against the supplied
    /// expectations.
    #[test]
    fn a_consistently_wrong_identity_is_refuted() {
        let mut foreign = granted(1, 1, 1, 0);
        foreign.snapshot.as_mut().expect("snapshot").group_id = [0xBB; 32];
        let history = vec![op(1, 0, 10, acquire(), foreign)];
        check(&oracle(), &history)
            .expect_err("even a consistent wrong group must fail the supplied oracle");
    }

    /// An ambiguous operation is free to land in the gap that explains the
    /// rest — or to be dropped when nothing needs it.
    #[test]
    fn an_ambiguous_operation_may_land_or_vanish() {
        // The ambiguous release explains how actor 2 acquired after 1.
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            Recorded {
                actor: actor(1),
                may_read_value: true,
                invoked: 11,
                returned: None,
                call: RegisterCall::Release { fence_epoch: 1 },
                observed: None,
            },
            op(2, 12, 40, acquire(), granted(2, 3, 3, 0)),
        ];
        check(&oracle(), &history).expect("the ambiguous release lands");

        // And with nothing depending on it, it vanishes.
        let history = vec![
            op(1, 0, 10, acquire(), granted(1, 1, 1, 0)),
            Recorded {
                actor: actor(1),
                may_read_value: true,
                invoked: 11,
                returned: None,
                call: RegisterCall::Release { fence_epoch: 1 },
                observed: None,
            },
        ];
        check(&oracle(), &history).expect("the ambiguous release vanishes");
    }
}
