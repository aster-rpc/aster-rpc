//! The voter runtime: the one owner of the Raft instance, its storage, its
//! controllers, and their shutdown.
//!
//! ## Why one owner
//!
//! §7.6 fixes cancellation as the strict reverse of construction — controllers,
//! then RPC deregistration, then `Raft::shutdown()`, then the state and log
//! actors and read leases, then the directory lock. That ordering is only
//! enforceable if one value owns all of it: a runtime assembled from
//! independently-dropped handles has whatever order the compiler picks, and
//! releasing the directory lock while an actor still holds a database open is
//! how a second voter is admitted to a directory the first has not finished
//! with.
//!
//! ## Shape
//!
//! | Module | Owns |
//! |---|---|
//! | [`config`] | the one pinned `openraft::Config` and its cross-constant invariants |
//! | [`lifecycle`] | the §7.6 table, and the only producer of a lifecycle code |
//! | [`owner`] | the one owner of the Raft, its storage, its controllers, and their shutdown |
//! | [`readiness`] | the published view and the level-triggered `wait_ready()` |
//!
//! ## Unreached-by-shipping-code allowance
//!
//! The runtime owner itself needs a `RaftNetworkFactory`, which is the next
//! checkpoint's, so nothing in a shipping build constructs one yet. Scoped to
//! this module tree rather than the crate root, and it comes off when the
//! network lands.

#![allow(dead_code)]

pub(crate) mod config;
pub(crate) mod lifecycle;
pub(crate) mod owner;
pub(crate) mod readiness;
