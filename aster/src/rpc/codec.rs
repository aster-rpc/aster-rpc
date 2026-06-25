//! RPC envelope codec — Fory-XLANG ser/de of the framework wire types.
//!
//! Mirrors `bindings/python/aster/protocol.py`. `StreamHeader` / `CallHeader` /
//! `RpcStatus` are framework-internal protocol types; they are always serialized
//! with Fory XLANG regardless of a service's *payload* serialization mode. Field
//! layout tracks Aster-SPEC §6 — keep in sync with the Python / Java / TS
//! declarations.
//!
//! Codec recipe is the same one proven by `core/src/attestation.rs`: a single,
//! process-global `Fory` runtime (`xlang(true).compatible(true)`) built once in a
//! `OnceLock` with all envelope types pre-registered, never re-initialised per
//! call. Payload types are per-service and registered into their own runtime via
//! [`new_payload_fory`].
//!
//! ## Cross-binding status
//!
//! Rust↔Rust only, today. The Rust `fory` crate (1.1.x) is a different major than
//! pyfory 0.17 and exposes no field-level IDs, so these bytes are **not** yet
//! proven wire-compatible with the other bindings. The structs are nonetheless
//! shaped field-for-field like `protocol.py` to keep that future delta (Step 2
//! §2B in `docs/_internal/aster-rust.md`) minimal. The golden tests below pin our
//! *own* wire so accidental layout changes are caught.

use std::sync::OnceLock;

use fory_core::Fory;
use fory_derive::ForyStruct;

/// Payload serialization mode negotiated in [`StreamHeader::serialization_mode`].
///
/// Only [`Xlang`](SerializationMode::Xlang) is implemented in Rust. JSON (`= 3`)
/// is intentionally unsupported (decision 2026-06-01); `Native`/`Row` are
/// reserved for parity with the other bindings but not yet wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub enum SerializationMode {
    Xlang = 0,
    Native = 1,
    Row = 2,
    // Json = 3 — intentionally not implemented in Rust.
}

impl SerializationMode {
    /// The on-the-wire `i8` value carried in `StreamHeader.serialization_mode`.
    pub fn as_i8(self) -> i8 {
        self as i8
    }
}

/// First frame on every stream (`FLAG_HEADER`): service routing, contract
/// version, per-call metadata, and the negotiated serialization mode.
#[derive(ForyStruct, Debug, Clone, PartialEq, Default)]
pub struct StreamHeader {
    pub service: String,
    pub method: String,
    pub version: i32,
    pub call_id: i32,
    /// Relative seconds; `0` = no deadline.
    pub deadline: i16,
    /// XLANG=0, NATIVE=1, ROW=2 (JSON=3 unsupported in Rust).
    pub serialization_mode: i8,
    pub metadata_keys: Vec<String>,
    pub metadata_values: Vec<String>,
    /// `0` = stateless SHARED-pool stream; non-zero = session-bound (spec §6).
    pub session_id: i32,
}

/// Per-call header within a session stream (`FLAG_CALL`); used by session-scoped
/// services where many RPCs share one QUIC stream.
#[derive(ForyStruct, Debug, Clone, PartialEq, Default)]
pub struct CallHeader {
    pub method: String,
    pub call_id: i32,
    /// Relative seconds; `0` = no deadline.
    pub deadline: i16,
    pub metadata_keys: Vec<String>,
    pub metadata_values: Vec<String>,
}

/// Trailing status frame (`FLAG_TRAILER`): the terminal frame on a stream,
/// carrying the outcome of the RPC.
#[derive(ForyStruct, Debug, Clone, PartialEq, Default)]
pub struct RpcStatus {
    pub code: i32,
    pub message: String,
    pub detail_keys: Vec<String>,
    pub detail_values: Vec<String>,
}

// ── Fory runtime (built once, shared) ─────────────────────────────────────────

static ENVELOPE_FORY: OnceLock<Fory> = OnceLock::new();

/// The shared, pre-registered envelope runtime. Built once; safe to share by
/// `&`. Re-initialising per call is a known Fory perf footgun, so all envelope
/// ser/de goes through this.
fn envelope_fory() -> &'static Fory {
    ENVELOPE_FORY.get_or_init(|| {
        let mut f = Fory::builder().xlang(true).compatible(true).build();
        f.register_by_name::<StreamHeader>("_aster.StreamHeader")
            .expect("register StreamHeader");
        f.register_by_name::<CallHeader>("_aster.CallHeader")
            .expect("register CallHeader");
        f.register_by_name::<RpcStatus>("_aster.RpcStatus")
            .expect("register RpcStatus");
        f
    })
}

/// Build a fresh `Fory` runtime configured for Aster *payloads* (xlang,
/// compatible). A service registers its own `#[derive(ForyStruct)]` contract
/// types into the returned runtime (`register_by_name`) and then calls
/// `serialize`/`deserialize`. Envelope types use the separate process-global
/// runtime above and must **not** be registered here.
pub fn new_payload_fory() -> Fory {
    Fory::builder().xlang(true).compatible(true).build()
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

// ── Envelope encode / decode ────────────────────────────────────────────────────

/// Serialize a [`StreamHeader`] to Fory-XLANG bytes (frame payload for `FLAG_HEADER`).
pub fn encode_stream_header(h: &StreamHeader) -> Result<Vec<u8>> {
    envelope_fory()
        .serialize(h)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

/// Deserialize a [`StreamHeader`] from a `FLAG_HEADER` frame payload.
pub fn decode_stream_header(bytes: &[u8]) -> Result<StreamHeader> {
    envelope_fory()
        .deserialize(bytes)
        .map_err(|e| CodecError::Decode(e.to_string()))
}

/// Serialize a [`CallHeader`] to Fory-XLANG bytes (frame payload for `FLAG_CALL`).
pub fn encode_call_header(h: &CallHeader) -> Result<Vec<u8>> {
    envelope_fory()
        .serialize(h)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

/// Deserialize a [`CallHeader`] from a `FLAG_CALL` frame payload.
pub fn decode_call_header(bytes: &[u8]) -> Result<CallHeader> {
    envelope_fory()
        .deserialize(bytes)
        .map_err(|e| CodecError::Decode(e.to_string()))
}

/// Serialize an [`RpcStatus`] to Fory-XLANG bytes (frame payload for `FLAG_TRAILER`).
pub fn encode_rpc_status(s: &RpcStatus) -> Result<Vec<u8>> {
    envelope_fory()
        .serialize(s)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

/// Deserialize an [`RpcStatus`] from a `FLAG_TRAILER` frame payload.
pub fn decode_rpc_status(bytes: &[u8]) -> Result<RpcStatus> {
    envelope_fory()
        .deserialize(bytes)
        .map_err(|e| CodecError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn stream_header_round_trip() {
        let h = sample_header();
        let bytes = encode_stream_header(&h).unwrap();
        assert_eq!(decode_stream_header(&bytes).unwrap(), h);
    }

    #[test]
    fn call_header_round_trip() {
        let h = sample_call_header();
        let bytes = encode_call_header(&h).unwrap();
        assert_eq!(decode_call_header(&bytes).unwrap(), h);
    }

    #[test]
    fn rpc_status_round_trip() {
        let s = sample_status();
        let bytes = encode_rpc_status(&s).unwrap();
        assert_eq!(decode_rpc_status(&bytes).unwrap(), s);
    }

    #[test]
    fn defaults_round_trip() {
        // Empty/zero-valued envelopes must survive the wire too.
        let h = StreamHeader::default();
        assert_eq!(
            decode_stream_header(&encode_stream_header(&h).unwrap()).unwrap(),
            h
        );
        let s = RpcStatus::default();
        assert_eq!(
            decode_rpc_status(&encode_rpc_status(&s).unwrap()).unwrap(),
            s
        );
    }

    #[test]
    fn serialization_is_deterministic() {
        let h = sample_header();
        assert_eq!(
            encode_stream_header(&h).unwrap(),
            encode_stream_header(&h).unwrap()
        );
        let s = sample_status();
        assert_eq!(
            encode_rpc_status(&s).unwrap(),
            encode_rpc_status(&s).unwrap()
        );
    }

    // Golden self-byte tests: pin our own wire layout so an accidental field /
    // type / order change is caught. These are NOT cross-binding goldens (see
    // module docs); the expected hex is captured from our own encoder.
    #[test]
    fn stream_header_golden_bytes() {
        let got = hex::encode(encode_stream_header(&sample_header()).unwrap());
        assert_eq!(
            got, GOLDEN_STREAM_HEADER,
            "StreamHeader wire layout changed"
        );
    }

    #[test]
    fn call_header_golden_bytes() {
        let got = hex::encode(encode_call_header(&sample_call_header()).unwrap());
        assert_eq!(got, GOLDEN_CALL_HEADER, "CallHeader wire layout changed");
    }

    #[test]
    fn rpc_status_golden_bytes() {
        let got = hex::encode(encode_rpc_status(&sample_status()).unwrap());
        assert_eq!(got, GOLDEN_RPC_STATUS, "RpcStatus wire layout changed");
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

    // Captured from our own encoder; see the golden tests above. Stable across
    // processes (Fory xlang is a portable format); a change here means the wire
    // layout moved — bump intentionally, never to "make the test pass".
    const GOLDEN_STREAM_HEADER: &str = "01ff1e00634071ab9f665c28e9116c1299222576538900ce9c80192254038c801ad0d2006c02c89140168c82687376c70c805005080b5ed0305805c892921cdda06050055491921cd0601654b09300c1306d44c480641654309300c1306ea05d09204c1530933b8650154891aa04401e0000540002010c167472616365010c0e6162632e61737369676e5f7461736b324167656e74436f6e74726f6c";
    const GOLDEN_CALL_HEADER: &str = "01ff1e003d30a2d056619f09e5116c12992222380165c22001888854038c801ad0d2005005080b5ed030601654b09300c1306d44c480641654309300c1306ea05d09204c1530933b8600000e00002e61737369676e5f7461736b";
    const GOLDEN_RPC_STATUS: &str = "01ff1e003010eadc7b1f4561e4116c1299221e56785626026a24480509c3205816540c930217b513126016548c930217ba8174248050153092900c400a010c227265736f75726365010c1a7461736b2f39266e6f7420666f756e64";
}
