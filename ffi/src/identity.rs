//! ABI 7.7: gateway identity for bindings acting as edges, and the
//! `aster.ops.NodeConfig` admin client (design
//! `docs/_internal/web_gateway/gateway-identity.md`).
//!
//! ```text
//!   aster_enrollment_mint_user_token(rt, e, json, *out)   edge-minted token for a user
//!                                                         (Rust AsterClient::user_token_issuer
//!                                                         + Issuer::mint_user_token)
//!   aster_call_options_t.metadata  "aster-on-behalf-of"   the token on one call
//!                                                         (Rust CallOptions::on_behalf_of)
//!   aster_enrollment_publisher_notice(rt, e, iss, kid)    an unknown kid seen on a forwarded
//!   aster_enrollment_publisher_notice_token(rt, e, jwt)   provider token: refresh its issuer
//!   aster_enrollment_publisher_status(rt, e, *out)        what the publisher last published
//!   aster_node_config_call(rt, conn, method, json, …)     aster.ops.NodeConfig get / set /
//!                                                         clear / ack (NodeConfigClient)
//! ```
//!
//! The node's `[identity]` settings (`aster_config_set_identity`) are kept
//! with it from `aster_node_start` / `aster_node_start_enrolled`, for the
//! server built on it and for a client's publisher.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::rpc::jwks_publisher::{IssuerKeyPublisher, PublisherHandle};
use aster::rpc::node_config::{
    AttrValue, IdentityOverride, NodeConfigAck, NodeConfigAckRequest, NodeConfigClearRequest,
    NodeConfigSetRequest, NodeConfigView, SectionOverride,
};
use aster::rpc::{CallOptions, NodeConfigClient, RpcConnection};
use aster::{AsterConfig, IdentityPublisherConfig};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::data::{bridge_for, in_slice, in_string, invalid, not_found, spawn_op_coded, Done};
use crate::*;

// ── The node's identity settings ────────────────────────────────────────────

static NODE_IDENTITY: Mutex<Option<HashMap<u64, Arc<AsterConfig>>>> = Mutex::new(None);

/// Keep `cfg`'s `[identity]` / `[remote_config]` (and their sources) with
/// `node`, for `aster_server_builder_new`.
pub(crate) fn remember_node_identity(node: aster_node_t, cfg: &AsterConfig) {
    let mut kept = AsterConfig::default();
    crate::config_identity::IdentityConfigSettings::from_config(cfg).apply(&mut kept);
    NODE_IDENTITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(node, Arc::new(kept));
}

pub(crate) fn forget_node_identity(node: aster_node_t) {
    if let Some(m) = NODE_IDENTITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        m.remove(&node);
    }
}

/// The identity settings `node` was started with (defaults if unknown).
pub(crate) fn node_identity(node: aster_node_t) -> Arc<AsterConfig> {
    NODE_IDENTITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&node).cloned())
        .unwrap_or_default()
}

/// `aster_node_start` (no Enrollment): `[identity.publisher]` needs
/// Enrollment, as `AsterClient` / `AsterServer` refuse it without.
pub(crate) fn check_unenrolled(cfg: &AsterConfig) -> Result<(), String> {
    if cfg.identity_publisher.is_some() {
        return Err(
            "[identity.publisher] needs [enrollment]: issuer keys are written into \
                    its fleet namespace"
                .into(),
        );
    }
    Ok(())
}

// ── The issuer-key publisher on an enrolled node ────────────────────────────

/// The publisher an Enrollment's node runs: a client's own (delivering to
/// the servers it enrolled with), or a server's (`aster_server_start`).
#[derive(Default)]
pub(crate) struct PublisherSlot {
    /// A client's `[identity.publisher]` (validated at start) and its
    /// key-set age bound.
    pub(crate) config: Option<(IdentityPublisherConfig, u64)>,
    owned: Option<IssuerKeyPublisher>,
    server: Option<PublisherHandle>,
}

impl PublisherSlot {
    pub(crate) fn client(config: Option<IdentityPublisherConfig>, max_age: u64) -> Self {
        Self {
            config: config.map(|c| (c, max_age)),
            ..Self::default()
        }
    }

    pub(crate) fn handle(&self) -> Option<PublisherHandle> {
        self.server
            .clone()
            .or_else(|| self.owned.as_ref().map(IssuerKeyPublisher::handle))
    }

    pub(crate) fn set_server(&mut self, handle: Option<PublisherHandle>) {
        self.server = handle;
    }

    /// A client enrolled with `server`: start the publisher on the first
    /// server, and add each later one as a delivery target of the running
    /// publisher (no restart: its schedule, status and any fetch in flight
    /// carry on). No-op without `[identity.publisher]`. An error is only a
    /// failed first start; the next enrollment tries again.
    pub(crate) fn enrolled(
        &mut self,
        node: &aster::Node,
        enrollment: &aster::enrollment::Enrollment,
        server: aster::NodeId,
    ) -> aster::Result<()> {
        let Some((config, max_age)) = &self.config else {
            return Ok(());
        };
        if let Some(publisher) = &self.owned {
            publisher.add_delivery_target(server);
            return Ok(());
        }
        let publisher =
            IssuerKeyPublisher::start(node, enrollment.clone(), config, *max_age, vec![server])?;
        self.owned = Some(publisher);
        Ok(())
    }

    pub(crate) fn release(&mut self) {
        self.owned = None;
        self.server = None;
    }
}

unsafe fn publisher_of(
    runtime: aster_runtime_t,
    enrollment: crate::enrollment::aster_enrollment_t,
) -> Result<PublisherHandle, i32> {
    let bridge = bridge_for(runtime)?;
    let state = crate::enrollment::enrollment_state(&bridge, enrollment)?;
    let handle = state.publisher_handle();
    handle.ok_or_else(|| {
        not_found("this node runs no issuer-key publisher ([identity.publisher] is not configured)")
    })
}

/// Tell the node's issuer-key publisher (`[identity.publisher]`) that a
/// token for issuer `iss` carried key id `kid`: when the last published set
/// lacks it, the issuer is refreshed at once (at most every
/// `min_refresh_secs`; a notice in the gap is deferred, not dropped). Rust
/// `PublisherHandle::notice_kid`. An edge calls it for every provider token
/// it forwards (or uses `aster_enrollment_publisher_notice_token`).
/// `NOT_FOUND` when the node runs no publisher.
///
/// # Safety
/// `iss` and `kid` must be readable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_publisher_notice(
    runtime: aster_runtime_t,
    enrollment: crate::enrollment::aster_enrollment_t,
    iss: aster_bytes_t,
    kid: aster_bytes_t,
) -> i32 {
    let iss = match in_string(&iss, "iss") {
        Ok(s) => s,
        Err(status) => return status,
    };
    let kid = match in_string(&kid, "kid") {
        Ok(s) => s,
        Err(status) => return status,
    };
    match publisher_of(runtime, enrollment) {
        Ok(handle) => {
            handle.notice_kid(&iss, &kid);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(status) => status,
    }
}

/// As `aster_enrollment_publisher_notice`, reading `iss` and `kid` from a
/// compact JWT (unverified; a token that does not parse is ignored). Rust
/// `PublisherHandle::notice_token`.
///
/// # Safety
/// `token` must be readable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_publisher_notice_token(
    runtime: aster_runtime_t,
    enrollment: crate::enrollment::aster_enrollment_t,
    token: aster_bytes_t,
) -> i32 {
    let token = match in_string(&token, "token") {
        Ok(s) => s,
        Err(status) => return status,
    };
    match publisher_of(runtime, enrollment) {
        Ok(handle) => {
            handle.notice_token(&token);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(status) => status,
    }
}

/// The publisher's state per issuer as owned JSON (`aster_string_release`):
/// `{"<iss>": {"kids": ["…"], "fetched_at": <unix s>|null, "publications":
/// n, "attempts": n, "last_error": "…"|null}}`. `NOT_FOUND` when the node runs no publisher.
///
/// # Safety
/// `out_json` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_publisher_status(
    runtime: aster_runtime_t,
    enrollment: crate::enrollment::aster_enrollment_t,
    out_json: *mut aster_bytes_t,
) -> i32 {
    if out_json.is_null() {
        return invalid("aster_enrollment_publisher_status: out_json is NULL");
    }
    ptr::write(
        out_json,
        aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        },
    );
    let handle = match publisher_of(runtime, enrollment) {
        Ok(handle) => handle,
        Err(status) => return status,
    };
    let status: Map<String, Value> = handle
        .status()
        .into_iter()
        .map(|(iss, s)| (iss, publish_status_json(&s)))
        .collect();
    ptr::write(
        out_json,
        alloc_owned_bytes(Value::Object(status).to_string().into_bytes()),
    );
    aster_status_t::ASTER_STATUS_OK as i32
}

fn publish_status_json(s: &aster::rpc::jwks_publisher::IssuerPublishStatus) -> Value {
    json!({
        "kids": s.kids,
        "fetched_at": s.fetched_at,
        "publications": s.publications,
        "attempts": s.attempts,
        "last_error": s.last_error.as_ref().map(ToString::to_string),
    })
}

// ── Edge-minted tokens ──────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MintRequest {
    sub: String,
    #[serde(default)]
    roles: Vec<String>,
    amr: Vec<String>,
    ttl_secs: u64,
}

/// Mint an edge token for a user (Rust `AsterClient::user_token_issuer` +
/// `Issuer::mint_user_token`): signed with this node's key, bounded by the
/// issuer delegation (`aster.identity.issuable_roles`) in the credential the
/// enrollment holds now. Attach it to calls as the `aster-on-behalf-of`
/// metadata entry; the node needs `aster.gateway`.
///
/// `request_json`: `{"sub": "…", "roles": ["…"], "amr": ["webauthn"],
/// "ttl_secs": 60}` (`amr` at least one entry; `ttl_secs` 1..=300). On
/// success `*out_token` is the compact JWT, owned (`aster_string_release`).
/// A role outside the delegation, a bad lifetime or value is
/// `INVALID_ARGUMENT`.
///
/// # Safety
/// `request_json` must be readable; `out_token` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_enrollment_mint_user_token(
    runtime: aster_runtime_t,
    enrollment: crate::enrollment::aster_enrollment_t,
    request_json: aster_bytes_t,
    out_token: *mut aster_bytes_t,
) -> i32 {
    if out_token.is_null() {
        return invalid("aster_enrollment_mint_user_token: out_token is NULL");
    }
    ptr::write(
        out_token,
        aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        },
    );
    let bridge = match bridge_for(runtime) {
        Ok(b) => b,
        Err(status) => return status,
    };
    let state = match crate::enrollment::enrollment_state(&bridge, enrollment) {
        Ok(s) => s,
        Err(status) => return status,
    };
    let raw = match in_slice(&request_json, "request_json") {
        Ok(raw) => raw,
        Err(status) => return status,
    };
    if raw.len() > 64 * 1024 {
        return invalid("request_json: larger than 64 KiB");
    }
    let request: MintRequest = match serde_json::from_slice(raw) {
        Ok(r) => r,
        Err(e) => return invalid(format!("request_json: {e}")),
    };
    let issuer = match aster::rpc::identity::Issuer::from_enrollment(&state.node, &state.enrollment)
    {
        Ok(issuer) => issuer,
        Err(e) => return invalid(e),
    };
    match issuer.mint_user_token(
        &request.sub,
        request.roles,
        request.amr,
        Duration::from_secs(request.ttl_secs),
    ) {
        Ok(token) => {
            ptr::write(out_token, alloc_owned_bytes(token.into_bytes()));
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(e) => invalid(e),
    }
}

// ── aster.ops.NodeConfig client ─────────────────────────────────────────────

/// `method` of `aster_node_config_call`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_node_config_method_t {
    ASTER_NODE_CONFIG_GET = 0,
    ASTER_NODE_CONFIG_SET = 1,
    ASTER_NODE_CONFIG_CLEAR = 2,
    ASTER_NODE_CONFIG_ACK = 3,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GetJson {
    #[serde(default)]
    deadline_ms: u64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SetJson {
    #[serde(default)]
    expected_generation: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    attributes: std::collections::BTreeMap<String, AttrValue>,
    /// `{"identity": {…}}`: section → its overridable fields.
    #[serde(default)]
    sections: Map<String, Value>,
    #[serde(default)]
    deadline_ms: u64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ClearJson {
    #[serde(default)]
    expected_generation: u64,
    #[serde(default)]
    deadline_ms: u64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AckJson {
    generation: u64,
    #[serde(default)]
    timeout_ms: u64,
    #[serde(default)]
    deadline_ms: u64,
}

fn value_json(value: Option<&aster::rpc::baseline::Value>) -> Value {
    match value.and_then(AttrValue::from_value) {
        None => Value::Null,
        Some(AttrValue::Bool(v)) => Value::Bool(v),
        Some(AttrValue::Int(v)) => json!(v),
        Some(AttrValue::Float(v)) => json!(v),
        Some(AttrValue::String(v)) => Value::String(v),
    }
}

fn section_json(section: &SectionOverride) -> Value {
    serde_json::from_str(&section.settings_json)
        .unwrap_or_else(|_| Value::String(section.settings_json.clone()))
}

fn view_json(view: &NodeConfigView) -> Value {
    json!({
        "node_id": view.node_id,
        "generation": view.generation,
        "applied_generation": view.applied_generation,
        "change": view.change,
        "changed_by": view.changed_by,
        "changed_at_ms": view.changed_at_ms,
        "name": view.name,
        "tags": view.tags,
        "attributes": view.typed_attributes(),
        "sections": view.sections.iter()
            .map(|s| (s.section.clone(), section_json(s)))
            .collect::<Map<String, Value>>(),
        "ignored_sections": view.ignored_sections,
        "settings": view.settings.iter().map(|s| json!({
            "key": s.key,
            "value": value_json(s.value.as_ref()),
            "source": s.source,
        })).collect::<Vec<_>>(),
        "overridable_sections": view.overridable_sections,
        "locked_sections": view.locked_sections,
    })
}

fn ack_json(ack: &NodeConfigAck) -> Value {
    json!({
        "complete": ack.complete,
        "generation": ack.generation,
        "applied_generation": ack.applied_generation,
        "outstanding": ack.outstanding.iter().map(|o| json!({
            "part": o.part,
            "state": o.state,
            "detail": o.detail,
        })).collect::<Vec<_>>(),
    })
}

fn options(deadline_ms: u64) -> CallOptions {
    let options = CallOptions::new();
    if deadline_ms == 0 {
        options
    } else {
        options.deadline(Duration::from_millis(deadline_ms))
    }
}

fn generation(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

enum NodeConfigRequest {
    Get(GetJson),
    Set(Box<SetJson>),
    Clear(ClearJson),
    Ack(AckJson),
}

/// Call `aster.ops.NodeConfig` on the server at the other end of
/// `connection` (an `aster/1` connection), as Rust's `NodeConfigClient`
/// does. The caller needs `aster.config-admin` from its own Enrollment
/// credential for `set` / `clear` (Gate 3 checks the role on every method).
///
/// `request_json` per `method` (all fields optional unless noted; every
/// request also takes `deadline_ms`, 0 = none):
///
/// - `GET`: `{}`.
/// - `SET`: `{"expected_generation": 0, "name": "…", "tags": ["…"],
///   "attributes": {"k": {"type": "bool"|"int"|"float"|"string", "value": …}},
///   "sections": {"identity": {"accept_on_behalf_of": true, "trusted_issuers":
///   [...], "edge_token_max_lifetime_secs": n, "clock_leeway_secs": n,
///   "max_key_set_age_secs": n}}}` (replaces the whole record).
/// - `CLEAR`: `{"expected_generation": 0}`.
/// - `ACK`: `{"generation": n (required), "timeout_ms": 0}`.
///
/// Completes with `ASTER_EVENT_NODE_CONFIG_RESULT` (`handle` = `connection`)
/// whose JSON payload is `{"code": 0, "result": …}` or, for a refused or
/// failed call, `{"code": <RPC status code>, "message": "…"}`. `GET`'s
/// result is the view `{node_id, generation, applied_generation, change,
/// changed_by, changed_at_ms, name, tags, attributes, sections,
/// ignored_sections, settings: [{key, value, source}], overridable_sections,
/// locked_sections}`; the others' is the acknowledgement `{complete,
/// generation, applied_generation, outstanding: [{part, state, detail}]}`.
///
/// # Safety
/// `request_json` must be readable; `out_operation` writable.
#[no_mangle]
pub unsafe extern "C" fn aster_node_config_call(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    method: u32,
    request_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_node_config_call: out_operation is NULL");
    }
    let bridge = match bridge_for(runtime) {
        Ok(b) => b,
        Err(status) => return status,
    };
    let Some(conn) = bridge.connections.get(connection) else {
        return not_found(format!("unknown connection {connection}"));
    };
    let raw = match in_slice(&request_json, "request_json") {
        Ok(raw) => raw,
        Err(status) => return status,
    };
    if raw.len() > 256 * 1024 {
        return invalid("request_json: larger than 256 KiB");
    }
    let raw = if raw.is_empty() {
        b"{}".as_slice()
    } else {
        raw
    };
    fn parse<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, i32> {
        serde_json::from_slice(raw).map_err(|e| invalid(format!("request_json: {e}")))
    }
    use aster_node_config_method_t as M;
    let request = match method {
        x if x == M::ASTER_NODE_CONFIG_GET as u32 => parse(raw).map(NodeConfigRequest::Get),
        x if x == M::ASTER_NODE_CONFIG_SET as u32 => {
            parse(raw).map(|r| NodeConfigRequest::Set(Box::new(r)))
        }
        x if x == M::ASTER_NODE_CONFIG_CLEAR as u32 => parse(raw).map(NodeConfigRequest::Clear),
        x if x == M::ASTER_NODE_CONFIG_ACK as u32 => parse(raw).map(NodeConfigRequest::Ack),
        other => Err(invalid(format!("unknown node-config method {other}"))),
    };
    let request = match request {
        Ok(r) => r,
        Err(status) => return status,
    };
    // A set's sections are checked here, so a typo fails the call rather
    // than reaching the server as an opaque section.
    if let NodeConfigRequest::Set(set) = &request {
        for (name, value) in &set.sections {
            if !value.is_object() {
                return invalid(format!("sections.{name}: expected a JSON object"));
            }
            if name == aster::rpc::node_config::IDENTITY_SECTION {
                if let Err(e) = serde_json::from_value::<IdentityOverride>(value.clone()) {
                    return invalid(format!("sections.identity: {e}"));
                }
            }
        }
    }
    let rpc = RpcConnection::from_core_connection((*conn).clone());
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let outcome = run_node_config(rpc, request).await;
        let payload = match outcome {
            Ok(result) => json!({"code": 0, "result": result}),
            Err(error) => {
                let status = aster::rpc::status_from_error(&error);
                json!({"code": status.code as i32, "message": status.message})
            }
        };
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_NODE_CONFIG_RESULT,
            connection,
        )
        .data(payload.to_string().into_bytes()))
    })
}

async fn run_node_config(rpc: RpcConnection, request: NodeConfigRequest) -> aster::Result<Value> {
    let client = NodeConfigClient::new(rpc);
    Ok(match request {
        NodeConfigRequest::Get(r) => {
            view_json(&client.with_options(options(r.deadline_ms)).get().await?)
        }
        NodeConfigRequest::Set(r) => {
            let mut req = NodeConfigSetRequest::new().name(r.name);
            req.expected_generation = generation(r.expected_generation);
            req.tags = r.tags;
            for (key, value) in r.attributes {
                req = req.attribute(key, value);
            }
            for (section, value) in r.sections {
                req = req.section(SectionOverride {
                    section,
                    settings_json: value.to_string(),
                });
            }
            ack_json(&client.with_options(options(r.deadline_ms)).set(req).await?)
        }
        NodeConfigRequest::Clear(r) => ack_json(
            &client
                .with_options(options(r.deadline_ms))
                .clear(NodeConfigClearRequest {
                    expected_generation: generation(r.expected_generation),
                })
                .await?,
        ),
        NodeConfigRequest::Ack(r) => ack_json(
            &client
                .with_options(options(r.deadline_ms))
                .ack(NodeConfigAckRequest {
                    generation: generation(r.generation),
                    timeout_ms: generation(r.timeout_ms),
                })
                .await?,
        ),
    })
}

// ── Handler-side views ──────────────────────────────────────────────────────

/// The NodeConfig properties as JSON: `{"generation": n, "name": "…",
/// "tags": ["…"], "attributes": {"k": {"type": …, "value": …}}}`.
pub(crate) fn properties_json(generation: u64, properties: &aster::rpc::NodeProperties) -> Vec<u8> {
    json!({
        "generation": generation,
        "name": properties.name(),
        "tags": properties.tags(),
        "attributes": properties.attributes(),
    })
    .to_string()
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_json_parses_the_documented_shape() {
        let set: SetJson = serde_json::from_str(
            r#"{"expected_generation": 2, "name": "edge-1", "tags": ["eu"],
                "attributes": {"tier": {"type": "int", "value": 3}},
                "sections": {"identity": {"accept_on_behalf_of": true}}}"#,
        )
        .unwrap();
        assert_eq!(set.expected_generation, 2);
        assert_eq!(set.attributes["tier"], AttrValue::Int(3));
        assert!(set.sections["identity"].is_object());
        assert!(serde_json::from_str::<SetJson>(r#"{"bogus": 1}"#).is_err());
        assert!(serde_json::from_str::<AckJson>("{}").is_err());
    }

    #[test]
    fn properties_json_shape() {
        let json: Value =
            serde_json::from_slice(&properties_json(0, &aster::rpc::NodeProperties::default()))
                .unwrap();
        assert_eq!(json["generation"], 0);
        assert_eq!(json["name"], "");
        assert!(json["attributes"].as_object().unwrap().is_empty());
    }

    #[test]
    fn mint_request_requires_amr_and_ttl() {
        assert!(serde_json::from_str::<MintRequest>(r#"{"sub": "bob"}"#).is_err());
        let r: MintRequest =
            serde_json::from_str(r#"{"sub": "bob", "amr": ["webauthn"], "ttl_secs": 60}"#).unwrap();
        assert!(r.roles.is_empty());
    }
}
