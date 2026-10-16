//! The one compact JWS implementation: `EdDSA` (Ed25519) over AWS-LC.
//!
//! Every Aster credential uses it, including enrollment JWTs. The profile is
//! deliberately narrow: the protected header is exactly `alg` and `typ`, every
//! segment is canonical unpadded base64url, signatures are exactly 64 bytes,
//! and JSON objects at any depth may not repeat a member. No header member
//! selects a key, and nothing here performs I/O.

use std::collections::BTreeSet;
use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

pub(crate) const ALG: &str = "EdDSA";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JwsError {
    /// Wrong segment count, noncanonical base64url, a signature segment that
    /// is not 64 bytes, or a payload with duplicate JSON members.
    Malformed,
    /// A header that is not exactly `{"alg":"EdDSA","typ":…}`.
    Header,
    /// The signature does not verify, or the signing key is unusable.
    Signature,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedHeader<'a> {
    alg: &'a str,
    typ: &'a str,
}

/// A decoded compact JWS whose signature has not been checked yet.
pub(crate) struct Compact<'a> {
    pub typ: String,
    pub payload: Vec<u8>,
    signing_input: &'a str,
    signature: [u8; 64],
}

/// Split and decode without trusting anything. The caller decides the `typ`
/// and the key, then calls [`Compact::verify`].
pub(crate) fn decode(token: &str) -> Result<Compact<'_>, JwsError> {
    let mut segments = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(JwsError::Malformed);
    };
    let header = canonical_segment(header)?;
    let payload_bytes = canonical_segment(payload)?;
    let signature: [u8; 64] = canonical_segment(signature)?
        .try_into()
        .map_err(|_| JwsError::Malformed)?;
    validate_unique_json(&header).map_err(|_| JwsError::Header)?;
    let parsed: ProtectedHeader<'_> =
        serde_json::from_slice(&header).map_err(|_| JwsError::Header)?;
    if parsed.alg != ALG {
        return Err(JwsError::Header);
    }
    let typ = parsed.typ.to_owned();
    validate_unique_json(&payload_bytes)?;
    // The signing input is the received text up to the second separator, so
    // verification covers exactly the bytes presented.
    let signing_input = &token[..token.rfind('.').ok_or(JwsError::Malformed)?];
    Ok(Compact {
        typ,
        payload: payload_bytes,
        signing_input,
        signature,
    })
}

impl Compact<'_> {
    pub fn verify(&self, key: &[u8; 32]) -> Result<(), JwsError> {
        use aws_lc_rs::signature::{UnparsedPublicKey, ED25519};
        UnparsedPublicKey::new(&ED25519, key)
            .verify(self.signing_input.as_bytes(), &self.signature)
            .map_err(|_| JwsError::Signature)
    }
}

/// Sign `payload` (already serialized JSON) as a compact JWS with the given
/// `typ`. The seed is the Ed25519 secret key; it is not retained.
pub(crate) fn sign(typ: &str, payload: &[u8], seed: &[u8; 32]) -> Result<String, JwsError> {
    let header =
        serde_json::to_vec(&ProtectedHeader { alg: ALG, typ }).map_err(|_| JwsError::Header)?;
    let mut token = URL_SAFE_NO_PAD.encode(header);
    token.push('.');
    token.push_str(&URL_SAFE_NO_PAD.encode(payload));
    let key = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| JwsError::Signature)?;
    let signature = key.sign(token.as_bytes());
    token.push('.');
    token.push_str(&URL_SAFE_NO_PAD.encode(signature.as_ref()));
    Ok(token)
}

/// Encoded length of a compact JWS before signing, so callers can bound the
/// complete artifact without producing it.
pub(crate) fn encoded_len(typ: &str, payload_len: usize) -> usize {
    let encoded = |len: usize| (len * 4).div_ceil(3);
    // {"alg":"EdDSA","typ":"…"} plus two separators and an 86-byte signature.
    encoded(24 + typ.len()) + encoded(payload_len) + 88
}

pub(crate) fn public_key(seed: &[u8; 32]) -> Result<[u8; 32], JwsError> {
    use aws_lc_rs::signature::KeyPair;
    let key = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| JwsError::Signature)?;
    key.public_key()
        .as_ref()
        .try_into()
        .map_err(|_| JwsError::Signature)
}

pub(crate) fn canonical_segment(segment: &str) -> Result<Vec<u8>, JwsError> {
    if segment.is_empty() {
        return Err(JwsError::Malformed);
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| JwsError::Malformed)?;
    // Reject alternative encodings of the same bytes (nonzero trailing bits).
    if URL_SAFE_NO_PAD.encode(&decoded) != segment {
        return Err(JwsError::Malformed);
    }
    Ok(decoded)
}

/// A bounded serde_json traversal that rejects every duplicate object key,
/// including keys a typed profile would ignore. serde_json's ordinary
/// recursion limit applies before deep attacker-controlled input can grow this
/// visitor's stack. Callers bound the input size.
struct UniqueJson;

pub(crate) fn validate_unique_json(bytes: &[u8]) -> Result<(), JwsError> {
    serde_json::from_slice::<UniqueJson>(bytes)
        .map(|_| ())
        .map_err(|_| JwsError::Malformed)
}

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut seen = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key) {
                        return Err(serde::de::Error::custom("duplicate JSON object key"));
                    }
                    map.next_value::<UniqueJson>()?;
                }
                Ok(UniqueJson)
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                while seq.next_element::<UniqueJson>()?.is_some() {}
                Ok(UniqueJson)
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson)
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper() {
        let token = sign("t+jwt", br#"{"a":1}"#, &[7; 32]).unwrap();
        assert_eq!(token.len(), encoded_len("t+jwt", 7));
        let key = public_key(&[7; 32]).unwrap();
        let compact = decode(&token).unwrap();
        assert_eq!(compact.typ, "t+jwt");
        compact.verify(&key).unwrap();
        assert_eq!(
            compact.verify(&public_key(&[8; 32]).unwrap()),
            Err(JwsError::Signature)
        );
        let mut tampered = token.clone();
        tampered.insert(token.find('.').unwrap() + 1, 'e');
        assert!(decode(&tampered).map_or(true, |c| c.verify(&key).is_err()));
    }

    #[test]
    fn rejects_extra_headers_padding_and_duplicates() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"t","kid":"x"}"#);
        let body = URL_SAFE_NO_PAD.encode(b"{}");
        let sig = URL_SAFE_NO_PAD.encode([0; 64]);
        assert_eq!(
            decode(&format!("{header}.{body}.{sig}")).err(),
            Some(JwsError::Header)
        );
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"t"}"#);
        let dup = URL_SAFE_NO_PAD.encode(br#"{"a":1,"a":2}"#);
        assert_eq!(
            decode(&format!("{header}.{dup}.{sig}")).err(),
            Some(JwsError::Malformed)
        );
        assert_eq!(
            decode(&format!("{header}.{body}=.{sig}")).err(),
            Some(JwsError::Malformed)
        );
        let short = URL_SAFE_NO_PAD.encode([0; 63]);
        assert_eq!(
            decode(&format!("{header}.{body}.{short}")).err(),
            Some(JwsError::Malformed)
        );
        assert_eq!(
            decode(&format!("{header}.{body}.{sig}.x")).err(),
            Some(JwsError::Malformed)
        );
    }
}
