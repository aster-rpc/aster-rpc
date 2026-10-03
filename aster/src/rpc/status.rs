//! RPC status codes and error mapping.
//!
//! [`StatusCode`] and the [`RpcStatus`] constructors (`ok`, `error`,
//! `status_code`, `is_ok`) live in [`aster_transport_core::envelope`] next to
//! the envelope codec (Aster-SPEC §6.5); this module maps them to and from
//! [`aster::Error`](crate::Error).

pub use aster_transport_core::envelope::StatusCode;

use super::codec::{CodecError, RpcStatus};
use crate::error::Error;

/// Convert a non-OK [`RpcStatus`] into an [`Error::Rpc`], zipping detail
/// key/value pairs.
pub(crate) fn status_to_error(status: RpcStatus) -> Error {
    let details = status
        .detail_keys
        .into_iter()
        .zip(status.detail_values)
        .collect();
    Error::Rpc {
        code: status.code,
        message: status.message,
        details,
    }
}

impl From<CodecError> for Error {
    fn from(e: CodecError) -> Self {
        // Envelope ser/de of a known framework type failing is an internal fault.
        Error::Transport(anyhow::anyhow!("rpc codec: {e}"))
    }
}

/// Build an [`RpcStatus`] from an [`Error`] returned by a handler: an
/// `Error::Rpc` keeps its code/message/details; anything else maps to
/// `INTERNAL`. Used by `#[aster::service]`-generated dispatchers.
pub fn status_from_error(e: &Error) -> RpcStatus {
    match e {
        Error::Rpc {
            code,
            message,
            details,
        } => RpcStatus {
            code: *code,
            message: message.clone(),
            detail_keys: details.iter().map(|(k, _)| k.clone()).collect(),
            detail_values: details.iter().map(|(_, v)| v.clone()).collect(),
        },
        other => RpcStatus::error(StatusCode::Internal, other.to_string()),
    }
}
