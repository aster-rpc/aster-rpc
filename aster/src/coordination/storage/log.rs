//! `RaftLogStorage` over `raft.redb` (§11.2).
//!
//! Checkpoint B. The tables, meta allowlist, and identity binding this will
//! write through already exist in [`super::schema`]; what is missing is the
//! bounded blocking log actor, the Fory entry/meta codecs, and the trait
//! implementation itself.
//!
//! Deliberately empty rather than partially implemented: a half-written log
//! store that compiles is exactly the thing the checkpoint order exists to
//! prevent, because OpenRaft's suite would then report a state-machine or
//! snapshot gap as a log failure.
