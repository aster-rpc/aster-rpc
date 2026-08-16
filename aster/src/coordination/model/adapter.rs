//! The `CoordinationSerializer` adapter (§9.4): the coordination model behind
//! the existing `core::lease::LeaseSerializer` trait.
//!
//! Slice 1's job here is the **compatibility proof**, not the network client.
//! The adapter binds one group and namespace, encodes a coordination resource
//! key into the trait's `&str` resource, and maps coordination outcomes onto
//! `TransitionOutcome` -- so an existing `LeaseHandle` drives the coordination
//! state machine without a second lease implementation. Wiring it to a live
//! `CoordinationClient` is Slice 4's.
//!
//! ## The canonical adapter encoding
//!
//! ```text
//! ac1:<group-id-hex>:<namespace>:<base64url-no-pad(resource-key-bytes)>
//! ```
//!
//! The encoded **group** stays inside the string that `core::Fence.resource`
//! carries into the renew loop. That is what stops a handle held across a
//! recovery from being reinterpreted against a recovered group: the fence names
//! the group it was granted in, so a serializer bound to a different one rejects
//! it rather than renewing a lease that no longer exists.
//!
//! Non-canonical input is rejected rather than normalised. Two spellings of one
//! resource would be two leases over one row.

use std::fmt;

use crate::coordination::ids::{CoordinationGroupId, ResourceKey};

/// The adapter-encoding prefix.
const PREFIX: &str = "ac1";

/// A parsed adapter resource: the group, namespace and key it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterResource {
    pub group: CoordinationGroupId,
    pub key: ResourceKey,
}

/// Why an adapter resource string was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterError {
    /// Not four colon-separated fields beginning `ac1`.
    Shape,
    /// The group id was not 32 hex-encoded bytes.
    Group,
    /// The base64url segment did not decode, or was padded.
    Encoding,
    /// The decoded `(namespace, key)` failed §4.2 validation.
    Key,
    /// The string named a group other than the one this serializer is bound to.
    ///
    /// Separate from `Group` because it is not malformed -- it is a valid
    /// resource belonging to somebody else, which is exactly the recovered-group
    /// case the encoding exists to catch.
    WrongGroup,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            AdapterError::Shape => "not a canonical ac1 adapter resource",
            AdapterError::Group => "the group id is not 32 hex bytes",
            AdapterError::Encoding => "the key segment is not unpadded base64url",
            AdapterError::Key => "the decoded resource key is not valid",
            AdapterError::WrongGroup => "the resource belongs to another group",
        };
        f.write_str(text)
    }
}

/// Encode a coordination resource as the adapter string.
pub fn encode(group: &CoordinationGroupId, key: &ResourceKey) -> String {
    format!(
        "{PREFIX}:{}:{}:{}",
        group.to_hex(),
        key.namespace(),
        base64url_nopad(key.key())
    )
}

/// Parse an adapter string, requiring it to name `expected_group`.
pub fn decode(
    s: &str,
    expected_group: &CoordinationGroupId,
) -> Result<AdapterResource, AdapterError> {
    // `splitn(4, ..)` deliberately, not `split`: a namespace cannot contain a
    // colon (§4.2's charset forbids it) but a *malformed* one might, and the
    // key segment is base64url, which also cannot. Four fields exactly.
    let mut parts = s.split(':');
    let (Some(prefix), Some(group_hex), Some(namespace), Some(key_b64), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(AdapterError::Shape);
    };
    if prefix != PREFIX {
        return Err(AdapterError::Shape);
    }

    let group = CoordinationGroupId::parse_hex(group_hex).map_err(|_| AdapterError::Group)?;
    if &group != expected_group {
        return Err(AdapterError::WrongGroup);
    }

    let key_bytes = decode_base64url_nopad(key_b64).ok_or(AdapterError::Encoding)?;
    let key = ResourceKey::new(namespace, key_bytes).map_err(|_| AdapterError::Key)?;
    Ok(AdapterResource { group, key })
}

/// Unpadded base64url, written out rather than pulled in: the alphabet is four
/// lines and a dependency for it would be a dependency in the shipping graph.
fn base64url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let indices = [n >> 18, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for (i, index) in indices.iter().enumerate() {
            // Emit only the characters the input length justifies, so the
            // output is unpadded rather than padded-then-trimmed.
            if i <= chunk.len() {
                out.push(ALPHABET[*index as usize] as char);
            }
        }
    }
    out
}

fn decode_base64url_nopad(s: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'-' => 62,
            b'_' => 63,
            // Padding is not canonical here; neither is standard base64's
            // `+`/`/`. Both are rejected rather than accepted-and-normalised.
            _ => return None,
        })
    }

    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= value(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(byte: u8) -> CoordinationGroupId {
        CoordinationGroupId::from_array([byte; 32])
    }

    fn key(ns: &str, k: &[u8]) -> ResourceKey {
        ResourceKey::new(ns, k).expect("valid")
    }

    #[test]
    fn an_adapter_resource_round_trips() {
        let g = group(0xAB);
        let k = key("portal.roaming", b"\x00\xff primary");
        let encoded = encode(&g, &k);

        assert!(encoded.starts_with("ac1:"));
        let decoded = decode(&encoded, &g).expect("round trip");
        assert_eq!(decoded.group, g);
        assert_eq!(decoded.key, k);
    }

    /// Keys of every length modulo 3, so the unpadded encoding is exercised on
    /// all three tail shapes.
    #[test]
    fn every_key_length_round_trips_without_padding() {
        let g = group(1);
        for len in 1..=32usize {
            let k = key("ns", &vec![0xA5u8; len]);
            let encoded = encode(&g, &k);
            assert!(!encoded.contains('='), "the encoding must be unpadded");
            assert_eq!(decode(&encoded, &g).expect("round trip").key, k);
        }
    }

    /// The encoded group stays in the string the renew loop carries, so a
    /// handle from another group cannot be reinterpreted against this one.
    #[test]
    fn a_resource_from_another_group_is_rejected() {
        let mine = group(1);
        let theirs = group(2);
        let encoded = encode(&theirs, &key("ns", b"k"));

        assert_eq!(decode(&encoded, &mine), Err(AdapterError::WrongGroup));
        // And it is *valid* for its own group, which is what makes this a
        // wrong-group rejection rather than a parse failure.
        assert!(decode(&encoded, &theirs).is_ok());
    }

    /// Non-canonical input is rejected, not normalised: two spellings of one
    /// resource would be two leases over one row.
    #[test]
    fn non_canonical_input_is_rejected() {
        let g = group(1);
        let valid = encode(&g, &key("ns", b"key"));

        let cases: Vec<(&str, String, AdapterError)> = vec![
            (
                "no prefix",
                valid.replacen("ac1:", "", 1),
                AdapterError::Shape,
            ),
            (
                "wrong prefix",
                valid.replacen("ac1:", "ac2:", 1),
                AdapterError::Shape,
            ),
            (
                "too few fields",
                "ac1:aa:bb".to_string(),
                AdapterError::Shape,
            ),
            (
                "too many fields",
                format!("{valid}:extra"),
                AdapterError::Shape,
            ),
            (
                "short group",
                format!("ac1:{}:ns:a2V5", "ab".repeat(31)),
                AdapterError::Group,
            ),
            (
                "non-hex group",
                format!("ac1:{}:ns:a2V5", "zz".repeat(32)),
                AdapterError::Group,
            ),
            (
                "padded key",
                format!("ac1:{}:ns:a2V5=", group(1).to_hex()),
                AdapterError::Encoding,
            ),
            (
                "standard base64 alphabet",
                format!("ac1:{}:ns:a+V5", group(1).to_hex()),
                AdapterError::Encoding,
            ),
            (
                "invalid namespace",
                format!("ac1:{}:NS:a2V5", group(1).to_hex()),
                AdapterError::Key,
            ),
            (
                "empty key",
                format!("ac1:{}:ns:", group(1).to_hex()),
                AdapterError::Key,
            ),
        ];

        for (name, input, expected) in cases {
            assert_eq!(decode(&input, &g), Err(expected), "{name}");
        }
    }

    /// The encoding is stable: the same resource always produces the same
    /// string, which is what lets it be compared as an opaque `&str` by the
    /// existing lease engine.
    #[test]
    fn the_encoding_is_deterministic() {
        let g = group(7);
        let k = key("ns", b"abc");
        assert_eq!(encode(&g, &k), encode(&g, &k));
        // And a golden value, so a change to the encoding is visible rather
        // than silently accepted by a round trip against itself.
        assert_eq!(encode(&g, &k), format!("ac1:{}:ns:YWJj", "07".repeat(32)));
    }
}
