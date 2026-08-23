//! The authenticated Raft transport (§5.3, §5.4, §6).
//!
//! ## Shape
//!
//! | Module | Owns |
//! |---|---|
//! | [`admit`] | the §5.3 request-check order, as one function |
//! | [`client`] | `RaftNetwork`/`RaftNetworkFactory` over Aster RPC |
//! | [`errors`] | `S3-NETERR`: the two exhaustive mappings into OpenRaft's error variants |
//! | [`service`] | the live `aster.coordination.Raft` v1 dispatcher |
//! | [`handoff`] | one stream's vote, metadata and body, bound from the wire to `install_full_snapshot` |
//!
//! ## Unreached-by-shipping-code allowance
//!
//! The mapping tables land before the network factory that uses them, because
//! §5.4 requires the factory to implement them **before it is used** — a
//! factory written first would have a single "network error" bucket in it for
//! however long it took to come back. Scoped to this module tree, and it comes
//! off when the factory lands.

#![allow(dead_code)]

pub(crate) mod admit;
pub(crate) mod client;
pub(crate) mod errors;
pub(crate) mod handoff;
pub(crate) mod service;
