//! Wire types of the @aster service, mirroring the Python dataclasses in
//! `aster-app/services/aster/types/` field for field. Python `int` is Fory
//! `int64` (`i64`), `Optional[T]` is `Option<T>`, `list[T]` is `Vec<T>`; the
//! wire tag is `aster/<Type>`. Signed request bodies are not Fory types: they
//! travel as JSON inside [`SignedRequest`] (see [`crate::payload`]).

use fory_derive::ForyStruct;
use serde::Serialize;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/CheckAvailabilityRequest")]
pub struct CheckAvailabilityRequest {
    pub handle: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/CheckAvailabilityResult")]
pub struct CheckAvailabilityResult {
    pub available: bool,
    pub reason: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/JoinResult")]
pub struct JoinResult {
    pub handle: String,
    pub status: String,
    pub verification_expires_at: String,
    pub code_expires_at: String,
    pub email_delivery_status: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/VerifyResult")]
pub struct VerifyResult {
    pub handle: String,
    pub status: String,
    pub pubkey: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ResendResult")]
pub struct ResendResult {
    pub code_expires_at: String,
    pub resends_remaining: i64,
    pub email_delivery_status: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/UpdateProfileResult")]
pub struct UpdateProfileResult {
    pub handle: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub url: Option<String>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/GetProfileRequest")]
pub struct GetProfileRequest {
    pub handle: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/PublicProfileResult")]
pub struct PublicProfileResult {
    pub handle: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub url: Option<String>,
    pub services_published: i64,
    pub registered_at: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/EndpointInfo")]
pub struct EndpointInfo {
    pub node_id: String,
    pub relay: String,
    pub direct_addrs: Vec<String>,
    pub registered_at: i64,
    pub ttl: i64,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/DelegationStatement")]
pub struct DelegationStatement {
    pub authority: String,
    pub mode: String,
    pub token_ttl: i64,
    pub rate_limit: Option<String>,
    pub roles: Vec<String>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/PublishResult")]
pub struct PublishResult {
    pub handle: String,
    pub service_name: String,
    pub version: i64,
    pub contract_id: String,
    pub producer_token: String,
    pub aster_root_pubkey: String,
    pub current_attestation: SigningKeyAttestation,
    pub endpoints_registered: i64,
    pub first_publish: bool,
    pub recovery_codes: Option<Vec<String>>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/DiscoverRequest")]
pub struct DiscoverRequest {
    pub query: String,
    pub limit: i64,
    pub offset: i64,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/DiscoverEntry")]
pub struct DiscoverEntry {
    pub handle: String,
    pub service_name: String,
    pub version: i64,
    pub contract_id: String,
    pub description: String,
    pub status: String,
    pub method_count: i64,
    pub endpoint_count: i64,
    pub visibility: String,
    pub delegation_mode: String,
    pub published_at: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/DiscoverResult")]
pub struct DiscoverResult {
    pub services: Vec<DiscoverEntry>,
    pub total: i64,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/DirectoryHandleEntry")]
pub struct DirectoryHandleEntry {
    pub handle: String,
    pub registered_at: String,
    pub last_published_at: Option<String>,
    pub service_count: i64,
    pub display_name: Option<String>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ListDirectoryHandlesRequest")]
pub struct ListDirectoryHandlesRequest {
    pub limit: i64,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ListDirectoryHandlesResult")]
pub struct ListDirectoryHandlesResult {
    pub handles: Vec<DirectoryHandleEntry>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/GetManifestRequest")]
pub struct GetManifestRequest {
    pub handle: String,
    pub service_name: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/GetManifestResult")]
pub struct GetManifestResult {
    pub handle: String,
    pub service_name: String,
    pub manifest_json: String,
    pub contract_id: String,
    pub version: i64,
    pub endpoint_count: i64,
    pub published_at: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ResolveRequest")]
pub struct ResolveRequest {
    pub handle: String,
    pub service_name: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ResolveResult")]
pub struct ResolveResult {
    pub handle: String,
    pub service_name: String,
    pub endpoints: Vec<EndpointInfo>,
    pub contract_id: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ListServicesRequest")]
pub struct ListServicesRequest {
    pub handle: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ListServicesResult")]
pub struct ListServicesResult {
    pub handle: String,
    pub services: Vec<DiscoverEntry>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/UnpublishResult")]
pub struct UnpublishResult {
    pub handle: String,
    pub service_name: String,
    pub removed: bool,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/UpdateServiceResult")]
pub struct UpdateServiceResult {
    pub handle: String,
    pub service_name: String,
    pub description: String,
    pub status: String,
    pub replacement: Option<String>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/UpdateDelegationResult")]
pub struct UpdateDelegationResult {
    pub handle: String,
    pub service_name: String,
    pub delegation: DelegationStatement,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/GrantAccessResult")]
pub struct GrantAccessResult {
    pub consumer_handle: String,
    pub service_name: String,
    pub role: String,
    pub scope: String,
    pub granted_at: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/RevokeAccessResult")]
pub struct RevokeAccessResult {
    pub consumer_handle: String,
    pub service_name: String,
    pub revoked: bool,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/AccessGrantEntry")]
pub struct AccessGrantEntry {
    pub consumer_handle: String,
    pub role: String,
    pub scope: String,
    pub granted_at: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/ListAccessResult")]
pub struct ListAccessResult {
    pub grants: Vec<AccessGrantEntry>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/SigningKeyAttestation")]
pub struct SigningKeyAttestation {
    pub signing_pubkey: String,
    pub key_id: String,
    pub valid_from: i64,
    pub valid_until: i64,
    pub root_signature: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/SignedRequest")]
pub struct SignedRequest {
    pub payload_json: String,
    pub signer_pubkey: String,
    pub signature: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/HandleStatusResult")]
pub struct HandleStatusResult {
    pub handle: String,
    pub status: String,
    pub email_masked: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub url: Option<String>,
    pub registered_at: String,
    pub services_published: i64,
    pub recovery_codes_remaining: i64,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq, Serialize)]
#[aster(wire = "aster/VisibilityResult")]
pub struct VisibilityResult {
    pub service_name: String,
    pub visibility: String,
}
