//! A span context as plain bytes, and its W3C `traceparent` text form (design
//! §5.1, §7.4): what bindings carry in their call-context carriers and write
//! into outbound metadata. Pure functions, no pipeline needed.

use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};

/// Length of a version-`00` `traceparent`: `00-<32 hex>-<16 hex>-<2 hex>`.
pub const TRACEPARENT_LEN: usize = 55;

/// A span context: trace id, span id, trace flags and whether it came from
/// another process. Field-for-field the C ABI's `aster_span_context_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TraceContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    /// W3C trace flags; bit 0 = sampled.
    pub trace_flags: u8,
    pub is_remote: bool,
}

impl TraceContext {
    /// The W3C sampled flag.
    pub const SAMPLED: u8 = 1;

    /// Both ids are non-zero (W3C: an all-zero id is invalid).
    pub fn is_valid(&self) -> bool {
        self.trace_id != [0; 16] && self.span_id != [0; 8]
    }

    /// The sampled flag is set.
    pub fn is_sampled(&self) -> bool {
        self.trace_flags & Self::SAMPLED != 0
    }

    /// From an OTel span context (`None` when it is invalid).
    pub fn from_span_context(sc: &SpanContext) -> Option<Self> {
        if !sc.is_valid() {
            return None;
        }
        Some(TraceContext {
            trace_id: sc.trace_id().to_bytes(),
            span_id: sc.span_id().to_bytes(),
            trace_flags: sc.trace_flags().to_u8(),
            is_remote: sc.is_remote(),
        })
    }

    /// As an OTel span context (empty trace state).
    pub fn to_span_context(&self) -> SpanContext {
        SpanContext::new(
            TraceId::from_bytes(self.trace_id),
            SpanId::from_bytes(self.span_id),
            TraceFlags::new(self.trace_flags),
            self.is_remote,
            TraceState::NONE,
        )
    }

    /// Write the version-`00` `traceparent` into `out` (lowercase hex).
    pub fn write_traceparent(&self, out: &mut [u8; TRACEPARENT_LEN]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut i = 0;
        let mut put = |b: u8, out: &mut [u8; TRACEPARENT_LEN]| {
            out[i] = b;
            i += 1;
        };
        put(b'0', out);
        put(b'0', out);
        put(b'-', out);
        for b in self.trace_id {
            put(HEX[(b >> 4) as usize], out);
            put(HEX[(b & 0xf) as usize], out);
        }
        put(b'-', out);
        for b in self.span_id {
            put(HEX[(b >> 4) as usize], out);
            put(HEX[(b & 0xf) as usize], out);
        }
        put(b'-', out);
        put(HEX[(self.trace_flags >> 4) as usize], out);
        put(HEX[(self.trace_flags & 0xf) as usize], out);
    }

    /// The version-`00` `traceparent` text.
    pub fn traceparent(&self) -> String {
        let mut buf = [0u8; TRACEPARENT_LEN];
        self.write_traceparent(&mut buf);
        // Only ASCII hex digits and dashes were written.
        buf.iter().map(|&b| b as char).collect()
    }

    /// Parse a W3C `traceparent` (the result is marked remote). Version `00`
    /// must be exactly 55 bytes; a higher version may append `-`-separated
    /// fields (W3C forward compatibility). Version `ff`, uppercase hex and
    /// all-zero ids are rejected.
    pub fn parse_traceparent(s: &[u8]) -> Option<Self> {
        if s.len() < TRACEPARENT_LEN {
            return None;
        }
        let version = hex_byte(s[0], s[1])?;
        if version == 0xff || (version == 0 && s.len() != TRACEPARENT_LEN) {
            return None;
        }
        if s.len() > TRACEPARENT_LEN && s[TRACEPARENT_LEN] != b'-' {
            return None;
        }
        if s[2] != b'-' || s[35] != b'-' || s[52] != b'-' {
            return None;
        }
        let mut trace_id = [0u8; 16];
        for (k, b) in trace_id.iter_mut().enumerate() {
            *b = hex_byte(s[3 + 2 * k], s[4 + 2 * k])?;
        }
        let mut span_id = [0u8; 8];
        for (k, b) in span_id.iter_mut().enumerate() {
            *b = hex_byte(s[36 + 2 * k], s[37 + 2 * k])?;
        }
        let trace_flags = hex_byte(s[53], s[54])?;
        let cx = TraceContext {
            trace_id,
            span_id,
            trace_flags,
            is_remote: true,
        };
        cx.is_valid().then_some(cx)
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some(hex_digit(hi)? << 4 | hex_digit(lo)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn round_trip() {
        let cx = TraceContext::parse_traceparent(SAMPLE.as_bytes()).unwrap();
        assert!(cx.is_remote && cx.is_sampled());
        assert_eq!(cx.span_id, [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]);
        assert_eq!(cx.traceparent(), SAMPLE);
        let sc = cx.to_span_context();
        assert_eq!(TraceContext::from_span_context(&sc), Some(cx));
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x",
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ] {
            assert_eq!(
                TraceContext::parse_traceparent(bad.as_bytes()),
                None,
                "{bad}"
            );
        }
        // A future version may carry more fields.
        let v1 = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra";
        assert!(TraceContext::parse_traceparent(v1.as_bytes()).is_some());
    }
}
