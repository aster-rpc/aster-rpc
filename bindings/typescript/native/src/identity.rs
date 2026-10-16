//! Gateway identity helpers for JavaScript (design
//! `docs/_internal/web_gateway/gateway-identity.md`): the
//! `aster.ops.NodeConfig` admin client over an `aster/1` connection (Rust
//! `NodeConfigClient`) and `bearerToken`. Edge-minted tokens and publisher
//! notices are on `AsterClient` (`aster_client.rs`); the handler side is on
//! the dispatch event (`rpc.rs`).
//!
//! NodeConfig requests and answers cross as JSON (the C ABI's
//! `aster_node_config_call` shapes): the wire types are Fory structs with a
//! `Value` union, which this avoids mirroring type by type.

use std::collections::BTreeMap;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use aster::rpc::node_config::{
    AttrValue, IdentityOverride, NodeConfigAck, NodeConfigAckRequest, NodeConfigClearRequest,
    NodeConfigSetRequest, NodeConfigView, SectionOverride, IDENTITY_SECTION,
};
use aster::rpc::{CallOptions, NodeConfigClient, RpcConnection};

use crate::net::IrohConnection;
use crate::rpc_client::rpc_err;

/// The token of an `Authorization: Bearer <token>` header value (Rust
/// `rpc::bearer_token`); `null` for any other scheme or an empty token.
#[napi]
pub fn bearer_token(authorization: String) -> Option<String> {
    aster::rpc::identity::bearer_token(&authorization).map(str::to_owned)
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
    attributes: BTreeMap<String, AttrValue>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckJson {
    generation: u64,
    #[serde(default)]
    timeout_ms: u64,
    #[serde(default)]
    deadline_ms: u64,
}

enum Request {
    Get(GetJson),
    Set(Box<SetJson>),
    Clear(ClearJson),
    Ack(AckJson),
}

fn invalid(msg: impl std::fmt::Display) -> Error {
    Error::new(Status::InvalidArg, msg.to_string())
}

fn parse<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|e| invalid(format!("node config request: {e}")))
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
            .map(|s| {
                let settings = serde_json::from_str(&s.settings_json)
                    .unwrap_or_else(|_| Value::String(s.settings_json.clone()));
                (s.section.clone(), settings)
            })
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

/// Call `aster.ops.NodeConfig` `method` (`get` / `set` / `clear` / `ack`) on
/// the server at the other end of `connection` (an `aster/1` connection),
/// as Rust's `NodeConfigClient`. The caller needs `aster.config-admin` from
/// its own Enrollment credential (Gate 3 checks it on every method).
///
/// `requestJson` (snake_case, every field optional unless noted, plus
/// `deadline_ms`): `get` `{}`; `set` `{expected_generation, name, tags,
/// attributes: {k: {type, value}}, sections: {identity: {…}}}` (replaces
/// the whole record); `clear` `{expected_generation}`; `ack` `{generation
/// (required), timeout_ms}`. Resolves the view (`get`) or the
/// acknowledgement as JSON; a refused call rejects with its RPC status.
#[napi(ts_return_type = "Promise<string>")]
pub fn node_config_call<'env>(
    env: &'env Env,
    connection: &IrohConnection,
    method: String,
    request_json: String,
) -> Result<PromiseRaw<'env, String>> {
    if request_json.len() > 256 * 1024 {
        return Err(invalid("node config request: larger than 256 KiB"));
    }
    let raw = if request_json.trim().is_empty() {
        "{}"
    } else {
        request_json.as_str()
    };
    let request = match method.as_str() {
        "get" => Request::Get(parse(raw)?),
        "set" => Request::Set(Box::new(parse(raw)?)),
        "clear" => Request::Clear(parse(raw)?),
        "ack" => Request::Ack(parse(raw)?),
        other => {
            return Err(invalid(format!(
                "unknown node config method {other:?} (get | set | clear | ack)"
            )))
        }
    };
    // A typo in a section fails here, not as an opaque section on the server.
    if let Request::Set(set) = &request {
        for (name, value) in &set.sections {
            if !value.is_object() {
                return Err(invalid(format!("sections.{name}: expected a JSON object")));
            }
            if name == IDENTITY_SECTION {
                serde_json::from_value::<IdentityOverride>(value.clone())
                    .map_err(|e| invalid(format!("sections.identity: {e}")))?;
            }
        }
    }
    let rpc = RpcConnection::from_core_connection(connection.core_clone());
    env.spawn_future(async move {
        let result = run(rpc, request).await.map_err(rpc_err)?;
        Ok(result.to_string())
    })
}

async fn run(rpc: RpcConnection, request: Request) -> aster::Result<Value> {
    let client = NodeConfigClient::new(rpc);
    Ok(match request {
        Request::Get(r) => view_json(&client.with_options(options(r.deadline_ms)).get().await?),
        Request::Set(r) => {
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
        Request::Clear(r) => ack_json(
            &client
                .with_options(options(r.deadline_ms))
                .clear(NodeConfigClearRequest {
                    expected_generation: generation(r.expected_generation),
                })
                .await?,
        ),
        Request::Ack(r) => ack_json(
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
