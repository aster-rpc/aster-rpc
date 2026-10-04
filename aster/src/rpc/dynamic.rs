//! Services described at runtime: the registration model for bindings
//! (design `docs/_internal/bindings/dynamic-dispatch-design.md` §1).
//!
//! A [`DynamicService`] is what a language binding (Python, TypeScript, the C
//! ABI) hands to Rust instead of a `#[aster::service]`-generated
//! [`ServiceDispatch`](super::ServiceDispatch). It is the **single source of
//! truth** for the service: Rust derives the [`ServiceContract`] from it, so
//! the `requires`, `pattern` and timeouts that dispatch enforces are exactly the
//! values the `contract_id` commits to. The binding never supplies a contract
//! separately.
//!
//! What the binding still owns is type identity: its own type-graph walker
//! (Python `contract/identity.py`, TS `contract/identity.ts`, Java
//! `TypeGraphWalker`) hashes each payload type with core's canonical encoder
//! and passes the 64-hex hash in a [`TypeRef`].
//!
//! The serde form is the registration schema used by the C ABI
//! (`aster_server_add_service_json`); it is documented in the design doc,
//! §1.5. Unknown keys are rejected so a misspelt `requires` cannot silently
//! drop an authorization check.
//!
//! Method order is significant: a method's index in
//! [`methods`](DynamicService::methods) is the `method_idx` that dispatch
//! reports to the binding. The contract sorts methods by name, so the order
//! does not affect `contract_id`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use aster_transport_core::contract::{
    build_contract_collection, canonical_xlang_bytes_service_contract_checked,
    canonical_xlang_bytes_type_def, compute_contract_id, compute_type_hash, normalize_identifier,
    ContractManifest, TypeKind,
};
use aster_transport_core::framing::MAX_FRAME_SIZE;

use super::auth::CapabilityRequirement;
use super::codec::SerializationMode;
use super::contract::{AsterType, MethodDef, MethodPattern, ScopeKind, ServiceContract, TypeDef};
use crate::error::{Error, Result};

/// A service registered at runtime by a binding. See the module docs.
///
/// Build one with [`DynamicService::new`] and the public fields, or parse the
/// JSON registration form with [`DynamicService::from_json`]. Call
/// [`validate`](DynamicService::validate) (or anything that validates, such as
/// [`contract_id`](DynamicService::contract_id)) before serving it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicService {
    /// Service name, matched against `StreamHeader.service`. Must already be
    /// NFC-normalized; the `aster.*` namespace is reserved.
    pub name: String,
    /// Service version, matched against `StreamHeader.version`.
    pub version: i32,
    /// SHARED (one instance) or SESSION (one instance per client session).
    #[serde(default)]
    pub scope: ScopeKind,
    /// Service-wide Gate 3 requirement, checked on every method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<CapabilityRequirement>,
    /// Declared payload modes, in contract order. Defaults to `["xlang"]`. An
    /// empty list is accepted (the oldest contract vectors use it) and is
    /// served as XLANG.
    #[serde(default = "default_modes", with = "modes_serde")]
    pub serialization_modes: Vec<SerializationMode>,
    /// Required (and only allowed) when `serialization_modes` contains
    /// `native`: one of `python`, `typescript`, `java`, `csharp`, `go`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub producer_language: String,
    /// Refuse calls whose peer id the iroh handshake did not authenticate
    /// (see [`ServiceDispatch::iroh_only`](super::ServiceDispatch::iroh_only)).
    /// Not part of the contract.
    #[serde(default, skip_serializing_if = "is_false")]
    pub iroh_only: bool,
    /// The methods; the index is the dispatch `method_idx`.
    pub methods: Vec<MethodDesc>,
    /// Type definitions to publish as `types/{hash}.bin`. Optional, but every
    /// entry must be reachable from a method's request or response type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_defs: Vec<TypeDef>,
    /// A binding-built `manifest.json` to publish instead of the core subset
    /// (design §1.3). Checked against the contract, then published verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_json: Option<String>,
}

/// One method of a [`DynamicService`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodDesc {
    /// Method name, matched against `StreamHeader.method`. NFC-normalized.
    pub name: String,
    /// Call pattern.
    pub pattern: MethodPattern,
    /// Request payload type (the element type for client-stream and bidi).
    pub request: TypeRef,
    /// Response payload type (the element type for server-stream and bidi).
    pub response: TypeRef,
    #[serde(default, skip_serializing_if = "is_false")]
    pub idempotent: bool,
    /// Default server-side timeout in seconds. Kept as the binding's `f64`
    /// rather than a `Duration`: it is hashed into the contract as a float64,
    /// and a round trip through nanoseconds could change its bits. `None` and
    /// `0.0` both mean unset (contract value `0.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout: Option<f64>,
    /// Method-level Gate 3 requirement, checked after the service-wide one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<CapabilityRequirement>,
    /// Transport limits. Not part of the contract.
    #[serde(default, skip_serializing_if = "MethodLimits::is_default")]
    pub limits: MethodLimits,
}

/// A payload type, by its contract identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeRef {
    /// Lowercase 64-hex BLAKE3 type hash (canonical `TypeDef` bytes).
    pub hash: String,
    /// Human-readable name (e.g. `interop/EchoRequest`). Diagnostics only;
    /// not part of the contract.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display: String,
}

/// Per-method transport limits. Not part of the contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodLimits {
    /// Maximum request body frame size in bytes; `None` = the global
    /// `MAX_FRAME_SIZE`. Must be in `1..=MAX_FRAME_SIZE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_frame_size: Option<u32>,
}

/// Why a [`DynamicService`] was refused.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ValidationError {
    #[error("service name is empty")]
    EmptyServiceName,
    #[error("{what} {name:?} is not NFC-normalized")]
    NotNormalized { what: &'static str, name: String },
    #[error("service name {0:?} is in the reserved `aster.*` namespace")]
    ReservedNamespace(String),
    #[error("method #{0} has an empty name")]
    EmptyMethodName(usize),
    #[error("duplicate method name {0:?}")]
    DuplicateMethod(String),
    #[error("duplicate serialization mode {0:?}")]
    DuplicateMode(&'static str),
    #[error("{at}: {value:?} is not a lowercase 64-hex hash")]
    MalformedHash { at: String, value: String },
    #[error("method {method:?}: default_timeout {value} must be finite and non-negative")]
    BadTimeout { method: String, value: f64 },
    #[error("method {method:?}: max_frame_size {value} is outside 1..={max}")]
    BadFrameLimit {
        method: String,
        value: u32,
        max: u32,
    },
    /// Covers `native` without `producer_language`, a `producer_language`
    /// without `native`, and an unknown language (core's §11.3.2.3 check).
    #[error("producer_language: {0}")]
    ProducerLanguage(String),
    #[error("type_defs contains {0} twice")]
    DuplicateTypeDef(String),
    #[error(
        "type_def {name} ({hash}) is not reachable from any method's request or response type"
    )]
    UnreachableTypeDef { name: String, hash: String },
}

impl From<ValidationError> for Error {
    fn from(e: ValidationError) -> Self {
        Error::InvalidArgument(format!("dynamic service: {e}"))
    }
}

impl ValidationError {
    /// A stable, binding-facing code for the failure (the `error` values in
    /// `conformance/vectors/dynamic-service.json`).
    pub fn code(&self) -> &'static str {
        match self {
            ValidationError::EmptyServiceName => "empty_service_name",
            ValidationError::NotNormalized { .. } => "not_normalized",
            ValidationError::ReservedNamespace(_) => "reserved_namespace",
            ValidationError::EmptyMethodName(_) => "empty_method_name",
            ValidationError::DuplicateMethod(_) => "duplicate_method",
            ValidationError::DuplicateMode(_) => "duplicate_mode",
            ValidationError::MalformedHash { .. } => "malformed_hash",
            ValidationError::BadTimeout { .. } => "bad_timeout",
            ValidationError::BadFrameLimit { .. } => "bad_frame_limit",
            ValidationError::ProducerLanguage(_) => "producer_language",
            ValidationError::DuplicateTypeDef(_) => "duplicate_type_def",
            ValidationError::UnreachableTypeDef { .. } => "unreachable_type_def",
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl MethodLimits {
    fn is_default(&self) -> bool {
        *self == MethodLimits::default()
    }
}

fn default_modes() -> Vec<SerializationMode> {
    vec![SerializationMode::Xlang]
}

/// Contract spelling of a serialization mode (`ServiceContract.serialization_modes`).
pub fn mode_name(mode: SerializationMode) -> &'static str {
    match mode {
        SerializationMode::Xlang => "xlang",
        SerializationMode::Native => "native",
        SerializationMode::Row => "row",
    }
}

fn mode_from_name(s: &str) -> Option<SerializationMode> {
    match s {
        "xlang" => Some(SerializationMode::Xlang),
        "native" => Some(SerializationMode::Native),
        "row" => Some(SerializationMode::Row),
        _ => None,
    }
}

mod modes_serde {
    use super::*;

    pub fn serialize<S: Serializer>(
        modes: &[SerializationMode],
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        s.collect_seq(modes.iter().map(|m| mode_name(*m)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Vec<SerializationMode>, D::Error> {
        let names = Vec::<String>::deserialize(d)?;
        names
            .iter()
            .map(|n| {
                mode_from_name(n).ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "unknown serialization mode {n:?} (expected xlang, native or row)"
                    ))
                })
            })
            .collect()
    }
}

impl TypeRef {
    /// A reference by hash, with no display name.
    pub fn new(hash: impl Into<String>) -> Self {
        TypeRef {
            hash: hash.into(),
            display: String::new(),
        }
    }

    /// The reference a `#[derive(AsterType)]` type contributes, with its wire
    /// name (`package/Name`) as the display name.
    pub fn of<T: AsterType>() -> Self {
        let td = T::aster_type_def();
        let display = if td.package.is_empty() {
            td.name
        } else {
            format!("{}/{}", td.package, td.name)
        };
        TypeRef {
            hash: T::aster_type_hash_hex(),
            display,
        }
    }
}

impl MethodDesc {
    /// A method with no timeout, requirement or limits.
    pub fn new(
        name: impl Into<String>,
        pattern: MethodPattern,
        request: TypeRef,
        response: TypeRef,
    ) -> Self {
        MethodDesc {
            name: name.into(),
            pattern,
            request,
            response,
            idempotent: false,
            default_timeout: None,
            requires: None,
            limits: MethodLimits::default(),
        }
    }

    /// The server-side default timeout, for dispatch
    /// ([`ServiceDispatch::method_timeout`](super::ServiceDispatch::method_timeout)).
    /// `None` when unset or zero. Call on a validated service: a negative or
    /// non-finite value yields `None` here, but `validate` refuses it.
    pub fn timeout(&self) -> Option<Duration> {
        self.default_timeout
            .filter(|t| *t > 0.0)
            .and_then(|t| Duration::try_from_secs_f64(t).ok())
    }

    /// The method's contract form.
    pub fn method_def(&self) -> MethodDef {
        MethodDef {
            name: self.name.clone(),
            pattern: self.pattern,
            request_type: self.request.hash.clone(),
            response_type: self.response.hash.clone(),
            idempotent: self.idempotent,
            default_timeout: self.default_timeout.unwrap_or(0.0),
            requires: self.requires.clone(),
        }
    }
}

impl DynamicService {
    /// A SHARED, XLANG service with no methods yet.
    pub fn new(name: impl Into<String>, version: i32) -> Self {
        DynamicService {
            name: name.into(),
            version,
            scope: ScopeKind::Shared,
            requires: None,
            serialization_modes: default_modes(),
            producer_language: String::new(),
            iroh_only: false,
            methods: Vec::new(),
            type_defs: Vec::new(),
            manifest_json: None,
        }
    }

    /// Parse the JSON registration form (design §1.5) and validate it.
    pub fn from_json(json: &str) -> Result<Self> {
        let svc: DynamicService = serde_json::from_str(json)
            .map_err(|e| Error::InvalidArgument(format!("dynamic service JSON: {e}")))?;
        svc.validate()?;
        Ok(svc)
    }

    /// The JSON registration form.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| Error::InvalidArgument(format!("dynamic service JSON: {e}")))
    }

    /// The registry key, `(name, version)`.
    pub fn key(&self) -> (String, i32) {
        (self.name.clone(), self.version)
    }

    /// The method at dispatch index `idx`.
    pub fn method(&self, idx: u32) -> Option<&MethodDesc> {
        self.methods.get(idx as usize)
    }

    /// The dispatch index of the method called `name`. A linear scan:
    /// dispatch should build its own name → index map once at registration.
    pub fn method_index(&self, name: &str) -> Option<u32> {
        self.methods
            .iter()
            .position(|m| m.name == name)
            .map(|i| i as u32)
    }

    /// Whether a call in `mode` is acceptable. An empty mode list means XLANG.
    pub fn accepts_mode(&self, mode: SerializationMode) -> bool {
        if self.serialization_modes.is_empty() {
            mode == SerializationMode::Xlang
        } else {
            self.serialization_modes.contains(&mode)
        }
    }

    /// Check everything the contract and dispatch rely on (design §1.2):
    /// names present, NFC-normalized and outside `aster.*`; method names
    /// unique; hashes well-formed; timeouts and frame limits in range; the
    /// `producer_language` invariant; and every `type_def` reachable from a
    /// method root.
    pub fn validate(&self) -> std::result::Result<(), ValidationError> {
        if self.name.is_empty() {
            return Err(ValidationError::EmptyServiceName);
        }
        check_nfc("service name", &self.name)?;
        if is_reserved_namespace(&self.name) {
            return Err(ValidationError::ReservedNamespace(self.name.clone()));
        }

        let mut seen_modes = HashSet::new();
        for m in &self.serialization_modes {
            if !seen_modes.insert(*m) {
                return Err(ValidationError::DuplicateMode(mode_name(*m)));
            }
        }

        let mut names = HashSet::new();
        for (i, m) in self.methods.iter().enumerate() {
            if m.name.is_empty() {
                return Err(ValidationError::EmptyMethodName(i));
            }
            check_nfc("method name", &m.name)?;
            if !names.insert(m.name.as_str()) {
                return Err(ValidationError::DuplicateMethod(m.name.clone()));
            }
            check_hash(&format!("{}.request", m.name), &m.request.hash)?;
            check_hash(&format!("{}.response", m.name), &m.response.hash)?;
            if let Some(t) = m.default_timeout {
                if !t.is_finite() || t.is_sign_negative() {
                    return Err(ValidationError::BadTimeout {
                        method: m.name.clone(),
                        value: t,
                    });
                }
            }
            if let Some(n) = m.limits.max_frame_size {
                if n == 0 || n > MAX_FRAME_SIZE {
                    return Err(ValidationError::BadFrameLimit {
                        method: m.name.clone(),
                        value: n,
                        max: MAX_FRAME_SIZE,
                    });
                }
            }
        }

        canonical_xlang_bytes_service_contract_checked(&self.contract())
            .map_err(|e| ValidationError::ProducerLanguage(e.to_string()))?;

        self.check_type_defs()
    }

    /// Every `type_def` must be reachable from a method's request or response
    /// hash through `Ref` fields, map keys and union variants. A cheap
    /// closure: the graph is only as large as `type_defs`.
    fn check_type_defs(&self) -> std::result::Result<(), ValidationError> {
        if self.type_defs.is_empty() {
            return Ok(());
        }
        let mut by_hash: HashMap<String, &TypeDef> = HashMap::new();
        for td in &self.type_defs {
            let h = hex::encode(compute_type_hash(&canonical_xlang_bytes_type_def(td)));
            for f in &td.fields {
                let at = format!("{}.{}", td.name, f.name);
                if f.type_kind == TypeKind::Ref {
                    check_hash(&at, &f.type_ref)?;
                }
                if f.container_key_kind == TypeKind::Ref {
                    check_hash(&format!("{at} (key)"), &f.container_key_ref)?;
                }
            }
            for v in &td.union_variants {
                check_hash(&format!("{}.{}", td.name, v.name), &v.type_ref)?;
            }
            if by_hash.insert(h.clone(), td).is_some() {
                return Err(ValidationError::DuplicateTypeDef(h));
            }
        }

        let mut reached: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = self
            .methods
            .iter()
            .flat_map(|m| [m.request.hash.as_str(), m.response.hash.as_str()])
            .collect();
        while let Some(h) = stack.pop() {
            if !reached.insert(h) {
                continue;
            }
            let Some(td) = by_hash.get(h) else { continue };
            for f in &td.fields {
                if f.type_kind == TypeKind::Ref {
                    stack.push(&f.type_ref);
                }
                if f.container_key_kind == TypeKind::Ref {
                    stack.push(&f.container_key_ref);
                }
            }
            for v in &td.union_variants {
                stack.push(&v.type_ref);
            }
        }

        // Report in declaration order, so the error is deterministic.
        for td in &self.type_defs {
            let h = hex::encode(compute_type_hash(&canonical_xlang_bytes_type_def(td)));
            if !reached.contains(h.as_str()) {
                return Err(ValidationError::UnreachableTypeDef {
                    name: format!("{}/{}", td.package, td.name),
                    hash: h,
                });
            }
        }
        Ok(())
    }

    /// The contract this service commits to (design §1.2). Pure mapping; does
    /// not validate.
    pub fn contract(&self) -> ServiceContract {
        ServiceContract {
            name: self.name.clone(),
            version: self.version,
            methods: self.methods.iter().map(MethodDesc::method_def).collect(),
            serialization_modes: self
                .serialization_modes
                .iter()
                .map(|m| mode_name(*m).to_string())
                .collect(),
            scoped: self.scope,
            requires: self.requires.clone(),
            producer_language: self.producer_language.clone(),
        }
    }

    /// The contract id: `BLAKE3(canonical bytes)` from core's encoder, the
    /// same function behind `compute_contract_id_from_json` that every binding
    /// calls. Validates first.
    pub fn contract_id(&self) -> Result<String> {
        self.validate()?;
        let bytes = canonical_xlang_bytes_service_contract_checked(&self.contract())?;
        Ok(compute_contract_id(&bytes))
    }

    /// The published contract collection (design §1.3): `contract.bin`, one
    /// `types/{hash}.bin` per `type_def`, and `manifest.json`, as
    /// `(entry_name, bytes)` pairs ready for `Blobs::add_collection`.
    ///
    /// Without [`manifest_json`](DynamicService::manifest_json) the manifest is
    /// core's subset. With it, the binding's manifest is parsed under core's
    /// size caps, checked against the contract (`contract_id`, `service`,
    /// `version`, and the set of method names and patterns), and published
    /// verbatim. Validates first.
    pub fn contract_collection(
        &self,
        published_by: &str,
        published_at_epoch_ms: i64,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        self.validate()?;
        let mut entries = build_contract_collection(
            &self.contract(),
            &self.type_defs,
            published_by,
            published_at_epoch_ms,
        )?;
        if let Some(json) = &self.manifest_json {
            let contract_id = compute_contract_id(&entries[0].1);
            self.check_manifest(json, &contract_id)?;
            if let Some(slot) = entries.iter_mut().find(|(n, _)| n == "manifest.json") {
                slot.1 = json.clone().into_bytes();
            }
        }
        Ok(entries)
    }

    fn check_manifest(&self, json: &str, contract_id: &str) -> Result<()> {
        let bad = |what: String| Error::InvalidArgument(format!("manifest_json: {what}"));
        let m = ContractManifest::from_json(json).map_err(|e| bad(e.to_string()))?;
        if m.contract_id != contract_id {
            return Err(bad(format!(
                "contract_id {} does not match the service's {contract_id}",
                m.contract_id
            )));
        }
        if m.service != self.name || m.version != self.version {
            return Err(bad(format!(
                "names {} v{}, but the service is {} v{}",
                m.service, m.version, self.name, self.version
            )));
        }
        let mut theirs: Vec<(&str, &str)> = m
            .methods
            .iter()
            .map(|mm| (mm.name.as_str(), mm.pattern.as_str()))
            .collect();
        let mut ours: Vec<(&str, &str)> = self
            .methods
            .iter()
            .map(|md| (md.name.as_str(), pattern_name(md.pattern)))
            .collect();
        theirs.sort_unstable();
        ours.sort_unstable();
        if theirs != ours {
            return Err(bad(format!(
                "methods {theirs:?} do not match the service's {ours:?}"
            )));
        }
        Ok(())
    }
}

/// Manifest spelling of a method pattern (matches core's `ManifestMethod`).
fn pattern_name(p: MethodPattern) -> &'static str {
    match p {
        MethodPattern::Unary => "unary",
        MethodPattern::ServerStream => "server_stream",
        MethodPattern::ClientStream => "client_stream",
        MethodPattern::BidiStream => "bidi_stream",
    }
}

/// Same rule as `#[aster::service]`: `aster`, `aster.*` and `aster/*` belong
/// to the framework's baseline services, which are native Rust and so never
/// registered dynamically.
fn is_reserved_namespace(name: &str) -> bool {
    name == "aster" || name.starts_with("aster.") || name.starts_with("aster/")
}

fn check_nfc(what: &'static str, name: &str) -> std::result::Result<(), ValidationError> {
    if normalize_identifier(name) != name {
        return Err(ValidationError::NotNormalized {
            what,
            name: name.to_string(),
        });
    }
    Ok(())
}

fn check_hash(at: &str, value: &str) -> std::result::Result<(), ValidationError> {
    let ok = value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !ok {
        return Err(ValidationError::MalformedHash {
            at: at.to_string(),
            value: value.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::CapabilityKind;

    const H0: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn echo() -> DynamicService {
        let mut s = DynamicService::new("Echo", 1);
        s.methods.push(MethodDesc::new(
            "echo",
            MethodPattern::Unary,
            TypeRef::new(H0),
            TypeRef::new(H0),
        ));
        s
    }

    #[test]
    fn valid_service_passes() {
        echo().validate().unwrap();
    }

    #[test]
    fn rejects_empty_and_unnormalized_names() {
        let mut s = echo();
        s.name.clear();
        assert_eq!(s.validate(), Err(ValidationError::EmptyServiceName));

        let mut s = echo();
        s.name = "cafe\u{301}".into();
        assert!(matches!(
            s.validate(),
            Err(ValidationError::NotNormalized {
                what: "service name",
                ..
            })
        ));

        let mut s = echo();
        s.methods[0].name = "e\u{301}cho".into();
        assert!(matches!(
            s.validate(),
            Err(ValidationError::NotNormalized {
                what: "method name",
                ..
            })
        ));

        let mut s = echo();
        s.methods[0].name.clear();
        assert_eq!(s.validate(), Err(ValidationError::EmptyMethodName(0)));
    }

    #[test]
    fn rejects_reserved_namespace() {
        for name in ["aster", "aster.ops.NodeInfo", "aster/x"] {
            let mut s = echo();
            s.name = name.into();
            assert_eq!(
                s.validate(),
                Err(ValidationError::ReservedNamespace(name.into()))
            );
        }
        // Only the namespace itself, not a prefix of a longer word.
        let mut s = echo();
        s.name = "asteroid.Belt".into();
        s.validate().unwrap();
    }

    #[test]
    fn rejects_duplicate_methods() {
        let mut s = echo();
        s.methods.push(s.methods[0].clone());
        assert_eq!(
            s.validate(),
            Err(ValidationError::DuplicateMethod("echo".into()))
        );
    }

    #[test]
    fn rejects_malformed_hashes() {
        for bad in ["", "00", &"A".repeat(64), &"g".repeat(64), &"0".repeat(65)] {
            let mut s = echo();
            s.methods[0].response.hash = bad.to_string();
            assert!(
                matches!(s.validate(), Err(ValidationError::MalformedHash { ref at, .. }) if at == "echo.response"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_timeouts_and_frame_limits() {
        for t in [-1.0, -0.0, f64::NAN, f64::INFINITY] {
            let mut s = echo();
            s.methods[0].default_timeout = Some(t);
            assert!(
                matches!(s.validate(), Err(ValidationError::BadTimeout { .. })),
                "{t}"
            );
        }
        for n in [0, MAX_FRAME_SIZE + 1] {
            let mut s = echo();
            s.methods[0].limits.max_frame_size = Some(n);
            assert!(matches!(
                s.validate(),
                Err(ValidationError::BadFrameLimit { .. })
            ));
        }
        let mut s = echo();
        s.methods[0].limits.max_frame_size = Some(MAX_FRAME_SIZE);
        s.methods[0].default_timeout = Some(0.0);
        s.validate().unwrap();
    }

    #[test]
    fn producer_language_invariant() {
        let mut s = echo();
        s.serialization_modes = vec![SerializationMode::Native];
        assert!(matches!(
            s.validate(),
            Err(ValidationError::ProducerLanguage(_))
        ));
        s.producer_language = "cobol".into();
        assert!(matches!(
            s.validate(),
            Err(ValidationError::ProducerLanguage(_))
        ));
        s.producer_language = "python".into();
        s.validate().unwrap();

        let mut s = echo();
        s.producer_language = "python".into();
        assert!(matches!(
            s.validate(),
            Err(ValidationError::ProducerLanguage(_))
        ));
    }

    #[test]
    fn rejects_duplicate_modes() {
        let mut s = echo();
        s.serialization_modes = vec![SerializationMode::Xlang, SerializationMode::Xlang];
        assert_eq!(s.validate(), Err(ValidationError::DuplicateMode("xlang")));
    }

    fn message(name: &str, refs: &[&str]) -> TypeDef {
        let fields = refs
            .iter()
            .enumerate()
            .map(|(i, r)| {
                super::super::contract::make_field(
                    &format!("f{i}"),
                    super::super::contract::Leaf::reference(
                        hex::decode(r).unwrap().try_into().unwrap(),
                    ),
                    false,
                    0,
                    None,
                    None,
                )
            })
            .collect();
        super::super::contract::message_type_def("t", name, fields)
    }

    fn hash_of(td: &TypeDef) -> String {
        hex::encode(compute_type_hash(&canonical_xlang_bytes_type_def(td)))
    }

    #[test]
    fn type_def_closure() {
        let leaf = message("Leaf", &[]);
        let root = message("Root", &[&hash_of(&leaf)]);
        let stray = message("Stray", &[]);

        let mut s = echo();
        s.methods[0].request = TypeRef::new(hash_of(&root));
        s.type_defs = vec![root.clone(), leaf.clone()];
        s.validate().unwrap();

        s.type_defs.push(stray.clone());
        assert_eq!(
            s.validate(),
            Err(ValidationError::UnreachableTypeDef {
                name: "t/Stray".into(),
                hash: hash_of(&stray),
            })
        );

        s.type_defs = vec![root.clone(), leaf.clone(), leaf.clone()];
        assert_eq!(
            s.validate(),
            Err(ValidationError::DuplicateTypeDef(hash_of(&leaf)))
        );

        let mut bad = root.clone();
        bad.fields[0].type_ref = "xyz".into();
        s.type_defs = vec![bad];
        assert!(matches!(
            s.validate(),
            Err(ValidationError::MalformedHash { .. })
        ));
    }

    #[test]
    fn dispatch_lookups() {
        let mut s = echo();
        let mut slow = MethodDesc::new(
            "slow",
            MethodPattern::ServerStream,
            TypeRef::new(H0),
            TypeRef::new(H0),
        );
        slow.default_timeout = Some(2.5);
        slow.requires = Some(CapabilityRequirement {
            kind: CapabilityKind::Role,
            roles: vec!["admin".into()],
        });
        slow.limits.max_frame_size = Some(1024);
        s.methods.push(slow);

        assert_eq!(s.key(), ("Echo".to_string(), 1));
        assert_eq!(s.method_index("slow"), Some(1));
        assert_eq!(s.method_index("nope"), None);
        let m = s.method(1).unwrap();
        assert_eq!(m.timeout(), Some(Duration::from_millis(2500)));
        assert_eq!(m.limits.max_frame_size, Some(1024));
        assert!(m.requires.is_some());
        assert_eq!(s.method(0).unwrap().timeout(), None);
        assert!(s.method(2).is_none());

        assert!(s.accepts_mode(SerializationMode::Xlang));
        assert!(!s.accepts_mode(SerializationMode::Native));
        s.serialization_modes.clear();
        assert!(s.accepts_mode(SerializationMode::Xlang));
    }

    #[test]
    fn json_round_trip_and_defaults() {
        let json = format!(
            r#"{{"name":"Echo","version":1,"methods":[
                {{"name":"echo","pattern":"unary","request":{{"hash":"{H0}"}},"response":{{"hash":"{H0}"}}}}
            ]}}"#
        );
        let s = DynamicService::from_json(&json).unwrap();
        assert_eq!(s, echo());
        assert_eq!(DynamicService::from_json(&s.to_json().unwrap()).unwrap(), s);

        // Unknown keys are refused rather than ignored.
        let typo = json.replace(r#""version":1"#, r#""version":1,"requiers":null"#);
        assert!(DynamicService::from_json(&typo).is_err());
        let bad_mode = json.replace(
            r#""version":1"#,
            r#""version":1,"serialization_modes":["json"]"#,
        );
        assert!(DynamicService::from_json(&bad_mode).is_err());
    }

    #[test]
    fn contract_id_matches_core_json_path() {
        let s = echo();
        let via_json = aster_transport_core::contract::compute_contract_id_from_json(
            &serde_json::to_string(&s.contract()).unwrap(),
        )
        .unwrap();
        assert_eq!(s.contract_id().unwrap(), via_json);
    }

    #[test]
    fn collection_with_and_without_binding_manifest() {
        let s = echo();
        let id = s.contract_id().unwrap();
        let core = s.contract_collection("me", 7).unwrap();
        assert_eq!(core[0].0, "contract.bin");
        assert_eq!(compute_contract_id(&core[0].1), id);
        let core_manifest = &core.iter().find(|(n, _)| n == "manifest.json").unwrap().1;

        // A binding manifest with extra (shell) fields is published verbatim.
        let mut v: serde_json::Value = serde_json::from_slice(core_manifest).unwrap();
        v["methods"][0]["fields"] = serde_json::json!([{"name": "x"}]);
        v["description"] = "from the binding".into();
        let mut with = s.clone();
        with.manifest_json = Some(v.to_string());
        let out = with.contract_collection("me", 7).unwrap();
        let published = &out.iter().find(|(n, _)| n == "manifest.json").unwrap().1;
        assert_eq!(published, v.to_string().as_bytes());

        for (key, val) in [
            ("contract_id", serde_json::json!("00")),
            ("service", serde_json::json!("Other")),
            ("version", serde_json::json!(2)),
        ] {
            let mut bad = v.clone();
            bad[key] = val;
            let mut svc = s.clone();
            svc.manifest_json = Some(bad.to_string());
            assert!(svc.contract_collection("me", 7).is_err(), "{key}");
        }
        let mut bad = v.clone();
        bad["methods"][0]["pattern"] = "server_stream".into();
        let mut svc = s.clone();
        svc.manifest_json = Some(bad.to_string());
        assert!(svc.contract_collection("me", 7).is_err());

        let mut svc = s.clone();
        svc.manifest_json = Some("{not json".into());
        assert!(svc.contract_collection("me", 7).is_err());
    }
}
