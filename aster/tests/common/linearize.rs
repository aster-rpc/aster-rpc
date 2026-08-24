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
//!   value revision, holdership and value.
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

/// What the caller observed when the operation responded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Observed {
    pub outcome_code: i32,
    pub reason_code: i32,
    /// Snapshot fields, zero/false/`None` when the reply carried no snapshot.
    pub epoch: u64,
    pub value_revision: u64,
    pub held: bool,
    /// `None` when absent or redacted; compared only when present.
    pub value: Option<Vec<u8>>,
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

/// What one sequential step must have produced.
#[derive(Debug, PartialEq)]
enum Expected {
    Applied {
        outcome_code: i32,
        epoch: u64,
        value_revision: u64,
        held: bool,
        value: Vec<u8>,
    },
    Refused {
        reason_code: i32,
    },
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
                let expected = Expected::Applied {
                    outcome_code: GRANTED,
                    epoch: next.epoch,
                    value_revision: next.value_revision,
                    held: true,
                    value: next.value.clone(),
                };
                (next, expected)
            }
            RegisterCall::Renew { fence_epoch } => {
                if let Some(reason) = self.fence_refusal(actor, *fence_epoch) {
                    return (next, refuse(reason));
                }
                next.lease_revision += 1;
                let expected = Expected::Applied {
                    outcome_code: RENEWED,
                    epoch: next.epoch,
                    value_revision: next.value_revision,
                    held: true,
                    value: next.value.clone(),
                };
                (next, expected)
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
                let expected = Expected::Applied {
                    outcome_code: MUTATED,
                    epoch: next.epoch,
                    value_revision: next.value_revision,
                    held: true,
                    value: next.value.clone(),
                };
                (next, expected)
            }
            RegisterCall::Release { fence_epoch } => {
                if let Some(reason) = self.fence_refusal(actor, *fence_epoch) {
                    return (next, refuse(reason));
                }
                next.held = false;
                next.holder = None;
                next.epoch += 1;
                next.lease_revision += 1;
                let expected = Expected::Applied {
                    outcome_code: RELEASED,
                    epoch: next.epoch,
                    value_revision: next.value_revision,
                    held: false,
                    value: next.value.clone(),
                };
                (next, expected)
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
                let expected = Expected::Applied {
                    outcome_code: REVOKED,
                    epoch: next.epoch,
                    value_revision: next.value_revision,
                    held: false,
                    value: next.value.clone(),
                };
                (next, expected)
            }
            RegisterCall::Read => {
                let expected = Expected::Applied {
                    outcome_code: 0,
                    epoch: self.epoch,
                    value_revision: self.value_revision,
                    held: self.held,
                    value: self.value.clone(),
                };
                (next, expected)
            }
        }
    }
}

/// Does `observed` match what the model required of this step?
fn matches(expected: &Expected, observed: &Observed, call: &RegisterCall) -> bool {
    match expected {
        Expected::Applied {
            outcome_code,
            epoch,
            value_revision,
            held,
            value,
        } => {
            if matches!(call, RegisterCall::Read) {
                // A read reply has no outcome code; it is all snapshot.
                observed.epoch == *epoch
                    && observed.value_revision == *value_revision
                    && observed.held == *held
                    && observed.value.as_ref().is_none_or(|v| v == value)
            } else {
                observed.outcome_code == *outcome_code
                    && observed.epoch == *epoch
                    && observed.value_revision == *value_revision
                    && observed.held == *held
                    && observed.value.as_ref().is_none_or(|v| v == value)
            }
        }
        Expected::Refused { reason_code } => {
            observed.outcome_code == REJECTED && observed.reason_code == *reason_code
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
                    if !matches(&expected, observed, &op.call) {
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

    fn granted(epoch: u64, value_revision: u64) -> Observed {
        Observed {
            outcome_code: GRANTED,
            reason_code: 0,
            epoch,
            value_revision,
            held: true,
            value: None,
        }
    }

    fn refused(reason_code: i32) -> Observed {
        Observed {
            outcome_code: REJECTED,
            reason_code,
            epoch: 0,
            value_revision: 0,
            held: false,
            value: None,
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

    /// A sequential grant/CAS/release chain with a justified denial in the
    /// middle linearizes, and the witness order respects real time.
    #[test]
    fn a_clean_history_linearizes() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 0)),
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
                Observed {
                    outcome_code: MUTATED,
                    reason_code: 0,
                    epoch: 1,
                    value_revision: 1,
                    held: true,
                    value: Some(vec![7]),
                },
            ),
            op(
                1,
                21,
                30,
                RegisterCall::Release { fence_epoch: 1 },
                Observed {
                    outcome_code: RELEASED,
                    reason_code: 0,
                    epoch: 2,
                    value_revision: 1,
                    held: false,
                    value: None,
                },
            ),
            op(2, 35, 45, RegisterCall::Acquire, granted(3, 1)),
        ];
        check(&history).expect("linearizable");
    }

    /// Two grants at the same epoch — two holders at once — cannot linearize.
    #[test]
    fn a_shared_epoch_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 0)),
            op(2, 2, 12, RegisterCall::Acquire, granted(1, 0)),
        ];
        check(&history).expect_err("two holders at one epoch");
    }

    /// A denial that no interleaving justifies — the register was free for
    /// the denied operation's whole interval — cannot linearize.
    #[test]
    fn an_unjustified_denial_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, refused(HELD_BY_OTHER)),
            op(1, 20, 30, RegisterCall::Acquire, granted(1, 0)),
        ];
        check(&history).expect_err("nobody held the register");
    }

    /// Real-time order binds: a write that responded before a read began
    /// must be visible to that read.
    #[test]
    fn a_stale_read_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                Observed {
                    outcome_code: MUTATED,
                    reason_code: 0,
                    epoch: 1,
                    value_revision: 1,
                    held: true,
                    value: Some(vec![9]),
                },
            ),
            // Invoked after the CAS responded, yet reads revision 0.
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                Observed {
                    outcome_code: 0,
                    reason_code: 0,
                    epoch: 1,
                    value_revision: 0,
                    held: true,
                    value: Some(Vec::new()),
                },
            ),
        ];
        check(&history).expect_err("the read is stale past a completed write");
    }

    /// A lost effect: a CAS the client saw commit whose value a later read
    /// never observes (and no later write explains) cannot linearize.
    #[test]
    fn a_lost_write_is_refuted() {
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 0)),
            op(
                1,
                11,
                20,
                RegisterCall::CompareAndSet {
                    fence_epoch: 1,
                    expected_value_revision: 0,
                    new_value: vec![9],
                },
                Observed {
                    outcome_code: MUTATED,
                    reason_code: 0,
                    epoch: 1,
                    value_revision: 1,
                    held: true,
                    value: Some(vec![9]),
                },
            ),
            op(
                2,
                25,
                35,
                RegisterCall::Read,
                Observed {
                    outcome_code: 0,
                    reason_code: 0,
                    epoch: 1,
                    value_revision: 1,
                    held: true,
                    // The revision advanced but the value is not the write's.
                    value: Some(vec![4]),
                },
            ),
        ];
        check(&history).expect_err("the committed value vanished");
    }

    /// An ambiguous operation is free to land in the gap that explains the
    /// rest — or to be dropped when nothing needs it.
    #[test]
    fn an_ambiguous_operation_may_land_or_vanish() {
        // The ambiguous release explains how actor 2 acquired after 1.
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 0)),
            Recorded {
                actor: actor(1),
                invoked: 11,
                returned: None,
                call: RegisterCall::Release { fence_epoch: 1 },
                observed: None,
            },
            op(2, 12, 40, RegisterCall::Acquire, granted(3, 0)),
        ];
        check(&history).expect("the ambiguous release lands");

        // And with nothing depending on it, it vanishes.
        let history = vec![
            op(1, 0, 10, RegisterCall::Acquire, granted(1, 0)),
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
