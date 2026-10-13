//! Enrollment driven by [`AsterConfig`]: what `AsterServer` and `AsterClient`
//! install from the `[enrollment]` section (design: "AsterServer
//! integration" in `docs/_internal/working_ideas/aster-enrollment.md`).
//!
//! Two public pieces are reusable outside the builders: [`PolicyConfig`], the
//! declarative admission policy, and [`RoleMapping`], which turns a peer's
//! verified claims into the attributes Gate 3 checks `requires` against.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{
    AdmissionClaims, AdmissionEvidenceSource, AdmissionPolicy, Enrollment, EnrollmentOptions,
    EnrollmentPolicyStore, Issuer, JwtEnrollmentVerifier, PolicyExpressionV1, PolicyPredicate,
    RenewalConfig, RenewalHandle, RotationPin, SourceCidr, TypedAttribute, DEFAULT_ISSUED_LIFETIME,
    MAX_CHAIN_BYTES,
};
use crate::config::{AsterConfig, EnrollmentConfig, RoleMappingConfig};
use crate::error::{Error, Result};
use crate::rpc::{AttributeSource, ROLE_ATTRIBUTE};
use crate::{Node, NodeAddr, NodeId};

/// Default bound on synchronizing the fleet namespace from a replica.
const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(15);

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("enrollment: {message}"))
}

// ── Declarative policy ──────────────────────────────────────────────────────

/// A [`PolicyExpressionV1`] as configuration: an externally tagged tree, the
/// same shape in TOML, JSON (`ASTER_ENROLLMENT_POLICY`) and the C ABI.
///
/// ```toml
/// [enrollment]
/// policy = { all_of = [{ scope = "ops" }, "tolerate_stale_policy"] }
/// ```
///
/// Nodes: `all_of` / `any_of` / `none_of` (non-empty arrays); `source_cidr`,
/// `audience`, `scope`, `tag` (strings); `attribute_equals`
/// (`{ key, value = { type = "bool"|"int"|"float"|"string", value } }`);
/// `issued_by` (64-hex key on the chain's issuer path);
/// `max_credential_age_secs`; `policy_synchronized_within`
/// (`{ max_age_secs, require_root }`); and the bare strings
/// `"tolerate_stale_policy"` and `"require_registration"`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PolicyConfig {
    AllOf(Vec<PolicyConfig>),
    AnyOf(Vec<PolicyConfig>),
    NoneOf(Vec<PolicyConfig>),
    SourceCidr(String),
    Audience(String),
    Scope(String),
    Tag(String),
    AttributeEquals {
        key: String,
        value: TypedAttribute,
    },
    TolerateStalePolicy,
    PolicySynchronizedWithin {
        max_age_secs: u64,
        #[serde(default)]
        require_root: bool,
    },
    RequireRegistration,
    IssuedBy(String),
    MaxCredentialAgeSecs(u64),
}

impl PolicyConfig {
    /// Parse the JSON form.
    pub fn from_json(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(value.clone()).map_err(|e| invalid(format!("policy: {e}")))
    }

    /// Lower to the expression Enrollment evaluates, validated against its
    /// depth and size bounds.
    pub fn to_expression(&self) -> Result<PolicyExpressionV1> {
        let expression = self.lower()?;
        expression
            .validate()
            .map_err(|e| invalid(format!("policy: {e}")))?;
        Ok(expression)
    }

    fn lower(&self) -> Result<PolicyExpressionV1> {
        let all = |children: &[PolicyConfig]| -> Result<Vec<PolicyExpressionV1>> {
            children.iter().map(PolicyConfig::lower).collect()
        };
        let predicate = PolicyExpressionV1::Predicate;
        Ok(match self {
            Self::AllOf(children) => PolicyExpressionV1::AllOf(all(children)?),
            Self::AnyOf(children) => PolicyExpressionV1::AnyOf(all(children)?),
            Self::NoneOf(children) => PolicyExpressionV1::NoneOf(all(children)?),
            Self::SourceCidr(cidr) => predicate(PolicyPredicate::SourceCidr(
                cidr.parse::<SourceCidr>()
                    .map_err(|_| invalid(format!("policy: invalid source_cidr {cidr:?}")))?,
            )),
            Self::Audience(value) => predicate(PolicyPredicate::Audience(value.clone())),
            Self::Scope(value) => predicate(PolicyPredicate::Scope(value.clone())),
            Self::Tag(value) => predicate(PolicyPredicate::Tag(value.clone())),
            Self::AttributeEquals { key, value } => predicate(PolicyPredicate::AttributeEquals {
                key: key.clone(),
                value: value.clone(),
            }),
            Self::TolerateStalePolicy => predicate(PolicyPredicate::TolerateStalePolicy),
            Self::PolicySynchronizedWithin {
                max_age_secs,
                require_root,
            } => predicate(PolicyPredicate::PolicySynchronizedWithin {
                max_age: Duration::from_secs(*max_age_secs),
                require_root: *require_root,
            }),
            Self::RequireRegistration => predicate(PolicyPredicate::RequireRegistration),
            Self::IssuedBy(key) => {
                predicate(PolicyPredicate::IssuedBy(hex32(key, "policy issued_by")?))
            }
            Self::MaxCredentialAgeSecs(seconds) => predicate(PolicyPredicate::MaxCredentialAge(
                Duration::from_secs(*seconds),
            )),
        })
    }
}

// ── Claims → roles ──────────────────────────────────────────────────────────

/// Turns a peer's verified claims into Gate-3 attributes (see
/// [`RoleMappingConfig`] for the rules). Only the mapping writes `aster.role`:
/// a claim attribute of that name counts only through
/// [`RoleMappingConfig::attribute`].
#[derive(Clone, Debug, Default)]
pub struct RoleMapping {
    config: RoleMappingConfig,
}

impl RoleMapping {
    pub fn new(config: RoleMappingConfig) -> Self {
        Self { config }
    }

    /// The roles these claims carry.
    pub fn roles(&self, claims: &AdmissionClaims) -> BTreeSet<String> {
        let mut roles = BTreeSet::new();
        let config = &self.config;
        if !config.attribute.is_empty() {
            if let Some(TypedAttribute::String(value)) = claims
                .attributes
                .as_ref()
                .and_then(|attributes| attributes.get(&config.attribute))
            {
                roles.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|role| !role.is_empty())
                        .map(str::to_string),
                );
            }
        }
        for scope in claims.scopes.iter().flatten() {
            if config.scopes_as_roles {
                roles.insert(scope.clone());
            }
            if let Some(mapped) = config.scopes.get(scope) {
                roles.extend(mapped.iter().cloned());
            }
        }
        for tag in claims.tags.iter().flatten() {
            if let Some(mapped) = config.tags.get(tag) {
                roles.extend(mapped.iter().cloned());
            }
        }
        roles
    }

    /// The Gate-3 attributes for these claims: the passed-through claim
    /// attributes (when enabled) and `aster.role`, the sorted, comma-joined
    /// role set (absent when there are no roles).
    pub fn attributes(&self, claims: &AdmissionClaims) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if self.config.include_attributes {
            for (key, value) in claims.attributes.iter().flatten() {
                if key == ROLE_ATTRIBUTE || key == &self.config.attribute {
                    continue;
                }
                let text = match value {
                    TypedAttribute::Bool(v) => v.to_string(),
                    TypedAttribute::Int(v) => v.to_string(),
                    TypedAttribute::Float(v) => v.to_string(),
                    TypedAttribute::String(v) => v.clone(),
                };
                out.insert(key.clone(), text);
            }
        }
        let roles = self.roles(claims);
        if !roles.is_empty() {
            out.insert(
                ROLE_ATTRIBUTE.to_string(),
                roles.into_iter().collect::<Vec<_>>().join(","),
            );
        }
        out
    }
}

/// Gate-3 attributes from each peer's live enrollment session. A peer with no
/// session (never enrolled, revoked, evicted) or expired evidence has none.
pub(crate) struct EnrollmentAttributes {
    pub(crate) enrollment: Enrollment,
    pub(crate) mapping: RoleMapping,
}

impl AttributeSource for EnrollmentAttributes {
    fn attributes(&self, peer: &str) -> Option<HashMap<String, String>> {
        let key = parse_hex32(peer)?;
        let evidence = self.enrollment.peer_evidence(&key)?;
        if evidence.expires_at().is_some_and(|exp| unix_now() >= exp) {
            return None;
        }
        Some(self.mapping.attributes(evidence.claims()))
    }
}

// ── Resolution ──────────────────────────────────────────────────────────────

/// Which builder is installing: a server keeps durable state and may issue; a
/// client may use temporary state and renews from its server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Server,
    Client,
}

/// Where the policy store lives.
pub enum StateLocation {
    Path(PathBuf),
    /// A client's temporary directory, removed when the client closes.
    Temporary(TempState),
}

/// A temporary state directory, removed on drop.
pub struct TempState(PathBuf);

impl TempState {
    fn create() -> Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|e| invalid(format!("state: {e}")))?;
        let dir = std::env::temp_dir().join(format!("aster-enrollment-{}", hex::encode(nonce)));
        std::fs::create_dir_all(&dir).map_err(|e| invalid(format!("state: {e}")))?;
        Ok(Self(dir))
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Everything validated before the node starts, so a misconfiguration never
/// leaves a half-started node behind.
pub struct Plan {
    side: Side,
    root: [u8; 32],
    namespace: [u8; 32],
    audience: String,
    credential: String,
    policy: Arc<dyn AdmissionPolicy>,
    options: EnrollmentOptions,
    leeway: Option<u64>,
    pub(crate) state: StateLocation,
    pub(crate) replicas: Vec<NodeAddr>,
    pub(crate) sync_timeout: Duration,
    delegation: Option<crate::delegation::Chain>,
    issuer_lifetime: Duration,
    renew_from: Option<NodeAddr>,
    renew: Option<bool>,
    pub(crate) synchronize: bool,
    pub(crate) mapping: RoleMapping,
}

/// Builder-supplied overrides of the configured policy and role mapping.
#[derive(Clone, Default)]
pub struct Overrides {
    pub policy: Option<Arc<dyn AdmissionPolicy>>,
    pub roles: Option<RoleMapping>,
}

/// Validate `config`'s enrollment section. `Ok(None)` when it is off. When it
/// is on, the node configuration is adjusted for Enrollment (fail-closed
/// hooks) and the caller registers [`super::ENROLLMENT_ALPN`].
pub fn plan(config: &mut AsterConfig, side: Side, overrides: Overrides) -> Result<Option<Plan>> {
    if !config.is_enrollment_enabled() {
        return Ok(None);
    }
    if let Some(path) = &config.root_pubkey_file {
        if !path.exists() {
            return Err(invalid(format!(
                "root_pubkey_file {} does not exist",
                path.display()
            )));
        }
    }
    let root = config.resolve_root_pubkey()?.ok_or_else(|| {
        invalid(
            "no trusted root: set [trust] root_pubkey or root_pubkey_file \
             (ASTER_ROOT_PUBKEY / ASTER_ROOT_PUBKEY_FILE)",
        )
    })?;
    let section: &EnrollmentConfig = &config.enrollment;
    let audience = section.audience.clone().ok_or_else(|| {
        invalid("audience is required ([enrollment] audience / ASTER_ENROLLMENT_AUDIENCE)")
    })?;
    let namespace = hex32(
        section.fleet_namespace.as_deref().ok_or_else(|| {
            invalid(
                "fleet_namespace is required ([enrollment] fleet_namespace / \
                 ASTER_ENROLLMENT_FLEET_NAMESPACE)",
            )
        })?,
        "fleet_namespace",
    )?;
    let credential = match (
        &config.enrollment_credential_chain,
        &config.enrollment_credential_file,
    ) {
        (Some(chain), _) => {
            let chain = chain.trim().to_string();
            if crate::delegation::Chain::from_text(&chain).is_err() {
                return Err(invalid(
                    "enrollment_credential_chain (ASTER_ENROLLMENT_CREDENTIAL_CHAIN) is not \
                     a credential chain; for a file use [trust] enrollment_credential \
                     (ASTER_ENROLLMENT_CREDENTIAL)",
                ));
            }
            chain
        }
        (None, Some(path)) => read_text(path, "enrollment_credential")?,
        (None, None) => {
            return Err(invalid(
                "this node has no credential: set [trust] enrollment_credential (a file) or \
                 enrollment_credential_chain (ASTER_ENROLLMENT_CREDENTIAL / \
                 ASTER_ENROLLMENT_CREDENTIAL_CHAIN); issue one with `aster enroll node`",
            ))
        }
    };
    if credential.is_empty() {
        return Err(invalid("the credential is empty"));
    }

    let policy: Arc<dyn AdmissionPolicy> = match (overrides.policy, &section.policy) {
        (Some(policy), _) => policy,
        (None, Some(json)) => Arc::new(PolicyConfig::from_json(json)?.to_expression()?),
        (None, None) => Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
    };

    let mut options = EnrollmentOptions {
        require_synchronized_clock: section.require_synchronized_clock,
        applied_rotations: config.root_rotations.clone(),
        ..Default::default()
    };
    if let Some(commitment) = &config.next_root_commitment {
        options.rotation = Some(RotationPin {
            next_commitment: hex32(commitment, "next_root_commitment")?,
            seq: config.root_seq,
        });
    } else if !config.root_rotations.is_empty() {
        return Err(invalid(
            "root_rotations needs next_root_commitment (the rotation pin they continue)",
        ));
    }
    let limits = &section.limits;
    let ms = Duration::from_millis;
    if let Some(value) = limits.handshake_timeout_ms {
        options.handshake_timeout = ms(value);
    }
    if let Some(value) = limits.step_timeout_ms {
        options.step_timeout = ms(value);
    }
    if let Some(value) = limits.policy_timeout_ms {
        options.policy_timeout = ms(value);
    }
    if let Some(value) = limits.reconciliation_interval_ms {
        options.reconciliation_interval = ms(value);
    }
    if let Some(value) = limits.max_admitted_peers {
        options.max_admitted_peers = usize::try_from(value)
            .map_err(|_| invalid("limits.max_admitted_peers is too large"))?;
    }
    let sync_timeout = limits.sync_timeout_ms.map_or(DEFAULT_SYNC_TIMEOUT, ms);

    let state = match (&section.state_path, &config.storage_path, side) {
        (Some(path), _, _) => StateLocation::Path(path.clone()),
        (None, Some(dir), _) => StateLocation::Path(dir.join("enrollment.json")),
        (None, None, Side::Client) => StateLocation::Temporary(TempState::create()?),
        (None, None, Side::Server) => {
            return Err(invalid(
                "a server keeps denials durably: set [enrollment] state_path \
                 (ASTER_ENROLLMENT_STATE_PATH) or a persistent [storage] path",
            ))
        }
    };

    let replicas = section
        .replicas
        .iter()
        .map(|replica| parse_addr(replica, "replicas"))
        .collect::<Result<Vec<_>>>()?;

    let delegation = match (&section.issuer_delegation_file, side) {
        (None, _) => None,
        (Some(_), Side::Client) => {
            return Err(invalid(
                "issuer.delegation_file is server-only; an AsterClient cannot serve renewals",
            ))
        }
        (Some(path), Side::Server) => {
            let text = read_text(path, "issuer.delegation_file")?;
            Some(
                crate::delegation::Chain::from_text(&text)
                    .map_err(|e| invalid(format!("issuer.delegation_file: {e}")))?,
            )
        }
    };
    let issuer_lifetime = section
        .issuer_lifetime_secs
        .map_or(DEFAULT_ISSUED_LIFETIME, Duration::from_secs);
    let renew_from = section
        .renew_from
        .as_deref()
        .map(|addr| parse_addr(addr, "renew_from"))
        .transpose()?;

    let mapping = overrides
        .roles
        .unwrap_or_else(|| RoleMapping::new(section.roles.clone()));
    let plan = Plan {
        side,
        root: root.to_bytes(),
        namespace,
        audience,
        credential,
        policy,
        options,
        leeway: section.activation_leeway_secs,
        state,
        replicas,
        sync_timeout,
        delegation,
        issuer_lifetime,
        renew_from,
        renew: section.renew,
        synchronize: section.synchronize.unwrap_or(true),
        mapping,
    };
    // Enrollment refuses fail-open hooks; it owns the node's admission.
    config.enable_hooks = true;
    config.hook_failure_mode = crate::HookFailureMode::FailClosed;
    Ok(Some(plan))
}

/// Enrollment installed on a started node from a [`Plan`].
pub struct Installed {
    pub enrollment: Enrollment,
    pub issuer: Option<Issuer>,
}

impl Plan {
    fn state_path(&self) -> PathBuf {
        match &self.state {
            StateLocation::Path(path) => path.clone(),
            StateLocation::Temporary(dir) => dir.0.join("enrollment.json"),
        }
    }

    /// Install on `node` (started with fail-closed hooks and the enrollment
    /// ALPN). Refuses a credential that does not verify for this node.
    pub fn install(&self, node: &Node) -> Result<Installed> {
        let path = self.state_path();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| invalid(format!("state_path {}: {e}", path.display())))?;
        }
        let mut store =
            EnrollmentPolicyStore::open(&path, self.root, self.namespace, self.audience.clone())
                .map_err(|e| invalid(format!("state_path {}: {e}", path.display())))?;
        let mut verifier = JwtEnrollmentVerifier::new(self.root, self.audience.clone())
            .map_err(|e| invalid(format!("root/audience: {e}")))?;
        if let Some(leeway) = self.leeway {
            store = store
                .with_activation_leeway(leeway)
                .map_err(|e| invalid(format!("activation_leeway_secs: {e}")))?;
            verifier = verifier
                .with_activation_leeway(leeway)
                .map_err(|e| invalid(format!("activation_leeway_secs: {e}")))?;
        }
        let enrollment = Enrollment::install(
            node,
            Arc::new(store),
            verifier,
            Some(self.credential.clone()),
            self.policy.clone(),
            self.options.clone(),
        )
        .map_err(|e| invalid(format!("{e:#}")))?;
        let issuer = match &self.delegation {
            None => None,
            Some(delegation) => Some(
                Issuer::new(
                    node,
                    enrollment.clone(),
                    delegation.clone(),
                    self.issuer_lifetime,
                )
                .map_err(|e| invalid(format!("issuer: {e:#}")))?,
            ),
        };
        Ok(Installed { enrollment, issuer })
    }

    /// Hold the fleet namespace this node serves policy from: synchronize it
    /// from the configured replicas, or, with none, import it read-only so
    /// the root's publication sessions can deliver into it.
    pub async fn attach_policy(&self, node: &Node, enrollment: &Enrollment) -> Result<()> {
        if self.replicas.is_empty() {
            let doc = node
                .docs()
                .open_or_import_read_namespace(crate::NamespaceId::from_bytes(self.namespace))
                .await?;
            enrollment
                .set_policy_document(&doc)
                .map_err(|e| invalid(format!("{e:#}")))?;
            return Ok(());
        }
        let (readiness, ticket) = tokio::time::timeout(
            self.sync_timeout,
            enrollment.connect_replicas(self.replicas.clone()),
        )
        .await
        .map_err(|_| invalid("no policy replica answered within sync_timeout_ms"))?
        .map_err(|e| invalid(format!("enrolling with the policy replicas: {e:#}")))?;
        let ticket = ticket
            .ok_or_else(|| invalid("the policy replica offered no fleet namespace ticket"))?;
        enrollment
            .synchronize(readiness.peer, ticket, self.sync_timeout)
            .await
            .map_err(|e| invalid(format!("synchronizing the fleet namespace: {e:#}")))?;
        Ok(())
    }

    /// Start renewing this node's credential, when configured: from
    /// `renew_from`, else (client only) from `server` when it signed the
    /// credential. `renew = false` disables both.
    pub fn start_renewal(
        &self,
        enrollment: &Enrollment,
        server: Option<&NodeAddr>,
    ) -> Option<RenewalHandle> {
        if self.renew == Some(false) {
            return None;
        }
        let issuer = match (&self.renew_from, server, self.side) {
            (Some(addr), _, _) => addr.clone(),
            (None, Some(server), Side::Client) => {
                let signer = credential_signer(enrollment)?;
                if hex::encode(signer) != server.node_id.as_str() {
                    return None;
                }
                server.clone()
            }
            _ => return None,
        };
        Some(enrollment.start_renewal(RenewalConfig::new(issuer)))
    }

    /// Which side this plan was validated for.
    pub fn side(&self) -> Side {
        self.side
    }

    /// The Gate-3 attribute source for a server: each peer's roles from its
    /// live enrollment session, mapped with this plan's [`RoleMapping`].
    pub fn attribute_source(&self, enrollment: &Enrollment) -> Arc<dyn AttributeSource> {
        Arc::new(EnrollmentAttributes {
            enrollment: enrollment.clone(),
            mapping: self.mapping.clone(),
        })
    }

    /// The client sequence against one server (design §15.7, steps 3 and 4):
    /// enroll over the reciprocal handshake within `sync_timeout_ms`, then
    /// synchronize the fleet namespace the server offers when `synchronize`.
    /// Renewal (step 6) is [`start_renewal`](Self::start_renewal) with the
    /// server's address.
    pub async fn enroll_with_server(
        &self,
        enrollment: &Enrollment,
        server: &NodeAddr,
    ) -> Result<super::EnrollmentReadiness> {
        let (readiness, ticket) =
            tokio::time::timeout(self.sync_timeout, enrollment.connect(server.clone()))
                .await
                .map_err(|_| {
                    invalid("the server did not complete the enrollment handshake in time")
                })?
                .map_err(|e| invalid(format!("enrolling with the server: {e:#}")))?;
        if self.synchronize {
            if let Some(ticket) = ticket {
                enrollment
                    .synchronize(readiness.peer, ticket, self.sync_timeout)
                    .await
                    .map_err(|e| invalid(format!("synchronizing the fleet namespace: {e:#}")))?;
            }
        }
        Ok(readiness)
    }
}

/// A client's inbound enrollment handshakes (a server re-presenting a renewed
/// credential): accept every connection on `node` and route
/// [`super::ENROLLMENT_ALPN`] ones into `enrollment`. Abort the task to stop.
pub fn serve_inbound_enrollment(
    node: &Node,
    enrollment: &Enrollment,
) -> tokio::task::JoinHandle<()> {
    let node = node.clone();
    let enrollment = enrollment.clone();
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == super::ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    if enrollment.accept(&connection).await.is_ok() {
                        connection.closed().await;
                    }
                });
            }
        }
    })
}

/// The key that signed this node's credential leaf: a delegated issuer, or
/// the root for a one-link credential.
fn credential_signer(enrollment: &Enrollment) -> Option<[u8; 32]> {
    let (_, evidence) = enrollment.credential()?;
    match evidence.source() {
        AdmissionEvidenceSource::PresentedChain { issuers, .. } => issuers.first().copied(),
        AdmissionEvidenceSource::RootAuthoredRecord { .. } => None,
    }
}

fn read_text(path: &Path, what: &str) -> Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)
        .map_err(|e| invalid(format!("{what} {}: {e}", path.display())))?;
    let mut text = String::new();
    file.take((MAX_CHAIN_BYTES + 2) as u64)
        .read_to_string(&mut text)
        .map_err(|e| invalid(format!("{what} {}: {e}", path.display())))?;
    if text.len() > MAX_CHAIN_BYTES + 1 {
        return Err(invalid(format!(
            "{what} {}: larger than a chain may be",
            path.display()
        )));
    }
    Ok(text.trim().to_string())
}

/// An `aster1…` ticket or a 64-hex node id.
pub(crate) fn parse_addr(value: &str, what: &str) -> Result<NodeAddr> {
    let value = value.trim();
    if let Ok(key) = hex32(value, what) {
        return Ok(NodeAddr {
            node_id: NodeId::from_hex(hex::encode(key)),
            relay_url: None,
            direct_addresses: Vec::new(),
        });
    }
    crate::Ticket::from_base58(value)
        .and_then(|ticket| ticket.to_node_addr())
        .map_err(|e| {
            invalid(format!(
                "{what}: {value:?} is neither an aster1 ticket nor a 64-hex node id ({e})"
            ))
        })
}

fn parse_hex32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    hex::decode(value).ok()?.try_into().ok()
}

fn hex32(value: &str, what: &str) -> Result<[u8; 32]> {
    parse_hex32(value.trim()).ok_or_else(|| invalid(format!("{what}: expected 64 hex characters")))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn claims(role: Option<&str>, scopes: &[&str], tags: &[&str]) -> AdmissionClaims {
        let mut attributes = BTreeMap::new();
        if let Some(role) = role {
            attributes.insert(ROLE_ATTRIBUTE.into(), TypedAttribute::String(role.into()));
        }
        attributes.insert("region".into(), TypedAttribute::String("eu".into()));
        attributes.insert("tier".into(), TypedAttribute::Int(2));
        AdmissionClaims {
            scopes: Some(scopes.iter().map(|s| s.to_string()).collect()),
            tags: Some(tags.iter().map(|s| s.to_string()).collect()),
            attributes: Some(attributes),
        }
    }

    #[test]
    fn default_mapping_reads_the_role_attribute_and_passes_attributes_through() {
        let mapping = RoleMapping::default();
        let out = mapping.attributes(&claims(Some(" ops.status, ops.logs ,"), &["ops"], &[]));
        assert_eq!(out[ROLE_ATTRIBUTE], "ops.logs,ops.status");
        assert_eq!(out["region"], "eu");
        assert_eq!(out["tier"], "2");
        // Scopes are not roles by default.
        assert!(!RoleMapping::default()
            .attributes(&claims(None, &["ops.admin"], &[]))
            .contains_key(ROLE_ATTRIBUTE));
    }

    #[test]
    fn scopes_and_tags_map_to_roles() {
        let mapping = RoleMapping::new(RoleMappingConfig {
            attribute: String::new(),
            scopes_as_roles: true,
            scopes: [("fleet".to_string(), vec!["ops.status".to_string()])].into(),
            tags: [("admin".to_string(), vec!["ops.admin".to_string()])].into(),
            include_attributes: false,
        });
        let out = mapping.attributes(&claims(Some("ops.ingest"), &["fleet"], &["admin"]));
        // The attribute is disabled, so its value grants nothing and, being
        // `aster.role`, is never passed through either.
        assert_eq!(out[ROLE_ATTRIBUTE], "fleet,ops.admin,ops.status");
        assert!(!out.contains_key("region"));
    }

    #[test]
    fn policy_config_lowers_every_node() {
        let json = serde_json::json!({"all_of": [
            {"any_of": [{"scope": "ops"}, {"tag": "t"}, {"audience": "a"}]},
            {"none_of": [{"source_cidr": "10.0.0.0/8"}]},
            {"attribute_equals": {"key": "k", "value": {"type": "int", "value": 3}}},
            {"policy_synchronized_within": {"max_age_secs": 60}},
            {"issued_by": "11".repeat(32)},
            {"max_credential_age_secs": 3600},
            "require_registration",
            "tolerate_stale_policy"
        ]});
        PolicyConfig::from_json(&json)
            .unwrap()
            .to_expression()
            .unwrap();
        for bad in [
            serde_json::json!({"all_of": []}),
            serde_json::json!({"scope": 1}),
            serde_json::json!("allow_everyone"),
            serde_json::json!({"source_cidr": "nope"}),
            serde_json::json!({"issued_by": "zz"}),
        ] {
            assert!(
                PolicyConfig::from_json(&bad)
                    .and_then(|p| p.to_expression())
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn addresses_are_tickets_or_node_ids() {
        let id = "ab".repeat(32);
        assert_eq!(parse_addr(&id, "r").unwrap().node_id.as_str(), id);
        assert!(parse_addr("not-an-address", "r").is_err());
    }
}
