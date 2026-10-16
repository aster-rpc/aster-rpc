//! A software authenticator: what a browser and a passkey produce for
//! `navigator.credentials.create` and `.get`, for tests and for driving the
//! edge without a browser. ES256, attestation `none`. Not a security device.

use aws_lc_rs::digest::{digest, SHA256};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use super::cbor::{self, Value};
use super::{FLAG_AT, FLAG_UP, FLAG_UV};

/// One assertion's parts, as the browser hands them to the page.
#[derive(Debug, Clone)]
pub struct Assertion {
    pub credential_id: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub signature: Vec<u8>,
}

/// A passkey held in memory.
pub struct SoftAuthenticator {
    rp_id: String,
    key: EcdsaKeyPair,
    credential_id: Vec<u8>,
    counter: u32,
    counting: bool,
    user_verified: bool,
}

impl SoftAuthenticator {
    /// A fresh passkey for `rp_id`.
    pub fn new(rp_id: impl Into<String>) -> Self {
        Self {
            rp_id: rp_id.into(),
            key: EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
                .expect("P-256 key generation failed"),
            credential_id: super::new_challenge()[..16].to_vec(),
            counter: 0,
            counting: true,
            user_verified: false,
        }
    }

    pub fn credential_id(&self) -> Vec<u8> {
        self.credential_id.clone()
    }

    /// The public key, SEC1 uncompressed.
    pub fn public_key(&self) -> Vec<u8> {
        self.key.public_key().as_ref().to_vec()
    }

    /// Set the signature counter (e.g. to play a cloned authenticator).
    pub fn set_counter(&mut self, counter: u32) {
        self.counter = counter;
    }

    /// Whether assertions advance the counter; `false` plays an
    /// authenticator that always reports 0.
    pub fn set_counting(&mut self, counting: bool) {
        self.counting = counting;
        if !counting {
            self.counter = 0;
        }
    }

    /// Whether ceremonies report the user verified.
    pub fn set_user_verified(&mut self, verified: bool) {
        self.user_verified = verified;
    }

    fn flags(&self) -> u8 {
        FLAG_UP | if self.user_verified { FLAG_UV } else { 0 }
    }

    fn client_data(ceremony: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": ceremony,
            "challenge": URL_SAFE_NO_PAD.encode(challenge),
            "origin": origin,
            "crossOrigin": false,
        }))
        .unwrap()
    }

    fn cose_key(&self) -> Vec<(Value, Value)> {
        let point = self.public_key();
        vec![
            (Value::Uint(1), Value::Uint(2)),
            (Value::Uint(3), Value::Neg(super::COSE_ES256)),
            (Value::Neg(-1), Value::Uint(1)),
            (Value::Neg(-2), Value::Bytes(point[1..33].to_vec())),
            (Value::Neg(-3), Value::Bytes(point[33..65].to_vec())),
        ]
    }

    fn auth_data_head(&self, flags: u8) -> Vec<u8> {
        let mut data = digest(&SHA256, self.rp_id.as_bytes()).as_ref().to_vec();
        data.push(flags);
        data.extend_from_slice(&self.counter.to_be_bytes());
        data
    }

    fn attestation_object(&self, key: Vec<(Value, Value)>) -> Vec<(Value, Value)> {
        let mut auth_data = self.auth_data_head(self.flags() | FLAG_AT);
        auth_data.extend_from_slice(&[0u8; 16]); // aaguid
        auth_data.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
        auth_data.extend_from_slice(&self.credential_id);
        auth_data.extend_from_slice(&cbor::encode(&Value::Map(key)));
        vec![
            (Value::Text("fmt".into()), Value::Text("none".into())),
            (Value::Text("attStmt".into()), Value::Map(Vec::new())),
            (Value::Text("authData".into()), Value::Bytes(auth_data)),
        ]
    }

    /// `navigator.credentials.create`: `(clientDataJSON, attestationObject)`.
    pub fn register(&mut self, challenge: &[u8], origin: &str) -> (Vec<u8>, Vec<u8>) {
        let object = self.attestation_object(self.cose_key());
        (
            Self::client_data("webauthn.create", challenge, origin),
            cbor::encode(&Value::Map(object)),
        )
    }

    /// The attestation object with its top-level entries changed by `change`.
    #[cfg(test)]
    pub(crate) fn attestation_object_with(
        &self,
        change: impl FnOnce(&mut Vec<(Value, Value)>),
    ) -> Vec<u8> {
        let mut object = self.attestation_object(self.cose_key());
        change(&mut object);
        cbor::encode(&Value::Map(object))
    }

    /// The attestation object with its COSE key entries changed by `change`.
    #[cfg(test)]
    pub(crate) fn attestation_object_with_key(
        &self,
        change: impl FnOnce(&mut Vec<(Value, Value)>),
    ) -> Vec<u8> {
        let mut key = self.cose_key();
        change(&mut key);
        cbor::encode(&Value::Map(self.attestation_object(key)))
    }

    /// `navigator.credentials.get` for `challenge` from a page at `origin`.
    pub fn assert(&mut self, challenge: &[u8], origin: &str) -> Assertion {
        self.assert_client_data(&Self::client_data("webauthn.get", challenge, origin))
    }

    /// An assertion over a given clientDataJSON.
    pub fn assert_client_data(&mut self, client_data_json: &[u8]) -> Assertion {
        if self.counting {
            self.counter += 1;
        }
        let authenticator_data = self.auth_data_head(self.flags());
        let mut signed = authenticator_data.clone();
        signed.extend_from_slice(digest(&SHA256, client_data_json).as_ref());
        let signature = self
            .key
            .sign(&SystemRandom::new(), &signed)
            .expect("signing failed")
            .as_ref()
            .to_vec();
        Assertion {
            credential_id: self.credential_id.clone(),
            client_data_json: client_data_json.to_vec(),
            authenticator_data,
            signature,
        }
    }
}
