//! Error mapping from anyhow/core errors to NAPI errors.

use napi::Error as NapiError;

/// Convert an anyhow::Error to a napi::Error.
pub fn to_napi_err(e: anyhow::Error) -> NapiError {
    NapiError::from_reason(format!("{:#}", e))
}
