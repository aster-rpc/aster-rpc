//! The crate's error type. No variant ever carries a header value, a secret or
//! an endpoint's userinfo: messages are built from field names and redacted
//! URLs only.

use std::fmt;

/// What can go wrong configuring or running the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A configuration value is invalid. The message names the setting (and its
    /// source), never a secret value.
    Config(String),
    /// [`crate::init`] was called again with a configuration that differs from
    /// the one in effect. The message lists the differing settings by name.
    /// The SDK globals are process-wide, so a second configuration cannot be
    /// honoured; shut the first pipeline down to replace it.
    AlreadyInitialized(String),
    /// An exporter or provider could not be built.
    Exporter(String),
    /// A flush or shutdown did not finish within its timeout.
    Timeout(&'static str),
    /// The SDK reported failures while flushing or shutting down.
    Sdk(String),
    /// The binding log bridge is not open (no pipeline, `init` without
    /// `log_bridge`, or a foreign global subscriber).
    LogBridgeNotOpen,
    /// A log drain already exists: the bridge has one consumer per process
    /// (design §7.2; the C ABI maps this to `ALREADY_EXISTS`).
    LogDrainBusy,
    /// The operation needs a crate feature this build lacks.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "invalid observability configuration: {m}"),
            Error::AlreadyInitialized(m) => write!(
                f,
                "observability is already initialised with a different configuration ({m})"
            ),
            Error::Exporter(m) => write!(f, "could not build the telemetry exporter: {m}"),
            Error::Timeout(what) => write!(f, "telemetry {what} timed out"),
            Error::Sdk(m) => write!(f, "telemetry SDK error: {m}"),
            Error::LogBridgeNotOpen => f.write_str("the binding log bridge is not open"),
            Error::LogDrainBusy => {
                f.write_str("a log drain is already active (one consumer per process)")
            }
            Error::Unsupported(what) => write!(f, "unsupported in this build: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// `Result` with [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
