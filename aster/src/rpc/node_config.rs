//! `aster.ops.NodeConfig`: remote overrides of a node's configuration
//! (design: "Remote overrides" in
//! `docs/_internal/web_gateway/gateway-identity.md`).
//!
//! A node's record holds **properties** (name, tags), **typed attributes**
//! (bool, int, float, string) and **overrides** of the configuration
//! sections that declare themselves overridable. `[identity]` is the first
//! such section. The shape follows portal-sync's node records and its
//! `NodeService` `policy_ack`.
//!
//! ## Who may change it
//!
//! The service is on by default ([`AsterConfig::remote_config_enabled`]).
//! Gate 3 requires [`CONFIG_ADMIN_ROLE`] on every method, and `set` / `clear`
//! check again that the role comes from the caller's own **Enrollment**
//! credential, on a chain to the root, over an Iroh-authenticated connection,
//! with no `aster-on-behalf-of` entry. A role from an
//! [`AttributeStore`](super::AttributeStore) entry or an authenticator does
//! not count, so a node without Enrollment refuses every change.
//!
//! ## Hardening
//!
//! Overridable is opt-in: [`OVERRIDABLE_SECTIONS`] names the sections, and
//! each section names its overridable fields. Everything else (the bind
//! port, `hook_failure_mode`, `[remote_config]` itself, …) is refused with
//! `PERMISSION_DENIED`. The local file can lock a section
//! (`[remote_config] lock = ["identity"]`, refused with
//! `FAILED_PRECONDITION`, and a stored override of a section locked later is
//! ignored) or turn the service off (`[remote_config] enabled = false`, which
//! also ignores the stored record).
//!
//! ## Attributes never authorise
//!
//! NodeConfig attributes live in their own map ([`NodeProperties`]), apart
//! from the caller's Gate-3 attributes, and keys starting `aster.` are
//! refused. Gate 3's `requires` never reads them. Handlers read them with
//! [`CallContext::node_properties`](super::CallContext::node_properties);
//! code outside a call reads [`NodeConfigHandle::properties`].
//!
//! ## Storage and authority
//!
//! The record lives in a namespace of the node's own docs store (persistent
//! when the node is), whose secret is derived from the node identity and
//! which only the node writes, as its own author. The authoritative copy is
//! the last `set` or `clear` an admin (the root, or an issuer holding the
//! role) made over RPC. There is no local reset: nothing on the node writes
//! the record except the service, and deleting files is not part of the
//! model. A `clear` is itself a record (a tombstone with the next
//! generation), so generations only grow and the last change stays
//! attributable.
//!
//! ## Precedence
//!
//! Built-in defaults, then file and environment (and code), then the
//! override. [`get`](NodeConfig::get) reports each overridable setting's
//! effective value with its [`ConfigSource`].
//!
//! ## Acknowledgement means applied
//!
//! A `set` validates the whole request (every section as a whole) before
//! anything is stored, records it in the audit outbox when one is configured
//! ([`AsterServerBuilder::audit_outbox`](super::AsterServerBuilder::audit_outbox)),
//! stores it durably, swaps the effective settings in atomically (an
//! `ArcSwap` every dispatcher reads per call) and only then answers, with
//! the same [`NodeConfigAck`] that [`ack`](NodeConfig::ack) returns. `ack`
//! waits (up to its timeout) until the generation asked about is applied,
//! then reads the stored record back and checks the effect itself: that the
//! settings in force equal those the record produces.

use std::collections::BTreeMap;
use std::sync::Arc;

use fory_derive::ForyStruct;
use serde::{Deserialize, Serialize};

use super::baseline::{Attr, BoolValue, FloatValue, IntValue, TextValue, Value};

/// The service name.
pub const NODE_CONFIG_SERVICE: &str = "aster.ops.NodeConfig";
/// The role Gate 3 requires on every `aster.ops.NodeConfig` method, granted
/// through Enrollment.
pub const CONFIG_ADMIN_ROLE: &str = "aster.config-admin";
/// NodeConfig attribute keys may not start with this (ASCII
/// case-insensitive): the namespace is the framework's, and attributes must
/// not look like the authorisation attributes that live there.
pub const RESERVED_ATTRIBUTE_PREFIX: &str = "aster.";
/// The `[identity]` section.
pub const IDENTITY_SECTION: &str = "identity";
/// The sections that declare themselves overridable.
pub const OVERRIDABLE_SECTIONS: &[&str] = &[IDENTITY_SECTION];
/// The overridable fields of `[identity]`.
pub const IDENTITY_FIELDS: &[&str] = &[
    "accept_on_behalf_of",
    "trusted_issuers",
    "edge_token_max_lifetime_secs",
    "clock_leeway_secs",
    "max_key_set_age_secs",
];
/// Most attributes on one record.
pub const MAX_ATTRIBUTES: usize = 64;
/// Most tags on one record.
pub const MAX_TAGS: usize = 64;
/// Longest name, tag or attribute key, in bytes.
pub const MAX_KEY_BYTES: usize = 128;
/// Longest string attribute value, in bytes.
pub const MAX_TEXT_BYTES: usize = 1024;
/// Longest section override (`settings_json`), in bytes.
pub const MAX_SECTION_BYTES: usize = 16 * 1024;

// ── Properties ──────────────────────────────────────────────────────────────

/// A typed NodeConfig attribute value. Serialized as
/// `{ "type": "bool"|"int"|"float"|"string", "value": … }`, the shape the
/// enrollment policy uses for typed attributes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum AttrValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

impl AttrValue {
    /// `bool`, `int`, `float` or `string`.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::String(_) => "string",
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(v) => Some(v),
            _ => None,
        }
    }

    /// The wire [`Value`].
    pub fn to_value(&self) -> Value {
        match self {
            Self::Bool(value) => Value::Bool(BoolValue { value: *value }),
            Self::Int(value) => Value::Int(IntValue { value: *value }),
            Self::Float(value) => Value::Float(FloatValue { value: *value }),
            Self::String(value) => Value::Text(TextValue {
                value: value.clone(),
            }),
        }
    }

    /// From a wire [`Value`]; `None` for a variant this build does not know.
    pub fn from_value(value: &Value) -> Option<Self> {
        Some(match value {
            Value::Bool(v) => Self::Bool(v.value),
            Value::Int(v) => Self::Int(v.value),
            Value::Float(v) => Self::Float(v.value),
            Value::Text(v) => Self::String(v.value.clone()),
            Value::Unknown(_) => return None,
        })
    }
}

impl From<bool> for AttrValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for AttrValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<f64> for AttrValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<&str> for AttrValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<String> for AttrValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl std::fmt::Display for AttrValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::String(v) => write!(f, "{v}"),
        }
    }
}

/// A node's NodeConfig properties: name, tags and typed attributes. They
/// describe the node and never authorise anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeProperties {
    name: String,
    tags: Vec<String>,
    attributes: BTreeMap<String, AttrValue>,
}

impl NodeProperties {
    /// The node's name; empty when unset.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The node's tags, sorted and deduplicated.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Whether the node carries `tag`.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// Every attribute, by key.
    pub fn attributes(&self) -> &BTreeMap<String, AttrValue> {
        &self.attributes
    }

    /// One attribute.
    pub fn attribute(&self, key: &str) -> Option<&AttrValue> {
        self.attributes.get(key)
    }
}

/// The properties every dispatcher of one server hands its handlers, swapped
/// atomically by NodeConfig.
#[derive(Clone, Default)]
pub(crate) struct LiveProperties(Arc<arc_swap::ArcSwap<NodeProperties>>);

impl LiveProperties {
    pub(crate) fn load(&self) -> Arc<NodeProperties> {
        self.0.load_full()
    }

    #[cfg_attr(not(feature = "docs"), allow(dead_code))]
    pub(crate) fn store(&self, properties: NodeProperties) {
        self.0.store(Arc::new(properties));
    }
}

// ── Sections ────────────────────────────────────────────────────────────────

/// An override of `[identity]`. Only the fields that are set override; the
/// rest keep their file / env / default values. `trusted_issuers` replaces
/// the whole list. The section that results is validated as a whole
/// ([`IdentitySettings::validate`](crate::rpc::IdentitySettings::validate))
/// before anything is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_on_behalf_of: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_issuers: Option<Vec<crate::TrustedIssuer>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_token_max_lifetime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_leeway_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_key_set_age_secs: Option<u64>,
}

impl IdentityOverride {
    /// `base` with this override's fields applied.
    pub fn apply(&self, base: &crate::rpc::IdentitySettings) -> crate::rpc::IdentitySettings {
        let mut out = base.clone();
        if let Some(value) = self.accept_on_behalf_of {
            out.accept_on_behalf_of = value;
        }
        if let Some(value) = &self.trusted_issuers {
            out.trusted_issuers = value.clone();
        }
        if let Some(value) = self.edge_token_max_lifetime_secs {
            out.edge_token_max_lifetime_secs = value;
        }
        if let Some(value) = self.clock_leeway_secs {
            out.clock_leeway_secs = value;
        }
        if let Some(value) = self.max_key_set_age_secs {
            out.max_key_set_age_secs = value;
        }
        out
    }

    /// The wire form, for a [`NodeConfigSetRequest`].
    pub fn to_section(&self) -> SectionOverride {
        SectionOverride {
            section: IDENTITY_SECTION.into(),
            settings_json: serde_json::to_string(self).expect("JSON of a plain struct"),
        }
    }
}

// ── Wire types ──────────────────────────────────────────────────────────────

/// One section override (wire `aster/NodeConfigSection`): the section's
/// overridable fields as a JSON object, e.g. `identity` /
/// `{"accept_on_behalf_of": true}`. JSON so a later structured field (the
/// trusted issuers) slots in without a wire change.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigSection")]
pub struct SectionOverride {
    pub section: String,
    pub settings_json: String,
}

/// `set`: replace this node's whole record (wire `aster/NodeConfigSetRequest`).
/// Sections absent from `sections` are not overridden.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigSetRequest")]
pub struct NodeConfigSetRequest {
    /// `0`: unconditional. Otherwise refused with `ABORTED` unless the
    /// current generation equals it.
    pub expected_generation: i64,
    pub name: String,
    pub tags: Vec<String>,
    /// Typed values only (no nulls); keys may not start with `aster.`.
    pub attributes: Vec<Attr>,
    pub sections: Vec<SectionOverride>,
}

impl NodeConfigSetRequest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn expected_generation(mut self, generation: u64) -> Self {
        self.expected_generation = i64::try_from(generation).unwrap_or(i64::MAX);
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    pub fn attribute(mut self, key: impl Into<String>, value: impl Into<AttrValue>) -> Self {
        self.attributes.push(Attr {
            name: key.into(),
            value: Some(value.into().to_value()),
        });
        self
    }

    pub fn section(mut self, section: SectionOverride) -> Self {
        self.sections.push(section);
        self
    }

    pub fn identity(self, identity: IdentityOverride) -> Self {
        self.section(identity.to_section())
    }
}

/// `clear`: remove every override and property, reverting the node to its
/// file configuration (wire `aster/NodeConfigClearRequest`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigClearRequest")]
pub struct NodeConfigClearRequest {
    /// As [`NodeConfigSetRequest::expected_generation`].
    pub expected_generation: i64,
}

/// `ack`: is `generation` in effect? (wire `aster/NodeConfigAckRequest`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigAckRequest")]
pub struct NodeConfigAckRequest {
    pub generation: i64,
    /// How long to wait for the generation to be applied. `<= 0`: 10 s;
    /// capped at 60 s and by the call's deadline.
    pub timeout_ms: i64,
}

/// One part of a record that is not (yet) in effect (wire
/// `aster/NodeConfigOutstanding`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigOutstanding")]
pub struct NodeConfigOutstanding {
    /// `record`, `properties` or a section name.
    pub part: String,
    /// `pending` (not stored yet), `superseded` (a later generation is
    /// stored), `not-applied` (stored, not yet swapped in) or `not-in-effect`
    /// (applied, but the settings in force differ).
    pub state: String,
    pub detail: String,
}

/// The answer of `set`, `clear` and `ack` (wire `aster/NodeConfigAck`):
/// `complete` only once the asked-about generation is stored and its effect
/// is live.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigAck")]
pub struct NodeConfigAck {
    pub complete: bool,
    /// The generation stored now.
    pub generation: i64,
    /// The generation whose effect is in force.
    pub applied_generation: i64,
    pub outstanding: Vec<NodeConfigOutstanding>,
}

/// One overridable setting's effective value (wire `aster/NodeConfigSetting`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigSetting")]
pub struct EffectiveSetting {
    /// `section.field`, e.g. `identity.accept_on_behalf_of`.
    pub key: String,
    pub value: Option<Value>,
    /// `default`, `file`, `env`, `code` or `override`.
    pub source: String,
}

/// The answer of `get` (wire `aster/NodeConfigView`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeConfigView")]
pub struct NodeConfigView {
    pub node_id: String,
    /// The stored record's generation; `0` when there has never been one.
    pub generation: i64,
    pub applied_generation: i64,
    /// `set`, `clear`, or empty when there has never been a record.
    pub change: String,
    /// The node id of the admin who made the last change.
    pub changed_by: String,
    pub changed_at_ms: i64,
    pub name: String,
    pub tags: Vec<String>,
    pub attributes: Vec<Attr>,
    /// The section overrides in force.
    pub sections: Vec<SectionOverride>,
    /// Stored section overrides this node ignores (locked since they were set).
    pub ignored_sections: Vec<String>,
    /// Every overridable setting, with its effective value and source.
    pub settings: Vec<EffectiveSetting>,
    pub overridable_sections: Vec<String>,
    pub locked_sections: Vec<String>,
}

impl NodeConfigView {
    /// One setting, by `section.field` key.
    pub fn setting(&self, key: &str) -> Option<&EffectiveSetting> {
        self.settings.iter().find(|s| s.key == key)
    }

    /// This view's attributes as typed values (unknown variants dropped).
    pub fn typed_attributes(&self) -> BTreeMap<String, AttrValue> {
        self.attributes
            .iter()
            .filter_map(|attr| {
                let value = AttrValue::from_value(attr.value.as_ref()?)?;
                Some((attr.name.clone(), value))
            })
            .collect()
    }
}

/// Check `[remote_config] lock` names: an unknown section fails the start, so
/// a typo does not leave a section the operator meant to lock open.
pub fn validate_locks(locks: &[String]) -> crate::Result<()> {
    for lock in locks {
        if !OVERRIDABLE_SECTIONS.contains(&lock.as_str()) {
            return Err(crate::Error::InvalidArgument(format!(
                "remote_config.lock: {lock:?} is not an overridable section (overridable: {})",
                OVERRIDABLE_SECTIONS.join(", ")
            )));
        }
    }
    Ok(())
}

// ── The service ─────────────────────────────────────────────────────────────

#[cfg(feature = "docs")]
pub use service::*;

#[cfg(feature = "docs")]
mod service {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::config::{
        ConfigSource, SETTING_ACCEPT_ON_BEHALF_OF, SETTING_CLOCK_LEEWAY,
        SETTING_EDGE_TOKEN_MAX_LIFETIME, SETTING_MAX_KEY_SET_AGE, SETTING_TRUSTED_ISSUERS,
    };
    use crate::docs::Doc;
    use crate::id::{AuthorId, NamespaceSecret};
    use crate::log_targets::AUDIT;
    use crate::rpc::auth::{AttributeSource, ROLE_ATTRIBUTE};
    use crate::rpc::identity::{IdentitySettings, LiveIdentity};
    use crate::rpc::{CallContext, StatusCode};
    use crate::{Error, Node, Result};

    const RECORD_KEY: &[u8] = b"aster/node-config/v1/record";
    const RECORD_VERSION: u32 = 1;
    /// Domain separation for the namespace secret derived from the identity.
    const NAMESPACE_CONTEXT: &str = "aster node-config namespace v1";
    const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_ACK_TIMEOUT: Duration = Duration::from_secs(60);

    /// Baseline service: remote overrides of this node's configuration.
    /// Registered by default by [`AsterServer`](crate::rpc::AsterServer)
    /// behind `requires(aster.config-admin)`; see the [module docs](super).
    #[aster_macros::service(name = "aster.ops.NodeConfig", version = 1)]
    pub trait NodeConfig {
        /// The record and every overridable setting's effective value and
        /// source.
        #[rpc(idempotent)]
        async fn get(&self) -> aster::Result<NodeConfigView>;

        /// Replace the record. Answers once the new generation is in effect.
        async fn set(
            &self,
            req: NodeConfigSetRequest,
            ctx: &CallContext,
        ) -> aster::Result<NodeConfigAck>;

        /// Revert to the file configuration. Answers once that is in effect.
        async fn clear(
            &self,
            req: NodeConfigClearRequest,
            ctx: &CallContext,
        ) -> aster::Result<NodeConfigAck>;

        /// Whether a generation is stored and in effect, waiting for it to be
        /// applied up to the request's timeout. Reports, rather than fails,
        /// what is outstanding.
        #[rpc(idempotent)]
        async fn ack(&self, req: NodeConfigAckRequest) -> aster::Result<NodeConfigAck>;
    }

    /// Where a durable audit record of every `set` and `clear` goes.
    #[async_trait::async_trait]
    pub(crate) trait AuditSink: Send + Sync + 'static {
        async fn record(
            &self,
            kind: &'static str,
            payload: serde_json::Value,
        ) -> std::result::Result<(), String>;
    }

    #[cfg(feature = "audit-outbox")]
    #[async_trait::async_trait]
    impl AuditSink for crate::audit_outbox::AuditOutbox {
        async fn record(
            &self,
            kind: &'static str,
            payload: serde_json::Value,
        ) -> std::result::Result<(), String> {
            self.submit(crate::audit_outbox::AuditEvent {
                timestamp_ms: now_ms(),
                kind: kind.to_string(),
                payload,
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Change {
        Set,
        Clear,
    }

    impl Change {
        fn as_str(self) -> &'static str {
            match self {
                Self::Set => "set",
                Self::Clear => "clear",
            }
        }
    }

    /// The record as stored (JSON).
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredRecord {
        version: u32,
        generation: u64,
        change: Change,
        changed_by: String,
        changed_at_ms: u64,
        #[serde(default)]
        name: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        attributes: BTreeMap<String, AttrValue>,
        /// Validated section overrides, by section.
        #[serde(default)]
        sections: BTreeMap<String, serde_json::Value>,
    }

    /// The node-owned namespace holding the record.
    struct Store {
        doc: Doc,
        author: AuthorId,
    }

    impl Store {
        async fn open(node: &Node) -> Result<Self> {
            let identity = node.export_secret_key()?;
            let secret = blake3::derive_key(NAMESPACE_CONTEXT, &identity.to_bytes());
            let docs = node.docs();
            let doc = docs
                .open_or_import_write_namespace(NamespaceSecret::from_bytes(secret))
                .await?;
            let author = docs.default_author().await?;
            Ok(Self { doc, author })
        }

        async fn read(&self) -> Result<Option<StoredRecord>> {
            let Some(bytes) = self
                .doc
                .get_exact(&self.author, RECORD_KEY.to_vec())
                .await?
                .filter(|bytes| !bytes.is_empty())
            else {
                return Ok(None);
            };
            let record: StoredRecord = serde_json::from_slice(&bytes).map_err(|e| {
                Error::InvalidArgument(format!("node-config: stored record does not decode: {e}"))
            })?;
            if record.version != RECORD_VERSION {
                return Err(Error::InvalidArgument(format!(
                    "node-config: stored record version {} is not {RECORD_VERSION}",
                    record.version
                )));
            }
            Ok(Some(record))
        }

        async fn write(&self, record: &StoredRecord) -> Result<()> {
            let bytes = serde_json::to_vec(record).expect("JSON of a plain struct");
            self.doc
                .set_bytes_durable(&self.author, RECORD_KEY.to_vec(), bytes)
                .await?;
            Ok(())
        }
    }

    /// What one record produces.
    #[derive(Debug, Clone, PartialEq)]
    struct Effect {
        identity: IdentitySettings,
        identity_override: Option<IdentityOverride>,
        properties: NodeProperties,
        ignored: Vec<String>,
    }

    /// What [`AsterServer`](crate::rpc::AsterServer) hands the runtime.
    pub(crate) struct Init {
        pub(crate) identity: Arc<LiveIdentity>,
        pub(crate) properties: LiveProperties,
        /// The file / env / code `[identity]` settings.
        pub(crate) base_identity: IdentitySettings,
        pub(crate) sources: BTreeMap<String, ConfigSource>,
        pub(crate) locks: Vec<String>,
        /// The Enrollment credentials' attributes; `None` without Enrollment.
        pub(crate) authority: Option<Arc<dyn AttributeSource>>,
        pub(crate) audit: Option<Arc<dyn AuditSink>>,
    }

    struct Inner {
        node_id: String,
        store: Store,
        identity: Arc<LiveIdentity>,
        properties: LiveProperties,
        base_identity: IdentitySettings,
        sources: BTreeMap<String, ConfigSource>,
        locks: BTreeSet<String>,
        authority: Option<Arc<dyn AttributeSource>>,
        audit: Option<Arc<dyn AuditSink>>,
        /// The record applied now.
        current: arc_swap::ArcSwapOption<StoredRecord>,
        /// The generation whose effect is in force.
        applied: tokio::sync::watch::Sender<u64>,
        /// Serializes changes, so generations are linear.
        writes: tokio::sync::Mutex<()>,
    }

    /// A read-only handle to a running server's NodeConfig (from
    /// [`AsterServer::node_config`](crate::rpc::AsterServer::node_config)).
    /// Changes come only from admins over RPC; there is no local setter.
    #[derive(Clone)]
    pub struct NodeConfigHandle {
        inner: Arc<Inner>,
    }

    impl NodeConfigHandle {
        /// Open the store, load the stored record and apply it. Called before
        /// the server serves, so no call sees the file settings when an
        /// override is stored.
        pub(crate) async fn open(node: &Node, init: Init) -> Result<Self> {
            validate_locks(&init.locks)?;
            let store = Store::open(node).await?;
            let stored = match store.read().await {
                Ok(record) => record,
                Err(error) => {
                    // Fail safe: the file settings, not a record this build
                    // cannot read. The next change replaces it.
                    tracing::warn!(
                        target: crate::log_targets::SERVER,
                        error = %error,
                        "ignoring the stored node-config record"
                    );
                    None
                }
            };
            let (applied, _) = tokio::sync::watch::channel(0);
            let handle = Self {
                inner: Arc::new(Inner {
                    node_id: node.id().to_string(),
                    store,
                    identity: init.identity,
                    properties: init.properties,
                    base_identity: init.base_identity,
                    sources: init.sources,
                    locks: init.locks.into_iter().collect(),
                    authority: init.authority,
                    audit: init.audit,
                    current: arc_swap::ArcSwapOption::empty(),
                    applied,
                    writes: tokio::sync::Mutex::new(()),
                }),
            };
            handle.apply(stored);
            Ok(handle)
        }

        /// The record and effective settings, as `get` reports them.
        pub fn view(&self) -> NodeConfigView {
            let inner = &*self.inner;
            let record = inner.current.load_full();
            let effect = self.effect(record.as_deref());
            let mut view = NodeConfigView {
                node_id: inner.node_id.clone(),
                applied_generation: to_i64(*inner.applied.borrow()),
                name: effect.properties.name.clone(),
                tags: effect.properties.tags.clone(),
                attributes: effect
                    .properties
                    .attributes
                    .iter()
                    .map(|(name, value)| Attr {
                        name: name.clone(),
                        value: Some(value.to_value()),
                    })
                    .collect(),
                sections: effect
                    .identity_override
                    .iter()
                    .map(IdentityOverride::to_section)
                    .collect(),
                ignored_sections: effect.ignored.clone(),
                settings: self.identity_settings(effect.identity_override.as_ref()),
                overridable_sections: OVERRIDABLE_SECTIONS.iter().map(|s| s.to_string()).collect(),
                locked_sections: inner.locks.iter().cloned().collect(),
                ..Default::default()
            };
            if let Some(record) = record.as_deref() {
                view.generation = to_i64(record.generation);
                view.change = record.change.as_str().into();
                view.changed_by = record.changed_by.clone();
                view.changed_at_ms = to_i64(record.changed_at_ms);
            }
            view
        }

        /// The properties handlers see now.
        pub fn properties(&self) -> Arc<NodeProperties> {
            self.inner.properties.load()
        }

        /// The `[identity]` settings in force now.
        pub fn identity(&self) -> IdentitySettings {
            self.inner.identity.load()
        }

        /// The generation whose effect is in force (`0`: never changed).
        pub fn generation(&self) -> u64 {
            *self.inner.applied.borrow()
        }

        /// Every overridable `[identity]` setting with its effective value
        /// and source. `trusted_issuers` is reported as its JSON text.
        fn identity_settings(&self, o: Option<&IdentityOverride>) -> Vec<EffectiveSetting> {
            let base = &self.inner.base_identity;
            let default = IdentitySettings::default();
            let issuers = |list: &Vec<crate::TrustedIssuer>| {
                Value::from(serde_json::to_string(list).expect("JSON of plain structs"))
            };
            let int = |value: u64| Value::from(i64::try_from(value).unwrap_or(i64::MAX));
            vec![
                self.setting(
                    SETTING_ACCEPT_ON_BEHALF_OF,
                    o.and_then(|o| o.accept_on_behalf_of).map(Value::from),
                    Value::from(base.accept_on_behalf_of),
                    Value::from(default.accept_on_behalf_of),
                ),
                self.setting(
                    SETTING_TRUSTED_ISSUERS,
                    o.and_then(|o| o.trusted_issuers.as_ref()).map(issuers),
                    issuers(&base.trusted_issuers),
                    issuers(&default.trusted_issuers),
                ),
                self.setting(
                    SETTING_EDGE_TOKEN_MAX_LIFETIME,
                    o.and_then(|o| o.edge_token_max_lifetime_secs).map(int),
                    int(base.edge_token_max_lifetime_secs),
                    int(default.edge_token_max_lifetime_secs),
                ),
                self.setting(
                    SETTING_CLOCK_LEEWAY,
                    o.and_then(|o| o.clock_leeway_secs).map(int),
                    int(base.clock_leeway_secs),
                    int(default.clock_leeway_secs),
                ),
                self.setting(
                    SETTING_MAX_KEY_SET_AGE,
                    o.and_then(|o| o.max_key_set_age_secs).map(int),
                    int(base.max_key_set_age_secs),
                    int(default.max_key_set_age_secs),
                ),
            ]
        }

        fn setting(
            &self,
            key: &str,
            overridden: Option<Value>,
            base: Value,
            default: Value,
        ) -> EffectiveSetting {
            let (value, source) = match overridden {
                Some(value) => (value, ConfigSource::Override),
                None => {
                    let source =
                        self.inner
                            .sources
                            .get(key)
                            .copied()
                            .unwrap_or(if base == default {
                                ConfigSource::Default
                            } else {
                                ConfigSource::Code
                            });
                    (base, source)
                }
            };
            EffectiveSetting {
                key: key.into(),
                value: Some(value),
                source: source.as_str().into(),
            }
        }

        /// What `record` produces on this node: the file settings with the
        /// record's unlocked overrides applied.
        fn effect(&self, record: Option<&StoredRecord>) -> Effect {
            let inner = &*self.inner;
            let mut effect = Effect {
                identity: inner.base_identity.clone(),
                identity_override: None,
                properties: NodeProperties::default(),
                ignored: Vec::new(),
            };
            let Some(record) = record.filter(|r| r.change == Change::Set) else {
                return effect;
            };
            effect.properties = NodeProperties {
                name: record.name.clone(),
                tags: record.tags.clone(),
                attributes: record.attributes.clone(),
            };
            for (section, settings) in &record.sections {
                if inner.locks.contains(section) {
                    effect.ignored.push(section.clone());
                    continue;
                }
                match parse_section(section, settings) {
                    Ok(Parsed::Identity(o)) => {
                        // The section as a whole, against today's file
                        // settings: a stored override that no longer
                        // validates is ignored, never half-applied.
                        let merged = o.apply(&inner.base_identity);
                        if merged.validate().is_err() {
                            effect.ignored.push(section.clone());
                            continue;
                        }
                        effect.identity = merged;
                        effect.identity_override = Some(o);
                    }
                    // A stored section this build no longer accepts.
                    Err(_) => effect.ignored.push(section.clone()),
                }
            }
            effect
        }

        /// Swap `record`'s effect in, then publish its generation.
        fn apply(&self, record: Option<StoredRecord>) {
            let inner = &*self.inner;
            let effect = self.effect(record.as_ref());
            let generation = record.as_ref().map_or(0, |r| r.generation);
            inner.identity.store(effect.identity);
            inner.properties.store(effect.properties);
            inner.current.store(record.map(Arc::new));
            inner.applied.send_replace(generation);
        }

        fn in_effect(&self, record: Option<&StoredRecord>) -> Vec<NodeConfigOutstanding> {
            let effect = self.effect(record);
            let mut out = Vec::new();
            if self.inner.identity.load() != effect.identity {
                out.push(outstanding(
                    IDENTITY_SECTION,
                    "not-in-effect",
                    "the [identity] settings in force differ from the record's",
                ));
            }
            if *self.inner.properties.load() != effect.properties {
                out.push(outstanding(
                    "properties",
                    "not-in-effect",
                    "the properties in force differ from the record's",
                ));
            }
            out
        }

        /// Report on `generation` from the stored record and the effect in
        /// force (portal's `policy_ack`: the answer comes from this node's
        /// store and its live settings, never from the sender's view).
        async fn report(&self, generation: u64) -> Result<NodeConfigAck> {
            let stored = self.inner.store.read().await?;
            let stored_generation = stored.as_ref().map_or(0, |r| r.generation);
            let applied = *self.inner.applied.borrow();
            let mut outstanding_parts = Vec::new();
            if stored_generation < generation {
                outstanding_parts.push(outstanding(
                    "record",
                    "pending",
                    &format!("generation {generation} is not stored (stored: {stored_generation})"),
                ));
            } else if stored_generation > generation {
                outstanding_parts.push(outstanding(
                    "record",
                    "superseded",
                    &format!("generation {stored_generation} replaced {generation}"),
                ));
            }
            if applied != stored_generation {
                outstanding_parts.push(outstanding(
                    "record",
                    "not-applied",
                    &format!("generation {stored_generation} is stored, {applied} is in effect"),
                ));
            } else {
                outstanding_parts.extend(self.in_effect(stored.as_ref()));
            }
            Ok(NodeConfigAck {
                complete: outstanding_parts.is_empty(),
                generation: to_i64(stored_generation),
                applied_generation: to_i64(applied),
                outstanding: outstanding_parts,
            })
        }

        async fn ack(&self, generation: u64, timeout: Duration) -> Result<NodeConfigAck> {
            let mut applied = self.inner.applied.subscribe();
            // A condition wait: the watch wakes on every apply. Timing out is
            // an answer (reported below), not an error.
            let _ = tokio::time::timeout(timeout, applied.wait_for(|g| *g >= generation)).await;
            self.report(generation).await
        }

        /// The admin making a change: an enrolled peer whose own credential
        /// grants [`CONFIG_ADMIN_ROLE`], calling as itself over Iroh.
        fn authorize(&self, ctx: &CallContext) -> Result<String> {
            let denied = |message: &str| Err(Error::rpc(StatusCode::PermissionDenied, message));
            let Some(authority) = &self.inner.authority else {
                return denied(
                    "this node has no Enrollment, so no peer can hold aster.config-admin: \
                     it accepts no configuration changes",
                );
            };
            if ctx.actor().is_some() {
                return denied("configuration changes are not accepted on behalf of a user");
            }
            if !ctx.provenance().is_authenticated() {
                return denied("configuration changes need an Iroh-authenticated caller");
            }
            let roles = authority
                .attributes(ctx.peer())
                .and_then(|sourced| sourced.attributes.get(ROLE_ATTRIBUTE).cloned())
                .unwrap_or_default();
            if !roles
                .split(',')
                .any(|role| role.trim() == CONFIG_ADMIN_ROLE)
            {
                return denied(
                    "the caller's Enrollment credential does not grant aster.config-admin",
                );
            }
            Ok(ctx.peer().to_string())
        }

        fn check_generation(&self, expected: i64) -> Result<u64> {
            let current = self
                .inner
                .current
                .load()
                .as_ref()
                .map_or(0, |r| r.generation);
            if expected > 0 && u64::try_from(expected).ok() != Some(current) {
                return Err(Error::rpc(
                    StatusCode::Aborted,
                    format!("the record is at generation {current}, not {expected}"),
                ));
            }
            Ok(current)
        }

        /// Audit, store, apply, then report. The caller holds `writes`.
        async fn commit(&self, record: StoredRecord) -> Result<NodeConfigAck> {
            let kind = match record.change {
                Change::Set => "aster.node_config.set",
                Change::Clear => "aster.node_config.clear",
            };
            let payload = serde_json::json!({
                "node_id": self.inner.node_id,
                "change": record.change.as_str(),
                "generation": record.generation,
                "principal": record.changed_by,
                "time_ms": record.changed_at_ms,
                "name": record.name,
                "tags": record.tags,
                "attributes": record.attributes.keys().collect::<Vec<_>>(),
                "sections": record.sections,
            });
            tracing::info!(
                target: AUDIT,
                kind,
                node_id = self.inner.node_id.as_str(),
                principal = record.changed_by.as_str(),
                generation = record.generation,
                "node config {}",
                record.change.as_str()
            );
            // Audited before it is stored, as Exec does: a crash in between
            // leaves an audited change that never happened, never the reverse.
            if let Some(audit) = &self.inner.audit {
                audit.record(kind, payload).await.map_err(|error| {
                    Error::rpc(
                        StatusCode::Unavailable,
                        format!("the audit outbox refused the change, so nothing changed: {error}"),
                    )
                })?;
            }
            self.inner.store.write(&record).await?;
            let generation = record.generation;
            self.apply(Some(record));
            self.report(generation).await
        }

        async fn set(&self, req: NodeConfigSetRequest, ctx: &CallContext) -> Result<NodeConfigAck> {
            let principal = self.authorize(ctx)?;
            let validated = self.validate(&req)?;
            let _writes = self.inner.writes.lock().await;
            let current = self.check_generation(req.expected_generation)?;
            self.commit(StoredRecord {
                version: RECORD_VERSION,
                generation: current + 1,
                change: Change::Set,
                changed_by: principal,
                changed_at_ms: now_ms(),
                ..validated
            })
            .await
        }

        async fn clear(
            &self,
            req: NodeConfigClearRequest,
            ctx: &CallContext,
        ) -> Result<NodeConfigAck> {
            let principal = self.authorize(ctx)?;
            let _writes = self.inner.writes.lock().await;
            let current = self.check_generation(req.expected_generation)?;
            self.commit(StoredRecord {
                version: RECORD_VERSION,
                generation: current + 1,
                change: Change::Clear,
                changed_by: principal,
                changed_at_ms: now_ms(),
                name: String::new(),
                tags: Vec::new(),
                attributes: BTreeMap::new(),
                sections: BTreeMap::new(),
            })
            .await
        }

        /// Validate a whole `set` before anything is stored.
        fn validate(&self, req: &NodeConfigSetRequest) -> Result<StoredRecord> {
            check_text("name", &req.name, MAX_KEY_BYTES, true)?;
            if req.tags.len() > MAX_TAGS {
                return Err(invalid(format!("at most {MAX_TAGS} tags")));
            }
            let mut tags = BTreeSet::new();
            for tag in &req.tags {
                check_text("tag", tag, MAX_KEY_BYTES, false)?;
                tags.insert(tag.clone());
            }
            if req.attributes.len() > MAX_ATTRIBUTES {
                return Err(invalid(format!("at most {MAX_ATTRIBUTES} attributes")));
            }
            let mut attributes = BTreeMap::new();
            for attr in &req.attributes {
                let key = &attr.name;
                check_text("attribute key", key, MAX_KEY_BYTES, false)?;
                if key
                    .get(..RESERVED_ATTRIBUTE_PREFIX.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(RESERVED_ATTRIBUTE_PREFIX))
                {
                    return Err(Error::rpc(
                        StatusCode::PermissionDenied,
                        format!(
                            "attribute {key:?}: keys starting {RESERVED_ATTRIBUTE_PREFIX:?} are \
                             reserved; NodeConfig attributes never authorise"
                        ),
                    ));
                }
                let value = attr
                    .value
                    .as_ref()
                    .ok_or_else(|| invalid(format!("attribute {key:?} has no value")))?;
                let value = AttrValue::from_value(value)
                    .ok_or_else(|| invalid(format!("attribute {key:?}: unknown value type")))?;
                match &value {
                    AttrValue::Float(v) if !v.is_finite() => {
                        return Err(invalid(format!("attribute {key:?}: float is not finite")))
                    }
                    AttrValue::String(v) => check_text("attribute value", v, MAX_TEXT_BYTES, true)?,
                    _ => {}
                }
                if attributes.insert(key.clone(), value).is_some() {
                    return Err(invalid(format!("attribute {key:?} appears twice")));
                }
            }
            let mut sections = BTreeMap::new();
            for section in &req.sections {
                let name = section.section.as_str();
                if !OVERRIDABLE_SECTIONS.contains(&name) {
                    return Err(Error::rpc(
                        StatusCode::PermissionDenied,
                        format!(
                            "[{name}] is not overridable (overridable: {})",
                            OVERRIDABLE_SECTIONS.join(", ")
                        ),
                    ));
                }
                if self.inner.locks.contains(name) {
                    return Err(Error::rpc(
                        StatusCode::FailedPrecondition,
                        format!("this node's configuration locks [{name}] ([remote_config] lock)"),
                    ));
                }
                if section.settings_json.len() > MAX_SECTION_BYTES {
                    return Err(invalid(format!(
                        "[{name}]: larger than {MAX_SECTION_BYTES} bytes"
                    )));
                }
                let settings: serde_json::Value = serde_json::from_str(&section.settings_json)
                    .map_err(|e| invalid(format!("[{name}]: not JSON: {e}")))?;
                match parse_section(name, &settings)? {
                    Parsed::Identity(o) => o
                        .apply(&self.inner.base_identity)
                        .validate()
                        .map_err(|e| invalid(format!("[{name}]: {e}")))?,
                }
                if sections.insert(name.to_string(), settings).is_some() {
                    return Err(invalid(format!("[{name}] appears twice")));
                }
            }
            Ok(StoredRecord {
                version: RECORD_VERSION,
                generation: 0,
                change: Change::Set,
                changed_by: String::new(),
                changed_at_ms: 0,
                name: req.name.clone(),
                tags: tags.into_iter().collect(),
                attributes,
                sections,
            })
        }
    }

    enum Parsed {
        Identity(IdentityOverride),
    }

    /// Validate one section override as a whole: an object, only the
    /// section's overridable fields, each with the right type, at least one.
    fn parse_section(section: &str, settings: &serde_json::Value) -> Result<Parsed> {
        let object = settings
            .as_object()
            .ok_or_else(|| invalid(format!("[{section}]: expected a JSON object")))?;
        let fields = match section {
            IDENTITY_SECTION => IDENTITY_FIELDS,
            other => {
                return Err(Error::rpc(
                    StatusCode::PermissionDenied,
                    format!("[{other}] is not overridable"),
                ))
            }
        };
        if object.is_empty() {
            return Err(invalid(format!("[{section}]: the override sets nothing")));
        }
        for key in object.keys() {
            if !fields.contains(&key.as_str()) {
                return Err(Error::rpc(
                    StatusCode::PermissionDenied,
                    format!(
                        "{section}.{key} is not overridable (overridable: {})",
                        fields.join(", ")
                    ),
                ));
            }
        }
        match section {
            IDENTITY_SECTION => serde_json::from_value(settings.clone())
                .map(Parsed::Identity)
                .map_err(|e| invalid(format!("[{section}]: {e}"))),
            _ => unreachable!("checked above"),
        }
    }

    fn check_text(what: &str, value: &str, max: usize, empty_ok: bool) -> Result<()> {
        if value.is_empty() && !empty_ok {
            return Err(invalid(format!("{what} is empty")));
        }
        if value.len() > max {
            return Err(invalid(format!("{what} is longer than {max} bytes")));
        }
        if value.chars().any(char::is_control) {
            return Err(invalid(format!("{what} contains a control character")));
        }
        Ok(())
    }

    fn invalid(message: String) -> Error {
        Error::rpc(StatusCode::InvalidArgument, message)
    }

    fn outstanding(part: &str, state: &str, detail: &str) -> NodeConfigOutstanding {
        NodeConfigOutstanding {
            part: part.into(),
            state: state.into(),
            detail: detail.into(),
        }
    }

    fn to_i64(value: u64) -> i64 {
        i64::try_from(value).unwrap_or(i64::MAX)
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// The stock implementation [`AsterServer`](crate::rpc::AsterServer)
    /// serves.
    pub(crate) struct LiveNodeConfig {
        pub(crate) handle: NodeConfigHandle,
    }

    #[async_trait::async_trait]
    impl NodeConfig for LiveNodeConfig {
        async fn get(&self) -> aster::Result<NodeConfigView> {
            Ok(self.handle.view())
        }

        async fn set(
            &self,
            req: NodeConfigSetRequest,
            ctx: &CallContext,
        ) -> aster::Result<NodeConfigAck> {
            self.handle.set(req, ctx).await
        }

        async fn clear(
            &self,
            req: NodeConfigClearRequest,
            ctx: &CallContext,
        ) -> aster::Result<NodeConfigAck> {
            self.handle.clear(req, ctx).await
        }

        async fn ack(&self, req: NodeConfigAckRequest) -> aster::Result<NodeConfigAck> {
            let generation = u64::try_from(req.generation)
                .map_err(|_| invalid("generation is negative".into()))?;
            let mut timeout = match u64::try_from(req.timeout_ms) {
                Ok(0) | Err(_) => DEFAULT_ACK_TIMEOUT,
                Ok(ms) => Duration::from_millis(ms).min(MAX_ACK_TIMEOUT),
            };
            if let Some(remaining) = CallContext::try_current().and_then(|ctx| ctx.remaining()) {
                // Leave room to answer before the caller gives up.
                timeout = timeout.min(remaining.saturating_sub(Duration::from_millis(250)));
            }
            self.handle.ack(generation, timeout).await
        }
    }
}
