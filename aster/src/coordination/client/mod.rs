//! The public coordination client (§3.4, S4-D, S4-E).
//!
//! `coordination-client` compiles all of this; nothing here reaches OpenRaft,
//! redb, or any voter runtime — `check-coordination-deps.sh` holds that graph
//! exact.

pub mod journal;
