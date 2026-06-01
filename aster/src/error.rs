//! Error type for the Aster facade.

/// Errors returned by the Aster API.
///
/// Most failures from the transport core surface as [`Error::Transport`]; the
/// other variants exist so callers can match on the common, recognisable
/// failure modes without string-matching.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A blob was not found locally (and could not be fetched).
    #[error("blob not found: {0}")]
    BlobNotFound(String),

    /// A document / namespace was not found in the local replica.
    #[error("doc not found: {0}")]
    DocNotFound(String),

    /// A connection-level failure (dial, accept, or admission rejection).
    #[error("connection error: {0}")]
    Connection(String),

    /// A ticket failed to parse or decode.
    #[error("ticket error: {0}")]
    Ticket(String),

    /// An invalid argument was supplied to the API (bad length, bad hex, …).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// An ownership-attestation operation failed (mint, verify, or decode).
    #[error("attestation error: {0}")]
    Attestation(String),

    /// An RPC call returned a non-OK status (the `rpc` feature). `code` is the
    /// [`StatusCode`](crate::rpc::StatusCode) value; `details` are the trailer's
    /// key/value pairs.
    #[cfg(feature = "rpc")]
    #[error("rpc error [{code}]: {message}")]
    Rpc {
        code: i32,
        message: String,
        details: Vec<(String, String)>,
    },

    /// Any other failure propagated from the transport core.
    #[error(transparent)]
    Transport(#[from] anyhow::Error),
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
