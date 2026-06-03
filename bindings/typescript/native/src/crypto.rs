//! Trust-config primitive helpers backed by Rust core.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::hpke_envelope::{
    hpke_generate_keypair as core_hpke_generate_keypair, hpke_open as core_hpke_open,
    hpke_public_key_from_private as core_hpke_public_key_from_private, hpke_seal as core_hpke_seal,
    HpkeEnvelope, HPKE_ENVELOPE_ALG,
};
use aster_transport_core::namespace::{
    namespace_secret_id as core_namespace_secret_id, CoreNamespaceCapability,
};

use crate::error::to_napi_err;

fn checked_32(bytes: &[u8], label: &str) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| Error::from_reason(format!("{label} must be 32 bytes")))
}

#[napi(object)]
pub struct DecodedNamespaceCapability {
    pub kind: String,
    pub namespace_id: Buffer,
    pub can_write: bool,
    /// Read id bytes for `kind == "read"` or namespace secret bytes for
    /// `kind == "write"`. Higher-level wrappers must redact on display.
    pub material: Buffer,
}

impl From<CoreNamespaceCapability> for DecodedNamespaceCapability {
    fn from(capability: CoreNamespaceCapability) -> Self {
        let namespace_id = capability.namespace_id();
        match capability {
            CoreNamespaceCapability::Read(id) => Self {
                kind: "read".to_string(),
                namespace_id: Buffer::from(namespace_id.to_vec()),
                can_write: false,
                material: Buffer::from(id.to_vec()),
            },
            CoreNamespaceCapability::Write(secret) => Self {
                kind: "write".to_string(),
                namespace_id: Buffer::from(namespace_id.to_vec()),
                can_write: true,
                material: Buffer::from(secret.to_vec()),
            },
        }
    }
}

#[napi(object)]
pub struct HpkeKeyPair {
    pub private_key: Buffer,
    pub public_key: Buffer,
}

#[napi(object)]
pub struct HpkeEnvelopeObject {
    pub alg: String,
    pub encapped_key: Buffer,
    pub ciphertext: Buffer,
}

impl From<HpkeEnvelope> for HpkeEnvelopeObject {
    fn from(envelope: HpkeEnvelope) -> Self {
        Self {
            alg: HPKE_ENVELOPE_ALG.to_string(),
            encapped_key: Buffer::from(envelope.encapped_key),
            ciphertext: Buffer::from(envelope.ciphertext),
        }
    }
}

fn envelope_from_object(envelope: HpkeEnvelopeObject) -> Result<HpkeEnvelope> {
    if envelope.alg != HPKE_ENVELOPE_ALG {
        return Err(Error::from_reason(format!(
            "HPKE envelope alg must be {}, got {}",
            HPKE_ENVELOPE_ALG, envelope.alg
        )));
    }
    Ok(HpkeEnvelope::new(
        envelope.encapped_key.to_vec(),
        envelope.ciphertext.to_vec(),
    ))
}

#[napi]
pub fn namespace_secret_id(secret: Buffer) -> Result<Buffer> {
    let id = core_namespace_secret_id(&secret).map_err(to_napi_err)?;
    Ok(Buffer::from(id.to_vec()))
}

#[napi]
pub fn namespace_capability_encode_read(namespace_id: Buffer) -> Result<Buffer> {
    let id = checked_32(&namespace_id, "namespace id")?;
    Ok(Buffer::from(
        CoreNamespaceCapability::Read(id).encode_fory(),
    ))
}

#[napi]
pub fn namespace_capability_encode_write(namespace_secret: Buffer) -> Result<Buffer> {
    let secret = checked_32(&namespace_secret, "namespace secret")?;
    Ok(Buffer::from(
        CoreNamespaceCapability::Write(secret).encode_fory(),
    ))
}

#[napi]
pub fn namespace_capability_decode(data: Buffer) -> Result<DecodedNamespaceCapability> {
    let capability = CoreNamespaceCapability::decode_fory(&data).map_err(to_napi_err)?;
    Ok(capability.into())
}

#[napi]
pub fn hpke_envelope_alg() -> String {
    HPKE_ENVELOPE_ALG.to_string()
}

#[napi]
pub fn hpke_generate_keypair() -> HpkeKeyPair {
    let keypair = core_hpke_generate_keypair();
    HpkeKeyPair {
        private_key: Buffer::from(keypair.private_key().to_vec()),
        public_key: Buffer::from(keypair.public_key().to_vec()),
    }
}

#[napi]
pub fn hpke_public_key_from_private(private_key: Buffer) -> Result<Buffer> {
    let public_key = core_hpke_public_key_from_private(&private_key).map_err(to_napi_err)?;
    Ok(Buffer::from(public_key.to_vec()))
}

#[napi]
pub fn hpke_seal(
    recipient_public_key: Buffer,
    associated_data: Buffer,
    plaintext: Buffer,
) -> Result<HpkeEnvelopeObject> {
    let envelope =
        core_hpke_seal(&recipient_public_key, &associated_data, &plaintext).map_err(to_napi_err)?;
    Ok(envelope.into())
}

#[napi]
pub fn hpke_open(
    recipient_private_key: Buffer,
    associated_data: Buffer,
    envelope: HpkeEnvelopeObject,
) -> Result<Buffer> {
    let envelope = envelope_from_object(envelope)?;
    let plaintext =
        core_hpke_open(&recipient_private_key, &associated_data, &envelope).map_err(to_napi_err)?;
    Ok(Buffer::from(plaintext.into_secret()))
}

#[napi]
pub fn hpke_envelope_encode(envelope: HpkeEnvelopeObject) -> Result<Buffer> {
    let envelope = envelope_from_object(envelope)?;
    Ok(Buffer::from(envelope.encode_fory()))
}

#[napi]
pub fn hpke_envelope_decode(data: Buffer) -> Result<HpkeEnvelopeObject> {
    let envelope = HpkeEnvelope::decode_fory(&data).map_err(to_napi_err)?;
    Ok(envelope.into())
}
