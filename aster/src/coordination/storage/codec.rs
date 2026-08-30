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

// Scoped to this module -- see the `storage` module docs. `encode_committed`
// additionally has no caller until Checkpoint C, which is the point: it exists
// so the apply path has a fatal encode available rather than reaching for the
// retryable one.
#![allow(dead_code)]

use fory_core::serializer::Serializer;
use fory_core::ForyDefault;

use super::error::{Db, StorageError, StorageResult};
use crate::rpc::contract::WireField;

/// One payload runtime per persisted root type, built once for the process.
///
/// ## Why this exists (S5-E)
///
/// [`payload_fory`](crate::coordination::contract::payload_fory) *constructs* a runtime: a fresh `PayloadRegistry`, a
/// `register_by_name` and the type meta that comes with it. Every persisted
/// encode and decode called it, so a bulk pass paid that construction **once per
/// row**. Measured on macOS aarch64, release, over a 130-byte `ResourceRowV1`:
///
/// | | p50 |
/// |---|---|
/// | `payload_fory::<ResourceRowV1>()` alone | 10.7 µs |
/// | `encode_committed` (constructing) | 33.2 µs |
/// | `decode_persisted_bounded` (constructing) | 37.6 µs |
/// | `serialize` against a reused runtime | 42 ns |
/// | `deserialize` against a reused runtime | 42 ns |
///
/// Three orders of magnitude, and it is the whole reason `rows::commit` and
/// `rows::load` cost tens of seconds at 200,000 rows while redb itself inserts
/// those rows in 290 ms and scans them in 8.6 ms. This is a defect, not a
/// property of the format.
///
/// ## Why a cache rather than threading a runtime through every caller
///
/// There are about fifty persisted encode/decode sites across seven modules,
/// several of them inside streaming visitors that cannot conveniently hold one.
/// Hoisting a runtime at each is fifty chances to hoist none, and the next
/// call site added would silently start constructing per row again. Caching
/// here fixes every site at once and keeps this module what its header says it
/// is: the single place a persisted value is encoded or decoded.
///
/// The runtimes are leaked deliberately. The set of persisted root types is
/// fixed and small — one per persisted table plus the metadata roots — so this
/// is a bounded one-time allocation, not growth.
mod runtime {
    use std::any::TypeId;
    use std::collections::HashMap;
    use std::sync::{OnceLock, RwLock};

    use fory_core::serializer::Serializer;
    use fory_core::ForyDefault;

    use crate::coordination::contract::payload_fory;
    use crate::rpc::contract::WireField;

    /// Runtimes actually constructed, so a gate can prove a bulk pass builds
    /// none rather than one per row. A wall-clock assertion would be the wrong
    /// shape for CI; a construction count is exact.
    #[cfg(test)]
    pub(super) static CONSTRUCTED: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    #[allow(clippy::type_complexity)]
    fn cache() -> &'static RwLock<HashMap<TypeId, &'static fory_core::Fory>> {
        static CACHE: OnceLock<RwLock<HashMap<TypeId, &'static fory_core::Fory>>> = OnceLock::new();
        CACHE.get_or_init(|| RwLock::new(HashMap::new()))
    }

    /// The process-wide runtime for `T`, constructing it at most once.
    pub(super) fn for_type<T>() -> &'static fory_core::Fory
    where
        T: WireField + Serializer + ForyDefault + 'static,
    {
        let key = TypeId::of::<T>();
        if let Some(found) = cache().read().expect("the codec cache").get(&key) {
            return found;
        }
        let mut guard = cache().write().expect("the codec cache");
        // Another thread may have inserted between the read and the write.
        if let Some(found) = guard.get(&key) {
            return found;
        }
        #[cfg(test)]
        CONSTRUCTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let built: &'static fory_core::Fory = Box::leak(Box::new(payload_fory::<T>()));
        guard.insert(key, built);
        built
    }
}

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

/// Copy a borrowed stored value out, refusing anything over its ceiling.
///
/// The check and the copy are one operation because their *order* is what
/// matters and is not observable: checking after `to_vec` produces the same
/// error for the same input, so no test can tell the two apart -- verified by
/// mutation. What a test cannot catch, a signature can. There is no borrowed
/// slice left to copy incorrectly once the caller has this.
pub(crate) fn copy_stored_bounded(
    db: Db,
    what: &'static str,
    bytes: &[u8],
    max_bytes: usize,
) -> StorageResult<Vec<u8>> {
    check_stored_len(db, what, bytes, max_bytes)?;
    Ok(bytes.to_vec())
}

/// Decode a persisted Fory root, refusing anything over its ceiling first.
///
/// The only decoder this module exports. §4.2 requires the bound *before*
/// allocation, and pairing a separate check with a separate decode is exactly
/// the arrangement that let `state.redb`'s metadata be copied and decoded with
/// no ceiling at all while the log's was bounded — the two halves were the
/// caller's to remember, and one caller forgot.
pub(crate) fn decode_persisted_bounded<T>(
    db: Db,
    what: &'static str,
    bytes: &[u8],
    max_bytes: usize,
) -> StorageResult<T>
where
    T: WireField + Serializer + ForyDefault + 'static,
{
    check_stored_len(db, what, bytes, max_bytes)?;
    decode_persisted(db, what, bytes)
}

/// The unbounded primitive.
///
/// Private on purpose: every persisted value has a declared ceiling, so
/// reaching this without one is always a mistake. [`decode_persisted_bounded`]
/// is the way in.
fn decode_persisted<T>(db: Db, what: &'static str, bytes: &[u8]) -> StorageResult<T>
where
    T: WireField + Serializer + ForyDefault + 'static,
{
    runtime::for_type::<T>()
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
    T: WireField + Serializer + ForyDefault + 'static,
{
    runtime::for_type::<T>()
        .serialize(value)
        .map_err(|source| StorageError::encode_failed(db, what, source))
}

/// Encode a value being written as part of applying an already-committed
/// entry, where failure is **fatal** rather than retryable.
///
/// The distinction Checkpoint C needs. [`encode_bounded`] is right for a value
/// a caller offered: refusing it rejects a request and writes nothing. It is
/// wrong once the entry is committed — the group has already decided that
/// entry applies, so a state machine that cannot encode the result has no
/// retry available and must not report one. Every apply-path encode goes
/// through here.
pub(crate) fn encode_committed<T>(
    db: Db,
    what: &'static str,
    value: &T,
    max_bytes: usize,
) -> StorageResult<Vec<u8>>
where
    T: WireField + Serializer + ForyDefault + 'static,
{
    encode_bounded(db, what, value, max_bytes).map_err(|err| {
        StorageError::corrupt(
            db,
            what,
            format!("failed to encode the result of an already-committed entry: {err}"),
        )
    })
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
    T: WireField + Serializer + ForyDefault + 'static,
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

    /// An over-ceiling stored value is refused before it is decoded.
    ///
    /// The bytes here are not a valid Fory root, so if the ceiling were
    /// checked after the decode the failure would name the decode instead --
    /// which is how an unbounded path stays invisible while looking correct.
    #[test]
    fn an_over_ceiling_stored_value_is_refused_before_decoding() {
        let oversized = vec![0xAAu8; 1024];
        let err = decode_persisted_bounded::<CoordinationMetaV1>(
            Db::State,
            "CoordinationMetaV1",
            &oversized,
            64,
        )
        .expect_err("over the ceiling");

        assert!(
            err.to_string().contains("over the 64-byte ceiling"),
            "the ceiling must be what refused it, not the decode: {err}"
        );
        assert!(matches!(err, StorageError::StoredCorruption { .. }));
    }

    /// An apply-path encode failure is fatal, not retryable.
    ///
    /// `encode_bounded` and `encode_committed` differ only in classification,
    /// and only the second is correct once the group has already decided the
    /// entry applies.
    #[test]
    fn a_committed_encode_failure_is_fatal_while_a_caller_encode_is_not() {
        let meta = CoordinationMetaV1 {
            authority_domain: "x".repeat(512),
            ..Default::default()
        };

        let offered = encode_bounded(Db::State, "CoordinationMetaV1", &meta, 8)
            .expect_err("over the ceiling");
        assert!(
            !offered.puts_durability_in_question(),
            "a value the caller offered can be refused and retried: {offered:?}"
        );

        let committed = encode_committed(Db::State, "CoordinationMetaV1", &meta, 8)
            .expect_err("over the ceiling");
        assert!(
            committed.puts_durability_in_question(),
            "an already-committed entry has no retry available: {committed:?}"
        );
        assert!(
            committed.to_string().contains("already-committed"),
            "and the message must say why: {committed}"
        );
    }

    /// **A bulk pass constructs no payload runtime at all** (S5-E).
    ///
    /// The defect this gate exists for was invisible to every functional test:
    /// building a fresh `payload_fory::<T>()` per row is *correct*, just three
    /// orders of magnitude too slow, so nothing failed and the cost only
    /// appeared as tens of seconds in the §13 import at 200,000 rows.
    ///
    /// A wall-clock assertion is the wrong instrument — loose enough to pass on
    /// a busy runner is loose enough to catch nothing. The exact statement is
    /// about *construction count*: after one warm-up encode and one warm-up
    /// decode of the same type, a thousand more of each must construct
    /// **zero** runtimes. Reverting `encode_owned`/`decode_persisted` to call
    /// `payload_fory` directly makes this two thousand.
    ///
    /// The warm-up is per type and is what makes the delta exact rather than
    /// "small": the count is process-wide and other tests share the process, so
    /// only a delta measured around this type's own calls means anything.
    #[test]
    fn a_bulk_pass_constructs_no_payload_runtime() {
        use std::sync::atomic::Ordering;

        let meta = CoordinationMetaV1 {
            schema_version: 1,
            admin_revision: 3,
            ..Default::default()
        };

        // Warm this type's runtime, whatever any other test already did.
        let bytes = encode_owned(Db::State, "CoordinationMetaV1", &meta).expect("encode");
        let _: CoordinationMetaV1 =
            decode_persisted(Db::State, "CoordinationMetaV1", &bytes).expect("decode");

        let before = runtime::CONSTRUCTED.load(Ordering::Relaxed);
        for _ in 0..1_000 {
            let encoded = encode_owned(Db::State, "CoordinationMetaV1", &meta).expect("encode");
            let back: CoordinationMetaV1 =
                decode_persisted(Db::State, "CoordinationMetaV1", &encoded).expect("decode");
            assert_eq!(back.admin_revision, 3, "the cached runtime round-trips");
        }
        let constructed = runtime::CONSTRUCTED.load(Ordering::Relaxed) - before;

        assert_eq!(
            constructed, 0,
            "2,000 persisted encodes/decodes of one already-warmed type constructed \
             {constructed} payload runtimes; the runtime is per type, not per value"
        );
    }

    /// **The cache is per type, not one runtime for everything.**
    ///
    /// A `HashMap` keyed on something constant — or a single `OnceLock` holding
    /// one runtime — would return the first type's runtime for every later
    /// type, and every decode after the first would fail or, worse, succeed
    /// against the wrong type meta. This asserts a second, distinct persisted
    /// root both constructs its own runtime on first use and round-trips.
    #[test]
    fn each_persisted_root_gets_its_own_runtime() {
        use crate::coordination::raft::ResourceRowV1;
        use std::sync::atomic::Ordering;

        // Warm the first type.
        let meta = CoordinationMetaV1::default();
        let _ = encode_owned(Db::State, "CoordinationMetaV1", &meta).expect("encode");

        let before = runtime::CONSTRUCTED.load(Ordering::Relaxed);
        let row = ResourceRowV1 {
            epoch: 4,
            ttl_ms: 1_000,
            lease_revision: 2,
            value_revision: 5,
            value: vec![0xAB; 8],
            acl_revision: 1,
            ..Default::default()
        };
        let encoded = encode_owned(Db::State, "ResourceRowV1", &row).expect("encode");
        let after_first = runtime::CONSTRUCTED.load(Ordering::Relaxed);
        let back: ResourceRowV1 =
            decode_persisted(Db::State, "ResourceRowV1", &encoded).expect("decode");

        assert_eq!(
            back.value_revision, 5,
            "a second root decodes against its own type meta"
        );
        assert_eq!(back.value, vec![0xAB; 8]);
        // Either this test warmed `ResourceRowV1` (delta 1) or an earlier test
        // in the same process already did (delta 0). What must never happen is
        // the second call constructing again.
        assert!(
            after_first - before <= 1,
            "the first use of a second root constructs at most one runtime"
        );
        assert_eq!(
            runtime::CONSTRUCTED.load(Ordering::Relaxed),
            after_first,
            "the decode reused the runtime the encode constructed"
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
