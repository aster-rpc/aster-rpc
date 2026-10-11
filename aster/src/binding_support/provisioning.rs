//! Portable, stateless adapters for the existing coordination authority rules.
//!
//! JSON is only a binding DTO: signed documents and envelopes always use the
//! authoritative Rust Fory codec. These helpers neither sign nor choose group
//! policy, sort sets, generate randomness, persist files, or host a voter.
use crate::coordination::model::authority::{self, GenesisError, PermitError};
use crate::coordination::raft::{GenesisV1, JoinPermitV1, NodeIdV1, VoterSeedV1};
use serde::{Deserialize, Serialize};

/// Bound on a portable JSON request or signed envelope accepted by this adapter.
pub const MAX_PROVISIONING_INPUT_BYTES: usize = 1024 * 1024;

/// Stable binding error code with the original native diagnostic detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisioningError {
    code: &'static str,
    detail: String,
}
impl ProvisioningError {
    pub fn code(&self) -> &str {
        self.code
    }
    pub fn detail(&self) -> &str {
        &self.detail
    }
    fn input(detail: impl Into<String>) -> Self {
        Self {
            code: "invalid_input",
            detail: detail.into(),
        }
    }
    fn encoding() -> Self {
        Self {
            code: "encoding",
            detail: "canonical Fory encoding failed".into(),
        }
    }
}
impl std::fmt::Display for ProvisioningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for ProvisioningError {}
impl From<GenesisError> for ProvisioningError {
    fn from(error: GenesisError) -> Self {
        let code = match &error {
            GenesisError::Malformed => "malformed",
            GenesisError::UnknownFormat(_) => "unknown_format",
            GenesisError::NonCanonical => "non_canonical",
            GenesisError::WrongAuthorityKey => "wrong_authority_key",
            GenesisError::BadSignature => "bad_signature",
            GenesisError::WrongDomain => "wrong_domain",
            GenesisError::FieldWidth => "field_width",
            GenesisError::UnknownProfile(_) => "unknown_profile",
            GenesisError::VoterCount { .. } => "voter_count",
            GenesisError::InitialVoters => "initial_voters",
            GenesisError::InitialAdmins => "initial_admins",
            GenesisError::RootIsNotDelegable => "root_is_not_delegable",
            GenesisError::RootIsNotAMember => "root_is_not_a_member",
            GenesisError::VoterSeedTicket { .. } => "voter_seed_ticket",
        };
        Self {
            code,
            detail: format!("{error:?}"),
        }
    }
}
impl From<PermitError> for ProvisioningError {
    fn from(error: PermitError) -> Self {
        let code = match error {
            PermitError::Malformed => "malformed",
            PermitError::UnknownFormat(_) => "unknown_format",
            PermitError::NonCanonical => "non_canonical",
            PermitError::WrongAuthorityKey => "wrong_authority_key",
            PermitError::BadSignature => "bad_signature",
            PermitError::WrongDomain => "wrong_domain",
            PermitError::WrongGroup => "wrong_group",
            PermitError::WrongGenesis => "wrong_genesis",
            PermitError::WrongNode => "wrong_node",
            PermitError::TicketMismatch => "ticket_mismatch",
            PermitError::BootstrapSenders => "bootstrap_senders",
            PermitError::FieldWidth => "field_width",
        };
        Self {
            code,
            detail: format!("{error:?}"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VoterInput {
    node_id: String,
    ticket: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenesisInput {
    group_id: String,
    authority_domain: String,
    genesis_nonce: String,
    deployment_profile_code: i32,
    initial_voters: Vec<VoterInput>,
    initial_admins: Vec<String>,
    recovery_export_hash: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinInput {
    group_id: String,
    authority_domain: String,
    genesis_hash: String,
    candidate_node_id: String,
    bootstrap_senders: Vec<String>,
    join_nonce: String,
}
fn bounded(bytes: &[u8]) -> Result<(), ProvisioningError> {
    if bytes.len() > MAX_PROVISIONING_INPUT_BYTES {
        return Err(ProvisioningError::input("provisioning input exceeds 1 MiB"));
    }
    Ok(())
}
fn decode<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, ProvisioningError> {
    bounded(json.as_bytes())?;
    serde_json::from_str(json).map_err(|error| ProvisioningError::input(error.to_string()))
}
fn fixed<const N: usize>(bytes: &[u8], field: &str) -> Result<[u8; N], ProvisioningError> {
    bytes
        .try_into()
        .map_err(|_| ProvisioningError::input(format!("{field} must contain exactly {N} bytes")))
}
fn hex32(value: &str, field: &str) -> Result<[u8; 32], ProvisioningError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProvisioningError::input(format!(
            "{field} must contain exactly 64 ASCII hexadecimal characters"
        )));
    }
    let mut out = [0; 32];
    hex::decode_to_slice(value, &mut out)
        .map_err(|error| ProvisioningError::input(error.to_string()))?;
    Ok(out)
}
fn ids(values: &[String], field: &str) -> Result<Vec<NodeIdV1>, ProvisioningError> {
    values
        .iter()
        .map(|value| {
            Ok(NodeIdV1 {
                node_id: hex32(value, field)?.to_vec(),
            })
        })
        .collect()
}

/// Encode a v1 genesis DTO without sorting, deduplication or policy inference.
/// Encoding does not establish validity; verify the externally signed envelope.
pub fn canonical_genesis_json(json: &str) -> Result<Vec<u8>, ProvisioningError> {
    let input: GenesisInput = decode(json)?;
    let document = GenesisV1 {
        format_version: authority::PERMIT_FORMAT_VERSION,
        group_id: hex32(&input.group_id, "group_id")?.to_vec(),
        authority_domain: input.authority_domain,
        genesis_nonce: hex32(&input.genesis_nonce, "genesis_nonce")?.to_vec(),
        deployment_profile_code: input.deployment_profile_code,
        initial_voters: input
            .initial_voters
            .into_iter()
            .map(|voter| {
                Ok(VoterSeedV1 {
                    node_id: hex32(&voter.node_id, "initial_voters.node_id")?.to_vec(),
                    ticket: voter.ticket,
                })
            })
            .collect::<Result<_, ProvisioningError>>()?,
        initial_admins: ids(&input.initial_admins, "initial_admins")?,
        recovery_export_hash: input
            .recovery_export_hash
            .as_deref()
            .map(|value| hex32(value, "recovery_export_hash").map(|bytes| bytes.to_vec()))
            .transpose()?,
    };
    authority::canonical_document_bytes(&document).map_err(|_| ProvisioningError::encoding())
}

/// Encode a v1 permit DTO. The candidate's addressless Open ticket is derived
/// natively; no caller-provided ticket can introduce address or credential data.
pub fn canonical_join_permit_json(json: &str) -> Result<Vec<u8>, ProvisioningError> {
    let input: JoinInput = decode(json)?;
    let candidate = hex32(&input.candidate_node_id, "candidate_node_id")?;
    let document = JoinPermitV1 {
        format_version: authority::PERMIT_FORMAT_VERSION,
        group_id: hex32(&input.group_id, "group_id")?.to_vec(),
        authority_domain: input.authority_domain,
        genesis_hash: hex32(&input.genesis_hash, "genesis_hash")?.to_vec(),
        candidate_node_id: candidate.to_vec(),
        candidate_ticket: authority::canonical_candidate_ticket(&candidate)?,
        bootstrap_senders: ids(&input.bootstrap_senders, "bootstrap_senders")?,
        join_nonce: hex32(&input.join_nonce, "join_nonce")?.to_vec(),
    };
    authority::canonical_document_bytes(&document).map_err(|_| ProvisioningError::encoding())
}

/// Exact genesis domain-separated preimage; does not decode or validate bytes.
pub fn genesis_signing_bytes(document_bytes: &[u8]) -> Vec<u8> {
    authority::genesis_signing_bytes(document_bytes)
}
/// Exact join domain-separated preimage; does not decode or validate bytes.
pub fn join_signing_bytes(document_bytes: &[u8]) -> Vec<u8> {
    authority::join_signing_bytes(document_bytes)
}
/// Hash of supplied genesis bytes, not a proof of their validity.
pub fn genesis_hash(document_bytes: &[u8]) -> [u8; 32] {
    authority::genesis_hash(document_bytes)
}

/// Assemble the native Fory envelope around externally signed opaque bytes.
pub fn signed_document_from_parts(
    document_bytes: &[u8],
    authority_public_key: &[u8],
    signature: &[u8],
) -> Result<Vec<u8>, ProvisioningError> {
    bounded(document_bytes)?;
    authority::signed_document_from_parts(
        document_bytes.to_vec(),
        &fixed(authority_public_key, "authority_public_key")?,
        &fixed(signature, "signature")?,
    )
    .map_err(|_| ProvisioningError::encoding())
}
/// Derive the exact canonical, addressless, credential-free candidate ticket.
pub fn canonical_candidate_ticket(node_id: &[u8]) -> Result<String, ProvisioningError> {
    authority::canonical_candidate_ticket(&fixed(node_id, "node_id")?).map_err(Into::into)
}

#[derive(Serialize)]
struct VoterSummary {
    node_id: String,
    ticket: String,
}
#[derive(Serialize)]
struct GenesisSummary {
    document_bytes: String,
    genesis_hash: String,
    group_id: String,
    authority_public_key: String,
    authority_domain: String,
    deployment_profile_code: i32,
    initial_voters: Vec<VoterSummary>,
    initial_admins: Vec<String>,
    recovery_export_hash: Option<String>,
}
#[derive(Serialize)]
struct JoinSummary {
    document_bytes: String,
    group_id: String,
    genesis_hash: String,
    candidate_node_id: String,
    bootstrap_senders: Vec<String>,
}

/// Verify against independently configured key/domain pins and serialize only
/// verified accessors. The informational DTO is not a native authority seal.
pub fn verify_genesis_json(
    authority_public_key: &[u8],
    authority_domain: &str,
    signed_bytes: &[u8],
) -> Result<String, ProvisioningError> {
    bounded(signed_bytes)?;
    let verified = authority::verify_genesis(
        &fixed(authority_public_key, "authority_public_key")?,
        authority_domain,
        signed_bytes,
    )?;
    let summary = GenesisSummary {
        document_bytes: hex::encode(verified.document_bytes()),
        genesis_hash: hex::encode(verified.genesis_hash()),
        group_id: hex::encode(verified.group_id()),
        authority_public_key: hex::encode(verified.authority_public_key()),
        authority_domain: verified.authority_domain().into(),
        deployment_profile_code: verified.deployment_profile_code(),
        initial_voters: verified
            .initial_voters()
            .iter()
            .map(|voter| VoterSummary {
                node_id: hex::encode(voter.node_id()),
                ticket: voter.ticket().into(),
            })
            .collect(),
        initial_admins: verified.initial_admins().iter().map(hex::encode).collect(),
        recovery_export_hash: verified.recovery_export_hash().map(hex::encode),
    };
    serde_json::to_string(&summary).map_err(|_| ProvisioningError::encoding())
}

/// Verify startup key/domain/candidate bindings. Group and genesis are extracted
/// from the verified permit, not compared with an independent expectation.
pub fn verify_join_permit_for_start_json(
    authority_public_key: &[u8],
    authority_domain: &str,
    candidate_node_id: &[u8],
    signed_bytes: &[u8],
) -> Result<String, ProvisioningError> {
    bounded(signed_bytes)?;
    let verified = authority::verify_join_permit_for_start(
        &fixed(authority_public_key, "authority_public_key")?,
        authority_domain,
        signed_bytes,
        &fixed(candidate_node_id, "candidate_node_id")?,
    )?;
    let summary = JoinSummary {
        document_bytes: hex::encode(verified.document_bytes()),
        group_id: hex::encode(verified.group_id()),
        genesis_hash: hex::encode(verified.genesis_hash()),
        candidate_node_id: hex::encode(verified.candidate()),
        bootstrap_senders: verified
            .bootstrap_senders()
            .iter()
            .map(hex::encode)
            .collect(),
    };
    serde_json::to_string(&summary).map_err(|_| ProvisioningError::encoding())
}
