/// Fory XLANG canonical byte encoding primitives.
///
/// Translated from `bindings/python/aster/contract/canonical.py`.
/// Sentinel byte for an absent optional field.
pub const NULL_FLAG: u8 = 0xFD;

/// Element-header byte written after a list length.
pub const LIST_ELEMENT_HEADER: u8 = 0x0C;

/// Write an unsigned LEB128 variable-length integer.
pub fn write_varint(buf: &mut Vec<u8>, value: u64) {
    let mut v = value;
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
}

/// Write a ZigZag-encoded signed 32-bit integer as a varint.
///
/// ZigZag(n) = (n << 1) ^ (n >> 31)
pub fn write_zigzag_i32(buf: &mut Vec<u8>, value: i32) {
    let zigzag = ((value << 1) ^ (value >> 31)) as u32;
    write_varint(buf, zigzag as u64);
}

/// Write a ZigZag-encoded signed 64-bit integer as a varint.
///
/// ZigZag(n) = (n << 1) ^ (n >> 63)
pub fn write_zigzag_i64(buf: &mut Vec<u8>, value: i64) {
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    write_varint(buf, zigzag);
}

/// Write a UTF-8 Fory XLANG string.
///
/// Format: `varint((utf8_byte_length << 2) | 2)` followed by the UTF-8 bytes.
pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    let encoded = s.as_bytes();
    let header = ((encoded.len() as u64) << 2) | 2;
    write_varint(buf, header);
    buf.extend_from_slice(encoded);
}

/// Write a raw bytes field.
///
/// Format: `varint(length)` followed by the raw bytes.
pub fn write_bytes_field(buf: &mut Vec<u8>, data: &[u8]) {
    write_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

/// Write a boolean: `0x01` for true, `0x00` for false.
pub fn write_bool(buf: &mut Vec<u8>, value: bool) {
    buf.push(if value { 0x01 } else { 0x00 });
}

/// Write a float64 as 8 bytes in little-endian IEEE 754.
pub fn write_float64(buf: &mut Vec<u8>, value: f64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Write a list header: `varint(length)` followed by the `LIST_ELEMENT_HEADER` byte.
pub fn write_list_header(buf: &mut Vec<u8>, length: usize) {
    write_varint(buf, length as u64);
    buf.push(LIST_ELEMENT_HEADER);
}

/// Write `NULL_FLAG` (`0xFD`) for an absent optional field.
pub fn write_optional_absent(buf: &mut Vec<u8>) {
    buf.push(NULL_FLAG);
}

/// Write `0x00` presence byte before a present optional field's value.
pub fn write_optional_present_prefix(buf: &mut Vec<u8>) {
    buf.push(0x00);
}

// ─── Reader primitives ──────────────────────────────────────────────────────
//
// One-to-one inverses of the `write_*` helpers above. Each takes a byte
// slice + cursor (`pos: &mut usize`), advances the cursor on success, and
// returns an error on malformed input (short buffer or wrong framing byte).
//
// The canonical byte sequences produced by `write_*` are self-describing
// enough that there is no ambiguity at decode time when the caller knows
// which field type to expect next. That contract matches the spec's
// §11.3.2 "schema-consistent mode" — decoders walk the ordered field
// schema just as encoders do.

use anyhow::{anyhow, bail, Result};

fn need(buf: &[u8], pos: usize, n: usize) -> Result<()> {
    if pos + n > buf.len() {
        bail!(
            "canonical: unexpected EOF (need {} bytes at offset {}, have {})",
            n,
            pos,
            buf.len()
        );
    }
    Ok(())
}

/// Read an unsigned LEB128 varint. Caps at 10 continuation bytes
/// (matches the max varint width for a u64).
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for _ in 0..10 {
        need(buf, *pos, 1)?;
        let byte = buf[*pos];
        *pos += 1;
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(anyhow!("canonical: varint too long (more than 10 bytes)"))
}

/// Read a ZigZag-decoded signed 32-bit integer.
pub fn read_zigzag_i32(buf: &[u8], pos: &mut usize) -> Result<i32> {
    let raw = read_varint(buf, pos)?;
    if raw > u32::MAX as u64 {
        bail!("canonical: zigzag i32 out of range ({})", raw);
    }
    let raw = raw as u32;
    Ok(((raw >> 1) as i32) ^ -((raw & 1) as i32))
}

/// Read a ZigZag-decoded signed 64-bit integer.
pub fn read_zigzag_i64(buf: &[u8], pos: &mut usize) -> Result<i64> {
    let raw = read_varint(buf, pos)?;
    Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
}

/// Read a UTF-8 Fory XLANG string.
///
/// Header is `varint((utf8_byte_length << 2) | 2)`. The low two bits must
/// be `0b10` — this is the UTF-8 variant marker in Fory's string encoding.
pub fn read_string(buf: &[u8], pos: &mut usize) -> Result<String> {
    let header = read_varint(buf, pos)?;
    if header & 0b11 != 0b10 {
        bail!(
            "canonical: string header has non-UTF8 encoding bits (header={:#x})",
            header
        );
    }
    let len = (header >> 2) as usize;
    need(buf, *pos, len)?;
    let bytes = &buf[*pos..*pos + len];
    *pos += len;
    String::from_utf8(bytes.to_vec())
        .map_err(|e| anyhow!("canonical: invalid UTF-8 in string: {}", e))
}

/// Read a raw bytes field (varint length + bytes).
pub fn read_bytes_field(buf: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let len = read_varint(buf, pos)? as usize;
    need(buf, *pos, len)?;
    let bytes = buf[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(bytes)
}

/// Read a boolean (`0x01` = true, `0x00` = false; anything else is an error).
pub fn read_bool(buf: &[u8], pos: &mut usize) -> Result<bool> {
    need(buf, *pos, 1)?;
    let byte = buf[*pos];
    *pos += 1;
    match byte {
        0x00 => Ok(false),
        0x01 => Ok(true),
        other => bail!("canonical: invalid bool byte {:#x}", other),
    }
}

/// Read a float64 (8 bytes little-endian IEEE 754).
pub fn read_float64(buf: &[u8], pos: &mut usize) -> Result<f64> {
    need(buf, *pos, 8)?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&buf[*pos..*pos + 8]);
    *pos += 8;
    Ok(f64::from_le_bytes(arr))
}

/// Read a list header. Returns the element count. Consumes the trailing
/// `LIST_ELEMENT_HEADER` byte and errors if it doesn't match.
pub fn read_list_header(buf: &[u8], pos: &mut usize) -> Result<usize> {
    let len = read_varint(buf, pos)? as usize;
    need(buf, *pos, 1)?;
    let marker = buf[*pos];
    *pos += 1;
    if marker != LIST_ELEMENT_HEADER {
        bail!(
            "canonical: list element header mismatch (got {:#x}, expected {:#x})",
            marker,
            LIST_ELEMENT_HEADER
        );
    }
    Ok(len)
}

/// Peek at the next byte to decide whether an optional is absent. Returns
/// `true` (and consumes the presence byte `0x00`) if present, `false`
/// (and consumes the `NULL_FLAG` byte) if absent. Any other byte is an
/// error — the caller framed the schema incorrectly.
pub fn read_optional_present(buf: &[u8], pos: &mut usize) -> Result<bool> {
    need(buf, *pos, 1)?;
    let byte = buf[*pos];
    *pos += 1;
    match byte {
        0x00 => Ok(true),
        NULL_FLAG => Ok(false),
        other => bail!("canonical: invalid optional marker {:#x}", other),
    }
}

/// Assert that all bytes of the buffer have been consumed. Callers that
/// decode a framed top-level value (TypeDef, ServiceContract) use this
/// to catch trailing-byte bugs that would otherwise pass silently.
pub fn expect_eof(buf: &[u8], pos: usize) -> Result<()> {
    if pos != buf.len() {
        bail!(
            "canonical: {} trailing bytes after decode (pos={}, len={})",
            buf.len() - pos,
            pos,
            buf.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- write_varint ----

    #[test]
    fn test_varint_zero() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);
    }

    #[test]
    fn test_varint_127() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 127);
        assert_eq!(buf, vec![0x7F]);
    }

    #[test]
    fn test_varint_128() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);
    }

    #[test]
    fn test_varint_300() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 300);
        assert_eq!(buf, vec![0xAC, 0x02]);
    }

    // ---- write_zigzag_i32 ----

    #[test]
    fn test_zigzag_i32_zero() {
        let mut buf = Vec::new();
        write_zigzag_i32(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);
    }

    #[test]
    fn test_zigzag_i32_neg1() {
        let mut buf = Vec::new();
        write_zigzag_i32(&mut buf, -1);
        assert_eq!(buf, vec![0x01]);
    }

    #[test]
    fn test_zigzag_i32_pos1() {
        let mut buf = Vec::new();
        write_zigzag_i32(&mut buf, 1);
        assert_eq!(buf, vec![0x02]);
    }

    #[test]
    fn test_zigzag_i32_neg2() {
        let mut buf = Vec::new();
        write_zigzag_i32(&mut buf, -2);
        assert_eq!(buf, vec![0x03]);
    }

    #[test]
    fn test_zigzag_i32_max() {
        let mut buf = Vec::new();
        write_zigzag_i32(&mut buf, i32::MAX); // 2147483647
                                              // ZigZag(2147483647) = 4294967294 = 0xFFFF_FFFE
        let mut expected = Vec::new();
        write_varint(&mut expected, 0xFFFF_FFFE);
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_zigzag_i32_min() {
        let mut buf = Vec::new();
        write_zigzag_i32(&mut buf, i32::MIN); // -2147483648
                                              // ZigZag(-2147483648) = 4294967295 = 0xFFFF_FFFF
        let mut expected = Vec::new();
        write_varint(&mut expected, 0xFFFF_FFFF);
        assert_eq!(buf, expected);
    }

    // ---- write_zigzag_i64 ----

    #[test]
    fn test_zigzag_i64_zero() {
        let mut buf = Vec::new();
        write_zigzag_i64(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);
    }

    #[test]
    fn test_zigzag_i64_neg1() {
        let mut buf = Vec::new();
        write_zigzag_i64(&mut buf, -1);
        assert_eq!(buf, vec![0x01]);
    }

    #[test]
    fn test_zigzag_i64_pos1() {
        let mut buf = Vec::new();
        write_zigzag_i64(&mut buf, 1);
        assert_eq!(buf, vec![0x02]);
    }

    #[test]
    fn test_zigzag_i64_neg2() {
        let mut buf = Vec::new();
        write_zigzag_i64(&mut buf, -2);
        assert_eq!(buf, vec![0x03]);
    }

    #[test]
    fn test_zigzag_i64_max() {
        let mut buf = Vec::new();
        write_zigzag_i64(&mut buf, i64::MAX);
        let mut expected = Vec::new();
        write_varint(&mut expected, 0xFFFF_FFFF_FFFF_FFFE);
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_zigzag_i64_min() {
        let mut buf = Vec::new();
        write_zigzag_i64(&mut buf, i64::MIN);
        let mut expected = Vec::new();
        write_varint(&mut expected, 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(buf, expected);
    }

    // ---- write_string ----

    #[test]
    fn test_string_empty() {
        let mut buf = Vec::new();
        write_string(&mut buf, "");
        // header = (0 << 2) | 2 = 2, varint(2) = [0x02]
        assert_eq!(buf, vec![0x02]);
    }

    #[test]
    fn test_string_xlang() {
        let mut buf = Vec::new();
        write_string(&mut buf, "xlang");
        // header = (5 << 2) | 2 = 22 = 0x16, varint(22) = [0x16]
        let mut expected = vec![0x16];
        expected.extend_from_slice(b"xlang");
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_string_empty_service() {
        let mut buf = Vec::new();
        write_string(&mut buf, "EmptyService");
        // header = (12 << 2) | 2 = 50 = 0x32, varint(50) = [0x32]
        let mut expected = vec![0x32];
        expected.extend_from_slice(b"EmptyService");
        assert_eq!(buf, expected);
    }

    // ---- write_bytes_field ----

    #[test]
    fn test_bytes_field_empty() {
        let mut buf = Vec::new();
        write_bytes_field(&mut buf, &[]);
        assert_eq!(buf, vec![0x00]);
    }

    #[test]
    fn test_bytes_field_32_bytes() {
        let mut buf = Vec::new();
        let data = vec![0xAA; 32];
        write_bytes_field(&mut buf, &data);
        // varint(32) = [0x20]
        let mut expected = vec![0x20];
        expected.extend_from_slice(&[0xAA; 32]);
        assert_eq!(buf, expected);
    }

    // ---- write_bool ----

    #[test]
    fn test_bool_true() {
        let mut buf = Vec::new();
        write_bool(&mut buf, true);
        assert_eq!(buf, vec![0x01]);
    }

    #[test]
    fn test_bool_false() {
        let mut buf = Vec::new();
        write_bool(&mut buf, false);
        assert_eq!(buf, vec![0x00]);
    }

    // ---- write_float64 ----

    #[test]
    fn test_float64_zero() {
        let mut buf = Vec::new();
        write_float64(&mut buf, 0.0);
        assert_eq!(buf, vec![0x00; 8]);
    }

    #[test]
    fn test_float64_30() {
        let mut buf = Vec::new();
        write_float64(&mut buf, 30.0);
        assert_eq!(buf, 30.0_f64.to_le_bytes().to_vec());
    }

    // ---- write_list_header ----

    #[test]
    fn test_list_header_zero() {
        let mut buf = Vec::new();
        write_list_header(&mut buf, 0);
        assert_eq!(buf, vec![0x00, 0x0C]);
    }

    #[test]
    fn test_list_header_three() {
        let mut buf = Vec::new();
        write_list_header(&mut buf, 3);
        assert_eq!(buf, vec![0x03, 0x0C]);
    }

    // ---- write_optional ----

    #[test]
    fn test_optional_absent() {
        let mut buf = Vec::new();
        write_optional_absent(&mut buf);
        assert_eq!(buf, vec![0xFD]);
    }

    #[test]
    fn test_optional_present_prefix() {
        let mut buf = Vec::new();
        write_optional_present_prefix(&mut buf);
        assert_eq!(buf, vec![0x00]);
    }

    // ---- Reader round-trips --------------------------------------------

    #[test]
    fn roundtrip_varint_edges() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let mut pos = 0;
            let got = read_varint(&buf, &mut pos).unwrap();
            assert_eq!(got, v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_zigzag_i32_edges() {
        for v in [0, 1, -1, 2, -2, 123456, -123456, i32::MAX, i32::MIN] {
            let mut buf = Vec::new();
            write_zigzag_i32(&mut buf, v);
            let mut pos = 0;
            let got = read_zigzag_i32(&buf, &mut pos).unwrap();
            assert_eq!(got, v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_zigzag_i64_edges() {
        for v in [0i64, 1, -1, i64::MAX, i64::MIN] {
            let mut buf = Vec::new();
            write_zigzag_i64(&mut buf, v);
            let mut pos = 0;
            let got = read_zigzag_i64(&buf, &mut pos).unwrap();
            assert_eq!(got, v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_string() {
        for s in ["", "x", "xlang", "EmptyService", "héllo 世界 ✨"] {
            let mut buf = Vec::new();
            write_string(&mut buf, s);
            let mut pos = 0;
            let got = read_string(&buf, &mut pos).unwrap();
            assert_eq!(got, s);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_bytes_field() {
        for b in [&[][..], &[0u8][..], &[0xAA; 32][..], &[0xFF; 1000][..]] {
            let mut buf = Vec::new();
            write_bytes_field(&mut buf, b);
            let mut pos = 0;
            let got = read_bytes_field(&buf, &mut pos).unwrap();
            assert_eq!(got, b);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_bool() {
        for v in [false, true] {
            let mut buf = Vec::new();
            write_bool(&mut buf, v);
            let mut pos = 0;
            assert_eq!(read_bool(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_float64() {
        for v in [0.0, 30.0, -1.5, f64::MIN, f64::MAX, f64::INFINITY] {
            let mut buf = Vec::new();
            write_float64(&mut buf, v);
            let mut pos = 0;
            let got = read_float64(&buf, &mut pos).unwrap();
            assert_eq!(got.to_bits(), v.to_bits());
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_list_header() {
        for n in [0usize, 1, 3, 255, 1024] {
            let mut buf = Vec::new();
            write_list_header(&mut buf, n);
            let mut pos = 0;
            assert_eq!(read_list_header(&buf, &mut pos).unwrap(), n);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_optional_absent() {
        let mut buf = Vec::new();
        write_optional_absent(&mut buf);
        let mut pos = 0;
        assert!(!read_optional_present(&buf, &mut pos).unwrap());
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn roundtrip_optional_present_prefix() {
        let mut buf = Vec::new();
        write_optional_present_prefix(&mut buf);
        let mut pos = 0;
        assert!(read_optional_present(&buf, &mut pos).unwrap());
        assert_eq!(pos, buf.len());
    }

    // ---- Reader error cases --------------------------------------------

    #[test]
    fn read_varint_short_buffer() {
        let buf = [0x80u8]; // continuation bit set, no following byte
        let mut pos = 0;
        assert!(read_varint(&buf, &mut pos).is_err());
    }

    #[test]
    fn read_string_short_buffer() {
        // header says 10 bytes, but only 2 follow
        let buf = [((10u64 << 2) | 2) as u8, b'h', b'i'];
        let mut pos = 0;
        assert!(read_string(&buf, &mut pos).is_err());
    }

    #[test]
    fn read_string_wrong_encoding_bits() {
        // low two bits = 0b00 (LATIN1 variant), not allowed in canonical
        let buf = [0u8];
        let mut pos = 0;
        assert!(read_string(&buf, &mut pos).is_err());
    }

    #[test]
    fn read_bool_invalid_byte() {
        let buf = [0x02u8];
        let mut pos = 0;
        assert!(read_bool(&buf, &mut pos).is_err());
    }

    #[test]
    fn read_list_header_wrong_marker() {
        let buf = [0x03u8, 0x0D]; // wrong marker byte
        let mut pos = 0;
        assert!(read_list_header(&buf, &mut pos).is_err());
    }

    #[test]
    fn read_optional_invalid_marker() {
        let buf = [0x42u8];
        let mut pos = 0;
        assert!(read_optional_present(&buf, &mut pos).is_err());
    }

    #[test]
    fn expect_eof_ok() {
        let buf = [0x00u8];
        let mut pos = 0;
        read_bool(&buf, &mut pos).unwrap();
        assert!(expect_eof(&buf, pos).is_ok());
    }

    #[test]
    fn expect_eof_trailing() {
        let buf = [0x00u8, 0xFF];
        let mut pos = 0;
        read_bool(&buf, &mut pos).unwrap();
        assert!(expect_eof(&buf, pos).is_err());
    }
}
