//! Aster observability: the Rust-owned `tracing` / OpenTelemetry pipeline that
//! every binding (Python, TypeScript, Java, .NET, Go) shares.
//!
//! Design: `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md`. This is
//! phase O0: an opt-in stderr sink, so Rust-side events are visible from any
//! binding before the full pipeline lands.
//!
//! ```text
//! ASTER_LOG_STDERR=1              enable (also: true, yes, on)
//! ASTER_LOG=warn,aster=debug      EnvFilter directives (default: warn,aster=info)
//! ```
//!
//! Each binding's native crate calls [`init_stderr_from_env`] once when it
//! loads. Later phases install the full subscriber from this same crate, so the
//! two never compete for the process-wide global default.

mod subscriber;

pub use subscriber::{init_stderr_from_env, StderrInit, DEFAULT_FILTER, ENV_FILTER, ENV_STDERR};
