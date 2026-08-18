//! The single decode site for persisted values (§4.2).
//!
//! ## Why this module exists rather than a lint
//!
//! Only two failures in this subsystem are retryable, and a persisted *decode*
//! classified as one makes a corrupt durable value look like a bad request the
//! caller should send again. Keeping that straight by review failed twice:
//! first the whole `Codec` variant was retryable, then a structural guard was
//! added that matched `try_into` on the same line as the constructor — and
//! rustfmt puts them on different lines, so mutating [`decode_array32`] back to
//! the retryable variant passed every guard and all 513 tests.
//!
//! A line-window guard is the wrong shape for this. The same problem was solved
//! for durability by giving write transactions exactly one construction site
//! ([`schema::durable_write`](super::schema::durable_write)); this is that
//! answer applied to decoding. Every persisted Fory root goes through
//! [`decode_persisted`], every persisted scalar through the helpers below, and
//! none of them can express the retryable variant at all — the direction is
//! encoded in which function you call, not in which constructor you remember.
//!
//! Checkpoint C's state machine routes its new persisted roots through here.

use fory_core::serializer::Serializer;
use fory_core::ForyDefault;

use super::error::{Db, StorageError, StorageResult};
use crate::coordination::contract::payload_fory;
use crate::rpc::contract::WireField;

/// Refuse a stored value over its ceiling, before it is decoded.
///
/// The read-side twin of [`encode_bounded`], and classified the other way: an
/// over-ceiling value on disk is corruption, not a request to retry. Applied
/// before allocation -- a length check that runs after the value is decoded
/// has not bounded anything.
pub(crate) fn check_stored_len(
    db: Db,
    what: &'static str,
    bytes: &[u8],
    max_bytes: usize,
) -> StorageResult<()> {
    if bytes.len() > max_bytes {
        return Err(StorageError::corrupt(
            db,
            what,
            format!(
                "stored value is {} bytes, over the {max_bytes}-byte ceiling",
                bytes.len()
            ),
        ));
    }
    Ok(())
}

/// Decode a persisted Fory root.
///
/// The only place in this subsystem where a stored Fory value is decoded, and
/// it has no way to report anything but corruption.
pub(crate) fn decode_persisted<T>(db: Db, what: &'static str, bytes: &[u8]) -> StorageResult<T>
where
    T: WireField + Serializer + ForyDefault,
{
    payload_fory::<T>()
        .deserialize(bytes)
        .map_err(|source| StorageError::corrupt(db, what, source))
}

/// Decode a persisted big-endian `u32`.
pub(crate) fn decode_u32(db: Db, what: &'static str, bytes: &[u8]) -> StorageResult<u32> {
    let array: [u8; 4] = bytes.try_into().map_err(|_| {
        StorageError::corrupt(db, what, format!("expected 4 bytes, got {}", bytes.len()))
    })?;
    Ok(u32::from_be_bytes(array))
}

/// Decode a persisted 32-byte identifier.
pub(crate) fn decode_array32(db: Db, what: &'static str, bytes: &[u8]) -> StorageResult<[u8; 32]> {
    bytes.try_into().map_err(|_| {
        StorageError::corrupt(db, what, format!("expected 32 bytes, got {}", bytes.len()))
    })
}

/// Decode a persisted UTF-8 string.
pub(crate) fn decode_string(db: Db, what: &'static str, bytes: &[u8]) -> StorageResult<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|source| StorageError::corrupt(db, what, format!("not valid UTF-8: {source}")))
}

/// Encode a value this build produced, for writing.
///
/// The counterpart, and the only place the retryable variant is raised at all:
/// a value that will not encode has written nothing, so the caller may
/// legitimately try again with a different one.
pub(crate) fn encode_owned<T>(db: Db, what: &'static str, value: &T) -> StorageResult<Vec<u8>>
where
    T: WireField + Serializer + ForyDefault,
{
    payload_fory::<T>()
        .serialize(value)
        .map_err(|source| StorageError::encode_failed(db, what, source))
}

/// Encode, and refuse a value over its §4.2 ceiling.
///
/// The ceiling lives here rather than at each call site so that "encode" and
/// "encode and check the bound" are not two things a caller has to remember to
/// pair. It also keeps the retryable classification to one module: an
/// over-ceiling value is a rejected input, since nothing has been written.
pub(crate) fn encode_bounded<T>(
    db: Db,
    what: &'static str,
    value: &T,
    max_bytes: usize,
) -> StorageResult<Vec<u8>>
where
    T: WireField + Serializer + ForyDefault,
{
    let bytes = encode_owned(db, what, value)?;
    if bytes.len() > max_bytes {
        return Err(StorageError::encode_failed(
            db,
            what,
            format!(
                "encoded to {} bytes, over the {max_bytes}-byte ceiling",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::raft::CoordinationMetaV1;

    /// Every scalar decoder reports corruption on malformed input.
    ///
    /// Tested directly, by behaviour, because the structural guard that was
    /// supposed to cover this missed [`decode_array32`] entirely: it looked for
    /// `try_into` on the constructor's line, and rustfmt had moved it.
    #[test]
    fn every_scalar_decoder_reports_corruption() {
        let cases: Vec<(&str, StorageError)> = vec![
            (
                "u32 short",
                decode_u32(Db::Raft, "schema_version", &[0, 1]).expect_err("short"),
            ),
            (
                "u32 long",
                decode_u32(Db::Raft, "schema_version", &[0; 9]).expect_err("long"),
            ),
            (
                "array32 short",
                decode_array32(Db::Raft, "group_id", &[7; 31]).expect_err("short"),
            ),
            (
                "array32 long",
                decode_array32(Db::Raft, "group_id", &[7; 33]).expect_err("long"),
            ),
            (
                "array32 empty",
                decode_array32(Db::Raft, "group_id", &[]).expect_err("empty"),
            ),
            (
                "string",
                decode_string(Db::State, "authority_domain", &[0xff, 0xfe]).expect_err("not utf-8"),
            ),
        ];

        for (label, err) in cases {
            assert!(
                matches!(err, StorageError::StoredCorruption { .. }),
                "{label} must be stored corruption, got: {err:?}"
            );
            assert!(
                err.puts_durability_in_question(),
                "{label} must be sticky, got: {err:?}"
            );
        }
    }

    /// A persisted Fory root that will not decode is corruption too.
    #[test]
    fn a_malformed_persisted_root_reports_corruption() {
        let err = decode_persisted::<CoordinationMetaV1>(
            Db::State,
            "CoordinationMetaV1",
            b"not a Fory CoordinationMetaV1",
        )
        .expect_err("malformed");
        assert!(
            matches!(err, StorageError::StoredCorruption { .. }),
            "got: {err:?}"
        );
    }

    /// Encoding keeps the retryable classification, and round-trips.
    #[test]
    fn encoding_is_retryable_and_round_trips() {
        let meta = CoordinationMetaV1 {
            schema_version: 1,
            admin_revision: 9,
            ..Default::default()
        };
        let bytes = encode_owned(Db::State, "CoordinationMetaV1", &meta).expect("encode");
        let back: CoordinationMetaV1 =
            decode_persisted(Db::State, "CoordinationMetaV1", &bytes).expect("decode");
        assert_eq!(back.admin_revision, 9);

        assert!(
            !StorageError::encode_failed(Db::State, "x", "y").puts_durability_in_question(),
            "an encode failure wrote nothing and stays retryable"
        );
    }
}
