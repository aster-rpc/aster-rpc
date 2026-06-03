//! HPKE envelope helpers for replicated encrypted config.
//!
//! The helper is deliberately algorithm-pinned to the simple trust config
//! default: HPKE base mode with DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, and
//! ChaCha20-Poly1305. Sender authority must come from the root-authored Docs
//! record that carries the envelope, not HPKE authenticated mode.

use anyhow::{anyhow, bail, Result};
use fory_core::Fory;
use fory_derive::ForyStruct;
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable,
    Kem as KemTrait, OpModeR, OpModeS, Serializable,
};
use rand_core::{OsRng, TryRngCore};
use std::fmt;
use std::sync::OnceLock;

use crate::namespace::DecryptedCapabilityPayload;

type Kem = X25519HkdfSha256;
type Aead = ChaCha20Poly1305;
type Kdf = HkdfSha256;

/// Stable algorithm id used in replicated grant records.
pub const HPKE_ENVELOPE_ALG: &str = "HPKE-Base-X25519-HKDF-SHA256-ChaCha20Poly1305";
/// Schema version for Fory XLANG HPKE envelope bytes.
pub const HPKE_ENVELOPE_VERSION: i32 = 1;
const HPKE_INFO: &[u8] = b"aster.hpke.base-x25519-hkdf-sha256-chacha20poly1305/v1";

#[derive(ForyStruct, Clone, PartialEq)]
pub struct HpkeEnvelopeRecord {
    #[fory(id = 1)]
    pub v: i32,
    #[fory(id = 2)]
    pub alg: String,
    #[fory(id = 3, bytes)]
    pub encapped_key: Vec<u8>,
    #[fory(id = 4, bytes)]
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for HpkeEnvelopeRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HpkeEnvelopeRecord")
            .field("v", &self.v)
            .field("alg", &self.alg)
            .field("encapped_key_len", &self.encapped_key.len())
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

static FORY: OnceLock<Fory> = OnceLock::new();

fn fory() -> &'static Fory {
    FORY.get_or_init(|| {
        let mut f = Fory::builder().xlang(true).compatible(true).build();
        f.register_by_name::<HpkeEnvelopeRecord>("aster.hpke", "Envelope")
            .expect("register HpkeEnvelopeRecord");
        f
    })
}

/// X25519 keypair for HPKE base-mode encryption.
#[derive(Clone, PartialEq, Eq)]
pub struct HpkeKeyPair {
    private_key: [u8; 32],
    public_key: [u8; 32],
}

impl HpkeKeyPair {
    pub fn private_key(&self) -> [u8; 32] {
        self.private_key
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.public_key
    }
}

impl fmt::Debug for HpkeKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HpkeKeyPair")
            .field("private_key", &"<redacted>")
            .field("public_key", &hex::encode(self.public_key))
            .finish()
    }
}

/// HPKE envelope fields.
///
/// `ciphertext` is HPKE's authenticated ciphertext: encrypted payload with
/// the AEAD tag appended by the HPKE context.
#[derive(Clone, PartialEq, Eq)]
pub struct HpkeEnvelope {
    pub encapped_key: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl HpkeEnvelope {
    pub fn new(encapped_key: Vec<u8>, ciphertext: Vec<u8>) -> Self {
        Self {
            encapped_key,
            ciphertext,
        }
    }

    pub fn alg(&self) -> &'static str {
        HPKE_ENVELOPE_ALG
    }

    /// Fory XLANG binary encoding for storage/wire tests.
    pub fn encode_fory(&self) -> Vec<u8> {
        encode_hpke_envelope(self)
    }

    pub fn decode_fory(bytes: &[u8]) -> Result<Self> {
        decode_hpke_envelope(bytes)
    }

    /// Alias for [`Self::encode_fory`].
    pub fn encode_canonical(&self) -> Vec<u8> {
        self.encode_fory()
    }

    /// Alias for [`Self::decode_fory`].
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        Self::decode_fory(bytes)
    }
}

impl fmt::Debug for HpkeEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HpkeEnvelope")
            .field("alg", &HPKE_ENVELOPE_ALG)
            .field("encapped_key_len", &self.encapped_key.len())
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

/// Generate an X25519 keypair suitable for the default HPKE envelope suite.
pub fn hpke_generate_keypair() -> HpkeKeyPair {
    let mut rng = OsRng.unwrap_err();
    let (private, public) = Kem::gen_keypair(&mut rng);
    HpkeKeyPair {
        private_key: private.to_bytes().into(),
        public_key: public.to_bytes().into(),
    }
}

/// Derive an HPKE public key from a 32-byte X25519 private key.
pub fn hpke_public_key_from_private(private_key: &[u8]) -> Result<[u8; 32]> {
    let private = <Kem as KemTrait>::PrivateKey::from_bytes(private_key)
        .map_err(|e| anyhow!("invalid HPKE private key: {e:?}"))?;
    Ok(Kem::sk_to_pk(&private).to_bytes().into())
}

/// Encrypt caller-owned plaintext to a recipient HPKE public key.
///
/// `associated_data` is authenticated and must be built by the caller from the
/// root policy namespace id, root node id, policy path, recipient, generation,
/// role, and record format version as appropriate for the application.
pub fn hpke_seal(
    recipient_public_key: &[u8],
    associated_data: &[u8],
    plaintext: &[u8],
) -> Result<HpkeEnvelope> {
    let public = <Kem as KemTrait>::PublicKey::from_bytes(recipient_public_key)
        .map_err(|e| anyhow!("invalid HPKE public key: {e:?}"))?;
    let mut rng = OsRng.unwrap_err();
    let (encapped_key, mut sender) =
        hpke::setup_sender::<Aead, Kdf, Kem, _>(&OpModeS::Base, &public, HPKE_INFO, &mut rng)
            .map_err(|e| anyhow!("HPKE sender setup failed: {e:?}"))?;
    let ciphertext = sender
        .seal(plaintext, associated_data)
        .map_err(|e| anyhow!("HPKE seal failed: {e:?}"))?;

    Ok(HpkeEnvelope {
        encapped_key: encapped_key.to_bytes().to_vec(),
        ciphertext,
    })
}

/// Decrypt an HPKE envelope with the recipient private key.
pub fn hpke_open(
    recipient_private_key: &[u8],
    associated_data: &[u8],
    envelope: &HpkeEnvelope,
) -> Result<DecryptedCapabilityPayload> {
    let private = <Kem as KemTrait>::PrivateKey::from_bytes(recipient_private_key)
        .map_err(|e| anyhow!("invalid HPKE private key: {e:?}"))?;
    let encapped_key = <Kem as KemTrait>::EncappedKey::from_bytes(&envelope.encapped_key)
        .map_err(|e| anyhow!("invalid HPKE encapped key: {e:?}"))?;
    let mut receiver =
        hpke::setup_receiver::<Aead, Kdf, Kem>(&OpModeR::Base, &private, &encapped_key, HPKE_INFO)
            .map_err(|e| anyhow!("HPKE receiver setup failed: {e:?}"))?;
    let plaintext = receiver
        .open(&envelope.ciphertext, associated_data)
        .map_err(|e| anyhow!("HPKE open failed: {e:?}"))?;
    Ok(DecryptedCapabilityPayload::new(plaintext))
}

/// Encode HPKE envelope fields to Fory XLANG binary.
pub fn encode_hpke_envelope(envelope: &HpkeEnvelope) -> Vec<u8> {
    let record = HpkeEnvelopeRecord {
        v: HPKE_ENVELOPE_VERSION,
        alg: HPKE_ENVELOPE_ALG.to_string(),
        encapped_key: envelope.encapped_key.clone(),
        ciphertext: envelope.ciphertext.clone(),
    };
    fory()
        .serialize(&record)
        .expect("HpkeEnvelopeRecord Fory serialization should not fail")
}

/// Decode Fory XLANG HPKE envelope bytes.
pub fn decode_hpke_envelope(bytes: &[u8]) -> Result<HpkeEnvelope> {
    let record: HpkeEnvelopeRecord = fory()
        .deserialize(bytes)
        .map_err(|e| anyhow!("HPKE envelope Fory decode failed: {e}"))?;
    if record.v != HPKE_ENVELOPE_VERSION {
        bail!("HPKE envelope version must be 1, got {}", record.v);
    }
    if record.alg != HPKE_ENVELOPE_ALG {
        bail!(
            "HPKE envelope alg must be {}, got {}",
            HPKE_ENVELOPE_ALG,
            record.alg
        );
    }

    if record.encapped_key.len() != 32 {
        bail!("HPKE encapped key must be 32 bytes");
    }
    if record.ciphertext.len() < 16 {
        bail!("HPKE ciphertext must include a 16-byte authentication tag");
    }

    Ok(HpkeEnvelope {
        encapped_key: record.encapped_key,
        ciphertext: record.ciphertext,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpke_round_trip_with_associated_data() {
        let keypair = hpke_generate_keypair();
        let aad = b"root_ns/root_node/path/recipient/generation/role/v1";
        let plaintext = b"namespace capability payload";

        let envelope = hpke_seal(&keypair.public_key(), aad, plaintext).unwrap();
        assert_eq!(envelope.alg(), HPKE_ENVELOPE_ALG);
        assert_eq!(envelope.encapped_key.len(), 32);
        assert!(envelope.ciphertext.len() >= plaintext.len() + 16);

        let decoded = HpkeEnvelope::decode_fory(&envelope.encode_fory()).unwrap();
        let decrypted = hpke_open(&keypair.private_key(), aad, &decoded).unwrap();
        assert_eq!(decrypted.expose_secret(), plaintext);
    }

    #[test]
    fn hpke_rejects_wrong_associated_data() {
        let keypair = hpke_generate_keypair();
        let envelope = hpke_seal(&keypair.public_key(), b"aad-1", b"payload").unwrap();

        let err = hpke_open(&keypair.private_key(), b"aad-2", &envelope).unwrap_err();
        assert!(err.to_string().contains("HPKE open failed"));
    }

    #[test]
    fn hpke_public_key_derives_from_private() {
        let keypair = hpke_generate_keypair();
        assert_eq!(
            hpke_public_key_from_private(&keypair.private_key()).unwrap(),
            keypair.public_key()
        );
    }

    #[test]
    fn hpke_envelope_decode_rejects_short_encapped_key() {
        let record = HpkeEnvelopeRecord {
            v: HPKE_ENVELOPE_VERSION,
            alg: HPKE_ENVELOPE_ALG.to_string(),
            encapped_key: vec![1, 2, 3],
            ciphertext: vec![0u8; 16],
        };
        let bytes = fory().serialize(&record).unwrap();

        let err = decode_hpke_envelope(&bytes).unwrap_err();
        assert!(err.to_string().contains("encapped key must be 32 bytes"));
    }

    #[test]
    fn decrypted_payload_debug_is_redacted() {
        let payload = DecryptedCapabilityPayload::new(b"secret".to_vec());
        let text = format!("{payload:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("secret"));
    }
}
