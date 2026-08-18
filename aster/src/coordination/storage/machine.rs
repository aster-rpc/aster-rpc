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
