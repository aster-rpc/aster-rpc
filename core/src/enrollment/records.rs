//! Versioned root-authored Docs policy records. Deserializing these types does
//! not authenticate them; only the pinned Docs read path creates verified data.
use super::{AdmissionClaims, Audience};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssuerStatus {
    Active,
    Revoked,
}

/// The root's statement about one delegate key: an online issuer it signed a
/// delegation link for. Only revocation matters for admission; the delegation
/// link itself is the issuer's authority, so an issuer needs no active record.
/// A revoked issuer's chains stop admitting on every verifier that has synced
/// this record. Registrations the root already published stay in force.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssuerStatusV1 {
    pub version: u32,
    /// The root.
    pub issuer: String,
    /// The delegate key.
    pub subject: String,
    pub audience: Audience,
    pub revision: u64,
    pub issued_at: u64,
    pub status: IssuerStatus,
    /// Reactivating a revoked issuer must name the exact revoked revision.
    pub reauthorizes_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum NamespacePurpose {
    /// Node registrations for one role group.
    Membership,
    /// Exec grants for one role.
    ExecGrants,
    /// Observer-written admission observations, read by the root.
    Observations,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindingStatus {
    Active,
    Retired,
}

/// A root-authored record in the fleet namespace that binds another namespace
/// to this fleet. Only bound namespaces confer authority, and a node receives
/// a read ticket for one only when its verified scopes intersect
/// `read_scopes`. `write_scopes` names observer scopes and is valid only for
/// the observations namespace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamespaceBindingV1 {
    pub version: u32,
    /// The root.
    pub issuer: String,
    /// The bound namespace id, lowercase hex.
    pub namespace: String,
    pub purpose: NamespacePurpose,
    pub read_scopes: BTreeSet<String>,
    pub write_scopes: BTreeSet<String>,
    pub revision: u64,
    pub status: BindingStatus,
}

/// The default scope that lets a node write observations (enrollment §5).
pub const OBSERVER_SCOPE: &str = "aster.enrollment.observer";

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
pub fn issuer_status_key(delegate: &[u8; 32]) -> Vec<u8> {
    format!("aster/enrollment/v1/issuers/{}", hex::encode(delegate)).into_bytes()
}
pub fn namespace_binding_key(namespace: &[u8; 32]) -> Vec<u8> {
    format!("aster/enrollment/v1/namespaces/{}", hex::encode(namespace)).into_bytes()
}
/// Written by the observer, in the observations namespace.
pub fn observation_key(observer: &[u8; 32], subject: &[u8; 32]) -> Vec<u8> {
    format!(
        "aster/enrollment/v1/observations/{}/{}",
        hex::encode(observer),
        hex::encode(subject)
    )
    .into_bytes()
}
/// Sealed observer write grants live in the fleet namespace outside
/// [`POLICY_RECORD_PREFIX`], since they are Fory grant records rather than
/// JSON policy. The full key is `<prefix>/grants/<observer node id>`.
pub const OBSERVER_GRANT_PREFIX: &str = "aster/enrollment-grants/v1/observers";
/// Binds every sealed observer grant (enrollment D2).
pub const OBSERVER_GRANT_APP: &str = "aster-enrollment/observer-grant/v1";
fn optional_date<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(d).map(Some)
}
