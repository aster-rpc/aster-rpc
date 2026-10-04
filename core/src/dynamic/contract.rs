//! [`DynContract`]: a service's methods bound to a [`DynSchema`].

use super::schema::DynSchema;
use super::value::{DynError, DynResult, DynValue};
use super::{DecodeOptions, EncodeOptions, JsonOptions};
use crate::contract::{
    compute_contract_id, compute_type_hash, decode_service_contract_bytes, decode_type_def_bytes,
    ContractManifest, MethodPattern, ServiceContract, TypeDef,
};

/// One method of a [`DynContract`].
#[derive(Clone, Debug)]
pub struct DynMethod {
    pub name: String,
    pub pattern: MethodPattern,
    /// Request type, `package.Name` (or the unresolved hash when the schema
    /// lacks it — calls then fail with `UnknownType`).
    pub request_type: String,
    /// Response type, as `request_type`.
    pub response_type: String,
    pub idempotent: bool,
    /// Default timeout in seconds (0 = none).
    pub timeout: f64,
}

/// A service contract ready for dynamic calls: name, version, methods and the
/// schema of every payload type. Build it from what the registry publishes
/// ([`from_collection`](Self::from_collection) /
/// [`from_manifest`](Self::from_manifest)) or from a `ServiceContract`.
#[derive(Clone, Debug)]
pub struct DynContract {
    pub service: String,
    pub version: i32,
    /// Hex contract id (empty when built without one).
    pub contract_id: String,
    pub methods: Vec<DynMethod>,
    pub schema: DynSchema,
    /// Options [`encode_request`](Self::encode_request) uses.
    pub encode_options: EncodeOptions,
}

fn pattern_from_str(s: &str) -> DynResult<MethodPattern> {
    Ok(match s {
        "unary" => MethodPattern::Unary,
        "server_stream" => MethodPattern::ServerStream,
        "client_stream" => MethodPattern::ClientStream,
        "bidi_stream" | "bidi" => MethodPattern::BidiStream,
        other => {
            return Err(DynError::schema(format!(
                "unknown method pattern {other:?}"
            )))
        }
    })
}

impl DynContract {
    fn build(
        service: String,
        version: i32,
        contract_id: String,
        methods: Vec<(String, MethodPattern, String, String, bool, f64)>,
        schema: DynSchema,
    ) -> DynContract {
        let resolve = |t: &str| {
            schema
                .resolve(t)
                .map(str::to_string)
                .unwrap_or_else(|| t.to_string())
        };
        let methods = methods
            .into_iter()
            .map(
                |(name, pattern, req, resp, idempotent, timeout)| DynMethod {
                    name,
                    pattern,
                    request_type: resolve(&req),
                    response_type: resolve(&resp),
                    idempotent,
                    timeout,
                },
            )
            .collect();
        DynContract {
            service,
            version,
            contract_id,
            methods,
            schema,
            encode_options: EncodeOptions::default(),
        }
    }

    /// From a published manifest plus the contract's `TypeDef`s. The
    /// manifest's method `request_type` / `response_type` are type hashes.
    pub fn from_manifest(manifest: &ContractManifest, type_defs: Vec<TypeDef>) -> DynResult<Self> {
        Self::from_manifest_and_schema(manifest, DynSchema::from_type_defs(type_defs)?)
    }

    /// From a `ServiceContract` plus its `TypeDef`s.
    pub fn from_service_contract(sc: &ServiceContract, type_defs: Vec<TypeDef>) -> DynResult<Self> {
        let schema = DynSchema::from_type_defs(type_defs)?;
        let methods = sc
            .methods
            .iter()
            .map(|m| {
                (
                    m.name.clone(),
                    m.pattern,
                    m.request_type.clone(),
                    m.response_type.clone(),
                    m.idempotent,
                    m.default_timeout,
                )
            })
            .collect();
        let contract_id = crate::contract::canonical_xlang_bytes_service_contract_checked(sc)
            .map(|b| compute_contract_id(&b))
            .unwrap_or_default();
        Ok(Self::build(
            sc.name.clone(),
            sc.version,
            contract_id,
            methods,
            schema,
        ))
    }

    /// From the entries of a published contract collection: `contract.bin`
    /// (canonical `ServiceContract`), `types/{hash}.bin` (canonical
    /// `TypeDef`s) and optionally `manifest.json`. Every `types/{hash}.bin`
    /// must hash to its name; `contract_id` is `blake3(contract.bin)`.
    pub fn from_collection(entries: &[(String, Vec<u8>)]) -> DynResult<Self> {
        let contract_bytes = entries
            .iter()
            .find(|(n, _)| n == "contract.bin")
            .map(|(_, b)| b)
            .ok_or_else(|| DynError::schema("collection has no contract.bin"))?;
        let sc = decode_service_contract_bytes(contract_bytes)
            .map_err(|e| DynError::schema(format!("contract.bin: {e}")))?;
        let mut defs = Vec::new();
        for (name, bytes) in entries {
            let Some(hash) = name
                .strip_prefix("types/")
                .and_then(|n| n.strip_suffix(".bin"))
            else {
                continue;
            };
            let actual = hex::encode(compute_type_hash(bytes));
            if !actual.eq_ignore_ascii_case(hash) {
                return Err(DynError::schema(format!(
                    "{name}: content hashes to {actual}"
                )));
            }
            defs.push(
                decode_type_def_bytes(bytes)
                    .map_err(|e| DynError::schema(format!("{name}: {e}")))?,
            );
        }
        let mut c = Self::from_service_contract(&sc, defs)?;
        c.contract_id = compute_contract_id(contract_bytes);
        Ok(c)
    }

    /// From JSON: a `ContractManifest` and an array of `TypeDef`s.
    pub fn from_json(manifest_json: &str, type_defs_json: &str) -> DynResult<Self> {
        let manifest = ContractManifest::from_json(manifest_json)
            .map_err(|e| DynError::schema(format!("manifest: {e}")))?;
        let schema = DynSchema::from_json(type_defs_json)?;
        Self::from_manifest_and_schema(&manifest, schema)
    }

    fn from_manifest_and_schema(manifest: &ContractManifest, schema: DynSchema) -> DynResult<Self> {
        let mut methods = Vec::with_capacity(manifest.methods.len());
        for m in &manifest.methods {
            methods.push((
                m.name.clone(),
                pattern_from_str(&m.pattern)?,
                m.request_type.clone(),
                m.response_type.clone(),
                m.idempotent,
                m.timeout,
            ));
        }
        Ok(Self::build(
            manifest.service.clone(),
            manifest.version,
            manifest.contract_id.clone(),
            methods,
            schema,
        ))
    }

    /// The method named `name`.
    pub fn method(&self, name: &str) -> DynResult<&DynMethod> {
        self.methods.iter().find(|m| m.name == name).ok_or_else(|| {
            DynError::unknown(format!(
                "{} has no method {name:?} (methods: {})",
                self.service,
                self.methods
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    /// Encode one request of `method`.
    pub fn encode_request(&self, method: &str, value: &DynValue) -> DynResult<Vec<u8>> {
        let m = self.method(method)?;
        self.schema
            .encode_with(&m.request_type, value, &self.encode_options)
    }

    /// Decode one response of `method`.
    pub fn decode_response(&self, method: &str, bytes: &[u8]) -> DynResult<DynValue> {
        let m = self.method(method)?;
        self.schema
            .decode_with(Some(&m.response_type), bytes, &DecodeOptions::default())
    }

    /// Encode one response of `method` (server side / tests).
    pub fn encode_response(&self, method: &str, value: &DynValue) -> DynResult<Vec<u8>> {
        let m = self.method(method)?;
        self.schema
            .encode_with(&m.response_type, value, &self.encode_options)
    }

    /// Decode one request of `method` (server side / tests).
    pub fn decode_request(&self, method: &str, bytes: &[u8]) -> DynResult<DynValue> {
        let m = self.method(method)?;
        self.schema
            .decode_with(Some(&m.request_type), bytes, &DecodeOptions::default())
    }

    /// JSON request in, payload bytes out.
    pub fn encode_request_json(&self, method: &str, json: &[u8]) -> DynResult<Vec<u8>> {
        let v: serde_json::Value =
            serde_json::from_slice(json).map_err(|e| DynError::invalid(format!("JSON: {e}")))?;
        self.encode_request(method, &DynValue::from_json(&v))
    }

    /// Payload bytes in, JSON response out.
    pub fn decode_response_json(&self, method: &str, bytes: &[u8]) -> DynResult<Vec<u8>> {
        let v = self.decode_response(method, bytes)?;
        Ok(serde_json::to_vec(&v.to_json(&JsonOptions::default())).expect("JSON value serializes"))
    }
}
