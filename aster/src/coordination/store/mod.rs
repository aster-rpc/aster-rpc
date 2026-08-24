//! The public `aster.coordination.Store` v1 surface (§5.2, S4-A).
//!
//! `coordination-client` compiles all of it: the frozen wire declarations, the
//! method classification, and (as Slice 4 proceeds) the redirecting client.
//! The voter-side handlers live in [`super::service`] and the crate-private
//! runtime, behind `feature = "coordination"`.
//!
//! ## One declaration, nineteen methods
//!
//! [`methods::METHODS`] is the single classification table: name, §5.2 method
//! code, consistency class, and required committed permission for every Store
//! method. The service reservation, the handler router, the permission gate and
//! the client all consume this table rather than keeping a switch of their own
//! — a nineteenth hand-written switch is where methods drift. The gate test
//! compares the table's name set against [`super::service::STORE_METHODS`]
//! **as a set of the nineteen §5.2 literal names**, so a twentieth reserved
//! method, a dropped row, or a duplicate fails before any handler runs.

pub mod methods;
pub mod wire;

/// The live voter-side service (S4-B/S4-C). Voter-only: it proposes through
/// Raft and reads the guarded store.
#[cfg(feature = "coordination")]
pub(crate) mod host;
