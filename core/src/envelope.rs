//! RPC envelope codec — `StreamHeader`, `CallHeader`, `RpcStatus`.
//!
//! The three framework types that wrap every Aster RPC (Aster-SPEC §6.2,
//! §6.4, session addendum) are encoded with a small hand-rolled canonical
//! format, **independent of Apache Fory**. Payloads stay Fory XLANG; only the
//! envelope moved. Every binding wraps these functions (PyO3 / NAPI / C FFI)
//! instead of re-implementing them, so the envelope cannot drift between
//! languages or with a Fory upgrade.
//!
//! # Wire format ("AEv1", normative in Aster-SPEC §6.9)
//!
//! ```text
//! envelope   := tag:u8  version:u8  field*
//! tag        := 0xA1 StreamHeader | 0xA2 CallHeader | 0xA3 RpcStatus
//! version    := 0x01
//! int        := zigzag varint (LEB128), range-checked to the field's width
//! string     := varint((utf8_len << 2) | 2)  utf8_bytes        (canonical.rs)
//! list<str>  := varint(count)  0x0C  string{count}              (canonical.rs)
//! ```
//!
//! Fields appear in the fixed order below, every field always present, no
//! padding, no trailing bytes. Each value therefore has exactly one encoding.
//!
//! | type          | fields, in order |
//! |---------------|------------------|
//! | StreamHeader  | service:string, method:string, version:i32, call_id:i32, deadline:i16, serialization_mode:i8, metadata_keys:list, metadata_values:list, session_id:i32 |
//! | CallHeader    | method:string, call_id:i32, deadline:i16, metadata_keys:list, metadata_values:list |
//! | RpcStatus     | code:i32, message:string, detail_keys:list, detail_values:list |
//!
//! The key and value lists of a pair must have equal lengths; encode and
//! decode both refuse a mismatch.
//!
//! The leading tag doubles as the frame sniff byte: it can never be `0x7B`
//! (`{`, the retired JSON mode) or `0x02` (Fory XLANG), and decoding one
//! envelope type's bytes as another fails on the tag instead of silently
//! yielding defaults.

use crate::canonical::{
    read_list_header, read_string, read_varint, write_list_header, write_string, write_varint,
};

/// First byte of an encoded [`StreamHeader`].
pub const TAG_STREAM_HEADER: u8 = 0xA1;
/// First byte of an encoded [`CallHeader`].
pub const TAG_CALL_HEADER: u8 = 0xA2;
/// First byte of an encoded [`RpcStatus`].
pub const TAG_RPC_STATUS: u8 = 0xA3;
/// The envelope format version this build reads and writes.
pub const ENVELOPE_VERSION: u8 = 0x01;

/// Payload serialization mode carried in [`StreamHeader::serialization_mode`].
///
/// JSON (`3`) is retired across all bindings; `Native`/`Row` are reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i8)]
pub enum SerializationMode {
    Xlang = 0,
    Native = 1,
    Row = 2,
}

impl SerializationMode {
    /// The on-the-wire `i8` value.
    pub fn as_i8(self) -> i8 {
        self as i8
    }
}

/// First frame on every stream (`FLAG_HEADER`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamHeader {
    pub service: String,
    pub method: String,
    pub version: i32,
    pub call_id: i32,
    /// Relative seconds; `0` = no deadline.
    pub deadline: i16,
    /// XLANG=0, NATIVE=1, ROW=2.
    pub serialization_mode: i8,
    pub metadata_keys: Vec<String>,
    pub metadata_values: Vec<String>,
    /// `0` = stateless SHARED-pool stream; non-zero = session-bound.
    pub session_id: i32,
}

/// Per-call header within a session stream (`FLAG_CALL`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallHeader {
    pub method: String,
    pub call_id: i32,
    /// Relative seconds; `0` = no deadline.
    pub deadline: i16,
    pub metadata_keys: Vec<String>,
    pub metadata_values: Vec<String>,
}

/// Trailing status frame (`FLAG_TRAILER`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RpcStatus {
    pub code: i32,
    pub message: String,
    pub detail_keys: Vec<String>,
    pub detail_values: Vec<String>,
}

/// RPC status codes (Aster-SPEC §6.5). 0–16 follow gRPC; 100+ are
/// application/Aster-native.
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

    /// Map a wire code back; unrecognised codes map to [`StatusCode::Unknown`].
    pub fn from_i32(code: i32) -> StatusCode {
        use StatusCode::*;
        match code {
            0 => Ok,
            1 => Cancelled,
            2 => Unknown,
            3 => InvalidArgument,
            4 => DeadlineExceeded,
            5 => NotFound,
            6 => AlreadyExists,
            7 => PermissionDenied,
            8 => ResourceExhausted,
            9 => FailedPrecondition,
            10 => Aborted,
            11 => OutOfRange,
            12 => Unimplemented,
            13 => Internal,
            14 => Unavailable,
            15 => DataLoss,
            16 => Unauthenticated,
            101 => ContractViolation,
            _ => Unknown,
        }
    }
}

impl RpcStatus {
    /// A success status (`OK`, empty message).
    pub fn ok() -> Self {
        RpcStatus::default()
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

/// Envelope encode/decode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeError(pub String);

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "envelope: {}", self.0)
    }
}

impl std::error::Error for EnvelopeError {}

/// Result alias for envelope operations.
pub type Result<T> = std::result::Result<T, EnvelopeError>;

fn err(msg: impl Into<String>) -> EnvelopeError {
    EnvelopeError(msg.into())
}

// ── primitives ──────────────────────────────────────────────────────────────

fn write_int(buf: &mut Vec<u8>, v: i64) {
    write_varint(buf, ((v << 1) ^ (v >> 63)) as u64);
}

fn read_int(buf: &[u8], pos: &mut usize, min: i64, max: i64, field: &str) -> Result<i64> {
    let start = *pos;
    let raw = read_varint(buf, pos).map_err(|e| err(format!("{field}: {e}")))?;
    let v = ((raw >> 1) as i64) ^ -((raw & 1) as i64);
    if v < min || v > max {
        return Err(err(format!("{field}: {v} out of range")));
    }
    // Reject non-minimal varints so every value has exactly one encoding.
    let mut canon = Vec::new();
    write_varint(&mut canon, raw);
    if buf.get(start..*pos) != Some(canon.as_slice()) {
        return Err(err(format!("{field}: non-minimal varint")));
    }
    Ok(v)
}

fn read_str(buf: &[u8], pos: &mut usize, field: &str) -> Result<String> {
    let start = *pos;
    let s = read_string(buf, pos).map_err(|e| err(format!("{field}: {e}")))?;
    let mut canon = Vec::new();
    write_string(&mut canon, &s);
    if buf.get(start..*pos) != Some(canon.as_slice()) {
        return Err(err(format!("{field}: non-canonical string header")));
    }
    Ok(s)
}

fn write_list(buf: &mut Vec<u8>, items: &[String]) {
    write_list_header(buf, items.len());
    for s in items {
        write_string(buf, s);
    }
}

fn read_list(buf: &[u8], pos: &mut usize, field: &str) -> Result<Vec<String>> {
    let start = *pos;
    let n = read_list_header(buf, pos).map_err(|e| err(format!("{field}: {e}")))?;
    let mut canon = Vec::new();
    write_list_header(&mut canon, n);
    if buf.get(start..*pos) != Some(canon.as_slice()) {
        return Err(err(format!("{field}: non-canonical list header")));
    }
    // Every string costs at least one byte: refuse counts the input cannot hold
    // before allocating for them.
    if n > buf.len() - *pos {
        return Err(err(format!("{field}: list count {n} exceeds input")));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_str(buf, pos, field)?);
    }
    Ok(out)
}

fn begin(tag: u8) -> Vec<u8> {
    vec![tag, ENVELOPE_VERSION]
}

fn expect_prefix(buf: &[u8], tag: u8, name: &str) -> Result<usize> {
    match buf {
        [t, v, ..] if *t == tag && *v == ENVELOPE_VERSION => Ok(2),
        [t, v, ..] if *t == tag => Err(err(format!("{name}: unsupported version {v:#04x}"))),
        [t, ..] => Err(err(format!("{name}: wrong tag {t:#04x} (want {tag:#04x})"))),
        [] => Err(err(format!("{name}: empty input"))),
    }
}

fn finish(buf: &[u8], pos: usize, name: &str) -> Result<()> {
    if pos != buf.len() {
        return Err(err(format!("{name}: {} trailing bytes", buf.len() - pos)));
    }
    Ok(())
}

fn check_pairs(keys: &[String], values: &[String], what: &str) -> Result<()> {
    if keys.len() != values.len() {
        return Err(err(format!(
            "{what}: {} keys but {} values",
            keys.len(),
            values.len()
        )));
    }
    Ok(())
}

// ── StreamHeader ────────────────────────────────────────────────────────────

/// Encode a [`StreamHeader`] (payload of a `FLAG_HEADER` frame).
pub fn encode_stream_header(h: &StreamHeader) -> Result<Vec<u8>> {
    check_pairs(
        &h.metadata_keys,
        &h.metadata_values,
        "StreamHeader.metadata",
    )?;
    let mut b = begin(TAG_STREAM_HEADER);
    write_string(&mut b, &h.service);
    write_string(&mut b, &h.method);
    write_int(&mut b, h.version.into());
    write_int(&mut b, h.call_id.into());
    write_int(&mut b, h.deadline.into());
    write_int(&mut b, h.serialization_mode.into());
    write_list(&mut b, &h.metadata_keys);
    write_list(&mut b, &h.metadata_values);
    write_int(&mut b, h.session_id.into());
    Ok(b)
}

/// Decode a [`StreamHeader`] from a `FLAG_HEADER` frame payload.
pub fn decode_stream_header(buf: &[u8]) -> Result<StreamHeader> {
    const N: &str = "StreamHeader";
    let mut p = expect_prefix(buf, TAG_STREAM_HEADER, N)?;
    let i32r = (i32::MIN as i64, i32::MAX as i64);
    let h = StreamHeader {
        service: read_str(buf, &mut p, "service")?,
        method: read_str(buf, &mut p, "method")?,
        version: read_int(buf, &mut p, i32r.0, i32r.1, "version")? as i32,
        call_id: read_int(buf, &mut p, i32r.0, i32r.1, "call_id")? as i32,
        deadline: read_int(buf, &mut p, i16::MIN as i64, i16::MAX as i64, "deadline")? as i16,
        serialization_mode: read_int(
            buf,
            &mut p,
            i8::MIN as i64,
            i8::MAX as i64,
            "serialization_mode",
        )? as i8,
        metadata_keys: read_list(buf, &mut p, "metadata_keys")?,
        metadata_values: read_list(buf, &mut p, "metadata_values")?,
        session_id: read_int(buf, &mut p, i32r.0, i32r.1, "session_id")? as i32,
    };
    finish(buf, p, N)?;
    check_pairs(
        &h.metadata_keys,
        &h.metadata_values,
        "StreamHeader.metadata",
    )?;
    Ok(h)
}

// ── CallHeader ──────────────────────────────────────────────────────────────

/// Encode a [`CallHeader`] (payload of a `FLAG_CALL` frame).
pub fn encode_call_header(h: &CallHeader) -> Result<Vec<u8>> {
    check_pairs(&h.metadata_keys, &h.metadata_values, "CallHeader.metadata")?;
    let mut b = begin(TAG_CALL_HEADER);
    write_string(&mut b, &h.method);
    write_int(&mut b, h.call_id.into());
    write_int(&mut b, h.deadline.into());
    write_list(&mut b, &h.metadata_keys);
    write_list(&mut b, &h.metadata_values);
    Ok(b)
}

/// Decode a [`CallHeader`] from a `FLAG_CALL` frame payload.
pub fn decode_call_header(buf: &[u8]) -> Result<CallHeader> {
    const N: &str = "CallHeader";
    let mut p = expect_prefix(buf, TAG_CALL_HEADER, N)?;
    let h = CallHeader {
        method: read_str(buf, &mut p, "method")?,
        call_id: read_int(buf, &mut p, i32::MIN as i64, i32::MAX as i64, "call_id")? as i32,
        deadline: read_int(buf, &mut p, i16::MIN as i64, i16::MAX as i64, "deadline")? as i16,
        metadata_keys: read_list(buf, &mut p, "metadata_keys")?,
        metadata_values: read_list(buf, &mut p, "metadata_values")?,
    };
    finish(buf, p, N)?;
    check_pairs(&h.metadata_keys, &h.metadata_values, "CallHeader.metadata")?;
    Ok(h)
}

// ── RpcStatus ───────────────────────────────────────────────────────────────

/// Encode an [`RpcStatus`] (payload of a `FLAG_TRAILER` frame).
pub fn encode_rpc_status(s: &RpcStatus) -> Result<Vec<u8>> {
    check_pairs(&s.detail_keys, &s.detail_values, "RpcStatus.details")?;
    let mut b = begin(TAG_RPC_STATUS);
    write_int(&mut b, s.code.into());
    write_string(&mut b, &s.message);
    write_list(&mut b, &s.detail_keys);
    write_list(&mut b, &s.detail_values);
    Ok(b)
}

/// Decode an [`RpcStatus`] from a `FLAG_TRAILER` frame payload.
pub fn decode_rpc_status(buf: &[u8]) -> Result<RpcStatus> {
    const N: &str = "RpcStatus";
    let mut p = expect_prefix(buf, TAG_RPC_STATUS, N)?;
    let s = RpcStatus {
        code: read_int(buf, &mut p, i32::MIN as i64, i32::MAX as i64, "code")? as i32,
        message: read_str(buf, &mut p, "message")?,
        detail_keys: read_list(buf, &mut p, "detail_keys")?,
        detail_values: read_list(buf, &mut p, "detail_values")?,
    };
    finish(buf, p, N)?;
    check_pairs(&s.detail_keys, &s.detail_values, "RpcStatus.details")?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> StreamHeader {
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
    fn round_trips() {
        let h = header();
        assert_eq!(
            decode_stream_header(&encode_stream_header(&h).unwrap()).unwrap(),
            h
        );
        let c = CallHeader {
            method: "m".into(),
            call_id: -7,
            deadline: i16::MAX,
            metadata_keys: vec![],
            metadata_values: vec![],
        };
        assert_eq!(
            decode_call_header(&encode_call_header(&c).unwrap()).unwrap(),
            c
        );
        let s = RpcStatus {
            code: i32::MIN,
            message: "ünïcode ✓".into(),
            detail_keys: vec!["a".into(), "".into()],
            detail_values: vec!["1".into(), "2".into()],
        };
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

    #[test]
    fn exact_bytes() {
        // Hand-derived from the format table, not captured from the encoder.
        let s = RpcStatus::error(StatusCode::NotFound, "no");
        assert_eq!(
            encode_rpc_status(&s).unwrap(),
            vec![0xA3, 0x01, 0x0A, 0x0A, b'n', b'o', 0x00, 0x0C, 0x00, 0x0C]
        );
        assert_eq!(
            encode_rpc_status(&RpcStatus::ok()).unwrap(),
            vec![0xA3, 0x01, 0x00, 0x02, 0x00, 0x0C, 0x00, 0x0C]
        );
    }

    #[test]
    fn cross_type_decode_fails_on_tag() {
        let b = encode_stream_header(&header()).unwrap();
        assert!(decode_rpc_status(&b).is_err());
        assert!(decode_call_header(&b).is_err());
    }

    #[test]
    fn rejects_malformed() {
        let good = encode_stream_header(&header()).unwrap();
        // trailing byte
        let mut t = good.clone();
        t.push(0);
        assert!(decode_stream_header(&t).is_err());
        // every truncation
        for n in 0..good.len() {
            assert!(
                decode_stream_header(&good[..n]).is_err(),
                "truncated at {n}"
            );
        }
        // bad version
        let mut v = good.clone();
        v[1] = 2;
        assert!(decode_stream_header(&v).is_err());
        // non-minimal varint for an int field (RpcStatus code 0 as 0x80 0x00)
        let nm = vec![0xA3, 0x01, 0x80, 0x00, 0x02, 0x00, 0x0C, 0x00, 0x0C];
        assert!(decode_rpc_status(&nm).is_err());
        // deadline out of i16 range
        let mut big = begin(TAG_CALL_HEADER);
        write_string(&mut big, "m");
        write_int(&mut big, 0);
        write_int(&mut big, 40_000);
        write_list(&mut big, &[]);
        write_list(&mut big, &[]);
        assert!(decode_call_header(&big).is_err());
        // huge list count
        let mut huge = begin(TAG_RPC_STATUS);
        write_int(&mut huge, 0);
        write_string(&mut huge, "");
        write_varint(&mut huge, u32::MAX as u64);
        huge.push(0x0C);
        assert!(decode_rpc_status(&huge).is_err());
        // key/value mismatch
        let mut m = header();
        m.metadata_values.clear();
        assert!(encode_stream_header(&m).is_err());
    }
}
