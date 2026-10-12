//! Versioned root-authored Docs policy records. Deserializing these types does
//! not authenticate them; only the pinned Docs read path creates verified data.
use super::{AdmissionClaims, Audience};
use serde::{Deserialize, Serialize};

pub const POLICY_RECORD_VERSION: u32 = 1;
pub const MAX_POLICY_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_POLICY_RECORDS: usize = 4096;
pub const POLICY_RECORD_PREFIX: &[u8] = b"aster/enrollment/v1/";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeRegistrationStatus {
    Active,
    Revoked,
    Superseded,
}

/// Independent root registration. Its lifetime and claims replace credential
/// evidence; they never extend a JWT's signed validity or union its grants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeRegistrationV1 {
    pub version: u32,
    pub issuer: String,
    pub subject: String,
    pub audience: Audience,
    pub revision: u64,
    pub issued_at: u64,
    #[serde(
        default,
        deserialize_with = "optional_date",
        skip_serializing_if = "Option::is_none"
    )]
    pub not_before: Option<u64>,
    #[serde(
        default,
        deserialize_with = "optional_date",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<u64>,
    pub status: NodeRegistrationStatus,
    pub claims: AdmissionClaims,
    pub mandatory: Vec<String>,
    /// A newer active registration must name the exact prior denied revision.
    /// A renewal issued before the prior expiry remains an ordinary update,
    /// even when an offline replica learns it after that expiry.
    pub reauthorizes_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialStatusV1 {
    pub version: u32,
    pub issuer: String,
    pub credential_id: String,
    pub revision: u64,
    pub issued_at: u64,
    pub revoked: bool,
    pub reauthorizes_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordAvailability {
    Available,
    Absent,
    Pending,
}

/// Leaf-only keys prevent a subject or credential id from choosing a prefix.
pub fn node_registration_key(subject: &[u8; 32]) -> Vec<u8> {
    format!("aster/enrollment/v1/nodes/{}", hex::encode(subject)).into_bytes()
}
pub fn credential_status_key(credential_id: &str) -> Vec<u8> {
    format!(
        "aster/enrollment/v1/credentials/{}",
        blake3::hash(credential_id.as_bytes()).to_hex()
    )
    .into_bytes()
}
fn optional_date<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(d).map(Some)
}
