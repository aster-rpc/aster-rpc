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
/// Set on the LAST request frame of a client-streaming or bidi-streaming
/// call. Tells the reactor to close the per-call request channel and stop
/// reading more request frames for this call.
pub const FLAG_END_STREAM: u8 = 0x40;
/// Tunnel-redeem stream marker. Set on the first (and only framed) frame of
/// a stream that is redeeming a tunnel capability. Payload is the 32-byte
/// `TunnelTicket`. After this frame the stream switches to raw mode and the
/// acceptor splices bytes to the backend socket. See `Aster-tunneling.md`.
pub const FLAG_TUNNEL: u8 = 0x80;

/// QUIC application error code used to poison a stream after `FLAG_CANCEL`
/// (§6).
///
/// A named constant rather than a bare `0`, because the peer is expected to
/// *recognise* it: a client that sees this code knows its cancel was honoured
/// and the stream was retired deliberately, as distinct from a transport fault
/// or a server crash. Stable across versions -- changing it is a wire-visible
/// change.
pub const RESET_CODE_CANCELLED: u32 = 0x41_53_00_01; // "AS" 00 01

/// QUIC application error code for a stream torn down because the **peer broke
/// the framing contract** -- e.g. starting call N+1 before call N's response
/// finished (§6).
///
/// Distinct from [`RESET_CODE_CANCELLED`] on purpose. That code normatively
/// means "your cancellation was honoured"; using it here would tell a client
/// its own cancel succeeded when in fact the server rejected what it sent, so a
/// client could log a clean cancellation for a bug it needs to fix. Same
/// stability rule: changing it is a wire-visible change.
pub const RESET_CODE_PROTOCOL_VIOLATION: u32 = 0x41_53_00_02; // "AS" 00 02

/// QUIC application error code for a tunnel stream torn down because its
/// **upstream failed**: the backend TCP read errored (connection reset or any
/// error other than a clean EOF) while the response was in flight.
///
/// Dropping a QUIC send half finishes it, so without an explicit reset the
/// consumer would read the truncated response as a complete one. A clean
/// upstream EOF is still a QUIC finish. Same stability rule as the other
/// codes: changing it is a wire-visible change.
pub const RESET_CODE_TUNNEL_UPSTREAM_ABORTED: u32 = 0x41_53_00_03; // "AS" 00 03

/// Maximum frame body size: 16 MiB.
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Maximum size of a **call header** frame, for every service without
/// exception: 64 KiB.
///
/// A header is small and its size does not depend on the service, so this can
/// be applied before the service is known -- which is the point, since the
/// header is what names the service. Body frames are bounded separately, per
/// method, once the header has been decoded.
pub const MAX_CALL_HEADER_SIZE: u32 = 64 * 1024;

// ── Payload compression (Aster-SPEC §5.6) ────────────────────────────────────

/// Payloads strictly larger than this many bytes are zstd-compressed on send
/// (matches Python `DEFAULT_COMPRESSION_THRESHOLD` / TS
/// `DEFAULT_COMPRESSION_THRESHOLD`).
pub const COMPRESSION_THRESHOLD: usize = 4096;

/// zstd level used on send (the zstd / python-zstandard / Node default).
pub const ZSTD_LEVEL: i32 = 3;

/// Decompression-bomb limit: a compressed payload may expand to at most this
/// many bytes (16 MiB, matches Python/TS `MAX_DECOMPRESSED_SIZE`). Enforced
/// while streaming; the frame's declared content size is never trusted.
pub const MAX_DECOMPRESSED_SIZE: usize = 16 * 1024 * 1024;

/// Compress `payload` for sending if it is larger than `threshold` bytes.
///
/// Returns `Some(compressed)` when the caller should send the compressed bytes
/// with [`FLAG_COMPRESSED`] set, `None` to send `payload` unchanged. Like the
/// TypeScript binding, a compressed form that is not smaller than the input is
/// discarded. Receivers accept both forms, so this choice never affects
/// interoperability.
pub fn maybe_compress(payload: &[u8], threshold: usize) -> Result<Option<Vec<u8>>> {
    if payload.len() <= threshold {
        return Ok(None);
    }
    let compressed = zstd::bulk::compress(payload, ZSTD_LEVEL)
        .map_err(|e| anyhow::anyhow!("zstd compress failed: {e}"))?;
    Ok((compressed.len() < payload.len()).then_some(compressed))
}

/// Decompress a `FLAG_COMPRESSED` payload, refusing output larger than
/// [`MAX_DECOMPRESSED_SIZE`].
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    decompress_with_limit(data, MAX_DECOMPRESSED_SIZE)
}

/// [`decompress`] with an explicit output limit (bytes).
pub fn decompress_with_limit(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let decoder = zstd::stream::read::Decoder::new(data)
        .map_err(|e| anyhow::anyhow!("zstd decoder init failed: {e}"))?;
    let mut out = Vec::new();
    // Read at most limit+1 bytes: one byte past the limit proves the bomb
    // without materialising the rest of it.
    decoder
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| anyhow::anyhow!("zstd decompress failed: {e}"))?;
    if out.len() > limit {
        bail!("decompressed payload exceeds limit of {limit} bytes");
    }
    Ok(out)
}

/// Encode a data frame, compressing the payload when it exceeds
/// [`COMPRESSION_THRESHOLD`] (and setting [`FLAG_COMPRESSED`]).
pub fn encode_payload_frame(payload: &[u8], flags: u8) -> Result<Vec<u8>> {
    encode_payload_frame_with_threshold(payload, flags, COMPRESSION_THRESHOLD)
}

/// [`encode_payload_frame`] with an explicit threshold (`usize::MAX` disables
/// compression).
pub fn encode_payload_frame_with_threshold(
    payload: &[u8],
    flags: u8,
    threshold: usize,
) -> Result<Vec<u8>> {
    match maybe_compress(payload, threshold)? {
        Some(c) => encode_frame(&c, flags | FLAG_COMPRESSED),
        None => encode_frame(payload, flags & !FLAG_COMPRESSED),
    }
}

/// The logical payload of a received frame: decompressed if `flags` carries
/// [`FLAG_COMPRESSED`], otherwise `payload` itself.
pub fn payload_of(payload: Vec<u8>, flags: u8) -> Result<Vec<u8>> {
    if flags & FLAG_COMPRESSED != 0 {
        decompress(&payload)
    } else {
        Ok(payload)
    }
}

/// Encode a frame: returns `[4-byte LE length][1-byte flags][payload]`.
///
/// *Length* equals `1` (the flags byte) plus `payload.len()`.
///
/// # Errors
/// - Empty payload without `FLAG_TRAILER` or `FLAG_CANCEL`.
/// - Frame body length exceeds `MAX_FRAME_SIZE`.
pub fn encode_frame(payload: &[u8], flags: u8) -> Result<Vec<u8>> {
    let frame_body_len = 1u32 + payload.len() as u32;

    // Empty payload only allowed for control frames: TRAILER, CANCEL, or an
    // END_STREAM that ends the request phase without a final request (a
    // client streaming from an iterator learns it is done only after the last
    // item went out). The reactor drops the empty END_STREAM frame and closes
    // the request channel.
    if payload.is_empty() && (flags & (FLAG_TRAILER | FLAG_CANCEL | FLAG_END_STREAM)) == 0 {
        bail!("empty payload only allowed with TRAILER, CANCEL or END_STREAM flags");
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
    fn test_empty_end_stream_frame() {
        let frame = encode_frame(&[], FLAG_END_STREAM).unwrap();
        assert_eq!(frame, vec![1, 0, 0, 0, FLAG_END_STREAM]);
        let (payload, flags, consumed) = decode_frame(&frame).unwrap();
        assert!(payload.is_empty());
        assert_eq!(flags, FLAG_END_STREAM);
        assert_eq!(consumed, 5);
    }

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

    // ---- Compression ----

    #[test]
    fn small_payloads_are_not_compressed() {
        let p = vec![b'a'; COMPRESSION_THRESHOLD];
        let f = encode_payload_frame(&p, FLAG_END_STREAM).unwrap();
        let (payload, flags, _) = decode_frame(&f).unwrap();
        assert_eq!(flags, FLAG_END_STREAM);
        assert_eq!(payload, p);
    }

    #[test]
    fn large_payloads_compress_and_round_trip() {
        let p = vec![b'a'; COMPRESSION_THRESHOLD + 1];
        let f = encode_payload_frame(&p, FLAG_END_STREAM).unwrap();
        let (payload, flags, _) = decode_frame(&f).unwrap();
        assert_eq!(flags, FLAG_END_STREAM | FLAG_COMPRESSED);
        assert!(payload.len() < p.len());
        assert_eq!(payload_of(payload, flags).unwrap(), p);
    }

    #[test]
    fn incompressible_payloads_are_sent_raw() {
        // xorshift noise: zstd cannot shrink it.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let p: Vec<u8> = (0..8192)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let f = encode_payload_frame(&p, 0).unwrap();
        let (payload, flags, _) = decode_frame(&f).unwrap();
        assert_eq!(flags & FLAG_COMPRESSED, 0);
        assert_eq!(payload, p);
    }

    #[test]
    fn decompression_bomb_is_refused() {
        let bomb = zstd::bulk::compress(&vec![0u8; MAX_DECOMPRESSED_SIZE + 1], 3).unwrap();
        assert!(bomb.len() < 64 * 1024);
        let e = decompress(&bomb).unwrap_err().to_string();
        assert!(e.contains("exceeds limit"), "{e}");
        // Exactly at the limit is fine.
        let ok = zstd::bulk::compress(&vec![0u8; 1024], 3).unwrap();
        assert_eq!(decompress_with_limit(&ok, 1024).unwrap().len(), 1024);
        assert!(decompress_with_limit(&ok, 1023).is_err());
    }

    #[test]
    fn corrupt_compressed_payload_errors() {
        assert!(payload_of(vec![1, 2, 3, 4], FLAG_COMPRESSED).is_err());
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
