//! A minimal WebAuthn relying party: passkey registration and assertion
//! verification for ES256 (ECDSA P-256 with SHA-256) only, over `aws-lc-rs`.
//!
//! What is checked ([WebAuthn Level 3 §7](https://www.w3.org/TR/webauthn-3/#sctn-rp-operations)):
//!
//! - `clientDataJSON`: `type` (`webauthn.create` / `webauthn.get`), the
//!   `challenge` this relying party issued (single use is the caller's job),
//!   the exact `origin`, and `crossOrigin` not `true`;
//! - authenticator data: `rpIdHash` = SHA-256 of the RP ID, the user-present
//!   flag, the user-verified flag when [`RelyingParty::require_user_verification`]
//!   is set, and no attested credential data in an assertion;
//! - registration: attestation format `none` only (the edge asks for none and
//!   trusts no authenticator vendor), an EC2 / P-256 / ES256 COSE key;
//! - assertion: the signature over `authenticatorData ‖ SHA-256(clientDataJSON)`
//!   with the registered key, and the signature counter: when either the
//!   stored or the new count is non-zero, the new one must be larger, or the
//!   assertion is refused as a possible cloned authenticator.
//!
//! Extensions are tolerated in authenticator data and never interpreted.

mod cbor;
pub mod soft;

use aws_lc_rs::digest::{digest, SHA256};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use aws_lc_rs::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_ASN1};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// Authenticator-data flag: the user was present.
pub const FLAG_UP: u8 = 0x01;
/// Authenticator-data flag: the user was verified (PIN, biometric).
pub const FLAG_UV: u8 = 0x04;
/// Authenticator-data flag: attested credential data follows.
pub const FLAG_AT: u8 = 0x40;
/// Authenticator-data flag: extensions follow.
pub const FLAG_ED: u8 = 0x80;
/// The COSE algorithm id of ES256.
pub const COSE_ES256: i128 = -7;

/// Largest credential id accepted (the spec's maximum).
const MAX_CREDENTIAL_ID: usize = 1023;
/// Largest clientDataJSON accepted.
const MAX_CLIENT_DATA: usize = 4096;

/// Why a ceremony was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebAuthnError {
    /// clientDataJSON, authenticator data or the attestation object does not
    /// parse.
    Malformed,
    /// clientDataJSON `type` is not the ceremony's.
    WrongType,
    /// clientDataJSON `challenge` is not the one issued.
    WrongChallenge,
    /// clientDataJSON `origin` is not the relying party's, or the call was
    /// cross-origin.
    WrongOrigin,
    /// `rpIdHash` is not SHA-256 of the RP ID.
    WrongRpId,
    /// The user-present flag is clear.
    UserNotPresent,
    /// User verification is required and the flag is clear.
    UserNotVerified,
    /// An attestation format other than `none`.
    UnsupportedAttestation,
    /// A credential key that is not EC2 / P-256 / ES256.
    UnsupportedKey,
    /// The assertion is for another credential.
    WrongCredential,
    /// The signature does not verify.
    BadSignature,
    /// The signature counter did not increase.
    CounterNotIncreased,
}

impl std::fmt::Display for WebAuthnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::Malformed => "malformed",
            Self::WrongType => "wrong ceremony type",
            Self::WrongChallenge => "wrong challenge",
            Self::WrongOrigin => "wrong origin",
            Self::WrongRpId => "wrong RP ID",
            Self::UserNotPresent => "user not present",
            Self::UserNotVerified => "user not verified",
            Self::UnsupportedAttestation => "unsupported attestation format",
            Self::UnsupportedKey => "unsupported credential key",
            Self::WrongCredential => "wrong credential",
            Self::BadSignature => "bad signature",
            Self::CounterNotIncreased => "signature counter did not increase",
        };
        write!(f, "WebAuthn: {reason}")
    }
}

impl std::error::Error for WebAuthnError {}

/// A registered passkey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    /// The credential id the authenticator chose.
    pub id: Vec<u8>,
    /// The P-256 public key, SEC1 uncompressed (`0x04 ‖ x ‖ y`).
    pub public_key: Vec<u8>,
    /// The last signature counter seen.
    pub sign_count: u32,
}

/// A relying party: the RP ID (a registrable domain, e.g. `localhost`) and
/// the exact origin pages are served from (e.g. `http://localhost:8080`).
#[derive(Debug, Clone)]
pub struct RelyingParty {
    pub rp_id: String,
    pub origin: String,
    /// Refuse ceremonies without the user-verified flag.
    pub require_user_verification: bool,
}

/// A fresh 32-byte challenge.
pub fn new_challenge() -> [u8; 32] {
    let mut challenge = [0u8; 32];
    SystemRandom::new()
        .fill(&mut challenge)
        .expect("the system random generator failed");
    challenge
}

/// Parsed authenticator data.
struct AuthData<'a> {
    rp_id_hash: &'a [u8],
    flags: u8,
    sign_count: u32,
    /// The bytes after the counter (attested credential data, extensions).
    rest: &'a [u8],
}

fn parse_auth_data(bytes: &[u8]) -> Result<AuthData<'_>, WebAuthnError> {
    if bytes.len() < 37 {
        return Err(WebAuthnError::Malformed);
    }
    Ok(AuthData {
        rp_id_hash: &bytes[..32],
        flags: bytes[32],
        sign_count: u32::from_be_bytes(bytes[33..37].try_into().unwrap()),
        rest: &bytes[37..],
    })
}

impl RelyingParty {
    pub fn new(rp_id: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            rp_id: rp_id.into(),
            origin: origin.into(),
            require_user_verification: false,
        }
    }

    pub fn require_user_verification(mut self, required: bool) -> Self {
        self.require_user_verification = required;
        self
    }

    fn check_client_data(
        &self,
        client_data_json: &[u8],
        ceremony: &str,
        challenge: &[u8],
    ) -> Result<(), WebAuthnError> {
        if client_data_json.len() > MAX_CLIENT_DATA {
            return Err(WebAuthnError::Malformed);
        }
        let data: serde_json::Value =
            serde_json::from_slice(client_data_json).map_err(|_| WebAuthnError::Malformed)?;
        let text = |key: &str| data.get(key).and_then(serde_json::Value::as_str);
        if text("type") != Some(ceremony) {
            return Err(WebAuthnError::WrongType);
        }
        let sent = text("challenge")
            .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
            .ok_or(WebAuthnError::WrongChallenge)?;
        if sent != challenge {
            return Err(WebAuthnError::WrongChallenge);
        }
        if text("origin") != Some(self.origin.as_str()) {
            return Err(WebAuthnError::WrongOrigin);
        }
        if data.get("crossOrigin").and_then(serde_json::Value::as_bool) == Some(true) {
            return Err(WebAuthnError::WrongOrigin);
        }
        Ok(())
    }

    fn check_auth_data(&self, data: &AuthData<'_>) -> Result<(), WebAuthnError> {
        if data.rp_id_hash != digest(&SHA256, self.rp_id.as_bytes()).as_ref() {
            return Err(WebAuthnError::WrongRpId);
        }
        if data.flags & FLAG_UP == 0 {
            return Err(WebAuthnError::UserNotPresent);
        }
        if self.require_user_verification && data.flags & FLAG_UV == 0 {
            return Err(WebAuthnError::UserNotVerified);
        }
        Ok(())
    }

    /// Verify a registration (`navigator.credentials.create`) answering
    /// `challenge`, and return the new credential.
    pub fn verify_registration(
        &self,
        challenge: &[u8],
        client_data_json: &[u8],
        attestation_object: &[u8],
    ) -> Result<Credential, WebAuthnError> {
        self.check_client_data(client_data_json, "webauthn.create", challenge)?;
        let object = cbor::decode_all(attestation_object).map_err(|_| WebAuthnError::Malformed)?;
        let fmt = object
            .get_text("fmt")
            .and_then(cbor::Value::as_text)
            .ok_or(WebAuthnError::Malformed)?;
        if fmt != "none" {
            return Err(WebAuthnError::UnsupportedAttestation);
        }
        match object.get_text("attStmt") {
            Some(cbor::Value::Map(entries)) if entries.is_empty() => {}
            _ => return Err(WebAuthnError::Malformed),
        }
        let auth_data = object
            .get_text("authData")
            .and_then(cbor::Value::as_bytes)
            .ok_or(WebAuthnError::Malformed)?;
        let data = parse_auth_data(auth_data)?;
        self.check_auth_data(&data)?;
        if data.flags & FLAG_AT == 0 {
            return Err(WebAuthnError::Malformed);
        }
        // aaguid (16) ‖ credentialIdLength (2) ‖ credentialId ‖ COSE key.
        let rest = data.rest;
        if rest.len() < 18 {
            return Err(WebAuthnError::Malformed);
        }
        let id_len = usize::from(u16::from_be_bytes([rest[16], rest[17]]));
        if id_len == 0 || id_len > MAX_CREDENTIAL_ID || rest.len() < 18 + id_len {
            return Err(WebAuthnError::Malformed);
        }
        let id = rest[18..18 + id_len].to_vec();
        let (key, used) =
            cbor::decode(&rest[18 + id_len..]).map_err(|_| WebAuthnError::Malformed)?;
        let after = &rest[18 + id_len + used..];
        if data.flags & FLAG_ED == 0 && !after.is_empty() {
            return Err(WebAuthnError::Malformed);
        }
        if data.flags & FLAG_ED != 0 {
            cbor::decode_all(after).map_err(|_| WebAuthnError::Malformed)?;
        }
        Ok(Credential {
            id,
            public_key: es256_public_key(&key)?,
            sign_count: data.sign_count,
        })
    }

    /// Verify an assertion (`navigator.credentials.get`) answering
    /// `challenge` with `credential`, and return the new signature counter to
    /// store.
    pub fn verify_assertion(
        &self,
        challenge: &[u8],
        credential: &Credential,
        credential_id: &[u8],
        client_data_json: &[u8],
        authenticator_data: &[u8],
        signature: &[u8],
    ) -> Result<u32, WebAuthnError> {
        if credential_id != credential.id {
            return Err(WebAuthnError::WrongCredential);
        }
        self.check_client_data(client_data_json, "webauthn.get", challenge)?;
        let data = parse_auth_data(authenticator_data)?;
        self.check_auth_data(&data)?;
        if data.flags & FLAG_AT != 0 {
            return Err(WebAuthnError::Malformed);
        }
        match (data.flags & FLAG_ED != 0, data.rest.is_empty()) {
            (false, true) => {}
            (true, false) => {
                cbor::decode_all(data.rest).map_err(|_| WebAuthnError::Malformed)?;
            }
            _ => return Err(WebAuthnError::Malformed),
        }
        let mut signed = authenticator_data.to_vec();
        signed.extend_from_slice(digest(&SHA256, client_data_json).as_ref());
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &credential.public_key)
            .verify(&signed, signature)
            .map_err(|_| WebAuthnError::BadSignature)?;
        let (stored, new) = (credential.sign_count, data.sign_count);
        if (stored != 0 || new != 0) && new <= stored {
            return Err(WebAuthnError::CounterNotIncreased);
        }
        Ok(new)
    }
}

/// The SEC1 point of an EC2 / P-256 / ES256 COSE key.
fn es256_public_key(key: &cbor::Value) -> Result<Vec<u8>, WebAuthnError> {
    let int = |label: i128| key.get_int(label).and_then(cbor::Value::as_int);
    // kty 2 = EC2, alg -7 = ES256, crv 1 = P-256.
    if int(1) != Some(2) || int(3) != Some(COSE_ES256) || int(-1) != Some(1) {
        return Err(WebAuthnError::UnsupportedKey);
    }
    let coordinate = |label: i128| {
        key.get_int(label)
            .and_then(cbor::Value::as_bytes)
            .filter(|bytes| bytes.len() == 32)
            .ok_or(WebAuthnError::UnsupportedKey)
    };
    let mut point = vec![0x04];
    point.extend_from_slice(coordinate(-2)?);
    point.extend_from_slice(coordinate(-3)?);
    Ok(point)
}

#[cfg(test)]
mod tests {
    use super::soft::SoftAuthenticator;
    use super::*;

    const ORIGIN: &str = "http://localhost:8080";

    fn rp() -> RelyingParty {
        RelyingParty::new("localhost", ORIGIN)
    }

    /// A registered authenticator and its credential.
    fn registered() -> (SoftAuthenticator, Credential) {
        let mut authenticator = SoftAuthenticator::new("localhost");
        let challenge = new_challenge();
        let (client_data, attestation) = authenticator.register(&challenge, ORIGIN);
        let credential = rp()
            .verify_registration(&challenge, &client_data, &attestation)
            .unwrap();
        (authenticator, credential)
    }

    /// Verify a fresh assertion after `tamper` changed its parts.
    fn assert_with(
        tamper: impl FnOnce(&mut soft::Assertion, &mut [u8; 32]),
    ) -> Result<u32, WebAuthnError> {
        let (mut authenticator, credential) = registered();
        let mut challenge = new_challenge();
        let mut assertion = authenticator.assert(&challenge, ORIGIN);
        tamper(&mut assertion, &mut challenge);
        rp().verify_assertion(
            &challenge,
            &credential,
            &assertion.credential_id,
            &assertion.client_data_json,
            &assertion.authenticator_data,
            &assertion.signature,
        )
    }

    #[test]
    fn registration_yields_the_authenticators_key() {
        let (authenticator, credential) = registered();
        assert_eq!(credential.id, authenticator.credential_id());
        assert_eq!(credential.public_key, authenticator.public_key());
        assert_eq!(credential.sign_count, 0);
    }

    #[test]
    fn a_valid_assertion_verifies_and_advances_the_counter() {
        let (mut authenticator, mut credential) = registered();
        for expected in 1..=3 {
            let challenge = new_challenge();
            let a = authenticator.assert(&challenge, ORIGIN);
            let count = rp()
                .verify_assertion(
                    &challenge,
                    &credential,
                    &a.credential_id,
                    &a.client_data_json,
                    &a.authenticator_data,
                    &a.signature,
                )
                .unwrap();
            assert_eq!(count, expected);
            credential.sign_count = count;
        }
    }

    #[test]
    fn a_tampered_assertion_is_refused() {
        use WebAuthnError::*;
        assert_eq!(assert_with(|_, _| {}), Ok(1));
        assert_eq!(
            assert_with(|a, _| {
                let last = a.signature.len() - 1;
                a.signature[last] ^= 1;
            }),
            Err(BadSignature)
        );
        assert_eq!(
            assert_with(|a, _| a.authenticator_data[36] ^= 0x10),
            Err(BadSignature),
            "the counter is signed"
        );
        assert_eq!(
            assert_with(|_, challenge| challenge[0] ^= 1),
            Err(WrongChallenge)
        );
        assert_eq!(
            assert_with(|a, _| a.credential_id[0] ^= 1),
            Err(WrongCredential)
        );
        assert_eq!(
            assert_with(|a, _| a.authenticator_data[0] ^= 1),
            Err(WrongRpId)
        );
        assert_eq!(
            assert_with(|a, _| a.authenticator_data[32] &= !FLAG_UP),
            Err(UserNotPresent)
        );
        assert_eq!(
            assert_with(|a, _| a.authenticator_data.truncate(36)),
            Err(Malformed)
        );
        assert_eq!(
            assert_with(|a, _| a.authenticator_data.push(0)),
            Err(Malformed),
            "trailing bytes without the extensions flag"
        );
    }

    #[test]
    fn client_data_must_name_the_ceremony_challenge_and_origin() {
        use WebAuthnError::*;
        let (mut authenticator, credential) = registered();
        let verify = |authenticator: &mut SoftAuthenticator, json: serde_json::Value| {
            let challenge = new_challenge();
            let mut json = json;
            if json["challenge"].is_null() {
                json["challenge"] = URL_SAFE_NO_PAD.encode(challenge).into();
            }
            let a = authenticator.assert_client_data(&serde_json::to_vec(&json).unwrap());
            rp().verify_assertion(
                &challenge,
                &credential,
                &a.credential_id,
                &a.client_data_json,
                &a.authenticator_data,
                &a.signature,
            )
        };
        let ok = serde_json::json!({"type": "webauthn.get", "origin": ORIGIN});
        assert!(verify(&mut authenticator, ok.clone()).is_ok());
        let mut wrong = ok.clone();
        wrong["origin"] = "http://evil.example".into();
        assert_eq!(verify(&mut authenticator, wrong), Err(WrongOrigin));
        let mut wrong = ok.clone();
        wrong["origin"] = "http://localhost:8081".into();
        assert_eq!(verify(&mut authenticator, wrong), Err(WrongOrigin));
        let mut wrong = ok.clone();
        wrong["crossOrigin"] = true.into();
        assert_eq!(verify(&mut authenticator, wrong), Err(WrongOrigin));
        let mut wrong = ok.clone();
        wrong["type"] = "webauthn.create".into();
        assert_eq!(verify(&mut authenticator, wrong), Err(WrongType));
        let mut wrong = ok.clone();
        wrong["challenge"] = "AAAA".into();
        assert_eq!(verify(&mut authenticator, wrong), Err(WrongChallenge));
    }

    #[test]
    fn the_counter_must_increase() {
        let (mut authenticator, mut credential) = registered();
        let challenge = new_challenge();
        let a = authenticator.assert(&challenge, ORIGIN);
        credential.sign_count = rp()
            .verify_assertion(
                &challenge,
                &credential,
                &a.credential_id,
                &a.client_data_json,
                &a.authenticator_data,
                &a.signature,
            )
            .unwrap();
        // A clone of the authenticator, still at the old count.
        authenticator.set_counter(0);
        let challenge = new_challenge();
        let a = authenticator.assert(&challenge, ORIGIN);
        assert_eq!(
            rp().verify_assertion(
                &challenge,
                &credential,
                &a.credential_id,
                &a.client_data_json,
                &a.authenticator_data,
                &a.signature,
            ),
            Err(WebAuthnError::CounterNotIncreased)
        );
        // An authenticator without a counter (always 0) is accepted.
        let (mut counterless, credential) = registered();
        counterless.set_counting(false);
        for _ in 0..2 {
            let challenge = new_challenge();
            let a = counterless.assert(&challenge, ORIGIN);
            assert_eq!(
                rp().verify_assertion(
                    &challenge,
                    &credential,
                    &a.credential_id,
                    &a.client_data_json,
                    &a.authenticator_data,
                    &a.signature,
                ),
                Ok(0)
            );
        }
    }

    #[test]
    fn user_verification_is_enforced_when_required() {
        let strict = rp().require_user_verification(true);
        let mut authenticator = SoftAuthenticator::new("localhost");
        let challenge = new_challenge();
        let (client_data, attestation) = authenticator.register(&challenge, ORIGIN);
        assert_eq!(
            strict.verify_registration(&challenge, &client_data, &attestation),
            Err(WebAuthnError::UserNotVerified)
        );
        authenticator.set_user_verified(true);
        let (client_data, attestation) = authenticator.register(&challenge, ORIGIN);
        let credential = strict
            .verify_registration(&challenge, &client_data, &attestation)
            .unwrap();
        let a = authenticator.assert(&challenge, ORIGIN);
        assert!(strict
            .verify_assertion(
                &challenge,
                &credential,
                &a.credential_id,
                &a.client_data_json,
                &a.authenticator_data,
                &a.signature,
            )
            .is_ok());
    }

    #[test]
    fn registration_refusals() {
        use WebAuthnError::*;
        let challenge = new_challenge();
        let mut authenticator = SoftAuthenticator::new("localhost");
        let (client_data, attestation) = authenticator.register(&challenge, ORIGIN);
        let other = new_challenge();
        assert_eq!(
            rp().verify_registration(&other, &client_data, &attestation),
            Err(WrongChallenge)
        );
        assert_eq!(
            RelyingParty::new("example.com", ORIGIN).verify_registration(
                &challenge,
                &client_data,
                &attestation
            ),
            Err(WrongRpId)
        );
        let packed = authenticator.attestation_object_with(|object| {
            object[0].1 = cbor::Value::Text("packed".into());
        });
        assert_eq!(
            rp().verify_registration(&challenge, &client_data, &packed),
            Err(UnsupportedAttestation)
        );
        let rs256 = authenticator.attestation_object_with_key(|key| {
            key[1].1 = cbor::Value::Neg(-257);
        });
        assert_eq!(
            rp().verify_registration(&challenge, &client_data, &rs256),
            Err(UnsupportedKey)
        );
        assert_eq!(
            rp().verify_registration(&challenge, &client_data, &attestation[..10]),
            Err(Malformed)
        );
    }

    /// A fixed vector made with OpenSSL, independently of this module and of
    /// the software authenticator: a P-256 key, authenticator data for RP ID
    /// `localhost` with UP and UV set and counter 42, the clientDataJSON
    /// below for challenge `01 02 … 20`, and `openssl dgst -sha256 -sign` over
    /// `authenticatorData ‖ SHA-256(clientDataJSON)`.
    #[test]
    fn an_openssl_assertion_vector() {
        let hex = |s: &str| hex::decode(s).unwrap();
        let public_key = hex(
            "0474577ffd5a0b5f0060c57ebf017ee2171a9d0378c9f93580a7cbf4cc70d064ed\
             4fa7c54a5083752dfad6da35c42cae7f83b07df764ed2e96f7cc4606e55ee530",
        );
        let authenticator_data =
            hex("49960de5880e8c687434170f6476605b8fe4aeb9a28632c7995cf3ba831d9763050000002a");
        let client_data_json = br#"{"type":"webauthn.get","challenge":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA","origin":"http://localhost:8080","crossOrigin":false}"#;
        let signature = hex(
            "3045022100abc07d807b8d3571d1a73fd0eea2839438cb473eb91e55bbcda809430fdd29\
             0d02200674f4295dd21880b0c776b3e1dbd9655759f7a3371af445b8ba9765fa65eba1",
        );
        let challenge: Vec<u8> = (1..=32).collect();
        let credential = Credential {
            id: vec![7; 16],
            public_key,
            sign_count: 41,
        };
        let verify = |rp: &RelyingParty, credential: &Credential, signature: &[u8]| {
            rp.verify_assertion(
                &challenge,
                credential,
                &[7; 16],
                client_data_json,
                &authenticator_data,
                signature,
            )
        };
        let strict = rp().require_user_verification(true);
        assert_eq!(verify(&strict, &credential, &signature), Ok(42));
        let mut tampered = signature.clone();
        tampered[10] ^= 1;
        assert_eq!(
            verify(&strict, &credential, &tampered),
            Err(WebAuthnError::BadSignature)
        );
        let seen = Credential {
            sign_count: 42,
            ..credential
        };
        assert_eq!(
            verify(&strict, &seen, &signature),
            Err(WebAuthnError::CounterNotIncreased),
            "a replayed assertion"
        );
    }
}
