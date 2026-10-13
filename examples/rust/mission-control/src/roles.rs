//! Capability roles for Mission Control (the auth chapter).
//!
//! Each role is a string in the `aster.role` claim of a node's enrollment
//! credential (comma-separated for several). `AsterServer` maps a peer's
//! verified claims to these roles, and Gate 3 matches them against each
//! method's `requires`. The values are identical to Python's `roles.py` and
//! TypeScript's `roles.ts`, so a credential works against any binding's server.

/// May read fleet status (`getStatus`).
pub const STATUS: &str = "ops.status";
/// May tail logs (`tailLogs`).
pub const LOGS: &str = "ops.logs";
/// Administers agents: tails logs and runs commands (`runCommand`).
pub const ADMIN: &str = "ops.admin";
/// May push metrics (`ingestMetrics`).
pub const INGEST: &str = "ops.ingest";
