//! Key-material inputs shared by the T7 modules (attestation, HPKE identity
//! keys, grants): 32-byte values given as 64 hex characters or raw bytes.

use napi::bindgen_prelude::*;

use aster::{PublicKey, SecretKey};

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

/// A 32-byte value given as 64 hex characters or raw bytes.
pub(crate) fn key32(what: &str, v: &Either<String, Uint8Array>) -> Result<[u8; 32]> {
    let raw = match v {
        Either::A(s) => hex::decode(s.trim())
            .map_err(|_| invalid(format!("{what} must be 64 hex characters or 32 bytes")))?,
        Either::B(b) => b.to_vec(),
    };
    raw.try_into()
        .map_err(|_| invalid(format!("{what} must be 64 hex characters or 32 bytes")))
}

/// A 32-byte value given as raw bytes only.
pub(crate) fn bytes32(what: &str, v: &[u8]) -> Result<[u8; 32]> {
    v.try_into()
        .map_err(|_| invalid(format!("{what} must be 32 bytes, got {}", v.len())))
}

pub(crate) fn secret_in(what: &str, v: &Either<String, Uint8Array>) -> Result<SecretKey> {
    Ok(SecretKey::from_bytes(key32(what, v)?))
}

pub(crate) fn public_in(what: &str, v: &Either<String, Uint8Array>) -> Result<PublicKey> {
    Ok(PublicKey::from_bytes(key32(what, v)?))
}
