//! RPC status codes and error mapping.
//!
//! Mirrors `bindings/python/aster/status.py` (Aster-SPEC §6.5). Codes 0–16
//! follow gRPC's `google.rpc.Code` semantically; 17–99 are reserved; 100+ are
//! Aster-native.

use super::codec::{CodecError, RpcStatus};
use crate::error::Error;

/// RPC status code carried in [`RpcStatus::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum StatusCode {
    Ok = 0,
    Cancelled = 1,
    Unknown = 2,
    InvalidArgument = 3,
    DeadlineExceeded = 4,
    NotFound = 5,
    AlreadyExists = 6,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    FailedPrecondition = 9,
    Aborted = 10,
    OutOfRange = 11,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
    DataLoss = 15,
    Unauthenticated = 16,
    /// Wire payload doesn't match the published contract (Aster-native).
    ContractViolation = 101,
}

impl StatusCode {
    /// The on-the-wire `i32` value.
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Map a wire code back to a [`StatusCode`]; unrecognised codes map to
    /// [`StatusCode::Unknown`].
    pub fn from_i32(code: i32) -> StatusCode {
        match code {
            0 => StatusCode::Ok,
            1 => StatusCode::Cancelled,
            2 => StatusCode::Unknown,
            3 => StatusCode::InvalidArgument,
            4 => StatusCode::DeadlineExceeded,
            5 => StatusCode::NotFound,
            6 => StatusCode::AlreadyExists,
            7 => StatusCode::PermissionDenied,
            8 => StatusCode::ResourceExhausted,
            9 => StatusCode::FailedPrecondition,
            10 => StatusCode::Aborted,
            11 => StatusCode::OutOfRange,
            12 => StatusCode::Unimplemented,
            13 => StatusCode::Internal,
            14 => StatusCode::Unavailable,
            15 => StatusCode::DataLoss,
            16 => StatusCode::Unauthenticated,
            101 => StatusCode::ContractViolation,
            _ => StatusCode::Unknown,
        }
    }
}

impl RpcStatus {
    /// A success status (`OK`, empty message).
    pub fn ok() -> Self {
        RpcStatus {
            code: StatusCode::Ok.as_i32(),
            ..Default::default()
        }
    }

    /// An error status with `code` and a human-readable `message`.
    pub fn error(code: StatusCode, message: impl Into<String>) -> Self {
        RpcStatus {
            code: code.as_i32(),
            message: message.into(),
            detail_keys: Vec::new(),
            detail_values: Vec::new(),
        }
    }

    /// The typed status code.
    pub fn status_code(&self) -> StatusCode {
        StatusCode::from_i32(self.code)
    }

    /// True if this status is `OK`.
    pub fn is_ok(&self) -> bool {
        self.code == StatusCode::Ok.as_i32()
    }
}

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
