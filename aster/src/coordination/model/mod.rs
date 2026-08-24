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

/// The §9.4 lease-engine adapter is **client** surface: `CoordinationSerializer`
/// binds the existing `LeaseSerializer` trait over a `CoordinationBackend`, and
/// Slice 4's live backend is the redirecting client. It compiles in the
/// `coordination-client` graph; nothing in it touches a transition.
pub mod adapter;
#[cfg(feature = "coordination")]
pub mod admin;
#[cfg(feature = "coordination")]
pub mod apply;
#[cfg(feature = "coordination")]
pub mod authority;
#[cfg(feature = "coordination")]
pub mod boundary;
#[cfg(feature = "coordination")]
pub mod classify;
#[cfg(feature = "coordination")]
pub mod expire;
#[cfg(feature = "coordination")]
pub mod grant;
#[cfg(feature = "coordination")]
pub mod history;
/// §12.1 logical limits and the §8.1 rights bits are **contract** facts, not
/// voter internals: the public wire declarations bound requests with them and
/// the client validates against them before sending. They compile in the
/// `coordination-client` graph; every transition that consumes them stays
/// voter-only.
pub mod limits;
#[cfg(feature = "coordination")]
pub mod membership;
#[cfg(feature = "coordination")]
pub mod project;
#[cfg(feature = "coordination")]
pub mod resource;
#[cfg(feature = "coordination")]
pub mod retire;
#[cfg(feature = "coordination")]
pub mod shape;
#[cfg(feature = "coordination")]
pub mod state;
#[cfg(feature = "coordination")]
pub mod tables;
#[cfg(feature = "coordination")]
pub mod wire;
