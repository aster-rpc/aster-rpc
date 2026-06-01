//! Strongly-typed identifiers and capability material.
//!
//! These newtypes keep iroh / iroh-docs types out of the public API. IDs that
//! the transport core represents as hex strings (`NodeId`, `Hash`, `AuthorId`)
//! are wrapped as hex-backed newtypes; namespace material is carried as raw
//! 32-byte arrays.

use crate::error::{Error, Result};
use std::fmt;
use std::str::FromStr;

/// A peer / endpoint identifier (an ed25519 public key, hex-encoded).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) String);

/// A content-addressed blob hash (BLAKE3, hex-encoded).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Hash(pub(crate) String);

/// A document author identifier (hex-encoded).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AuthorId(pub(crate) String);

macro_rules! hex_string_id {
    ($name:ident) => {
        impl $name {
            /// Wrap a hex string without validation (the transport core
            /// validates on use).
            pub fn from_hex(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            /// The hex representation.
            pub fn as_str(&self) -> &str {
                &self.0
            }
            /// Consume into the owned hex string.
            pub fn into_string(self) -> String {
                self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), &self.0)
            }
        }
        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Ok(Self(s.to_string()))
            }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }
    };
}

hex_string_id!(NodeId);
hex_string_id!(Hash);
hex_string_id!(AuthorId);

/// The public identifier of a document namespace (32 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NamespaceId(pub(crate) [u8; 32]);

impl NamespaceId {
    /// The raw 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }
    /// Construct from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    /// The hex representation (64 chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
    /// Parse from a 64-char hex string.
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s).map_err(|e| Error::InvalidArgument(e.to_string()))?;
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| Error::InvalidArgument("namespace id must be 32 bytes".into()))
    }
}

impl fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NamespaceId({})", self.to_hex())
    }
}

/// Writable capability material for a namespace (a 32-byte secret).
///
/// A deterministic value (e.g. `BLAKE3("portal-node-config/v1")`) used with
/// [`crate::Docs::open_or_import_write_namespace`] yields a stable namespace
/// across peers and restarts. The public [`NamespaceId`] is derived from the
/// secret via [`NamespaceSecret::id`].
#[derive(Clone)]
pub struct NamespaceSecret(pub(crate) [u8; 32]);

impl NamespaceSecret {
    /// Construct from raw 32 bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    /// The raw 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }
    /// The public [`NamespaceId`] derived from this secret. Pure / offline —
    /// lets a caller learn the namespace id without opening the document.
    pub fn id(&self) -> NamespaceId {
        let secret = iroh_docs::NamespaceSecret::from_bytes(&self.0);
        NamespaceId(*secret.id().as_bytes())
    }
}

impl fmt::Debug for NamespaceSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the secret bytes.
        f.write_str("NamespaceSecret(<redacted>)")
    }
}

/// A 32-byte Ed25519 public key — an attestation identity (anchor / root / node).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(pub(crate) [u8; 32]);

impl PublicKey {
    /// Construct from raw 32 bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    /// The raw 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }
    /// The hex representation (64 chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
    /// Parse from a 64-char hex string.
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s).map_err(|e| Error::InvalidArgument(e.to_string()))?;
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| Error::InvalidArgument("public key must be 32 bytes".into()))
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.to_hex())
    }
}

/// A node secret key (32 bytes), used to pin / restore a node's identity.
#[derive(Clone)]
pub struct SecretKey(pub(crate) [u8; 32]);

impl SecretKey {
    /// Construct from raw 32 bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    /// The raw 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// A peer's address: its [`NodeId`] plus the relay / direct addresses we know.
#[derive(Clone, Debug)]
pub struct NodeAddr {
    /// The peer's endpoint id.
    pub node_id: NodeId,
    /// The peer's home relay URL, if known.
    pub relay_url: Option<String>,
    /// Known direct socket addresses.
    pub direct_addresses: Vec<String>,
}

impl NodeAddr {
    pub(crate) fn to_core(&self) -> aster_transport_core::CoreNodeAddr {
        aster_transport_core::CoreNodeAddr {
            endpoint_id: self.node_id.as_str().to_string(),
            relay_url: self.relay_url.clone(),
            direct_addresses: self.direct_addresses.clone(),
        }
    }
}
