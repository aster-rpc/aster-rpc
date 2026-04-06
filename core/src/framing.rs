/// Aster wire-framing: encode and decode length-prefixed frames.
///
/// Translated from the byte-manipulation parts of `bindings/python/aster/framing.py`.
/// Only synchronous encode/decode — async I/O stays in each language binding.
use anyhow::{bail, Result};

// Flag constants
pub const FLAG_COMPRESSED: u8 = 0x01;
pub const FLAG_TRAILER: u8 = 0x02;
pub const FLAG_HEADER: u8 = 0x04;
pub const FLAG_ROW_SCHEMA: u8 = 0x08;
pub const FLAG_CALL: u8 = 0x10;
pub const FLAG_CANCEL: u8 = 0x20;

/// Maximum frame body size: 16 MiB.
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Encode a frame: returns `[4-byte LE length][1-byte flags][payload]`.
///
/// *Length* equals `1` (the flags byte) plus `payload.len()`.
///
/// # Errors
/// - Empty payload without `FLAG_TRAILER` or `FLAG_CANCEL`.
/// - Frame body length exceeds `MAX_FRAME_SIZE`.
pub fn encode_frame(payload: &[u8], flags: u8) -> Result<Vec<u8>> {
    let frame_body_len = 1u32 + payload.len() as u32;

    // Empty payload only allowed for control frames (TRAILER or CANCEL)
    if payload.is_empty() && (flags & (FLAG_TRAILER | FLAG_CANCEL)) == 0 {
        bail!("empty payload only allowed with TRAILER or CANCEL flags");
    }

    if frame_body_len > MAX_FRAME_SIZE {
        bail!(
            "frame body length {} exceeds MAX_FRAME_SIZE {}",
            frame_body_len,
            MAX_FRAME_SIZE
        );
    }

    let mut out = Vec::with_capacity(4 + frame_body_len as usize);
    out.extend_from_slice(&frame_body_len.to_le_bytes());
    out.push(flags);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decode a frame from a byte slice.
///
/// Returns `(payload, flags, bytes_consumed)`.
///
/// # Errors
/// - Fewer than 4 bytes available.
/// - Zero or oversized frame body length.
/// - Incomplete frame (buffer shorter than the declared length).
pub fn decode_frame(data: &[u8]) -> Result<(Vec<u8>, u8, usize)> {
    if data.len() < 4 {
        bail!(
            "insufficient data for frame length header: need 4 bytes, got {}",
            data.len()
        );
    }

    let frame_body_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);

    if frame_body_len == 0 {
        bail!("frame body length is zero");
    }

    if frame_body_len > MAX_FRAME_SIZE {
        bail!(
            "frame body length {} exceeds MAX_FRAME_SIZE {}",
            frame_body_len,
            MAX_FRAME_SIZE
        );
    }

    let total_len = 4 + frame_body_len as usize;
    if data.len() < total_len {
        bail!(
            "incomplete frame: need {} bytes, got {}",
            total_len,
            data.len()
        );
    }

    let flags = data[4];
    let payload = data[5..total_len].to_vec();

    Ok((payload, flags, total_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Round-trip tests ----

    #[test]
    fn test_roundtrip_simple_payload() {
        let payload = b"hello world";
        let flags = FLAG_CALL;
        let frame = encode_frame(payload, flags).unwrap();
        let (decoded_payload, decoded_flags, consumed) = decode_frame(&frame).unwrap();
        assert_eq!(decoded_payload, payload);
        assert_eq!(decoded_flags, flags);
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn test_roundtrip_header_flag() {
        let payload = vec![0x01, 0x02, 0x03];
        let flags = FLAG_HEADER;
        let frame = encode_frame(&payload, flags).unwrap();
        let (decoded_payload, decoded_flags, consumed) = decode_frame(&frame).unwrap();
        assert_eq!(decoded_payload, payload);
        assert_eq!(decoded_flags, flags);
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn test_roundtrip_combined_flags() {
        let payload = b"compressed data";
        let flags = FLAG_COMPRESSED | FLAG_CALL;
        let frame = encode_frame(payload, flags).unwrap();
        let (decoded_payload, decoded_flags, _) = decode_frame(&frame).unwrap();
        assert_eq!(decoded_payload, payload);
        assert_eq!(decoded_flags, flags);
    }

    // ---- Empty control frames ----

    #[test]
    fn test_empty_trailer() {
        let frame = encode_frame(&[], FLAG_TRAILER).unwrap();
        let (payload, flags, consumed) = decode_frame(&frame).unwrap();
        assert!(payload.is_empty());
        assert_eq!(flags, FLAG_TRAILER);
        assert_eq!(consumed, 5); // 4 length + 1 flags byte
    }

    #[test]
    fn test_empty_cancel() {
        let frame = encode_frame(&[], FLAG_CANCEL).unwrap();
        let (payload, flags, consumed) = decode_frame(&frame).unwrap();
        assert!(payload.is_empty());
        assert_eq!(flags, FLAG_CANCEL);
        assert_eq!(consumed, 5);
    }

    #[test]
    fn test_empty_trailer_cancel_combined() {
        let frame = encode_frame(&[], FLAG_TRAILER | FLAG_CANCEL).unwrap();
        let (payload, flags, _) = decode_frame(&frame).unwrap();
        assert!(payload.is_empty());
        assert_eq!(flags, FLAG_TRAILER | FLAG_CANCEL);
    }

    // ---- Error cases ----

    #[test]
    fn test_error_empty_payload_no_control_flags() {
        let result = encode_frame(&[], 0x00);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("empty payload"));
    }

    #[test]
    fn test_error_empty_payload_non_control_flag() {
        let result = encode_frame(&[], FLAG_HEADER);
        assert!(result.is_err());
    }

    #[test]
    fn test_error_decode_insufficient_header() {
        let result = decode_frame(&[0x01, 0x02]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("insufficient data"));
    }

    #[test]
    fn test_error_decode_zero_length() {
        let data = [0x00, 0x00, 0x00, 0x00, 0xFF];
        let result = decode_frame(&data);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("zero"));
    }

    #[test]
    fn test_error_decode_incomplete_frame() {
        // Declare length=10 but only provide 6 bytes total
        let mut data = Vec::new();
        data.extend_from_slice(&10u32.to_le_bytes());
        data.extend_from_slice(&[0x00, 0x01]); // only 2 body bytes instead of 10
        let result = decode_frame(&data);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("incomplete frame"));
    }

    #[test]
    fn test_error_oversized_frame() {
        // Create a payload that would exceed MAX_FRAME_SIZE
        // We can't actually allocate 16 MiB in a test, so we craft raw bytes
        // with an oversized length header for decode.
        let oversized = MAX_FRAME_SIZE + 1;
        let mut data = Vec::new();
        data.extend_from_slice(&oversized.to_le_bytes());
        data.push(0x00); // flags
        let result = decode_frame(&data);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("exceeds MAX_FRAME_SIZE"));
    }

    // ---- Exact byte layout ----

    #[test]
    fn test_exact_byte_layout() {
        let payload = b"\xDE\xAD";
        let flags = 0x04; // FLAG_HEADER
        let frame = encode_frame(payload, flags).unwrap();

        // frame_body_len = 1 + 2 = 3
        assert_eq!(frame[0..4], 3u32.to_le_bytes());
        assert_eq!(frame[4], 0x04); // flags
        assert_eq!(frame[5], 0xDE); // payload[0]
        assert_eq!(frame[6], 0xAD); // payload[1]
        assert_eq!(frame.len(), 7);
    }

    // ---- Multiple frames ----

    #[test]
    fn test_decode_multiple_frames() {
        let frame1 = encode_frame(b"first", FLAG_CALL).unwrap();
        let frame2 = encode_frame(b"second", FLAG_HEADER).unwrap();
        let frame3 = encode_frame(&[], FLAG_TRAILER).unwrap();

        let mut combined = Vec::new();
        combined.extend_from_slice(&frame1);
        combined.extend_from_slice(&frame2);
        combined.extend_from_slice(&frame3);

        // Decode first frame
        let (p1, f1, c1) = decode_frame(&combined).unwrap();
        assert_eq!(p1, b"first");
        assert_eq!(f1, FLAG_CALL);

        // Decode second frame
        let (p2, f2, c2) = decode_frame(&combined[c1..]).unwrap();
        assert_eq!(p2, b"second");
        assert_eq!(f2, FLAG_HEADER);

        // Decode third frame
        let (p3, f3, c3) = decode_frame(&combined[c1 + c2..]).unwrap();
        assert!(p3.is_empty());
        assert_eq!(f3, FLAG_TRAILER);

        // All bytes consumed
        assert_eq!(c1 + c2 + c3, combined.len());
    }
}
