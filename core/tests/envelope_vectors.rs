//! Cross-binding golden vectors for the AEv1 RPC envelope (Aster-SPEC §6.9),
//! published as `conformance/vectors/envelope.json`.
//!
//! The JSON files are the contract other bindings test against; this test
//! keeps them honest from the Rust side: every encode vector must reproduce
//! its hex exactly, every decode vector must decode to its input, and every
//! invalid vector must be refused.
//!
//! Regenerate (only for an intended, recorded format change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster_transport_core --test envelope_vectors`

use std::path::PathBuf;

use aster_transport_core::envelope::{
    decode_call_header, decode_rpc_status, decode_stream_header, encode_call_header,
    encode_rpc_status, encode_stream_header, CallHeader, RpcStatus, StreamHeader,
};
use serde_json::{json, Value};

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors")
}

fn writing() -> bool {
    std::env::var_os("ASTER_VECTORS_WRITE").is_some()
}

fn sv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// ── envelope ────────────────────────────────────────────────────────────────

fn stream_header_json(h: &StreamHeader) -> Value {
    json!({
        "service": h.service, "method": h.method, "version": h.version,
        "callId": h.call_id, "deadline": h.deadline,
        "serializationMode": h.serialization_mode,
        "metadataKeys": h.metadata_keys, "metadataValues": h.metadata_values,
        "sessionId": h.session_id,
    })
}

fn call_header_json(h: &CallHeader) -> Value {
    json!({
        "method": h.method, "callId": h.call_id, "deadline": h.deadline,
        "metadataKeys": h.metadata_keys, "metadataValues": h.metadata_values,
    })
}

fn rpc_status_json(s: &RpcStatus) -> Value {
    json!({
        "code": s.code, "message": s.message,
        "detailKeys": s.detail_keys, "detailValues": s.detail_values,
    })
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap()
}

fn stream_header_from(v: &Value) -> StreamHeader {
    StreamHeader {
        service: v["service"].as_str().unwrap().into(),
        method: v["method"].as_str().unwrap().into(),
        version: i(&v["version"]) as i32,
        call_id: i(&v["callId"]) as i32,
        deadline: i(&v["deadline"]) as i16,
        serialization_mode: i(&v["serializationMode"]) as i8,
        metadata_keys: strs(&v["metadataKeys"]),
        metadata_values: strs(&v["metadataValues"]),
        session_id: i(&v["sessionId"]) as i32,
    }
}

fn call_header_from(v: &Value) -> CallHeader {
    CallHeader {
        method: v["method"].as_str().unwrap().into(),
        call_id: i(&v["callId"]) as i32,
        deadline: i(&v["deadline"]) as i16,
        metadata_keys: strs(&v["metadataKeys"]),
        metadata_values: strs(&v["metadataValues"]),
    }
}

fn rpc_status_from(v: &Value) -> RpcStatus {
    RpcStatus {
        code: i(&v["code"]) as i32,
        message: v["message"].as_str().unwrap().into(),
        detail_keys: strs(&v["detailKeys"]),
        detail_values: strs(&v["detailValues"]),
    }
}

fn envelope_cases() -> Vec<(&'static str, &'static str, Value, Vec<u8>)> {
    let mut out = Vec::new();
    let mut sh = |name, h: StreamHeader| {
        out.push((
            name,
            "StreamHeader",
            stream_header_json(&h),
            encode_stream_header(&h).unwrap(),
        ))
    };
    sh(
        "StreamHeader: basic unary",
        StreamHeader {
            service: "Echo".into(),
            method: "echo".into(),
            version: 1,
            call_id: 1,
            ..Default::default()
        },
    );
    sh(
        "StreamHeader: session open (empty method, session id)",
        StreamHeader {
            service: "AgentSession".into(),
            version: 1,
            session_id: 3,
            ..Default::default()
        },
    );
    sh(
        "StreamHeader: deadline + metadata",
        StreamHeader {
            service: "MissionControl".into(),
            method: "getStatus".into(),
            version: 2,
            call_id: 300,
            deadline: 30,
            metadata_keys: sv(&["aster.role", "trace"]),
            metadata_values: sv(&["ops.status", ""]),
            ..Default::default()
        },
    );
    sh("StreamHeader: all defaults", StreamHeader::default());
    sh(
        "StreamHeader: extremes + unicode",
        StreamHeader {
            service: "Größe✓".into(),
            method: "m".into(),
            version: i32::MAX,
            call_id: -1,
            deadline: i16::MIN,
            serialization_mode: 2,
            session_id: i32::MIN,
            ..Default::default()
        },
    );
    let mut ch = |name, h: CallHeader| {
        out.push((
            name,
            "CallHeader",
            call_header_json(&h),
            encode_call_header(&h).unwrap(),
        ))
    };
    ch(
        "CallHeader: basic session call",
        CallHeader {
            method: "step".into(),
            call_id: 7,
            ..Default::default()
        },
    );
    ch(
        "CallHeader: deadline + metadata",
        CallHeader {
            method: "assign_task".into(),
            call_id: 8,
            deadline: 5,
            metadata_keys: sv(&["k"]),
            metadata_values: sv(&["v"]),
        },
    );
    let mut st = |name, s: RpcStatus| {
        out.push((
            name,
            "RpcStatus",
            rpc_status_json(&s),
            encode_rpc_status(&s).unwrap(),
        ))
    };
    st("RpcStatus: OK", RpcStatus::ok());
    st(
        "RpcStatus: NOT_FOUND with details",
        RpcStatus {
            code: 5,
            message: "not found".into(),
            detail_keys: sv(&["resource"]),
            detail_values: sv(&["task/9"]),
        },
    );
    st(
        "RpcStatus: application code + unicode",
        RpcStatus {
            code: 101,
            message: "contrat violé - ✗".into(),
            detail_keys: sv(&["a", "b"]),
            detail_values: sv(&["1", "2"]),
        },
    );
    out
}

fn invalid_envelope_cases() -> Vec<(&'static str, &'static str, String, &'static str)> {
    let good = encode_rpc_status(&RpcStatus::ok()).unwrap(); // a3 01 00 02 00 0c 00 0c
    let mut trailing = good.clone();
    trailing.push(0);
    let mut bad_version = good.clone();
    bad_version[1] = 2;
    vec![
        ("empty input", "RpcStatus", String::new(), "empty"),
        (
            "wrong tag (StreamHeader bytes read as RpcStatus)",
            "RpcStatus",
            hex::encode(encode_stream_header(&StreamHeader::default()).unwrap()),
            "tag",
        ),
        (
            "unsupported version",
            "RpcStatus",
            hex::encode(bad_version),
            "version",
        ),
        (
            "trailing byte",
            "RpcStatus",
            hex::encode(trailing),
            "trailing",
        ),
        (
            "truncated",
            "RpcStatus",
            hex::encode(&good[..good.len() - 1]),
            "truncated",
        ),
        (
            "non-minimal varint (code 0 as 80 00)",
            "RpcStatus",
            "a301800002000c000c".into(),
            "non-minimal",
        ),
        (
            "string header with non-UTF8 marker bits (0x01)",
            "RpcStatus",
            "a3010001000c000c".into(),
            "string",
        ),
        (
            "metadata key/value count mismatch (1 key, 0 values)",
            "CallHeader",
            "a201126d657468 0000 010c066b 000c".replace(' ', ""),
            "mismatch",
        ),
        (
            "deadline out of int16 range (40000)",
            "CallHeader",
            "a201126d657468 00 80f104 000c000c".replace(' ', ""),
            "range",
        ),
        (
            "list element marker wrong (0x00 instead of 0x0C)",
            "RpcStatus",
            "a30100020000000c".into(),
            "list",
        ),
    ]
}

#[test]
fn envelope_vectors() {
    let path = vectors_dir().join("envelope.json");
    if writing() {
        let vectors: Vec<Value> = envelope_cases()
            .into_iter()
            .map(|(name, ty, input, bytes)| {
                json!({ "name": name, "type": ty, "input": input,
                        "hex": hex::encode(&bytes), "length": bytes.len() })
            })
            .collect();
        let invalid: Vec<Value> = invalid_envelope_cases()
            .into_iter()
            .map(|(name, ty, hex, why)| json!({ "name": name, "type": ty, "hex": hex, "reason": why }))
            .collect();
        let doc = json!({
            "description": "Aster RPC envelope vectors, format AEv1 (Aster-SPEC §6.9). These are frame PAYLOADS (HEADER / CALL / TRAILER frames), not whole frames. Fory is not involved.",
            "format": {
                "prefix": "tag:u8 version:u8 (0x01); tags: 0xA1 StreamHeader, 0xA2 CallHeader, 0xA3 RpcStatus",
                "int": "zigzag LEB128 varint, minimal length, range-checked to the field width",
                "string": "varint((utf8_len << 2) | 2) then UTF-8 bytes",
                "list<string>": "varint(count) 0x0C then each string",
                "StreamHeader": "service, method, version:i32, callId:i32, deadline:i16, serializationMode:i8, metadataKeys, metadataValues, sessionId:i32",
                "CallHeader": "method, callId:i32, deadline:i16, metadataKeys, metadataValues",
                "RpcStatus": "code:i32, message, detailKeys, detailValues",
                "rules": "every field always present, fixed order, no trailing bytes; key/value lists must have equal length"
            },
            "note": "Encoders MUST produce `hex` exactly for `input`; decoders MUST decode `hex` to `input` and MUST reject every `invalid` vector.",
            "vectors": vectors,
            "invalid": invalid,
        });
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    }

    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for v in doc["vectors"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let want = hex::decode(v["hex"].as_str().unwrap()).unwrap();
        match v["type"].as_str().unwrap() {
            "StreamHeader" => {
                let h = stream_header_from(&v["input"]);
                assert_eq!(encode_stream_header(&h).unwrap(), want, "{name}");
                assert_eq!(decode_stream_header(&want).unwrap(), h, "{name}");
            }
            "CallHeader" => {
                let h = call_header_from(&v["input"]);
                assert_eq!(encode_call_header(&h).unwrap(), want, "{name}");
                assert_eq!(decode_call_header(&want).unwrap(), h, "{name}");
            }
            "RpcStatus" => {
                let s = rpc_status_from(&v["input"]);
                assert_eq!(encode_rpc_status(&s).unwrap(), want, "{name}");
                assert_eq!(decode_rpc_status(&want).unwrap(), s, "{name}");
            }
            t => panic!("unknown type {t}"),
        }
    }
    for v in doc["invalid"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let bytes = hex::decode(v["hex"].as_str().unwrap()).unwrap();
        let refused = match v["type"].as_str().unwrap() {
            "StreamHeader" => decode_stream_header(&bytes).is_err(),
            "CallHeader" => decode_call_header(&bytes).is_err(),
            "RpcStatus" => decode_rpc_status(&bytes).is_err(),
            t => panic!("unknown type {t}"),
        };
        assert!(refused, "invalid vector accepted: {name}");
    }
}
