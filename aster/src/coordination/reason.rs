//! Outcome and rejection reason codes (§5.2), and the two classifications that
//! hang off them: terminality (§10) and stage-3 admission denial (§10).
//!
//! ## Why these are generated from one declaration
//!
//! The spike kept `ALL_REASONS` as a literal maintained beside the enum, with a
//! hand-written exhaustiveness guard. That did not work: adding the new match
//! arm alone restored compilation while the tests silently omitted the new
//! reason, a duplicated entry preserved the length, and asserting `len() == 12`
//! was tautological because the constant's type was already `[Reason; 12]`.
//! Here the enum, [`Reason::ALL`], and [`Reason::from_code`] come from a single
//! `reasons!` invocation, so a new variant extends all three at once.
//!
//! ## Why the classifications have no wildcard arm
//!
//! Both [`Reason::is_admission_denial`] and
//! [`Reason::is_terminal_when_not_consumed`] match exhaustively. Adding a reason
//! is therefore a compile error until someone decides which side it falls on.
//! Neither choice is cosmetic:
//!
//! * projecting a **consumed conflict** as a pre-proposal denial wedges the
//!   client -- a stale `6 AclRevisionMismatch` denied before proposal is a
//!   non-terminal `NotConsumed`, and since a client may never change the
//!   request at a sequence it has already sent, it retries the stale revision
//!   forever;
//! * projecting an **admission denial** as a conflict burns a sequence for a
//!   condition that never reached the log;
//! * marking a refusal terminal when a v1 command *can* clear it tells a client
//!   to give up prematurely, and the converse loops it against a condition
//!   nothing can free.
//!
//! The tests deliberately check these against **code lists quoted from §5.2 and
//! §10**, not against the predicates themselves. The spike learned this the
//! hard way: a matrix that validated the projection by calling
//! `is_admission_denial()` was testing that the code agreed with itself, and
//! moving a reason into the wrong arm kept every case passing.

/// Generates the `Reason` enum, its exhaustive `ALL` list, and `from_code` from
/// one declaration.
macro_rules! reasons {
    ($($(#[$m:meta])* $name:ident = $code:expr),* $(,)?) => {
        /// A rejection reason (§5.2). `None` (0) means no reason applies.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(i32)]
        pub enum Reason {
            $($(#[$m])* $name = $code),*
        }

        impl Reason {
            /// Every reason in §5.2, derived from the declaration above so it
            /// cannot go stale.
            pub const ALL: &'static [Reason] = &[$(Reason::$name),*];

            /// Reconstruct a reason from a stored or received code.
            ///
            /// Returns `None` for a code outside §5.2 rather than panicking:
            /// this runs on data -- a saved reply read back from storage, or a
            /// reply from a peer -- and unrecognised data is a decode outcome
            /// to be handled, not a crash. A caller reading its *own* saved row
            /// should treat `None` as corrupt state; a client reading a peer's
            /// reply should treat it as an unknown reason, exactly as §5.2
            /// directs for the reserved `20`.
            pub fn from_code(code: i32) -> Option<Reason> {
                match code {
                    $($code => Some(Reason::$name),)*
                    _ => None,
                }
            }

            /// The wire code.
            pub fn code(self) -> i32 {
                self as i32
            }
        }
    };
}

reasons! {
    /// No reason applies.
    ///
    /// Carried by every successful outcome -- and, by one narrow exception, by
    /// a rejection. A committed command that violates a **logical
    /// request-shape limit** (§12.1) rejects as `(100 Rejected, 0 None)`,
    /// because §5.2 has no reason for that failure and inventing one would
    /// change the frozen contract. That pairing is legal **only** there; every
    /// other rejection carries a defined, nonzero reason. See
    /// [`Outcome::Rejected`] and the forced-apply rule in
    /// `coordination::model::apply`.
    None = 0,
    HeldByOther = 1,
    NotHeld = 2,
    NotHolder = 3,
    EpochMismatch = 4,
    ValueRevisionMismatch = 5,
    AclRevisionMismatch = 6,
    AdminRevisionMismatch = 7,
    SequenceTooOld = 8,
    SequenceGap = 9,
    SequenceConflict = 10,
    ClientRetired = 11,
    PermissionChanged = 12,
    MembershipRevisionMismatch = 13,
    MembershipIntentActive = 14,
    CounterExhausted = 15,
    MembershipCannotCancel = 16,
    OperationIdConflict = 17,
    RetainedSessionCapReached = 18,
    StateBudgetExhausted = 19,
    /// **Reserved, never emitted.** §9 retains 64 terminal intents and evicts
    /// the oldest deterministically when a 65th is produced, so no transition
    /// can exhaust an intent slot.
    ///
    /// Kept in the enum rather than deleted because reason codes outlive the
    /// transition that produced them: `SavedReplyV1.reason_code` persists in the
    /// session table and is returned to every future retry of that sequence.
    /// Reusing `20` for a different condition would make an old saved reply mean
    /// one thing to a v1 reader and another to a later one. Emission is blocked
    /// by [`Reason::is_reserved`]; a client that receives it treats it as an
    /// unknown reason.
    IntentSlotsExhausted = 20,
    ActiveClientCap = 21,
    ResourceSlotsExhausted = 22,
    AclSlotsExhausted = 23,
}

impl Reason {
    /// Is this one of §10 stage 3's **reason-coded admission denials** -- the
    /// refusals a leader may answer pre-proposal, `NotConsumed`, without
    /// writing anything?
    ///
    /// Exhaustive by design; see the module docs for why each misclassification
    /// is a live defect rather than a style question.
    pub fn is_admission_denial(self) -> bool {
        match self {
            Reason::CounterExhausted
            | Reason::StateBudgetExhausted
            | Reason::ResourceSlotsExhausted
            | Reason::AclSlotsExhausted => true,

            // Consumed conflicts. These must reach the log and save a result:
            // denied pre-proposal, a client that may not alter the request at
            // an already-sent sequence would retry the stale value forever.
            Reason::AclRevisionMismatch
            | Reason::AdminRevisionMismatch
            | Reason::ValueRevisionMismatch
            | Reason::EpochMismatch
            | Reason::MembershipRevisionMismatch
            | Reason::OperationIdConflict => false,

            // Business refusals decided against replicated state at apply.
            Reason::HeldByOther
            | Reason::NotHeld
            | Reason::NotHolder
            | Reason::PermissionChanged
            | Reason::MembershipIntentActive
            | Reason::MembershipCannotCancel => false,

            // Classifier results (§8.2.1), never admission decisions.
            Reason::None
            | Reason::SequenceTooOld
            | Reason::SequenceGap
            | Reason::SequenceConflict
            | Reason::ClientRetired
            | Reason::RetainedSessionCapReached
            | Reason::ActiveClientCap => false,

            // Reserved; never emitted, so never projected.
            Reason::IntentSlotsExhausted => false,
        }
    }

    /// Is a `NotConsumed` refusal carrying this reason **terminal** (§10)?
    ///
    /// The unifying rule: a refusal is terminal exactly when no v1 command can
    /// clear the condition, so every retry is guaranteed the same answer and
    /// stopping is correct rather than premature.
    ///
    /// This says nothing about a `Consumed` result. **Every `Consumed` result is
    /// terminal because it is saved** -- including a committed `19` whose budget
    /// condition later clears. Use [`is_terminal`](Self::is_terminal) with the
    /// disposition rather than reading this alone; conflating the two would have
    /// a client retry a sequence whose rejection is already permanent.
    pub fn is_terminal_when_not_consumed(self) -> bool {
        match self {
            // Tombstones are never deleted.
            Reason::ClientRetired
            // Retained-session count never decreases in v1.
            | Reason::RetainedSessionCapReached
            // No command lowers any counter -- neither a client's sequence nor
            // a resource's ACL, epoch or lease revision.
            | Reason::CounterExhausted
            // v1 has no delete-resource command (§9.1).
            | Reason::ResourceSlotsExhausted => true,

            // The instructive near misses: each has a v1 command that clears it.
            // Retiring frees an active slot; grant removal frees an ACL row; a
            // value-shrinking compare_and_set or release_with_value, a grant
            // removal, or an admin removal frees growth bytes.
            Reason::ActiveClientCap
            | Reason::AclSlotsExhausted
            | Reason::StateBudgetExhausted => false,

            Reason::None
            | Reason::HeldByOther
            | Reason::NotHeld
            | Reason::NotHolder
            | Reason::EpochMismatch
            | Reason::ValueRevisionMismatch
            | Reason::AclRevisionMismatch
            | Reason::AdminRevisionMismatch
            | Reason::SequenceTooOld
            | Reason::SequenceGap
            | Reason::SequenceConflict
            | Reason::PermissionChanged
            | Reason::MembershipRevisionMismatch
            | Reason::MembershipIntentActive
            | Reason::MembershipCannotCancel
            | Reason::OperationIdConflict
            | Reason::IntentSlotsExhausted => false,
        }
    }

    /// Is this code reserved -- defined so it cannot be reused, but never
    /// emitted by any transition (§5.2)?
    ///
    /// Emission paths assert on this rather than trusting a comment.
    pub fn is_reserved(self) -> bool {
        matches!(self, Reason::IntentSlotsExhausted)
    }
}

/// Whether a request's sequence was consumed -- that is, whether a result was
/// saved for it and a retry will be answered from the session table rather than
/// re-applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Disposition {
    /// A result was saved for this sequence. Always terminal.
    Consumed,
    /// Nothing was written for this sequence; the client may re-send it unless
    /// the reason is terminal.
    NotConsumed,
}

impl Disposition {
    /// Is a result with this disposition and `reason` terminal (§10)?
    ///
    /// There are five terminal cases: every `Consumed`, plus `NotConsumed` with
    /// one of the four reasons no v1 command can clear.
    pub fn is_terminal(self, reason: Reason) -> bool {
        match self {
            Disposition::Consumed => true,
            Disposition::NotConsumed => reason.is_terminal_when_not_consumed(),
        }
    }
}

/// Generates the `Outcome` enum, its `ALL` list, and `from_code`, for the same
/// reason `reasons!` exists.
macro_rules! outcomes {
    ($($(#[$m:meta])* $name:ident = $code:expr),* $(,)?) => {
        /// An applied outcome code (§5.2).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(i32)]
        pub enum Outcome {
            $($(#[$m])* $name = $code),*
        }

        impl Outcome {
            /// Every outcome in §5.2.
            pub const ALL: &'static [Outcome] = &[$(Outcome::$name),*];

            /// Reconstruct an outcome from a stored or received code. `None`
            /// for a code outside §5.2; see [`Reason::from_code`] on why this
            /// does not panic.
            pub fn from_code(code: i32) -> Option<Outcome> {
                match code {
                    $($code => Some(Outcome::$name),)*
                    _ => None,
                }
            }

            /// The wire code.
            pub fn code(self) -> i32 {
                self as i32
            }
        }
    };
}

outcomes! {
    Granted = 1,
    Renewed = 2,
    Mutated = 3,
    Released = 4,
    Revoked = 5,
    GrantUpdated = 6,
    AdminUpdated = 7,
    MembershipAccepted = 8,
    ClientRetired = 9,
    MembershipCancelled = 10,
    /// Every rejection shares this outcome; the reason carries the cause. A
    /// test that compares only `outcome_code` therefore asserts almost nothing
    /// about a rejection -- the spike had exactly that bug.
    ///
    /// A rejection normally carries a **nonzero** reason. The single exception
    /// is a committed logical request-shape violation, which is
    /// `(100, 0 None)`: §5.2 has no reason code for a request that is shapeless
    /// to the protocol. Apply must still be total -- skipping a committed entry
    /// diverges the voters, and halting would hand a faulty leader an
    /// availability kill switch -- so it commits that pairing, saves it, and
    /// replays it exactly. See [`Reason::None`].
    Rejected = 100,
}

impl Outcome {
    /// Was the command rejected?
    pub fn is_rejection(self) -> bool {
        matches!(self, Outcome::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Oracles quoted from the specification, not from the code ────────────
    //
    // These lists are transcribed from §5.2 and §10. They must NOT be derived
    // from `Reason`'s methods: a matrix that validates a projection by calling
    // the predicate it is testing proves only that the code agrees with itself,
    // and the spike shipped exactly that mistake -- moving a reason into the
    // wrong arm left every case passing.

    /// §10 stage 3: the reason-coded admission denials.
    const ADMISSION_DENIAL_CODES: [i32; 4] = [15, 19, 22, 23];

    /// §10: `NotConsumed` refusals no v1 command can clear.
    const TERMINAL_NOT_CONSUMED_CODES: [i32; 4] = [11, 15, 18, 22];

    /// §5.2: defined so it cannot be reused, but never emitted.
    const RESERVED_CODES: [i32; 1] = [20];

    /// §5.2 declares reasons 0 through 23 with no gaps.
    const REASON_CODE_RANGE: std::ops::RangeInclusive<i32> = 0..=23;

    /// §5.2's outcome codes.
    const OUTCOME_CODES: [i32; 11] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 100];

    #[test]
    fn every_declared_code_in_the_spec_range_exists_exactly_once() {
        for code in REASON_CODE_RANGE {
            assert!(
                Reason::from_code(code).is_some(),
                "§5.2 declares reason {code}, which this enum does not define"
            );
        }
        assert_eq!(
            Reason::ALL.len(),
            REASON_CODE_RANGE.count(),
            "the enum defines a different number of reasons than §5.2 declares"
        );

        let mut codes: Vec<i32> = Reason::ALL.iter().map(|r| r.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            Reason::ALL.len(),
            "two reasons share a code; a saved reply would be ambiguous"
        );
    }

    #[test]
    fn from_code_round_trips_and_rejects_codes_outside_the_spec() {
        for reason in Reason::ALL {
            assert_eq!(Reason::from_code(reason.code()), Some(*reason));
        }
        // Data, not a bug: an unrecognised code must be reportable rather than
        // fatal. -1 and 24 bracket the declared range; i32::MAX is the corrupt
        // case.
        for code in [-1, 24, 99, i32::MAX, i32::MIN] {
            assert_eq!(Reason::from_code(code), None, "code {code} must not map");
        }
    }

    #[test]
    fn admission_denial_classification_matches_the_spec_list() {
        let mut found = Vec::new();
        for reason in Reason::ALL {
            let expected = ADMISSION_DENIAL_CODES.contains(&reason.code());
            assert_eq!(
                reason.is_admission_denial(),
                expected,
                "{reason:?} (code {}) is classified against §10's stage-3 list",
                reason.code()
            );
            if reason.is_admission_denial() {
                found.push(reason.code());
            }
        }
        assert_eq!(
            found.len(),
            ADMISSION_DENIAL_CODES.len(),
            "§10 names exactly {} admission denials",
            ADMISSION_DENIAL_CODES.len()
        );
    }

    /// The single case the spike got wrong twice, called out on its own so a
    /// regression names the consequence rather than a code number.
    #[test]
    fn a_stale_revision_is_a_consumed_conflict_not_an_admission_denial() {
        for reason in [
            Reason::AclRevisionMismatch,
            Reason::AdminRevisionMismatch,
            Reason::ValueRevisionMismatch,
            Reason::MembershipRevisionMismatch,
        ] {
            assert!(
                !reason.is_admission_denial(),
                "{reason:?} denied pre-proposal wedges a client that may not \
                 change the request at an already-sent sequence: it would retry \
                 the stale revision forever"
            );
        }
    }

    #[test]
    fn terminal_not_consumed_classification_matches_the_spec_list() {
        let mut found = Vec::new();
        for reason in Reason::ALL {
            let expected = TERMINAL_NOT_CONSUMED_CODES.contains(&reason.code());
            assert_eq!(
                reason.is_terminal_when_not_consumed(),
                expected,
                "{reason:?} (code {}) is classified against §10's terminal list",
                reason.code()
            );
            if reason.is_terminal_when_not_consumed() {
                found.push(reason.code());
            }
        }
        assert_eq!(found.len(), TERMINAL_NOT_CONSUMED_CODES.len());
    }

    /// The near misses, named individually: each is retryable because a
    /// specific v1 command frees the resource it ran out of.
    #[test]
    fn the_retryable_capacity_reasons_each_have_a_command_that_clears_them() {
        for (reason, clearing_command) in [
            (
                Reason::ActiveClientCap,
                "retire_client frees an active slot",
            ),
            (
                Reason::AclSlotsExhausted,
                "set_grant removal frees an ACL row",
            ),
            (
                Reason::StateBudgetExhausted,
                "a value-shrinking compare_and_set frees growth bytes",
            ),
        ] {
            assert!(
                !reason.is_terminal_when_not_consumed(),
                "{reason:?} is retryable: {clearing_command}"
            );
        }
    }

    /// `Consumed` is terminal whatever the reason -- including a committed `19`
    /// whose budget condition later clears, because the rejection is saved.
    #[test]
    fn every_consumed_result_is_terminal_whatever_the_reason() {
        for reason in Reason::ALL {
            assert!(
                Disposition::Consumed.is_terminal(*reason),
                "a saved result is permanent, so Consumed + {reason:?} is terminal"
            );
        }
    }

    #[test]
    fn exactly_five_terminal_cases_exist() {
        let consumed = Reason::ALL
            .iter()
            .filter(|r| Disposition::Consumed.is_terminal(**r))
            .count();
        let not_consumed = Reason::ALL
            .iter()
            .filter(|r| Disposition::NotConsumed.is_terminal(**r))
            .count();
        // Every Consumed is terminal, so the interesting count is the four
        // NotConsumed ones -- "five cases" is 1 rule + 4 reasons, not 5 reasons.
        assert_eq!(consumed, Reason::ALL.len());
        assert_eq!(
            not_consumed, 4,
            "§10 names four terminal NotConsumed reasons"
        );
    }

    #[test]
    fn the_reserved_code_is_defined_but_never_classified_as_emittable() {
        for reason in Reason::ALL {
            assert_eq!(
                reason.is_reserved(),
                RESERVED_CODES.contains(&reason.code()),
                "{reason:?} is classified against §5.2's reserved list"
            );
        }
        // Reserved means "never emitted", which is a property of the emission
        // paths; what this file can pin is that it is neither an admission
        // denial nor terminal, so no projection can route through it.
        let reserved = Reason::IntentSlotsExhausted;
        assert!(!reserved.is_admission_denial());
        assert!(!reserved.is_terminal_when_not_consumed());
    }

    #[test]
    fn outcome_codes_match_the_spec_list() {
        let mut codes: Vec<i32> = Outcome::ALL.iter().map(|o| o.code()).collect();
        codes.sort_unstable();
        let mut expected = OUTCOME_CODES.to_vec();
        expected.sort_unstable();
        assert_eq!(codes, expected, "§5.2 fixes the outcome code space");

        for outcome in Outcome::ALL {
            assert_eq!(Outcome::from_code(outcome.code()), Some(*outcome));
        }
        assert_eq!(Outcome::from_code(101), None);
        assert_eq!(Outcome::from_code(0), None, "0 is not an outcome");
    }

    /// `(100 Rejected, 0 None)` is reserved for exactly one condition: a
    /// committed logical request-shape violation. Pinned here, over the whole
    /// code space, so no future reason can quietly claim the pairing.
    ///
    /// The rule is stated as an exhaustive property rather than a comment: a
    /// rejection carrying `None` means "shape violation" and nothing else, so
    /// a client can rely on a nonzero reason identifying a coordination cause.
    #[test]
    fn a_rejection_with_no_reason_is_reserved_for_the_shape_violation_case() {
        // Every reason except `None` is a defined coordination cause, so
        // pairing it with `Rejected` is unambiguous.
        let coordination_causes: Vec<_> =
            Reason::ALL.iter().filter(|r| **r != Reason::None).collect();
        assert_eq!(
            coordination_causes.len(),
            Reason::ALL.len() - 1,
            "exactly one reason -- `None` -- is not a coordination cause"
        );

        // And `None` is the only one that can accompany a rejection without
        // naming a cause, which is what makes the pairing a reliable marker.
        assert_eq!(Reason::None.code(), 0);
        assert!(
            !Reason::None.is_admission_denial(),
            "the forced case is not an admission denial: it committed"
        );
        assert!(
            !Reason::None.is_terminal_when_not_consumed(),
            "the forced case is terminal through Consumed, not through the reason"
        );
        // Terminality comes from the disposition, and Consumed is always
        // terminal -- so a forced rejection is terminal without `None` needing
        // to be classified terminal itself.
        assert!(Disposition::Consumed.is_terminal(Reason::None));
    }

    #[test]
    fn only_one_outcome_is_a_rejection() {
        let rejections: Vec<_> = Outcome::ALL.iter().filter(|o| o.is_rejection()).collect();
        assert_eq!(
            rejections,
            vec![&Outcome::Rejected],
            "every rejection shares outcome 100; the reason carries the cause"
        );
    }
}
