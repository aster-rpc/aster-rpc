//! The storage-independent replicated state machine (§8-§12).
//!
//! Everything here is a pure function of `(state, command)`. `apply()` reads
//! compiled logical constants and replicated state only -- never host-local
//! configuration, the wall clock, disk pressure, or physical store state. That
//! is not a style preference: two voters applying the same log index must reach
//! byte-identical state, and any host-dependent input breaks it silently, on one
//! replica, under load.
//!
//! Storage (Slice 2) and consensus (Slice 3) call into this module rather than
//! reimplementing its transitions.

pub mod classify;
pub mod limits;
pub mod state;
