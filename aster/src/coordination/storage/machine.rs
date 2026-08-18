//! `RaftStateMachine` over `state.redb` (§11.3).
//!
//! Checkpoint C. The §11.3 tables are created and validated by
//! [`super::schema`]; what is missing is the bounded state actor, the typed
//! conversions between each model row and its declared Fory storage root, the
//! one-transaction apply batch, and `CommandReplyV1::NonCommand` for
//! OpenRaft's native Blank and Membership entries.
//!
//! The transition decision stays single-sourced in
//! [`model::boundary::apply_wire_command`](crate::coordination::model::boundary::apply_wire_command);
//! this module persists its result, it does not re-derive it.
//!
//! ## One rule fixed in advance
//!
//! Every encode on the apply path goes through
//! [`codec::encode_committed`](super::codec::encode_committed), never
//! [`encode_bounded`](super::codec::encode_bounded).
//!
//! The two differ only in classification, and the difference matters exactly
//! here. A retryable `EncodeFailed` is correct for a value a *caller* offered:
//! refusing it rejects a request and writes nothing. Once an entry is
//! committed the group has already decided it applies, so a state machine that
//! cannot encode the result has no retry available — reporting one would let
//! the runtime carry on with memory ahead of disk, which §4.4 forbids
//! outright.
