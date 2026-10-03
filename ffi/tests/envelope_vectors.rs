//! The C ABI envelope + zstd functions (ABI 3.1) against the conformance
//! vectors: `conformance/vectors/envelope.json` (every valid vector encodes
//! to its exact `hex` and decodes back; every invalid vector is refused) and
//! `conformance/vectors/compression.json` (decode / reject vectors, threshold
//! rule).

use std::path::PathBuf;
use std::{ptr, slice};

use aster_transport_ffi::envelope::*;
use aster_transport_ffi::{
    aster_bytes_list_t, aster_bytes_t, aster_last_error_message, aster_status_t,
    aster_string_release,
};
use serde_json::Value;

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;

fn vectors(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/vectors")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

// ── C-side views over Rust data ─────────────────────────────────────────────

/// Keeps the borrowed buffers alive while a C struct points into them.
struct Strs {
    items: Vec<aster_bytes_t>,
}

fn b(s: &str) -> aster_bytes_t {
    aster_bytes_t {
        ptr: s.as_ptr(),
        len: s.len(),
    }
}

fn strs(v: &[String]) -> Strs {
    Strs {
        items: v.iter().map(|s| b(s)).collect(),
    }
}

impl Strs {
    fn list(&self) -> aster_bytes_list_t {
        aster_bytes_list_t {
            items: self.items.as_ptr(),
            len: self.items.len(),
        }
    }
}

unsafe fn read_str(x: &aster_bytes_t) -> String {
    if x.len == 0 {
        assert!(x.ptr.is_null(), "empty string must be NULL");
        return String::new();
    }
    String::from_utf8(slice::from_raw_parts(x.ptr, x.len).to_vec()).unwrap()
}

unsafe fn read_list(l: &aster_bytes_list_t) -> Vec<String> {
    if l.len == 0 {
        assert!(l.items.is_null(), "empty list must be NULL");
        return vec![];
    }
    slice::from_raw_parts(l.items, l.len)
        .iter()
        .map(|x| read_str(x))
        .collect()
}

unsafe fn take(out: aster_bytes_t) -> Vec<u8> {
    let v = if out.len == 0 {
        vec![]
    } else {
        slice::from_raw_parts(out.ptr, out.len).to_vec()
    };
    assert_eq!(aster_string_release(out.ptr, out.len), OK);
    v
}

fn last_error() -> String {
    let mut buf = vec![0u8; 512];
    let n = unsafe { aster_last_error_message(buf.as_mut_ptr(), buf.len()) };
    String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned()
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap().to_owned()
}
fn sl(v: &Value, k: &str) -> Vec<String> {
    v[k].as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}
fn i(v: &Value, k: &str) -> i64 {
    v[k].as_i64().unwrap()
}

fn empty_bytes() -> aster_bytes_t {
    aster_bytes_t {
        ptr: ptr::null(),
        len: 0,
    }
}

// ── per-type round trips ────────────────────────────────────────────────────

unsafe fn check_stream_header(input: &Value, want: &[u8]) {
    let (service, method) = (s(input, "service"), s(input, "method"));
    let (keys, values) = (sl(input, "metadataKeys"), sl(input, "metadataValues"));
    let (k, v) = (strs(&keys), strs(&values));
    let h = aster_stream_header_t {
        struct_size: std::mem::size_of::<aster_stream_header_t>() as u32,
        version: i(input, "version") as i32,
        call_id: i(input, "callId") as i32,
        session_id: i(input, "sessionId") as i32,
        deadline: i(input, "deadline") as i16,
        serialization_mode: i(input, "serializationMode") as i8,
        reserved0: 0,
        reserved1: 0,
        service: b(&service),
        method: b(&method),
        metadata_keys: k.list(),
        metadata_values: v.list(),
        storage: ptr::null_mut(),
    };
    let mut out = empty_bytes();
    assert_eq!(
        aster_stream_header_encode(&h, &mut out),
        OK,
        "{}",
        last_error()
    );
    assert_eq!(take(out), want);

    let mut d: aster_stream_header_t = std::mem::zeroed();
    assert_eq!(
        aster_stream_header_decode(want.as_ptr(), want.len(), &mut d),
        OK,
        "{}",
        last_error()
    );
    assert_eq!(
        d.struct_size as usize,
        std::mem::size_of::<aster_stream_header_t>()
    );
    assert!(!d.storage.is_null());
    assert_eq!(read_str(&d.service), service);
    assert_eq!(read_str(&d.method), method);
    assert_eq!(d.version, h.version);
    assert_eq!(d.call_id, h.call_id);
    assert_eq!(d.session_id, h.session_id);
    assert_eq!(d.deadline, h.deadline);
    assert_eq!(d.serialization_mode, h.serialization_mode);
    assert_eq!(read_list(&d.metadata_keys), keys);
    assert_eq!(read_list(&d.metadata_values), values);
    assert_eq!(aster_stream_header_release(&mut d), OK);
    assert!(d.storage.is_null() && d.service.ptr.is_null());
    // Second release of the same struct is a no-op.
    assert_eq!(aster_stream_header_release(&mut d), OK);
}

unsafe fn check_call_header(input: &Value, want: &[u8]) {
    let method = s(input, "method");
    let (keys, values) = (sl(input, "metadataKeys"), sl(input, "metadataValues"));
    let (k, v) = (strs(&keys), strs(&values));
    let h = aster_call_header_t {
        struct_size: std::mem::size_of::<aster_call_header_t>() as u32,
        call_id: i(input, "callId") as i32,
        deadline: i(input, "deadline") as i16,
        reserved0: 0,
        reserved1: 0,
        method: b(&method),
        metadata_keys: k.list(),
        metadata_values: v.list(),
        storage: ptr::null_mut(),
    };
    let mut out = empty_bytes();
    assert_eq!(
        aster_call_header_encode(&h, &mut out),
        OK,
        "{}",
        last_error()
    );
    assert_eq!(take(out), want);

    let mut d: aster_call_header_t = std::mem::zeroed();
    assert_eq!(
        aster_call_header_decode(want.as_ptr(), want.len(), &mut d),
        OK,
        "{}",
        last_error()
    );
    assert_eq!(read_str(&d.method), method);
    assert_eq!(d.call_id, h.call_id);
    assert_eq!(d.deadline, h.deadline);
    assert_eq!(read_list(&d.metadata_keys), keys);
    assert_eq!(read_list(&d.metadata_values), values);
    assert_eq!(aster_call_header_release(&mut d), OK);
}

unsafe fn check_rpc_status(input: &Value, want: &[u8]) {
    let message = s(input, "message");
    let (keys, values) = (sl(input, "detailKeys"), sl(input, "detailValues"));
    let (k, v) = (strs(&keys), strs(&values));
    let st = aster_rpc_status_t {
        struct_size: std::mem::size_of::<aster_rpc_status_t>() as u32,
        code: i(input, "code") as i32,
        message: b(&message),
        detail_keys: k.list(),
        detail_values: v.list(),
        storage: ptr::null_mut(),
    };
    let mut out = empty_bytes();
    assert_eq!(
        aster_rpc_status_encode(&st, &mut out),
        OK,
        "{}",
        last_error()
    );
    assert_eq!(take(out), want);

    let mut d: aster_rpc_status_t = std::mem::zeroed();
    assert_eq!(
        aster_rpc_status_decode(want.as_ptr(), want.len(), &mut d),
        OK,
        "{}",
        last_error()
    );
    assert_eq!(d.code, st.code);
    assert_eq!(read_str(&d.message), message);
    assert_eq!(read_list(&d.detail_keys), keys);
    assert_eq!(read_list(&d.detail_values), values);
    assert_eq!(aster_rpc_status_release(&mut d), OK);
}

#[test]
fn envelope_vectors_encode_and_decode_exactly() {
    let doc = vectors("envelope.json");
    let vs = doc["vectors"].as_array().unwrap();
    assert!(vs.len() >= 10);
    for v in vs {
        let want = hex::decode(v["hex"].as_str().unwrap()).unwrap();
        let input = &v["input"];
        unsafe {
            match v["type"].as_str().unwrap() {
                "StreamHeader" => check_stream_header(input, &want),
                "CallHeader" => check_call_header(input, &want),
                "RpcStatus" => check_rpc_status(input, &want),
                t => panic!("unknown type {t} in {}", v["name"]),
            }
        }
    }
}

#[test]
fn envelope_invalid_vectors_are_refused() {
    let doc = vectors("envelope.json");
    let vs = doc["invalid"].as_array().unwrap();
    assert!(!vs.is_empty());
    for v in vs {
        let bytes = hex::decode(v["hex"].as_str().unwrap()).unwrap();
        let name = v["name"].as_str().unwrap();
        // Empty input is passed as {NULL, 0}, the C convention.
        let p = if bytes.is_empty() {
            ptr::null()
        } else {
            bytes.as_ptr()
        };
        unsafe {
            let rc = match v["type"].as_str().unwrap() {
                "StreamHeader" => {
                    let mut d: aster_stream_header_t = std::mem::zeroed();
                    let rc = aster_stream_header_decode(p, bytes.len(), &mut d);
                    assert!(d.storage.is_null(), "{name}");
                    rc
                }
                "CallHeader" => {
                    let mut d: aster_call_header_t = std::mem::zeroed();
                    let rc = aster_call_header_decode(p, bytes.len(), &mut d);
                    assert!(d.storage.is_null(), "{name}");
                    rc
                }
                "RpcStatus" => {
                    let mut d: aster_rpc_status_t = std::mem::zeroed();
                    let rc = aster_rpc_status_decode(p, bytes.len(), &mut d);
                    assert!(d.storage.is_null(), "{name}");
                    rc
                }
                t => panic!("unknown type {t}"),
            };
            assert_eq!(rc, INVALID, "{name} must be refused");
            assert!(!last_error().is_empty(), "{name}: last error set");
        }
    }
}

#[test]
fn envelope_encode_rejects_bad_input() {
    unsafe {
        let mut out = empty_bytes();
        // wrong struct_size
        let mut st: aster_rpc_status_t = std::mem::zeroed();
        st.struct_size = 8;
        assert_eq!(aster_rpc_status_encode(&st, &mut out), INVALID);
        assert!(last_error().contains("struct_size"));
        st.struct_size = std::mem::size_of::<aster_rpc_status_t>() as u32;
        assert_eq!(aster_rpc_status_encode(&st, &mut out), OK);
        assert_eq!(
            take(out),
            vec![0xA3, 0x01, 0x00, 0x02, 0x00, 0x0C, 0x00, 0x0C]
        );

        // key/value count mismatch
        let keys = vec!["k".to_string()];
        let k = strs(&keys);
        st.detail_keys = k.list();
        let mut out = empty_bytes();
        assert_eq!(aster_rpc_status_encode(&st, &mut out), INVALID);
        assert!(out.ptr.is_null());

        // invalid UTF-8 message
        let bad = [0xFFu8, 0xFE];
        let mut st2: aster_rpc_status_t = std::mem::zeroed();
        st2.struct_size = std::mem::size_of::<aster_rpc_status_t>() as u32;
        st2.message = aster_bytes_t {
            ptr: bad.as_ptr(),
            len: 2,
        };
        assert_eq!(aster_rpc_status_encode(&st2, &mut out), INVALID);
        assert!(last_error().contains("UTF-8"));

        // NULL pointer with non-zero length
        st2.message = aster_bytes_t {
            ptr: ptr::null(),
            len: 3,
        };
        assert_eq!(aster_rpc_status_encode(&st2, &mut out), INVALID);

        // NULL arguments
        assert_eq!(aster_rpc_status_encode(ptr::null(), &mut out), INVALID);
        assert_eq!(aster_rpc_status_encode(&st, ptr::null_mut()), INVALID);
        assert_eq!(
            aster_rpc_status_decode(ptr::null(), 0, ptr::null_mut()),
            INVALID
        );
        assert_eq!(aster_rpc_status_release(ptr::null_mut()), OK);
    }
}

// ── zstd ────────────────────────────────────────────────────────────────────

fn payload(rule: &str, len: usize) -> Vec<u8> {
    match rule {
        "byte[i] = i % 251" => (0..len).map(|i| (i % 251) as u8).collect(),
        "byte[i] = 0" => vec![0; len],
        r => panic!("unknown payload_rule {r}"),
    }
}

/// Split a whole frame (`u32 LE length | flags | payload`) into (flags, payload).
fn split_frame(frame: &[u8]) -> (u8, &[u8]) {
    let n = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(n + 4, frame.len());
    (frame[4], &frame[5..])
}

#[test]
fn zstd_decode_vectors() {
    let doc = vectors("compression.json");
    for v in doc["decode_vectors"].as_array().unwrap() {
        let frame = hex::decode(v["frame_hex"].as_str().unwrap()).unwrap();
        let (flags, body) = split_frame(&frame);
        assert_eq!(flags as u64, v["flags"].as_u64().unwrap());
        assert!(flags & 0x01 != 0);
        let mut out = empty_bytes();
        let rc = unsafe { aster_zstd_decompress(body.as_ptr(), body.len(), &mut out) };
        assert_eq!(rc, OK, "{}: {}", v["name"], last_error());
        let got = unsafe { take(out) };
        let len = v["payload_length"].as_u64().unwrap() as usize;
        assert_eq!(got.len(), len);
        assert_eq!(got, payload(v["payload_rule"].as_str().unwrap(), len));
    }
}

#[test]
fn zstd_reject_vectors() {
    let doc = vectors("compression.json");
    for v in doc["reject_vectors"].as_array().unwrap() {
        let frame = hex::decode(v["frame_hex"].as_str().unwrap()).unwrap();
        let (_, body) = split_frame(&frame);
        let mut out = empty_bytes();
        let rc = unsafe { aster_zstd_decompress(body.as_ptr(), body.len(), &mut out) };
        assert_eq!(rc, INVALID, "{} must be refused", v["name"]);
        assert!(out.ptr.is_null());
    }
}

#[test]
fn zstd_encode_vectors_follow_threshold_rule() {
    let doc = vectors("compression.json");
    let threshold = doc["parameters"]["threshold_bytes"].as_u64().unwrap() as usize;
    assert_eq!(threshold, ASTER_COMPRESSION_THRESHOLD);
    assert_eq!(
        doc["parameters"]["max_decompressed_bytes"]
            .as_u64()
            .unwrap() as usize,
        ASTER_MAX_DECOMPRESSED_SIZE
    );
    for v in doc["encode_vectors"].as_array().unwrap() {
        let len = v["payload_length"].as_u64().unwrap() as usize;
        let p = payload(v["payload_rule"].as_str().unwrap(), len);
        let frame = hex::decode(v["expected_frame_hex"].as_str().unwrap()).unwrap();
        let (flags, _) = split_frame(&frame);
        let mut out = empty_bytes();
        let rc = unsafe { aster_zstd_maybe_compress(p.as_ptr(), p.len(), threshold, &mut out) };
        assert_eq!(rc, OK);
        let compressed = !out.ptr.is_null();
        assert_eq!(compressed, flags & 0x01 != 0, "{}", v["name"]);
        if compressed {
            // zstd bytes may differ across zstd versions; check round trip.
            let c = unsafe { take(out) };
            assert!(c.len() < p.len());
            let mut back = empty_bytes();
            let rc = unsafe { aster_zstd_decompress(c.as_ptr(), c.len(), &mut back) };
            assert_eq!(rc, OK);
            assert_eq!(unsafe { take(back) }, p);
        }
    }
    // Empty input below threshold: nothing allocated.
    let mut out = empty_bytes();
    assert_eq!(
        unsafe { aster_zstd_maybe_compress(ptr::null(), 0, threshold, &mut out) },
        OK
    );
    assert!(out.ptr.is_null() && out.len == 0);
}
