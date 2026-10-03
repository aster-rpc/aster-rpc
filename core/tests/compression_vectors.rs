//! Cross-binding golden vectors for zstd frame compression (Aster-SPEC §5.6),
//! published as `conformance/vectors/compression.json`. Decode vectors must
//! inflate to the described payload, the at-threshold encode vector must be
//! reproduced exactly, and reject vectors (bomb, corrupt) must be refused.
//!
//! Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster_transport_core --test compression_vectors`

use std::path::PathBuf;

use aster_transport_core::framing::{
    decode_frame, encode_payload_frame, payload_of, COMPRESSION_THRESHOLD, FLAG_COMPRESSED,
    FLAG_END_STREAM, MAX_DECOMPRESSED_SIZE,
};
use serde_json::{json, Value};

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors")
}

fn writing() -> bool {
    std::env::var_os("ASTER_VECTORS_WRITE").is_some()
}

// ── compression ─────────────────────────────────────────────────────────────

/// Deterministic, compressible payload: `byte[i] = i % 251`.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn compression_vectors() {
    let path = vectors_dir().join("compression.json");
    if writing() {
        let big = pattern(8192);
        let frame = encode_payload_frame(&big, FLAG_END_STREAM).unwrap();
        let at_threshold = encode_payload_frame(&pattern(COMPRESSION_THRESHOLD), 0).unwrap();
        let bomb_payload = zstd::bulk::compress(&vec![0u8; MAX_DECOMPRESSED_SIZE + 1], 3).unwrap();
        let mut bomb = Vec::new();
        bomb.extend_from_slice(&((bomb_payload.len() + 1) as u32).to_le_bytes());
        bomb.push(FLAG_COMPRESSED);
        bomb.extend_from_slice(&bomb_payload);
        let limit_payload = zstd::bulk::compress(&vec![0u8; MAX_DECOMPRESSED_SIZE], 3).unwrap();
        let mut at_limit = Vec::new();
        at_limit.extend_from_slice(&((limit_payload.len() + 1) as u32).to_le_bytes());
        at_limit.push(FLAG_COMPRESSED);
        at_limit.extend_from_slice(&limit_payload);
        let doc = json!({
            "description": "Aster zstd payload compression vectors (Aster-SPEC §5.6, §6.1 COMPRESSED flag 0x01). Whole frames: 4-byte LE length + flags + payload.",
            "parameters": {
                "threshold_bytes": COMPRESSION_THRESHOLD,
                "compress_when": "payload length > threshold (strictly greater)",
                "zstd_level": 3,
                "max_decompressed_bytes": MAX_DECOMPRESSED_SIZE,
                "decompression": "streaming; never trust the zstd frame content-size; refuse output > max_decompressed_bytes"
            },
            "note": "zstd output is not byte-identical across zstd library versions, so encoders are NOT required to reproduce `frame_hex`. Decoders MUST decode `frame_hex` to the payload described (`payload_rule`, `payload_length`, `payload_blake3`).",
            "decode_vectors": [
                {
                    "name": "8192-byte END_STREAM data frame, compressed",
                    "frame_hex": hex::encode(&frame),
                    "flags": FLAG_END_STREAM | FLAG_COMPRESSED,
                    "payload_rule": "byte[i] = i % 251",
                    "payload_length": big.len(),
                    "payload_blake3": blake3::hash(&big).to_hex().to_string(),
                },
                {
                    "name": "exactly 16 MiB of zeros (at the limit) is accepted",
                    "frame_hex": hex::encode(&at_limit),
                    "flags": FLAG_COMPRESSED,
                    "payload_rule": "byte[i] = 0",
                    "payload_length": MAX_DECOMPRESSED_SIZE,
                    "payload_blake3": blake3::hash(&vec![0u8; MAX_DECOMPRESSED_SIZE]).to_hex().to_string(),
                }
            ],
            "encode_vectors": [
                {
                    "name": "payload exactly at threshold is sent uncompressed",
                    "payload_rule": "byte[i] = i % 251",
                    "payload_length": COMPRESSION_THRESHOLD,
                    "input_flags": 0,
                    "expected_frame_hex": hex::encode(&at_threshold),
                }
            ],
            "reject_vectors": [
                {
                    "name": "decompression bomb: 16 MiB + 1 zero bytes",
                    "frame_hex": hex::encode(&bomb),
                    "reason": "decompressed size exceeds max_decompressed_bytes",
                },
                {
                    "name": "COMPRESSED flag on bytes that are not a zstd frame",
                    "frame_hex": "050000000101020304",
                    "reason": "corrupt zstd",
                }
            ],
        });
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    }

    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for v in doc["decode_vectors"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let wire = hex::decode(v["frame_hex"].as_str().unwrap()).unwrap();
        let (payload, flags, used) = decode_frame(&wire).unwrap();
        assert_eq!(used, wire.len(), "{name}");
        assert_eq!(flags as u64, v["flags"].as_u64().unwrap(), "{name}");
        let out = payload_of(payload, flags).unwrap();
        assert_eq!(
            out.len() as u64,
            v["payload_length"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            blake3::hash(&out).to_hex().to_string(),
            v["payload_blake3"].as_str().unwrap(),
            "{name}"
        );
    }
    for v in doc["encode_vectors"].as_array().unwrap() {
        let p = pattern(v["payload_length"].as_u64().unwrap() as usize);
        let got = encode_payload_frame(&p, v["input_flags"].as_u64().unwrap() as u8).unwrap();
        assert_eq!(hex::encode(got), v["expected_frame_hex"].as_str().unwrap());
    }
    for v in doc["reject_vectors"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let wire = hex::decode(v["frame_hex"].as_str().unwrap()).unwrap();
        let (payload, flags, _) = decode_frame(&wire).unwrap();
        assert!(payload_of(payload, flags).is_err(), "accepted: {name}");
    }
}
