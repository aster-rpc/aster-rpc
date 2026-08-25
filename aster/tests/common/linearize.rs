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
//!   observation: outcome code, reason code, and the snapshot's epoch,
//!   lease revision, value revision, holdership, **holder identity** and
//!   value. A successful reply must carry a snapshot and a zero reason; a
//!   refusal must carry the model's reason, and any snapshot it does carry
//!   must state the unchanged pre-state.
//!
//! If no such order exists the history is not linearizable and the checker
//! says which operations could not be placed.
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
    Acquire,
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

/// The snapshot a reply carried, in full: every fact the wire snapshot
/// states is a fact the model must agree with. `value` is `None` only when
/// the reply redacted it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotFacts {
    pub epoch: u64,
    pub lease_revision: u64,
    pub value_revision: u64,
    pub held: bool,
    /// The holder the reply named; `None` when the row is free.
    pub holder: Option<[u8; 32]>,
    /// `None` when redacted; compared only when present.
    pub value: Option<Vec<u8>>,
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
            RegisterCall::Acquire => {
                if self.exists && self.held {
                    return (next, refuse(HELD_BY_OTHER));
                }
                next.exists = true;
                next.epoch += 1;
                next.lease_revision += 1;
                next.held = true;
                next.holder = Some(*actor);
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

/// Every fact the snapshot states must be the register's fact. The value is
/// compared only when the reply did not redact it.
fn facts_match(snapshot: &SnapshotFacts, register: &Register) -> bool {
    snapshot.epoch == register.epoch
        && snapshot.lease_revision == register.lease_revision
        && snapshot.value_revision == register.value_revision
        && snapshot.held == register.held
        && snapshot.holder == register.holder
        && snapshot.value.as_ref().is_none_or(|v| *v == register.value)
}

/// Does `observed` match what the model required of this step?
///
/// `pre` is the register the step ran on and `post` the register it
/// produced; a refusal changes nothing, so any snapshot a refusal carries
/// must state `pre`, while an applied reply must carry a snapshot stating
/// `post`.
fn matches(
    expected: &Expected,
    observed: &Observed,
    call: &RegisterCall,
    pre: &Register,
    post: &Register,
) -> bool {
    match expected {
        Expected::Applied { outcome_code } => {
            let outcome_matches = if matches!(call, RegisterCall::Read) {
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
                    .is_some_and(|snapshot| facts_match(snapshot, post))
        }
        Expected::Refused { reason_code } => {
            observed.outcome_code == REJECTED
                && observed.reason_code == *reason_code
                && observed
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| facts_match(snapshot, pre))
        }
    }
}

/// Search for a linearization of `history`. `Ok` carries the witness order
/// (indices into `history`); `Err` carries a diagnosis.
pub fn check(history: &[Recorded]) -> Result<Vec<usize>, String> {
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
            match &op.observed {
                Some(observed) => {
                    if !matches(&expected, observed, &op.call, &register, &next) {
                        continue;
                    }
                }
                // Ambiguous: only the applied branch matters — a refusal has
                // no effect, which the "never linearized" branch covers.
                None => {
                    if matches!(expected, Expected::Refused { .. }) {
                        continue;
                    }
                }
            }
            order.push(i);
            if dfs(
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

    fn actor(tag: u8) -> [u8; 32] {
        [tag; 32]
    }

    /// A granted acquire's observation: held by `holder`, epoch and lease
    /// revision as the model's transition produces them.
    fn granted(holder_tag: u8, epoch: u64, lease_revision: u64, value_revision: u64) -> Observed {
        Observed {
            outcome_code: GRANTED,
            reason_code: 0,
            snapshot: Some(SnapshotFacts {
                epoch,
                lease_revision,
                value_revision,
                held: true,
                holder: Some(actor(holder_tag)),
                value: None,
            }),
        }
    }

    fn refused(reason_code: i32) -> Observed {
        Observed {
            outcome_code: REJECTED,
            reason_code,
            snapshot: None,
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
            invoked,
            returned: Some(returned),
            call,
            observed: Some(observed),
        }
    }

    fn applied(
        outcome_code: i32,
        holder_tag: Option<u8>,
        epoch: u64,
        lease_revision: u64,
        value_revision: u64,
        value: Option<Vec<u8>>,
    ) -> Observed {
        Observed {
            outcome_code,
            reason_code: 0,
            snapshot: Some(SnapshotFacts {
                epoch,
                lease_revision,
                value_revision,
                held: holder_tag.is_some(),
                holder: holder_tag.map(actor),
                value,
            }),
        }
    }

    /// A sequential grant/CAS/release chain with a justified denial in the
    /// middle linearizes, and the witness order respects real time.
    #[test]
    fn a_clean_history_linearizes() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            // Concurrent with the release below: the denial is legal before
            // it, the second grant after it.
            op(2, 5, 40, RegisterCall::Acquire, refused(HELD_BY_OTHER)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![7],
                },
                applied(MUTATED, Some(1), 1, 1, 1, Some(vec![7])),
            ),
            op(
                1,
                21,
                30,
                RegisterCall::Release { fence_epoch: 1 },
                applied(RELEASED, None, 2, 2, 1, None),
            ),
            op(2, 35, 45, RegisterCall::Acquire, granted(2, 3, 3, 1)),
        ];
        check(&history).expect("linearizable");
    }

    /// Two grants at the same epoch — two holders at once — cannot linearize.
    #[test]
    fn a_shared_epoch_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            op(2, 2, 12, RegisterCall::Acquire, granted(2, 1, 1, 0)),
        ];
        check(&history).expect_err("two holders at one epoch");
    }

    /// A denial that no interleaving justifies — the register was free for
    /// the denied operation's whole interval — cannot linearize.
    #[test]
    fn an_unjustified_denial_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, refused(HELD_BY_OTHER)),
            op(1, 20, 30, RegisterCall::Acquire, granted(1, 1, 1, 0)),
        ];
        check(&history).expect_err("nobody held the register");
    }

    /// Real-time order binds: a write that responded before a read began
    /// must be visible to that read.
    #[test]
    fn a_stale_read_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                applied(MUTATED, Some(1), 1, 1, 1, Some(vec![9])),
            ),
            // Invoked after the CAS responded, yet reads revision 0.
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                applied(0, Some(1), 1, 1, 0, Some(Vec::new())),
            ),
        ];
        check(&history).expect_err("the read is stale past a completed write");
    }

    /// A lost effect: a CAS the client saw commit whose value a later read
    /// never observes (and no later write explains) cannot linearize.
    #[test]
    fn a_lost_write_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                applied(MUTATED, Some(1), 1, 1, 1, Some(vec![9])),
            ),
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                // The revision advanced but the value is not the write's.
                applied(0, Some(1), 1, 1, 1, Some(vec![4])),
            ),
        ];
        check(&history).expect_err("the committed value vanished");
    }

    /// A reply naming the wrong holder — actor 1 acquired, the snapshot says
    /// actor 2 holds — cannot linearize. This is the fact the holder field
    /// exists to pin: without it a grant handed to the wrong identity reads
    /// as healthy.
    #[test]
    fn a_wrong_holder_reply_is_refuted() {
        let history = vec![op(1, 0, 10, RegisterCall::Acquire, granted(2, 1, 1, 0))];
        check(&history).expect_err("the snapshot names a holder that is not the acquirer");
    }

    /// A success carrying a refusal reason is malformed: outcome `GRANTED`
    /// with reason `HeldByOther` is neither a grant nor a refusal, and no
    /// interleaving may absorb it.
    #[test]
    fn a_success_with_a_refusal_reason_is_refuted() {
        let mut observed = granted(1, 1, 1, 0);
        observed.reason_code = HELD_BY_OTHER;
        let history = vec![op(1, 0, 10, RegisterCall::Acquire, observed)];
        check(&history).expect_err("a success must carry reason 0");
    }

    /// A renewal that does not advance the lease revision did not renew
    /// anything: the §9.2 timer identity is `(epoch, lease_revision)`, and a
    /// "renewal" that leaves it unchanged would never re-arm the expiry.
    #[test]
    fn a_renewal_that_freezes_the_lease_revision_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::Renew { fence_epoch: 1 },
                // Same lease revision as the grant: nothing was renewed.
                applied(RENEWED, Some(1), 1, 1, 0, None),
            ),
        ];
        check(&history).expect_err("a renewal must advance the lease revision");

        // The same history with the advanced revision linearizes, so the
        // refutation above is the lease-revision check and nothing else.
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::Renew { fence_epoch: 1 },
                applied(RENEWED, Some(1), 1, 2, 0, None),
            ),
        ];
        check(&history).expect("the honest renewal linearizes");
    }

    /// A refusal that carries a snapshot must state the unchanged pre-state;
    /// one that contradicts it — claiming the register free while the model
    /// holds it — is refuted.
    #[test]
    fn a_refusal_contradicting_the_state_is_refuted() {
        let mut denial = refused(HELD_BY_OTHER);
        denial.snapshot = Some(SnapshotFacts {
            epoch: 0,
            lease_revision: 0,
            value_revision: 0,
            held: false,
            holder: None,
            value: None,
        });
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            // Denied because held — but the snapshot claims nobody holds it.
            op(2, 11, 20, RegisterCall::Acquire, denial),
        ];
        check(&history).expect_err("a refusal's snapshot must state the pre-state");
    }

    /// An ambiguous operation is free to land in the gap that explains the
    /// rest — or to be dropped when nothing needs it.
    #[test]
    fn an_ambiguous_operation_may_land_or_vanish() {
        // The ambiguous release explains how actor 2 acquired after 1.
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            Recorded {
                actor: actor(1),
                invoked: 11,
                returned: None,
                call: RegisterCall::Release { fence_epoch: 1 },
                observed: None,
            },
            op(2, 12, 40, RegisterCall::Acquire, granted(2, 3, 3, 0)),
        ];
        check(&history).expect("the ambiguous release lands");

        // And with nothing depending on it, it vanishes.
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 1, 1, 0)),
            Recorded {
                actor: actor(1),
                invoked: 11,
                returned: None,
                call: RegisterCall::Release { fence_epoch: 1 },
                observed: None,
            },
        ];
        check(&history).expect("the ambiguous release vanishes");
    }
}
