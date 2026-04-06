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
}
