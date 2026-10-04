//! RPC codec: the envelope (`StreamHeader` / `CallHeader` / `RpcStatus`) and
//! the payload Fory runtime.
//!
//! The envelope types and their byte format live in
//! [`aster_transport_core::envelope`] — a hand-rolled canonical encoding
//! ("AEv1", Aster-SPEC §6.9) that does not use Fory, so every binding wraps the
//! same Rust implementation and the envelope is immune to Fory upgrades. This
//! module re-exports them and maps their errors into [`CodecError`].
//!
//! Payloads remain Apache Fory XLANG, built through
//! [`aster_transport_core::xlang::new_fory`] (xlang on, compatible on, ref
//! tracking off — the cross-binding standard).

use aster_transport_core::envelope::{self, EnvelopeError};
pub use aster_transport_core::envelope::{CallHeader, RpcStatus, SerializationMode, StreamHeader};
use fory_core::Fory;

/// Build a fresh `Fory` runtime configured for Aster *payloads*. A service
/// registers its own `#[derive(ForyStruct)]` contract types into the returned
/// runtime (`register_by_name`) and then calls `serialize`/`deserialize`.
pub fn new_payload_fory() -> Fory {
    aster_transport_core::xlang::new_fory()
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Envelope encode/decode failure. Payload codec errors surface through the same
/// shape from the per-service runtime.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("encode error: {0}")]
    Encode(String),
    #[error("decode error: {0}")]
    Decode(String),
}

/// Crate-local result alias for codec operations.
pub type Result<T> = std::result::Result<T, CodecError>;

fn enc(e: EnvelopeError) -> CodecError {
    CodecError::Encode(e.to_string())
}

fn dec(e: EnvelopeError) -> CodecError {
    CodecError::Decode(e.to_string())
}

// ── Envelope encode / decode ────────────────────────────────────────────────────

/// Encode a caller's time budget as `StreamHeader.deadline`: whole seconds,
/// rounded **up** and clamped to `i16::MAX`. Rounding up keeps the server's
/// deadline (which starts at receipt) from firing before the caller's own.
/// A zero budget stays 0, which means "no deadline" on the wire.
pub fn deadline_header_secs(budget: std::time::Duration) -> i16 {
    let secs = budget.as_secs() + u64::from(budget.subsec_nanos() > 0);
    secs.min(i16::MAX as u64) as i16
}

/// Serialize a [`StreamHeader`] (frame payload for `FLAG_HEADER`).
pub fn encode_stream_header(h: &StreamHeader) -> Result<Vec<u8>> {
    envelope::encode_stream_header(h).map_err(enc)
}

/// Deserialize a [`StreamHeader`] from a `FLAG_HEADER` frame payload.
pub fn decode_stream_header(bytes: &[u8]) -> Result<StreamHeader> {
    envelope::decode_stream_header(bytes).map_err(dec)
}

/// Serialize a [`CallHeader`] (frame payload for `FLAG_CALL`).
pub fn encode_call_header(h: &CallHeader) -> Result<Vec<u8>> {
    envelope::encode_call_header(h).map_err(enc)
}

/// Deserialize a [`CallHeader`] from a `FLAG_CALL` frame payload.
pub fn decode_call_header(bytes: &[u8]) -> Result<CallHeader> {
    envelope::decode_call_header(bytes).map_err(dec)
}

/// Serialize an [`RpcStatus`] (frame payload for `FLAG_TRAILER`).
pub fn encode_rpc_status(s: &RpcStatus) -> Result<Vec<u8>> {
    envelope::encode_rpc_status(s).map_err(enc)
}

/// Deserialize an [`RpcStatus`] from a `FLAG_TRAILER` frame payload.
pub fn decode_rpc_status(bytes: &[u8]) -> Result<RpcStatus> {
    envelope::decode_rpc_status(bytes).map_err(dec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fory_derive::ForyStruct;

    fn sample_header() -> StreamHeader {
        StreamHeader {
            service: "AgentControl".into(),
            method: "assign_task".into(),
            version: 1,
            call_id: 42,
            deadline: 30,
            serialization_mode: SerializationMode::Xlang.as_i8(),
            metadata_keys: vec!["trace".into()],
            metadata_values: vec!["abc".into()],
            session_id: 0,
        }
    }

    #[test]
    fn deadline_header_secs_rounds_up_and_clamps() {
        use std::time::Duration;
        assert_eq!(deadline_header_secs(Duration::ZERO), 0);
        assert_eq!(deadline_header_secs(Duration::from_millis(1)), 1);
        assert_eq!(deadline_header_secs(Duration::from_millis(1500)), 2);
        assert_eq!(deadline_header_secs(Duration::from_secs(30)), 30);
        assert_eq!(deadline_header_secs(Duration::from_secs(1 << 40)), i16::MAX);
    }

    fn sample_call_header() -> CallHeader {
        CallHeader {
            method: "assign_task".into(),
            call_id: 7,
            deadline: 0,
            metadata_keys: vec![],
            metadata_values: vec![],
        }
    }

    fn sample_status() -> RpcStatus {
        RpcStatus {
            code: 5,
            message: "not found".into(),
            detail_keys: vec!["resource".into()],
            detail_values: vec!["task/9".into()],
        }
    }

    #[test]
    fn round_trips() {
        let h = sample_header();
        assert_eq!(
            decode_stream_header(&encode_stream_header(&h).unwrap()).unwrap(),
            h
        );
        let c = sample_call_header();
        assert_eq!(
            decode_call_header(&encode_call_header(&c).unwrap()).unwrap(),
            c
        );
        let s = sample_status();
        assert_eq!(
            decode_rpc_status(&encode_rpc_status(&s).unwrap()).unwrap(),
            s
        );
        let d = StreamHeader::default();
        assert_eq!(
            decode_stream_header(&encode_stream_header(&d).unwrap()).unwrap(),
            d
        );
    }

    // Golden bytes for the AEv1 envelope. The same values are published as
    // cross-binding vectors in `conformance/vectors/envelope.json`.
    #[test]
    fn golden_bytes() {
        assert_eq!(
            hex::encode(encode_stream_header(&sample_header()).unwrap()),
            "a101324167656e74436f6e74726f6c2e61737369676e5f7461736b02543c00010c16747261636501\
             0c0e61626300"
        );
        assert_eq!(
            hex::encode(encode_call_header(&sample_call_header()).unwrap()),
            "a2012e61737369676e5f7461736b0e00000c000c"
        );
        assert_eq!(
            hex::encode(encode_rpc_status(&sample_status()).unwrap()),
            "a3010a266e6f7420666f756e64010c227265736f75726365010c1a7461736b2f39"
        );
    }

    #[test]
    fn payload_fory_round_trip() {
        #[derive(ForyStruct, Debug, Clone, PartialEq, Default)]
        struct TaskAssignment {
            task_id: String,
            priority: i32,
            tags: Vec<String>,
        }

        let mut f = new_payload_fory();
        f.register_by_name::<TaskAssignment>("test.TaskAssignment")
            .unwrap();
        let t = TaskAssignment {
            task_id: "t1".into(),
            priority: 5,
            tags: vec!["a".into(), "b".into()],
        };
        let bytes = f.serialize(&t).unwrap();
        let back: TaskAssignment = f.deserialize(&bytes).unwrap();
        assert_eq!(t, back);
    }
}
