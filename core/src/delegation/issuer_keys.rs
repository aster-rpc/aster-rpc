//! Issuer key sets: the records that carry a trusted identity provider's
//! verification keys through the fleet namespace, so every core (air-gapped
//! ones too) verifies provider tokens without fetching anything. Design:
//! `docs/_internal/web_gateway/gateway-identity.md`, "Issuer keys: replicated,
//! never fetched by the core".
//!
//! One record per issuer and author, at [`issuer_keys_key`]:
//! `aster/identity/v1/issuers/<base64url(iss)>/jwks`. The issuer is encoded so
//! that no issuer's key is a prefix of another's (`https://a` and
//! `https://a/tenant`). The value is JSON:
//!
//! | Member | Meaning |
//! |---|---|
//! | `version` | `1` |
//! | `iss` | The issuer; must match the entry key |
//! | `source_url` | Where the publisher fetched the JWK set (TLS-validated) |
//! | `fetched_at` | When, in Unix seconds |
//! | `jwks` | `{"keys": [...]}`, minimal public JWKs (see [`ProviderKey::to_jwk`]) |
//! | `credential` | The publisher's enrollment credential chain; absent for a root-registered publisher |
//!
//! Decoding checks only the record. Authority comes from the entry's docs
//! **author**, which is the publisher's node key (Aster nodes write as their
//! node identity, and Docs verifies every entry's author signature), and is
//! decided by the reader: the author's credential (the carried chain, or the
//! root's registration of the author) must grant `aster.identity-publisher`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

use super::jws::validate_unique_json;
use super::provider_token::{parse_jwk_set, JwkSetError, ProviderKey, MAX_CLAIM_STRING_BYTES};
use super::MAX_CHAIN_BYTES;

/// Where key-set records live in the fleet namespace.
pub const ISSUER_KEYS_PREFIX: &str = "aster/identity/v1/issuers/";
/// Largest record.
pub const MAX_KEY_SET_RECORD_BYTES: usize = 64 * 1024;
/// Longest `source_url`.
pub const MAX_SOURCE_URL_BYTES: usize = 2048;
pub const KEY_SET_RECORD_VERSION: u32 = 1;

/// The entry key of `iss`'s key set.
pub fn issuer_keys_key(iss: &str) -> Vec<u8> {
    format!("{ISSUER_KEYS_PREFIX}{}/jwks", URL_SAFE_NO_PAD.encode(iss)).into_bytes()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    iss: String,
    source_url: String,
    fetched_at: u64,
    jwks: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
}

/// A decoded key-set record. Its authority is not established yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySetRecord {
    pub iss: String,
    pub source_url: String,
    pub fetched_at: u64,
    /// The usable keys, non-empty, unique by `kid`.
    pub keys: Vec<ProviderKey>,
    pub credential: Option<String>,
}

/// Why a key-set record was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySetError {
    /// Larger than [`MAX_KEY_SET_RECORD_BYTES`], or a member above its cap.
    Size,
    /// Not the record's JSON shape, a wrong version, or an empty or
    /// control-character text member.
    Malformed,
    /// The entry key is not [`issuer_keys_key`] of the record's `iss`.
    KeyMismatch,
    /// The JWK set holds no usable signature key.
    NoUsableKeys,
    /// `fetched_at` is later than `now + leeway`.
    Future,
}

impl fmt::Display for KeySetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Size => "too large",
            Self::Malformed => "malformed",
            Self::KeyMismatch => "entry key does not name its issuer",
            Self::NoUsableKeys => "no usable signature key",
            Self::Future => "fetched in the future",
        };
        write!(f, "issuer key set rejected: {reason}")
    }
}

impl std::error::Error for KeySetError {}

fn valid_text(value: &str, max: usize) -> Result<(), KeySetError> {
    if value.len() > max {
        return Err(KeySetError::Size);
    }
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(KeySetError::Malformed);
    }
    Ok(())
}

fn usable(jwks: &Value) -> Result<Vec<ProviderKey>, KeySetError> {
    let keys = parse_jwk_set(jwks).map_err(|error| match error {
        JwkSetError::Malformed => KeySetError::Malformed,
        JwkSetError::TooManyKeys => KeySetError::Size,
    })?;
    if keys.is_empty() {
        return Err(KeySetError::NoUsableKeys);
    }
    Ok(keys)
}

/// Encode a record for `iss` from a JWK set as the provider serves it. Only
/// the usable public keys are kept, in their minimal form, so private or
/// unrelated members never reach the namespace.
pub fn encode_key_set(
    iss: &str,
    source_url: &str,
    fetched_at: u64,
    jwks: &Value,
    credential: Option<&str>,
) -> Result<Vec<u8>, KeySetError> {
    valid_text(iss, MAX_CLAIM_STRING_BYTES)?;
    valid_text(source_url, MAX_SOURCE_URL_BYTES)?;
    if let Some(chain) = credential {
        valid_text(chain, MAX_CHAIN_BYTES)?;
    }
    let keys = usable(jwks)?;
    let record = Record {
        version: KEY_SET_RECORD_VERSION,
        iss: iss.to_string(),
        source_url: source_url.to_string(),
        fetched_at,
        jwks: serde_json::json!({ "keys": keys.iter().map(ProviderKey::to_jwk).collect::<Vec<_>>() }),
        credential: credential.map(str::to_string),
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| KeySetError::Malformed)?;
    if bytes.len() > MAX_KEY_SET_RECORD_BYTES {
        return Err(KeySetError::Size);
    }
    Ok(bytes)
}

/// Decode the record stored at `entry_key`, at `now` (with `leeway` for a
/// publisher clock ahead of ours).
pub fn decode_key_set(
    entry_key: &[u8],
    bytes: &[u8],
    now: u64,
    leeway: u64,
) -> Result<KeySetRecord, KeySetError> {
    if bytes.len() > MAX_KEY_SET_RECORD_BYTES {
        return Err(KeySetError::Size);
    }
    validate_unique_json(bytes).map_err(|_| KeySetError::Malformed)?;
    let record: Record = serde_json::from_slice(bytes).map_err(|_| KeySetError::Malformed)?;
    if record.version != KEY_SET_RECORD_VERSION {
        return Err(KeySetError::Malformed);
    }
    valid_text(&record.iss, MAX_CLAIM_STRING_BYTES)?;
    valid_text(&record.source_url, MAX_SOURCE_URL_BYTES)?;
    if let Some(chain) = &record.credential {
        valid_text(chain, MAX_CHAIN_BYTES)?;
    }
    if issuer_keys_key(&record.iss) != entry_key {
        return Err(KeySetError::KeyMismatch);
    }
    if record.fetched_at > now.saturating_add(leeway) {
        return Err(KeySetError::Future);
    }
    let keys = usable(&record.jwks)?;
    Ok(KeySetRecord {
        iss: record.iss,
        source_url: record.source_url,
        fetched_at: record.fetched_at,
        keys,
        credential: record.credential,
    })
}

#[cfg(test)]
mod tests {
    use super::super::provider_token::test_keys::TestKey;
    use super::*;

    const ISS: &str = "https://accounts.example.com";
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn round_trip_keeps_only_minimal_public_keys() {
        let rsa = TestKey::rsa(false);
        let mut jwk = rsa.jwk("k1", true);
        jwk["x5c"] = serde_json::json!(["MIIB..."]);
        let mut private = TestKey::ec().jwk("secret", true);
        private["d"] = "AAAA".into();
        let set = serde_json::json!({"keys": [jwk, private]});
        let bytes =
            encode_key_set(ISS, "https://accounts.example.com/jwks", NOW, &set, None).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("x5c") && !text.contains("secret"));
        let record = decode_key_set(&issuer_keys_key(ISS), &bytes, NOW, 60).unwrap();
        assert_eq!(record.iss, ISS);
        assert_eq!(record.keys.len(), 1);
        assert_eq!(record.keys[0].kid(), "k1");
        assert_eq!(record.credential, None);
    }

    #[test]
    fn records_fail_closed() {
        let set = serde_json::json!({"keys": [TestKey::ec().jwk("k", true)]});
        let bytes = encode_key_set(ISS, "https://x/jwks", NOW, &set, Some("chain")).unwrap();
        let key = issuer_keys_key(ISS);
        // Another issuer's key, including a prefix of it.
        assert_eq!(
            decode_key_set(
                &issuer_keys_key("https://accounts.example.co"),
                &bytes,
                NOW,
                60
            ),
            Err(KeySetError::KeyMismatch)
        );
        assert_eq!(
            decode_key_set(&key, &bytes, NOW - 61, 60),
            Err(KeySetError::Future)
        );
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["extra"] = 1.into();
        assert_eq!(
            decode_key_set(&key, &serde_json::to_vec(&value).unwrap(), NOW, 60),
            Err(KeySetError::Malformed)
        );
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["jwks"] = serde_json::json!({"keys": []});
        assert_eq!(
            decode_key_set(&key, &serde_json::to_vec(&value).unwrap(), NOW, 60),
            Err(KeySetError::NoUsableKeys)
        );
        assert_eq!(
            decode_key_set(&key, &vec![b' '; MAX_KEY_SET_RECORD_BYTES + 1], NOW, 60),
            Err(KeySetError::Size)
        );
        assert_eq!(
            encode_key_set(
                ISS,
                "https://x",
                NOW,
                &serde_json::json!({"keys": []}),
                None
            ),
            Err(KeySetError::NoUsableKeys)
        );
        assert_eq!(
            encode_key_set("", "https://x", NOW, &set, None),
            Err(KeySetError::Malformed)
        );
        assert_ne!(issuer_keys_key("https://a"), issuer_keys_key("https://a/"));
        assert!(!issuer_keys_key("https://a/tenant").starts_with(&issuer_keys_key("https://a")[..]));
    }
}
