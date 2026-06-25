//! Docs namespace capability primitives.
//!
//! This module intentionally keeps the wire shape small and app-agnostic. A
//! logical application id such as a tree id is policy data; the only capability
//! material here is either a read namespace id or a write namespace secret.

use anyhow::{anyhow, bail, Result};
use fory_core::Fory;
use fory_derive::ForyStruct;
use iroh_docs::NamespaceSecret;
use std::fmt;
use std::sync::OnceLock;

/// Schema version for Fory XLANG namespace capability bytes.
pub const NAMESPACE_CAPABILITY_VERSION: i32 = 1;
/// Fory XLANG kind value for `Read(NamespaceId)`.
pub const NAMESPACE_CAPABILITY_READ: i32 = 0;
/// Fory XLANG kind value for `Write(NamespaceSecret)`.
pub const NAMESPACE_CAPABILITY_WRITE: i32 = 1;

#[derive(ForyStruct, Clone, PartialEq)]
pub struct NamespaceCapabilityRecord {
    #[fory(id = 1)]
    pub v: i32,
    #[fory(id = 2)]
    pub kind: i32,
    #[fory(id = 3, bytes)]
    pub material: Vec<u8>,
}

impl fmt::Debug for NamespaceCapabilityRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamespaceCapabilityRecord")
            .field("v", &self.v)
            .field("kind", &self.kind)
            .field("material", &"<redacted>")
            .finish()
    }
}

static FORY: OnceLock<Fory> = OnceLock::new();

fn fory() -> &'static Fory {
    FORY.get_or_init(|| {
        let mut f = Fory::builder().xlang(true).compatible(true).build();
        f.register_by_name::<NamespaceCapabilityRecord>("aster.namespace.NamespaceCapability")
            .expect("register NamespaceCapabilityRecord");
        f
    })
}

/// A typed Docs namespace capability.
///
/// `Read` carries the public namespace id. `Write` carries only the namespace
/// secret; derive the corresponding read id with [`namespace_secret_id`].
#[derive(Clone, PartialEq, Eq)]
pub enum CoreNamespaceCapability {
    Read([u8; 32]),
    Write([u8; 32]),
}

impl CoreNamespaceCapability {
    /// Return the public namespace id represented by this capability.
    pub fn namespace_id(&self) -> [u8; 32] {
        match self {
            Self::Read(id) => *id,
            Self::Write(secret) => namespace_secret_id_array(secret),
        }
    }

    /// Return true if this capability includes write authority.
    pub fn can_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }

    /// Fory XLANG binary encoding.
    pub fn encode_fory(&self) -> Vec<u8> {
        encode_namespace_capability(self)
    }

    /// Decode Fory XLANG namespace capability bytes.
    pub fn decode_fory(bytes: &[u8]) -> Result<Self> {
        decode_namespace_capability(bytes)
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

impl fmt::Debug for CoreNamespaceCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(_) => f.write_str("CoreNamespaceCapability::Read(<redacted>)"),
            Self::Write(_) => f.write_str("CoreNamespaceCapability::Write(<redacted>)"),
        }
    }
}

/// Redacted wrapper for decrypted capability payload bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct DecryptedCapabilityPayload(Vec<u8>);

impl DecryptedCapabilityPayload {
    /// Wrap decrypted bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrow the decrypted bytes. Callers must avoid logging them.
    pub fn expose_secret(&self) -> &[u8] {
        &self.0
    }

    /// Consume and return the decrypted bytes.
    pub fn into_secret(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Debug for DecryptedCapabilityPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DecryptedCapabilityPayload(<redacted>; {} bytes)",
            self.0.len()
        )
    }
}

impl fmt::Display for DecryptedCapabilityPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Derive a public namespace id from a 32-byte namespace secret.
pub fn namespace_secret_id(secret: &[u8]) -> Result<[u8; 32]> {
    let secret = checked_32(secret, "namespace secret")?;
    Ok(namespace_secret_id_array(&secret))
}

fn namespace_secret_id_array(secret: &[u8; 32]) -> [u8; 32] {
    let secret = NamespaceSecret::from_bytes(secret);
    *secret.id().as_bytes()
}

/// Encode a namespace capability to Fory XLANG bytes.
pub fn encode_namespace_capability(capability: &CoreNamespaceCapability) -> Vec<u8> {
    let (kind, material) = match capability {
        CoreNamespaceCapability::Read(id) => (NAMESPACE_CAPABILITY_READ, id.to_vec()),
        CoreNamespaceCapability::Write(secret) => (NAMESPACE_CAPABILITY_WRITE, secret.to_vec()),
    };
    let record = NamespaceCapabilityRecord {
        v: NAMESPACE_CAPABILITY_VERSION,
        kind,
        material,
    };
    fory()
        .serialize(&record)
        .expect("NamespaceCapabilityRecord Fory serialization should not fail")
}

/// Decode Fory XLANG namespace capability bytes.
pub fn decode_namespace_capability(bytes: &[u8]) -> Result<CoreNamespaceCapability> {
    let record: NamespaceCapabilityRecord = fory()
        .deserialize(bytes)
        .map_err(|e| anyhow!("namespace capability Fory decode failed: {e}"))?;
    if record.v != NAMESPACE_CAPABILITY_VERSION {
        bail!("namespace capability version must be 1, got {}", record.v);
    }

    let material = checked_32(&record.material, "namespace capability material")?;

    match record.kind {
        NAMESPACE_CAPABILITY_READ => Ok(CoreNamespaceCapability::Read(material)),
        NAMESPACE_CAPABILITY_WRITE => Ok(CoreNamespaceCapability::Write(material)),
        other => bail!("namespace capability kind must be 0(read) or 1(write), got {other}"),
    }
}

fn checked_32(bytes: &[u8], label: &str) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| anyhow!("{label} must be 32 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_secret_id_derives_public_read_capability() {
        let secret = [7u8; 32];
        let id = namespace_secret_id(&secret).unwrap();
        assert_ne!(id, secret);
        assert_eq!(
            CoreNamespaceCapability::Write(secret).namespace_id(),
            id,
            "write capability should derive its read namespace id"
        );
    }

    #[test]
    fn namespace_capability_fory_vectors() {
        let read = CoreNamespaceCapability::Read([0u8; 32]).encode_fory();
        let write = CoreNamespaceCapability::Write([0xffu8; 32]).encode_fory();

        assert_eq!(
            read,
            CoreNamespaceCapability::Read([0u8; 32]).encode_fory(),
            "Fory encoding should be deterministic"
        );
        assert_eq!(
            write,
            CoreNamespaceCapability::Write([0xffu8; 32]).encode_fory(),
            "Fory encoding should be deterministic"
        );
        assert_ne!(read, write);

        assert_eq!(
            CoreNamespaceCapability::decode_fory(&read).unwrap(),
            CoreNamespaceCapability::Read([0u8; 32])
        );
        assert_eq!(
            CoreNamespaceCapability::decode_fory(&write).unwrap(),
            CoreNamespaceCapability::Write([0xffu8; 32])
        );
    }

    #[test]
    fn namespace_capability_decode_rejects_bad_material_len() {
        let record = NamespaceCapabilityRecord {
            v: NAMESPACE_CAPABILITY_VERSION,
            kind: NAMESPACE_CAPABILITY_READ,
            material: vec![1, 2, 3],
        };
        let bytes = fory().serialize(&record).unwrap();

        let err = decode_namespace_capability(&bytes).unwrap_err();
        assert!(err.to_string().contains("material must be 32 bytes"));
    }

    #[test]
    fn namespace_capability_debug_redacts_capability_material() {
        let read_text = format!("{:?}", CoreNamespaceCapability::Read([8u8; 32]));
        assert!(read_text.contains("<redacted>"));
        assert!(!read_text.contains("080808"));

        let write_text = format!("{:?}", CoreNamespaceCapability::Write([9u8; 32]));
        assert!(write_text.contains("<redacted>"));
        assert!(!write_text.contains("090909"));
    }
}
