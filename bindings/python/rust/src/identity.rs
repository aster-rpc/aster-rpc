//! Gateway identity for Python (design
//! `docs/_internal/web_gateway/gateway-identity.md`): thin wrappers over
//! `aster::rpc::identity` and `aster::rpc::node_config`.
//!
//! - [`ForwardedIdentity`]: the verified user of a call an edge made on its
//!   behalf (`ServerCall.on_behalf_of`, Rust `CallContext::on_behalf_of`).
//! - [`NativeNodeConfigClient`]: `aster.ops.NodeConfig` get / set / clear /
//!   ack over one `aster/1` connection (Rust `NodeConfigClient`); the Python
//!   `aster.identity.NodeConfigClient` wraps the dicts it returns.
//! - `bearer_token` and the role / metadata / sign-in constants.
//!
//! The server wiring (`[identity]`, `[remote_config]`) is in `rpc.rs`
//! (`DispatchServer`), the edge's minting and publisher notices in
//! `enrollment.rs` (`Enrollment`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyString};
use pyo3_async_runtimes::tokio::future_into_py;
use serde_json::{json, Map, Value};

use aster::rpc::node_config::{
    AttrValue, IdentityOverride, NodeConfigAck, NodeConfigAckRequest, NodeConfigClearRequest,
    NodeConfigSetRequest, NodeConfigView, SectionOverride, IDENTITY_SECTION,
};
use aster::rpc::{CallOptions, NodeConfigClient, NodeProperties, RpcConnection};

use crate::net::IrohConnection;

// ── Handler side ────────────────────────────────────────────────────────────

/// The verified user of a call a gateway made on its behalf (Rust
/// `aster::rpc::identity::ForwardedIdentity`).
#[pyclass(frozen, module = "aster._aster")]
pub struct ForwardedIdentity(pub(crate) Arc<aster::rpc::identity::ForwardedIdentity>);

#[pymethods]
impl ForwardedIdentity {
    /// `iss|sub`: the user's stable identity (the call's principal).
    #[getter]
    fn principal(&self) -> &str {
        self.0.principal()
    }
    /// The edge that forwarded the call (its node id, the transport peer).
    #[getter]
    fn actor(&self) -> &str {
        self.0.actor()
    }
    /// The token's `iss`; for an edge-minted token, the edge's node id.
    #[getter]
    fn issuer(&self) -> &str {
        self.0.issuer()
    }
    /// The token's `sub`.
    #[getter]
    fn subject(&self) -> &str {
        self.0.subject()
    }
    /// The token's roles, sorted: what Gate 3 checked.
    #[getter]
    fn roles(&self) -> Vec<String> {
        self.0.roles().to_vec()
    }
    /// The token's `amr`: how the user signed in.
    #[getter]
    fn auth_methods(&self) -> Vec<String> {
        self.0.auth_methods().to_vec()
    }
    /// The token's `jti`.
    #[getter]
    fn jti(&self) -> &str {
        self.0.jti()
    }
    /// The token's `exp`, in Unix seconds.
    #[getter]
    fn expires_at(&self) -> u64 {
        self.0.expires_at()
    }

    fn __repr__(&self) -> String {
        format!(
            "ForwardedIdentity(principal={:?}, actor={:?}, auth_methods={:?})",
            self.0.principal(),
            self.0.actor(),
            self.0.auth_methods()
        )
    }
}

pub(crate) fn attr_to_py<'py>(py: Python<'py>, value: &AttrValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        AttrValue::Bool(v) => PyBool::new(py, *v).to_owned().into_any(),
        AttrValue::Int(v) => v.into_pyobject(py)?.into_any(),
        AttrValue::Float(v) => PyFloat::new(py, *v).into_any(),
        AttrValue::String(v) => PyString::new(py, v).into_any(),
    })
}

fn attr_from_py(key: &str, value: &Bound<'_, PyAny>) -> PyResult<AttrValue> {
    if value.is_instance_of::<PyBool>() {
        return Ok(AttrValue::Bool(value.extract()?));
    }
    if value.is_instance_of::<PyInt>() {
        return Ok(AttrValue::Int(value.extract()?));
    }
    if value.is_instance_of::<PyFloat>() {
        return Ok(AttrValue::Float(value.extract()?));
    }
    if value.is_instance_of::<PyString>() {
        return Ok(AttrValue::String(value.extract()?));
    }
    Err(PyValueError::new_err(format!(
        "attributes.{key}: expected bool, int, float or str"
    )))
}

/// `NodeProperties` as `{"generation", "name", "tags", "attributes"}`.
pub(crate) fn properties_to_py<'py>(
    py: Python<'py>,
    generation: u64,
    properties: &NodeProperties,
) -> PyResult<Bound<'py, PyDict>> {
    let attributes = PyDict::new(py);
    for (key, value) in properties.attributes() {
        attributes.set_item(key, attr_to_py(py, value)?)?;
    }
    let out = PyDict::new(py);
    out.set_item("generation", generation)?;
    out.set_item("name", properties.name())?;
    out.set_item("tags", properties.tags().to_vec())?;
    out.set_item("attributes", attributes)?;
    Ok(out)
}

/// The token of an `Authorization: Bearer <token>` header value (Rust
/// `aster::rpc::bearer_token`), for forwarding a provider's token unchanged.
#[pyfunction]
fn bearer_token(authorization: &str) -> Option<String> {
    aster::rpc::identity::bearer_token(authorization).map(str::to_string)
}

// ── aster.ops.NodeConfig client ─────────────────────────────────────────────

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
    let attributes: Map<String, Value> = view
        .typed_attributes()
        .into_iter()
        .map(|(k, v)| (k, value_json(Some(&v.to_value()))))
        .collect();
    json!({
        "node_id": view.node_id,
        "generation": view.generation,
        "applied_generation": view.applied_generation,
        "change": view.change,
        "changed_by": view.changed_by,
        "changed_at_ms": view.changed_at_ms,
        "name": view.name,
        "tags": view.tags,
        "attributes": attributes,
        "sections": view.sections.iter()
            .map(|s| (
                s.section.clone(),
                serde_json::from_str(&s.settings_json)
                    .unwrap_or_else(|_| Value::String(s.settings_json.clone())),
            ))
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

fn json_to_py(value: &Value) -> PyResult<Py<PyAny>> {
    Python::attach(|py| {
        Ok(py
            .import("json")?
            .call_method1("loads", (value.to_string(),))?
            .unbind())
    })
}

fn options(deadline: Option<f64>) -> CallOptions {
    match deadline {
        Some(secs) if secs > 0.0 && secs.is_finite() => {
            CallOptions::new().deadline(Duration::from_secs_f64(secs))
        }
        _ => CallOptions::new(),
    }
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `aster.ops.NodeConfig` on the server at the other end of one `aster/1`
/// connection (Rust `NodeConfigClient`). Every method needs
/// `aster.config-admin` in the caller's credential (Gate 3). Results are
/// plain dicts; `aster.identity.NodeConfigClient` is the typed surface.
#[pyclass(frozen, module = "aster._aster", name = "NativeNodeConfigClient")]
pub struct NativeNodeConfigClient {
    conn: RpcConnection,
}

#[pymethods]
impl NativeNodeConfigClient {
    #[new]
    fn new(connection: PyRef<'_, IrohConnection>) -> Self {
        Self {
            conn: RpcConnection::from_core_connection(connection.core_clone()),
        }
    }

    /// The node's record, effective settings and their sources.
    #[pyo3(signature = (*, deadline=None))]
    fn get<'py>(&self, py: Python<'py>, deadline: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let client = NodeConfigClient::new(self.conn.clone()).with_options(options(deadline));
        future_into_py(py, async move {
            let view = client.get().await.map_err(crate::rpc::client_err)?;
            json_to_py(&view_json(&view))
        })
    }

    /// Replace the whole record: `attributes` maps keys to bool / int /
    /// float / str, `sections` maps an overridable section (`identity`) to
    /// its fields. Returns the acknowledgement dict.
    #[pyo3(signature = (*, name=String::new(), tags=Vec::new(), attributes=None, sections=None, expected_generation=0, deadline=None))]
    #[allow(clippy::too_many_arguments)]
    fn set<'py>(
        &self,
        py: Python<'py>,
        name: String,
        tags: Vec<String>,
        attributes: Option<Bound<'py, PyDict>>,
        sections: Option<Bound<'py, PyDict>>,
        expected_generation: u64,
        deadline: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut req = NodeConfigSetRequest::new().name(name);
        req.expected_generation = to_i64(expected_generation);
        req.tags = tags;
        if let Some(attributes) = attributes {
            let mut typed = BTreeMap::new();
            for (key, value) in attributes.iter() {
                let key: String = key.extract()?;
                typed.insert(key.clone(), attr_from_py(&key, &value)?);
            }
            for (key, value) in typed {
                req = req.attribute(key, value);
            }
        }
        if let Some(sections) = sections {
            let dumps = py.import("json")?.getattr("dumps")?;
            for (section, value) in sections.iter() {
                let section: String = section.extract()?;
                let text: String = dumps.call1((value,))?.extract()?;
                let parsed: Value = serde_json::from_str(&text)
                    .map_err(|e| PyValueError::new_err(format!("sections.{section}: {e}")))?;
                if !parsed.is_object() {
                    return Err(PyValueError::new_err(format!(
                        "sections.{section}: expected a dict"
                    )));
                }
                if section == IDENTITY_SECTION {
                    serde_json::from_value::<IdentityOverride>(parsed.clone())
                        .map_err(|e| PyValueError::new_err(format!("sections.identity: {e}")))?;
                }
                req = req.section(SectionOverride {
                    section,
                    settings_json: parsed.to_string(),
                });
            }
        }
        let client = NodeConfigClient::new(self.conn.clone()).with_options(options(deadline));
        future_into_py(py, async move {
            let ack = client.set(req).await.map_err(crate::rpc::client_err)?;
            json_to_py(&ack_json(&ack))
        })
    }

    /// Remove the record: the node runs on its file settings again.
    #[pyo3(signature = (*, expected_generation=0, deadline=None))]
    fn clear<'py>(
        &self,
        py: Python<'py>,
        expected_generation: u64,
        deadline: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = NodeConfigClient::new(self.conn.clone()).with_options(options(deadline));
        future_into_py(py, async move {
            let ack = client
                .clear(NodeConfigClearRequest {
                    expected_generation: to_i64(expected_generation),
                })
                .await
                .map_err(crate::rpc::client_err)?;
            json_to_py(&ack_json(&ack))
        })
    }

    /// Wait up to `timeout` seconds for `generation` to take effect.
    #[pyo3(signature = (generation, *, timeout=0.0, deadline=None))]
    fn ack<'py>(
        &self,
        py: Python<'py>,
        generation: u64,
        timeout: f64,
        deadline: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout_ms = if timeout > 0.0 && timeout.is_finite() {
            (timeout * 1000.0) as i64
        } else {
            0
        };
        let client = NodeConfigClient::new(self.conn.clone()).with_options(options(deadline));
        future_into_py(py, async move {
            let ack = client
                .ack(NodeConfigAckRequest {
                    generation: to_i64(generation),
                    timeout_ms,
                })
                .await
                .map_err(crate::rpc::client_err)?;
            json_to_py(&ack_json(&ack))
        })
    }
}

pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    use aster::rpc::identity as id;
    m.add_class::<ForwardedIdentity>()?;
    m.add_class::<NativeNodeConfigClient>()?;
    m.add_function(wrap_pyfunction!(bearer_token, m)?)?;
    m.add("ON_BEHALF_OF_METADATA", id::ON_BEHALF_OF_METADATA)?;
    m.add("GATEWAY_ROLE", id::GATEWAY_ROLE)?;
    m.add("AMR_WEBAUTHN", id::AMR_WEBAUTHN)?;
    m.add("AMR_PASSWORD", id::AMR_PASSWORD)?;
    m.add("AMR_API_KEY", id::AMR_API_KEY)?;
    m.add("ISSUABLE_ROLES_ATTRIBUTE", id::ISSUABLE_ROLES_ATTRIBUTE)?;
    m.add(
        "IDENTITY_PUBLISHER_ROLE",
        aster::rpc::issuer_keys::IDENTITY_PUBLISHER_ROLE,
    )?;
    m.add("CONFIG_ADMIN_ROLE", aster::rpc::CONFIG_ADMIN_ROLE)?;
    Ok(())
}
