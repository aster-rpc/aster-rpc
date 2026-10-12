//! Bodies of signed requests. Each is serialised to JSON, extended with
//! `action`, `timestamp` and `nonce`, and signed by the profile's root key
//! ([`crate::Signer`]). The server re-serialises the body it parsed and checks
//! the signature over that, so every field must be present (`None` as
//! `null`), exactly as the Python dataclass declares it.

use serde::Serialize;

use crate::wire::{DelegationStatement, EndpointInfo};

/// A signed request body and the `action` the server expects for it.
pub trait Payload: Serialize {
    const ACTION: &'static str;
}

/// Signed `join` request body.
#[derive(Debug, Clone, Serialize)]
pub struct JoinPayload {
    pub handle: String,
    pub email: String,
    pub announcements: bool,
}

/// Signed `verify` request body.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyPayload {
    pub handle: String,
    pub code: String,
}

/// Signed `resend_verification` request body.
#[derive(Debug, Clone, Serialize)]
pub struct ResendPayload {
    pub handle: String,
}

/// Signed `handle_status` request body.
#[derive(Debug, Clone, Serialize)]
pub struct HandleStatusPayload {}

/// Signed `update_profile` request body.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateProfilePayload {
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub url: Option<String>,
}

/// Signed `publish` request body.
#[derive(Debug, Clone, Serialize)]
pub struct PublishPayload {
    pub handle: String,
    pub service_name: String,
    pub contract_id: String,
    pub manifest_json: String,
    pub description: String,
    pub status: String,
    pub endpoints: Vec<EndpointInfo>,
    pub delegation: DelegationStatement,
}

/// Signed `unpublish` request body.
#[derive(Debug, Clone, Serialize)]
pub struct UnpublishPayload {
    pub handle: String,
    pub service_name: String,
}

/// Signed `update_service` request body.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateServicePayload {
    pub handle: String,
    pub service_name: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub replacement: Option<String>,
}

/// Signed `update_delegation` request body.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateDelegationPayload {
    pub handle: String,
    pub service_name: String,
    pub delegation: DelegationStatement,
}

/// Signed `grant_access` request body.
#[derive(Debug, Clone, Serialize)]
pub struct GrantAccessPayload {
    pub handle: String,
    pub service_name: String,
    pub consumer_handle: String,
    pub role: String,
    pub scope: String,
    pub scope_node_id: Option<String>,
}

/// Signed `revoke_access` request body.
#[derive(Debug, Clone, Serialize)]
pub struct RevokeAccessPayload {
    pub handle: String,
    pub service_name: String,
    pub consumer_handle: String,
}

/// Signed `list_access` request body.
#[derive(Debug, Clone, Serialize)]
pub struct ListAccessPayload {
    pub handle: String,
    pub service_name: String,
}

/// Signed `set_visibility` request body.
#[derive(Debug, Clone, Serialize)]
pub struct SetVisibilityPayload {
    pub handle: String,
    pub service_name: String,
    pub visibility: String,
}

impl Payload for JoinPayload {
    const ACTION: &'static str = "join";
}

impl Payload for VerifyPayload {
    const ACTION: &'static str = "verify";
}

impl Payload for ResendPayload {
    const ACTION: &'static str = "resend_verification";
}

impl Payload for HandleStatusPayload {
    const ACTION: &'static str = "handle_status";
}

impl Payload for UpdateProfilePayload {
    const ACTION: &'static str = "update_profile";
}

impl Payload for PublishPayload {
    const ACTION: &'static str = "publish";
}

impl Payload for UnpublishPayload {
    const ACTION: &'static str = "unpublish";
}

impl Payload for UpdateServicePayload {
    const ACTION: &'static str = "update_service";
}

impl Payload for UpdateDelegationPayload {
    const ACTION: &'static str = "update_delegation";
}

impl Payload for GrantAccessPayload {
    const ACTION: &'static str = "grant_access";
}

impl Payload for RevokeAccessPayload {
    const ACTION: &'static str = "revoke_access";
}

impl Payload for ListAccessPayload {
    const ACTION: &'static str = "list_access";
}

impl Payload for SetVisibilityPayload {
    const ACTION: &'static str = "set_visibility";
}
