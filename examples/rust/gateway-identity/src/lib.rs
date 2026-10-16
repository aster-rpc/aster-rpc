//! Gateway identity example: a browser signs in at an HTTP edge, and the edge
//! calls an Aster service over iroh **on behalf of** that user.
//!
//! ```text
//! browser ──HTTP──▶ edge (Salvo + AsterClient) ──iroh──▶ service (AsterServer)
//!                     │ aster.gateway                        │ requires = roles
//!                     │ forwards the provider's JWT,         │ CallContext:
//!                     │ or mints one after a passkey         │ principal, actor,
//!                     │ ceremony (amr: webauthn)             │ auth_methods
//! ```
//!
//! - [`service`]: the `Notes` service; `publish` needs a passkey sign-in.
//! - [`edge`]: the HTTP routes, the bearer forward and the session mint.
//! - [`webauthn`]: a minimal ES256 relying party (registration, assertion)
//!   over `aws-lc-rs`, and a software authenticator for tests.
//! - [`idp`]: a stand-in OpenID provider serving its JWK set over HTTPS.
//! - [`fleet`]: root, service and edge, set up in one process.
//!
//! Run it with `cargo run -p gateway-identity` and open
//! <http://localhost:8080>. The guide is "Calling Aster services on behalf
//! of users" in `docs/aster-rust-getstarted.md`.

pub mod edge;
pub mod fleet;
pub mod idp;
pub mod service;
pub mod webauthn;
