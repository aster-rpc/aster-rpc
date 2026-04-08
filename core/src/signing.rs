//! Canonical signing bytes construction and ed25519 verification.
//!
//! Translates the canonical signing logic from `bindings/python/aster/trust/signing.py`.
//!
//! Spec reference: Aster-trust-spec.md sections 2.2 and 2.4.
//!
//! ## Canonical signing bytes formats
//!
//! **EnrollmentCredential (producer):**
//! ```text
//! endpoint_id.as_bytes()        // variable length
//! || root_pubkey                // 32 bytes
//! || u64_be(expires_at)         // 8 bytes
//! || canonical_json(attributes) // UTF-8, sorted keys
//! ```
//!
//! **ConsumerEnrollmentCredential:**
//! ```text
//! u8(type_code)                 // 0x00 = policy, 0x01 = ott
//! || u8(has_endpoint_id)        // 0x00 or 0x01
//! || endpoint_id.as_bytes()?    // present only if has_endpoint_id == 0x01
//! || root_pubkey                // 32 bytes
//! || u64_be(expires_at)         // 8 bytes
//! || canonical_json(attributes) // UTF-8, sorted keys
//! || u8(has_nonce)              // 0x00 or 0x01
//! || nonce?                     // present only if has_nonce == 0x01 (32 bytes)
//! ```

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

/// Credential data for a producer enrollment.
#[derive(Debug, Clone, Deserialize)]
pub struct EnrollmentCredentialData {
    pub endpoint_id: String,
    /// Hex-encoded 32-byte root public key.
    pub root_pubkey: String,
    pub expires_at: u64,
    pub attributes: BTreeMap<String, String>,
}

/// Credential data for a consumer enrollment.
#[derive(Debug, Clone, Deserialize)]
pub struct ConsumerEnrollmentCredentialData {
    /// `"policy"` or `"ott"`.
    pub credential_type: String,
    /// Hex-encoded 32-byte root public key.
    pub root_pubkey: String,
    pub expires_at: u64,
    pub attributes: BTreeMap<String, String>,
    pub endpoint_id: Option<String>,
    /// Hex-encoded 32-byte nonce (for OTT credentials).
    pub nonce: Option<String>,
}

/// Combined credential for JSON dispatch.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum CredentialData {
    #[serde(rename = "producer")]
    Producer(EnrollmentCredentialData),
    #[serde(rename = "consumer")]
    Consumer(ConsumerEnrollmentCredentialData),
}

/// Encode attributes as canonical JSON: UTF-8, sorted keys, no extra whitespace.
///
/// `BTreeMap` guarantees sorted key order. `serde_json::to_string` produces
/// compact output (`{"key":"value"}`) matching the Python equivalent
/// `json.dumps(attrs, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`.
pub fn canonical_json(attributes: &BTreeMap<String, String>) -> Vec<u8> {
    // serde_json::to_string on a BTreeMap produces sorted, compact JSON.
    serde_json::to_string(attributes)
        .expect("BTreeMap<String, String> serialization cannot fail")
        .into_bytes()
}

/// Produce the canonical signing bytes for a producer enrollment credential.
///
/// Format: `endpoint_id || root_pubkey_bytes(32) || u64_be(expires_at) || canonical_json(attributes)`
pub fn producer_signing_bytes(cred: &EnrollmentCredentialData) -> Result<Vec<u8>> {
    let root_pubkey_bytes =
        hex::decode(&cred.root_pubkey).map_err(|e| anyhow!("invalid hex in root_pubkey: {e}"))?;
    if root_pubkey_bytes.len() != 32 {
        bail!(
            "root_pubkey must be 32 bytes, got {}",
            root_pubkey_bytes.len()
        );
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(cred.endpoint_id.as_bytes());
    buf.extend_from_slice(&root_pubkey_bytes);
    buf.extend_from_slice(&cred.expires_at.to_be_bytes());
    buf.extend_from_slice(&canonical_json(&cred.attributes));
    Ok(buf)
}

/// Produce the canonical signing bytes for a consumer enrollment credential.
///
/// Format:
/// ```text
/// type_code(1) || has_endpoint_id(1) || endpoint_id? || root_pubkey(32)
/// || u64_be(expires_at)(8) || canonical_json(attributes) || has_nonce(1) || nonce?(32)
/// ```
pub fn consumer_signing_bytes(cred: &ConsumerEnrollmentCredentialData) -> Result<Vec<u8>> {
    let type_code: u8 = match cred.credential_type.as_str() {
        "policy" => 0x00,
        "ott" => 0x01,
        other => bail!("unknown credential_type: {other}"),
    };

    let root_pubkey_bytes =
        hex::decode(&cred.root_pubkey).map_err(|e| anyhow!("invalid hex in root_pubkey: {e}"))?;
    if root_pubkey_bytes.len() != 32 {
        bail!(
            "root_pubkey must be 32 bytes, got {}",
            root_pubkey_bytes.len()
        );
    }

    let mut buf = Vec::new();

    // type code
    buf.push(type_code);

    // optional endpoint_id
    match &cred.endpoint_id {
        Some(eid) => {
            buf.push(0x01);
            buf.extend_from_slice(eid.as_bytes());
        }
        None => {
            buf.push(0x00);
        }
    }

    // root pubkey + expires_at + attributes
    buf.extend_from_slice(&root_pubkey_bytes);
    buf.extend_from_slice(&cred.expires_at.to_be_bytes());
    buf.extend_from_slice(&canonical_json(&cred.attributes));

    // optional nonce
    match &cred.nonce {
        Some(nonce_hex) => {
            let nonce_bytes =
                hex::decode(nonce_hex).map_err(|e| anyhow!("invalid hex in nonce: {e}"))?;
            if nonce_bytes.len() != 32 {
                bail!("nonce must be 32 bytes, got {}", nonce_bytes.len());
            }
            buf.push(0x01);
            buf.extend_from_slice(&nonce_bytes);
        }
        None => {
            buf.push(0x00);
        }
    }

    Ok(buf)
}

/// Deserialize a JSON string as a [`CredentialData`] and dispatch to the
/// appropriate signing bytes function.
pub fn canonical_signing_bytes_from_json(json_str: &str) -> Result<Vec<u8>> {
    let cred: CredentialData = serde_json::from_str(json_str)
        .map_err(|e| anyhow!("failed to parse credential JSON: {e}"))?;
    match cred {
        CredentialData::Producer(ref p) => producer_signing_bytes(p),
        CredentialData::Consumer(ref c) => consumer_signing_bytes(c),
    }
}

/// Verify an ed25519 signature.
///
/// - `public_key`: 32-byte ed25519 public key.
/// - `message`: the message bytes that were signed.
/// - `signature`: 64-byte ed25519 signature.
///
/// Returns `Ok(true)` if verification succeeds, `Ok(false)` if the signature
/// is invalid, and `Err` for malformed inputs.
pub fn ed25519_verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<bool> {
    let vk = VerifyingKey::from_bytes(
        public_key
            .try_into()
            .map_err(|_| anyhow!("public_key must be 32 bytes, got {}", public_key.len()))?,
    )
    .map_err(|e| anyhow!("invalid ed25519 public key: {e}"))?;

    let sig = Signature::from_bytes(
        signature
            .try_into()
            .map_err(|_| anyhow!("signature must be 64 bytes, got {}", signature.len()))?,
    );

    match vk.verify(message, &sig) {
        Ok(()) => Ok(true),
        Err(ref e) if e.to_string().contains("invalid") || e.to_string().contains("signature") => {
            Ok(false)
        }
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn test_canonical_json_sorted() {
        // Even though BTreeMap is always sorted, verify output matches expected JSON.
        let mut attrs = BTreeMap::new();
        attrs.insert("b".to_string(), "2".to_string());
        attrs.insert("a".to_string(), "1".to_string());
        let result = canonical_json(&attrs);
        assert_eq!(result, b"{\"a\":\"1\",\"b\":\"2\"}");
    }

    #[test]
    fn test_canonical_json_empty() {
        let attrs = BTreeMap::new();
        assert_eq!(canonical_json(&attrs), b"{}");
    }

    #[test]
    fn test_producer_signing_bytes() {
        let root_pubkey_bytes = [0xABu8; 32];
        let root_pubkey_hex = hex::encode(root_pubkey_bytes);

        let cred = EnrollmentCredentialData {
            endpoint_id: "node1".to_string(),
            root_pubkey: root_pubkey_hex,
            expires_at: 1000,
            attributes: BTreeMap::new(),
        };

        let result = producer_signing_bytes(&cred).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(b"node1");
        expected.extend_from_slice(&[0xAB; 32]);
        expected.extend_from_slice(&1000u64.to_be_bytes());
        expected.extend_from_slice(b"{}");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_producer_signing_bytes_with_attributes() {
        let root_pubkey_bytes = [0x01u8; 32];
        let root_pubkey_hex = hex::encode(root_pubkey_bytes);

        let mut attrs = BTreeMap::new();
        attrs.insert("role".to_string(), "admin".to_string());
        attrs.insert("name".to_string(), "alice".to_string());

        let cred = EnrollmentCredentialData {
            endpoint_id: "ep42".to_string(),
            root_pubkey: root_pubkey_hex,
            expires_at: 9999,
            attributes: attrs,
        };

        let result = producer_signing_bytes(&cred).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(b"ep42");
        expected.extend_from_slice(&[0x01; 32]);
        expected.extend_from_slice(&9999u64.to_be_bytes());
        expected.extend_from_slice(b"{\"name\":\"alice\",\"role\":\"admin\"}");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_consumer_signing_bytes_policy_no_endpoint_no_nonce() {
        let root_pubkey_bytes = [0xCCu8; 32];
        let root_pubkey_hex = hex::encode(root_pubkey_bytes);

        let cred = ConsumerEnrollmentCredentialData {
            credential_type: "policy".to_string(),
            root_pubkey: root_pubkey_hex,
            expires_at: 500,
            attributes: BTreeMap::new(),
            endpoint_id: None,
            nonce: None,
        };

        let result = consumer_signing_bytes(&cred).unwrap();

        let mut expected = Vec::new();
        expected.push(0x00); // type_code = policy
        expected.push(0x00); // has_endpoint_id = false
        expected.extend_from_slice(&[0xCC; 32]);
        expected.extend_from_slice(&500u64.to_be_bytes());
        expected.extend_from_slice(b"{}");
        expected.push(0x00); // has_nonce = false
        assert_eq!(result, expected);
    }

    #[test]
    fn test_consumer_signing_bytes_ott_with_nonce() {
        let root_pubkey_bytes = [0xDDu8; 32];
        let root_pubkey_hex = hex::encode(root_pubkey_bytes);
        let nonce_bytes = [0xEEu8; 32];
        let nonce_hex = hex::encode(nonce_bytes);

        let cred = ConsumerEnrollmentCredentialData {
            credential_type: "ott".to_string(),
            root_pubkey: root_pubkey_hex,
            expires_at: 2000,
            attributes: BTreeMap::new(),
            endpoint_id: Some("consumer-ep".to_string()),
            nonce: Some(nonce_hex),
        };

        let result = consumer_signing_bytes(&cred).unwrap();

        let mut expected = Vec::new();
        expected.push(0x01); // type_code = ott
        expected.push(0x01); // has_endpoint_id = true
        expected.extend_from_slice(b"consumer-ep");
        expected.extend_from_slice(&[0xDD; 32]);
        expected.extend_from_slice(&2000u64.to_be_bytes());
        expected.extend_from_slice(b"{}");
        expected.push(0x01); // has_nonce = true
        expected.extend_from_slice(&[0xEE; 32]);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_ed25519_verify_valid() {
        use ed25519_dalek::Signer;

        let mut rng = rand_core_06::OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let verifying_key = signing_key.verifying_key();

        let message = b"hello world";
        let signature = signing_key.sign(message);

        let result =
            ed25519_verify(verifying_key.as_bytes(), message, &signature.to_bytes()).unwrap();
        assert!(result, "valid signature should verify");
    }

    #[test]
    fn test_ed25519_verify_tampered() {
        use ed25519_dalek::Signer;

        let mut rng = rand_core_06::OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let verifying_key = signing_key.verifying_key();

        let message = b"hello world";
        let signature = signing_key.sign(message);

        // Tamper with the message.
        let result =
            ed25519_verify(verifying_key.as_bytes(), b"tampered", &signature.to_bytes()).unwrap();
        assert!(!result, "tampered message should not verify");
    }

    #[test]
    fn test_ed25519_verify_wrong_key() {
        use ed25519_dalek::Signer;

        let mut rng = rand_core_06::OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let other_key = SigningKey::generate(&mut rng);

        let message = b"hello world";
        let signature = signing_key.sign(message);

        let result = ed25519_verify(
            other_key.verifying_key().as_bytes(),
            message,
            &signature.to_bytes(),
        )
        .unwrap();
        assert!(!result, "wrong key should not verify");
    }

    #[test]
    fn test_ed25519_verify_bad_key_length() {
        let result = ed25519_verify(&[0u8; 16], b"msg", &[0u8; 64]);
        assert!(result.is_err());
    }

    #[test]
    fn test_ed25519_verify_bad_signature_length() {
        let result = ed25519_verify(&[0u8; 32], b"msg", &[0u8; 32]);
        assert!(result.is_err());
    }

    #[test]
    fn test_canonical_signing_bytes_from_json_producer() {
        let root_pubkey_hex = hex::encode([0xAAu8; 32]);
        let json = format!(
            r#"{{"kind":"producer","endpoint_id":"ep1","root_pubkey":"{}","expires_at":100,"attributes":{{}}}}"#,
            root_pubkey_hex
        );

        let result = canonical_signing_bytes_from_json(&json).unwrap();

        // Should match producer_signing_bytes output.
        let cred = EnrollmentCredentialData {
            endpoint_id: "ep1".to_string(),
            root_pubkey: root_pubkey_hex,
            expires_at: 100,
            attributes: BTreeMap::new(),
        };
        let expected = producer_signing_bytes(&cred).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_canonical_signing_bytes_from_json_consumer() {
        let root_pubkey_hex = hex::encode([0xBBu8; 32]);
        let nonce_hex = hex::encode([0xFFu8; 32]);
        let json = format!(
            r#"{{"kind":"consumer","credential_type":"ott","root_pubkey":"{}","expires_at":200,"attributes":{{}},"endpoint_id":"c-ep","nonce":"{}"}}"#,
            root_pubkey_hex, nonce_hex
        );

        let result = canonical_signing_bytes_from_json(&json).unwrap();

        let cred = ConsumerEnrollmentCredentialData {
            credential_type: "ott".to_string(),
            root_pubkey: root_pubkey_hex,
            expires_at: 200,
            attributes: BTreeMap::new(),
            endpoint_id: Some("c-ep".to_string()),
            nonce: Some(nonce_hex),
        };
        let expected = consumer_signing_bytes(&cred).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_invalid_root_pubkey_length() {
        let cred = EnrollmentCredentialData {
            endpoint_id: "ep".to_string(),
            root_pubkey: hex::encode([0u8; 16]), // too short
            expires_at: 0,
            attributes: BTreeMap::new(),
        };
        assert!(producer_signing_bytes(&cred).is_err());
    }

    #[test]
    fn test_invalid_credential_type() {
        let cred = ConsumerEnrollmentCredentialData {
            credential_type: "unknown".to_string(),
            root_pubkey: hex::encode([0u8; 32]),
            expires_at: 0,
            attributes: BTreeMap::new(),
            endpoint_id: None,
            nonce: None,
        };
        assert!(consumer_signing_bytes(&cred).is_err());
    }

    #[test]
    fn test_invalid_nonce_length() {
        let cred = ConsumerEnrollmentCredentialData {
            credential_type: "ott".to_string(),
            root_pubkey: hex::encode([0u8; 32]),
            expires_at: 0,
            attributes: BTreeMap::new(),
            endpoint_id: None,
            nonce: Some(hex::encode([0u8; 16])), // too short
        };
        assert!(consumer_signing_bytes(&cred).is_err());
    }
}
