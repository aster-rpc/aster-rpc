//! Enrollment from `AsterConfig`, for the Python `AsterServer` / `AsterClient`.
//!
//! Thin wrappers over `aster::binding_support::enrollment`, which runs the
//! same sequence as the Rust `AsterServer` / `AsterClient` (contract:
//! "AsterServer integration" in `docs/_internal/working_ideas/aster-enrollment.md`):
//!
//! ```python
//! plan = EnrollmentPlan.from_config(config, side="server")  # None when off
//! node = await Node.start(plan.config, alpns=[ENROLLMENT_ALPN])
//! enrollment = plan.install(node)
//! DispatchServer.start(node, services, enrollment=enrollment)
//! await enrollment.attach_policy(node)
//! enrollment.start_renewal()
//! ```
//!
//! The `_EnrollmentTestRoot` class and the `_enrollment_test_*` functions are
//! **test-only** stand-ins for the root's CLI operations (`aster enroll node`,
//! `aster enroll delegate`, `aster enroll revoke --peer`). They are not part of
//! the Python API.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pyo3::exceptions::{PyLookupError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_async_runtimes::tokio::future_into_py;

use aster::binding_support::enrollment::{
    plan as validate, serve_inbound_enrollment, Installed, Overrides, Plan, Side,
};
use aster::binding_support::identity::client_publisher_plan;
use aster::enrollment::{
    AdmissionClaims, Registration, RenewalHandle, TypedAttribute, ENROLLMENT_ALPN,
};
use aster::rpc::jwks_publisher::{IssuerKeyPublisher, PublisherHandle};
use aster::IdentityPublisherConfig;

use crate::config::NativeAsterConfig;
use crate::error::aster_err_to_py;
use crate::net::NodeAddr;
use crate::node::{extract_config, Node};

fn to_aster_addr(addr: &NodeAddr) -> aster::NodeAddr {
    aster::NodeAddr {
        node_id: aster::NodeId::from_hex(addr.endpoint_id.clone()),
        relay_url: addr.relay_url.clone(),
        direct_addresses: addr.direct_addresses.clone(),
    }
}

fn hex32(value: &str, what: &str) -> PyResult<[u8; 32]> {
    hex::decode(value.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| PyValueError::new_err(format!("{what}: expected 64 hex characters")))
}

fn claims(
    attributes: Option<HashMap<String, String>>,
    scopes: Vec<String>,
    tags: Vec<String>,
) -> AdmissionClaims {
    let attributes: BTreeMap<String, TypedAttribute> = attributes
        .unwrap_or_default()
        .into_iter()
        .map(|(key, value)| (key, TypedAttribute::String(value)))
        .collect();
    AdmissionClaims {
        scopes: Some(scopes.into_iter().collect()),
        tags: Some(tags.into_iter().collect()),
        attributes: Some(attributes),
    }
}

/// A validated `[enrollment]` configuration (see module docs).
#[pyclass(frozen, module = "aster._aster")]
pub struct EnrollmentPlan {
    plan: Arc<Plan>,
    config: aster::AsterConfig,
    /// A client's `[identity.publisher]` and its key-set age bound.
    publisher: Option<(IdentityPublisherConfig, u64)>,
}

/// The issuer-key publisher an Enrollment's node runs: a client's own
/// (delivering to the servers it enrolled with, restarted on each new one),
/// or a server's (from `DispatchServer.attach_identity`).
#[derive(Default)]
struct PublisherSlot {
    config: Option<(IdentityPublisherConfig, u64)>,
    owned: Option<(IssuerKeyPublisher, Vec<aster::NodeId>)>,
    server: Option<PublisherHandle>,
}

impl PublisherSlot {
    fn handle(&self) -> Option<PublisherHandle> {
        self.server
            .clone()
            .or_else(|| self.owned.as_ref().map(|(p, _)| p.handle()))
    }

    /// A client enrolled with `server`: (re)start the publisher so it also
    /// delivers there. No-op without `[identity.publisher]`.
    fn enrolled(
        &mut self,
        node: &aster::Node,
        enrollment: &aster::enrollment::Enrollment,
        server: aster::NodeId,
    ) -> aster::Result<()> {
        let Some((config, max_age)) = &self.config else {
            return Ok(());
        };
        let mut deliver_to = self
            .owned
            .as_ref()
            .map(|(_, d)| d.clone())
            .unwrap_or_default();
        if deliver_to.contains(&server) {
            return Ok(());
        }
        deliver_to.push(server);
        let publisher = IssuerKeyPublisher::start(
            node,
            enrollment.clone(),
            config,
            *max_age,
            deliver_to.clone(),
        )?;
        self.owned = Some((publisher, deliver_to));
        Ok(())
    }
}

#[pymethods]
impl EnrollmentPlan {
    /// Validate `config` (an `aster.AsterConfig`) for `side` (`"server"` or
    /// `"client"`). Returns `None` when enrollment is not enabled. Any missing
    /// or malformed setting raises `ValueError` naming it: enrollment never
    /// falls back to an open node.
    #[staticmethod]
    #[pyo3(signature = (config, *, side))]
    fn from_config(config: &Bound<'_, PyAny>, side: &str) -> PyResult<Option<Self>> {
        let side = match side {
            "server" => Side::Server,
            "client" => Side::Client,
            other => {
                return Err(PyValueError::new_err(format!(
                    "side: expected 'server' or 'client', got {other:?}"
                )))
            }
        };
        let mut config = extract_config(Some(config))?;
        let is_client = matches!(side, Side::Client);
        let plan = validate(&mut config, side, Overrides::default()).map_err(aster_err_to_py)?;
        // A client's `[identity.publisher]` needs Enrollment, as the Rust
        // `AsterClient` checks (a server's is checked at `DispatchServer.start`).
        let publisher = if is_client {
            client_publisher_plan(&config, plan.is_some())
                .map_err(aster_err_to_py)?
                .map(|p| (p, config.identity_max_key_set_age_secs))
        } else {
            None
        };
        Ok(plan.map(|plan| Self {
            plan: Arc::new(plan),
            config,
            publisher,
        }))
    }

    /// The configuration to start the node with: hooks on, failing closed.
    #[getter]
    fn config(&self) -> NativeAsterConfig {
        NativeAsterConfig {
            inner: self.config.clone(),
        }
    }

    /// Install Enrollment on `node`, started from :attr:`config` with
    /// `ENROLLMENT_ALPN`. Raises `ValueError` when this node's credential does
    /// not verify for its key, the root or the audience.
    fn install(&self, node: PyRef<'_, Node>) -> PyResult<Enrollment> {
        crate::ensure_tokio_runtime();
        let _guard = pyo3_async_runtimes::tokio::get_runtime().enter();
        let installed = self.plan.install(&node.inner).map_err(aster_err_to_py)?;
        Ok(Enrollment {
            plan: self.plan.clone(),
            installed: Arc::new(installed),
            node: node.inner.clone(),
            renewal: Mutex::new(None),
            inbound: Mutex::new(None),
            publisher: Arc::new(Mutex::new(PublisherSlot {
                config: self.publisher.clone(),
                ..PublisherSlot::default()
            })),
        })
    }
}

/// Enrollment installed on a node (see module docs).
#[pyclass(frozen, module = "aster._aster")]
pub struct Enrollment {
    plan: Arc<Plan>,
    installed: Arc<Installed>,
    node: aster::Node,
    renewal: Mutex<Option<RenewalHandle>>,
    inbound: Mutex<Option<tokio::task::JoinHandle<()>>>,
    publisher: Arc<Mutex<PublisherSlot>>,
}

impl Enrollment {
    /// The installed enrollment and its plan, for the server's identity
    /// wiring (`aster::binding_support::identity`).
    pub(crate) fn parts(&self) -> (Arc<Installed>, Arc<Plan>) {
        (self.installed.clone(), self.plan.clone())
    }

    /// A server's `[identity.publisher]`, once its identity is attached.
    pub(crate) fn set_server_publisher(&self, handle: Option<PublisherHandle>) {
        self.publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .server = handle;
    }

    fn publisher_handle(&self) -> PyResult<PublisherHandle> {
        self.publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handle()
            .ok_or_else(|| {
                PyLookupError::new_err(
                    "this node runs no issuer-key publisher ([identity.publisher] is not \
                     configured)",
                )
            })
    }

    /// Server: wire the role source, the issuer and `ENROLLMENT_ALPN` into
    /// the RPC server.
    pub(crate) fn serve_on(
        &self,
        server: aster::rpc::Server,
        attributes: &aster::rpc::AttributeStore,
    ) -> aster::rpc::Server {
        attributes.set_source(self.plan.attribute_source(&self.installed.enrollment));
        let mut server = server;
        if let Some(issuer) = &self.installed.issuer {
            server = server.register(issuer.clone());
        }
        let routed = self.installed.enrollment.clone();
        server.route_alpn(ENROLLMENT_ALPN, move |connection| {
            let enrollment = routed.clone();
            tokio::spawn(async move {
                if enrollment.accept(&connection).await.is_ok() {
                    connection.closed().await;
                }
            });
        })
    }
}

#[pymethods]
impl Enrollment {
    /// Server, once serving: synchronize the fleet namespace from the
    /// configured replicas, or hold it for the root to publish into. Raises
    /// `ValueError` when no replica answers within `sync_timeout_ms`.
    fn attach_policy<'py>(
        &self,
        py: Python<'py>,
        node: PyRef<'_, Node>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let plan = self.plan.clone();
        let installed = self.installed.clone();
        let node = node.inner.clone();
        future_into_py(py, async move {
            plan.attach_policy(&node, &installed.enrollment)
                .await
                .map_err(aster_err_to_py)
        })
    }

    /// Client: serve inbound enrollment connections, then enroll with
    /// `server` (an `aster.NodeAddr`) over the reciprocal handshake and
    /// synchronize the fleet namespace it offers.
    fn enroll_with<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        node: PyRef<'_, Node>,
        server: PyRef<'_, NodeAddr>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let this = slf.get();
        let plan = this.plan.clone();
        let installed = this.installed.clone();
        let node = node.inner.clone();
        let server = to_aster_addr(&server);
        future_into_py(py, async move {
            let inbound = serve_inbound_enrollment(&node, &installed.enrollment);
            if let Some(previous) = slf.get().inbound.lock().unwrap().replace(inbound) {
                previous.abort();
            }
            node.add_node_addr(server.clone())
                .map_err(aster_err_to_py)?;
            plan.enroll_with_server(&installed.enrollment, &server)
                .await
                .map_err(aster_err_to_py)?;
            // `[identity.publisher]`: deliver issuer keys to this server too.
            slf.get()
                .publisher
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .enrolled(&node, &installed.enrollment, server.node_id)
                .map_err(aster_err_to_py)
        })
    }

    /// Mint a short-lived token for user `sub` with `roles`, signed in by
    /// `amr` (at least one, e.g. `AMR_WEBAUTHN`), valid for `ttl` seconds
    /// (1..=300), signed with this node's key (Rust
    /// `AsterClient::user_token_issuer` + `Issuer::mint_user_token`). The
    /// roles must be within the issuer delegation
    /// (`aster.identity.issuable_roles`) in the credential held now; anything
    /// else raises `ValueError`. Attach it with
    /// `CallOptions(on_behalf_of=token)`; the node needs `aster.gateway`.
    #[pyo3(signature = (sub, roles, amr, ttl))]
    fn mint_user_token(
        &self,
        sub: &str,
        roles: Vec<String>,
        amr: Vec<String>,
        ttl: f64,
    ) -> PyResult<String> {
        if !(ttl.is_finite() && ttl >= 1.0) {
            return Err(PyValueError::new_err(format!(
                "ttl must be at least 1 second, got {ttl}"
            )));
        }
        let issuer =
            aster::rpc::identity::Issuer::from_enrollment(&self.node, &self.installed.enrollment)
                .map_err(aster_err_to_py)?;
        issuer
            .mint_user_token(sub, roles, amr, Duration::from_secs_f64(ttl.floor()))
            .map_err(aster_err_to_py)
    }

    /// Whether this node runs an issuer-key publisher (`[identity.publisher]`).
    #[getter]
    fn has_publisher(&self) -> bool {
        self.publisher_handle().is_ok()
    }

    /// Tell the issuer-key publisher that a token of `iss` carried key id
    /// `kid`: an unknown kid refreshes `iss` at once (at most every
    /// `min_refresh_secs`). Rust `PublisherHandle::notice_kid`. Raises
    /// `LookupError` when the node runs no publisher.
    fn notice_kid(&self, iss: &str, kid: &str) -> PyResult<()> {
        self.publisher_handle()?.notice_kid(iss, kid);
        Ok(())
    }

    /// `notice_kid` with the `iss` and `kid` of a compact JWT (unverified; a
    /// token that does not parse is ignored). Rust
    /// `PublisherHandle::notice_token`.
    fn notice_token(&self, token: &str) -> PyResult<()> {
        self.publisher_handle()?.notice_token(token);
        Ok(())
    }

    /// What the publisher last published, per issuer: `{iss: {"kids",
    /// "fetched_at", "publications", "attempts", "last_error"}}`.
    fn publisher_status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let out = PyDict::new(py);
        for (iss, s) in self.publisher_handle()?.status() {
            let entry = PyDict::new(py);
            entry.set_item("kids", s.kids)?;
            entry.set_item("fetched_at", s.fetched_at)?;
            entry.set_item("publications", s.publications)?;
            entry.set_item("attempts", s.attempts)?;
            entry.set_item("last_error", s.last_error.as_ref().map(ToString::to_string))?;
            out.set_item(iss, entry)?;
        }
        Ok(out)
    }

    /// Start renewing this node's credential when configured: from
    /// `renew_from`, else (client) from `server` when it signed the
    /// credential; `renew = false` disables it. Returns whether renewal runs.
    #[pyo3(signature = (server=None))]
    fn start_renewal(&self, server: Option<PyRef<'_, NodeAddr>>) -> bool {
        crate::ensure_tokio_runtime();
        let _guard = pyo3_async_runtimes::tokio::get_runtime().enter();
        let server = server.map(|addr| to_aster_addr(&addr));
        let handle = self
            .plan
            .start_renewal(&self.installed.enrollment, server.as_ref());
        let running = handle.is_some();
        *self.renewal.lock().unwrap() = handle;
        running
    }

    /// Whether this node is renewing its credential.
    #[getter]
    fn is_renewing(&self) -> bool {
        self.renewal.lock().unwrap().is_some()
    }

    /// This node's current credential chain (changes on renewal).
    #[getter]
    fn credential(&self) -> Option<String> {
        self.installed
            .enrollment
            .credential()
            .map(|(chain, _)| chain)
    }

    /// The trusted root public key (hex).
    #[getter]
    fn trusted_root(&self) -> String {
        hex::encode(self.installed.enrollment.trusted_root())
    }

    /// The fleet policy namespace (hex).
    #[getter]
    fn fleet_namespace(&self) -> String {
        hex::encode(self.installed.enrollment.fleet_namespace())
    }

    /// The fleet audience.
    #[getter]
    fn audience(&self) -> String {
        self.installed.enrollment.audience().to_string()
    }

    /// Whether this server runs a delegated issuer
    /// (`[enrollment.issuer] delegation_file`).
    #[getter]
    fn has_issuer(&self) -> bool {
        self.installed.issuer.is_some()
    }

    /// Issue a credential for `subject` (a 64-hex node id) as this server's
    /// delegated issuer, with string claim `attributes` (roles go in
    /// `aster.role`, comma-separated), `scopes` and `tags`. Returns the
    /// credential chain to hand to the node (`[trust] enrollment_credential`).
    #[pyo3(signature = (subject, *, attributes=None, scopes=Vec::new(), tags=Vec::new(), persistent=false))]
    fn issue_for(
        &self,
        subject: &str,
        attributes: Option<HashMap<String, String>>,
        scopes: Vec<String>,
        tags: Vec<String>,
        persistent: bool,
    ) -> PyResult<String> {
        let issuer = self.installed.issuer.as_ref().ok_or_else(|| {
            PyRuntimeError::new_err(
                "this server runs no issuer: set [enrollment.issuer] delegation_file",
            )
        })?;
        let registration = if persistent {
            Registration::Persistent
        } else {
            Registration::Ephemeral
        };
        issuer
            .issue_for(
                &hex32(subject, "subject")?,
                claims(attributes, scopes, tags),
                registration,
            )
            .map(|issued| issued.into_chain())
            .map_err(|e| PyValueError::new_err(format!("issue_for: {e}")))
    }

    /// Stop renewal, the publisher and inbound handling. Idempotent.
    fn close(&self) {
        self.renewal.lock().unwrap().take();
        {
            let mut publisher = self.publisher.lock().unwrap_or_else(|e| e.into_inner());
            publisher.owned = None;
            publisher.server = None;
        }
        if let Some(task) = self.inbound.lock().unwrap().take() {
            task.abort();
        }
    }
}

impl Drop for Enrollment {
    fn drop(&mut self) {
        if let Some(task) = self.inbound.get_mut().ok().and_then(Option::take) {
            task.abort();
        }
    }
}

// ── Test-only root stand-in ────────────────────────────────────────────────

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn root_key(seed: &[u8]) -> PyResult<aster::SecretKey> {
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| PyValueError::new_err("root seed: expected 32 bytes"))?;
    Ok(aster::SecretKey::from_bytes(seed))
}

fn role_attributes(roles: Option<String>) -> Option<HashMap<String, String>> {
    roles.map(|roles| HashMap::from([("aster.role".to_string(), roles)]))
}

/// **Test-only.** What `aster enroll node --subject <subject> --audience
/// <audience> [--attributes roles.json] [--scope ..] [--tag ..]` writes: a
/// root-issued credential chain.
#[pyfunction]
#[pyo3(name = "_enrollment_test_credential", signature = (root_seed, subject, audience, *, roles=None, scopes=Vec::new(), tags=Vec::new(), lifetime_secs=3600, attributes=None))]
#[allow(clippy::too_many_arguments)]
fn test_credential(
    root_seed: &[u8],
    subject: &str,
    audience: &str,
    roles: Option<String>,
    scopes: Vec<String>,
    tags: Vec<String>,
    lifetime_secs: u64,
    attributes: Option<HashMap<String, String>>,
) -> PyResult<String> {
    let mut request = aster::enrollment::EnrollmentIssueRequest::new(
        hex32(subject, "subject")?,
        audience,
        now() + lifetime_secs,
    );
    // Extra string claim attributes (e.g. `aster.identity.issuable_roles`).
    let mut claim_attributes = role_attributes(roles).unwrap_or_default();
    claim_attributes.extend(attributes.unwrap_or_default());
    let claim_attributes = (!claim_attributes.is_empty()).then_some(claim_attributes);
    request.claims = claims(claim_attributes, scopes, tags);
    aster::enrollment::issue_credential(&root_key(root_seed)?, &request, now())
        .map(|issued| issued.into_chain())
        .map_err(|e| PyValueError::new_err(format!("issue_credential: {e}")))
}

/// **Test-only.** What `aster enroll delegate --subject <issuer> --audience
/// <audience> --attribute-key aster.role --out issuer.chain` writes.
#[pyfunction]
#[pyo3(name = "_enrollment_test_delegation", signature = (root_seed, issuer, audience, *, lifetime_secs=3600))]
fn test_delegation(
    root_seed: &[u8],
    issuer: &str,
    audience: &str,
    lifetime_secs: u64,
) -> PyResult<String> {
    use aster::delegation::{issue_delegation, DelegationGrant, LinkKind, Validity};
    let grant = DelegationGrant {
        may_issue: [LinkKind::Enrollment].into(),
        audiences: [audience.to_string()].into(),
        scopes: BTreeSet::new(),
        tags: BTreeSet::new(),
        attribute_keys: ["aster.role".to_string()].into(),
        max_child_lifetime: lifetime_secs,
        max_depth: 0,
        subjects: None,
        operations: BTreeSet::new(),
        mandatory: BTreeSet::new(),
        may_register: false,
    };
    issue_delegation(
        &root_key(root_seed)?,
        None,
        &aster::PublicKey::from_bytes(hex32(issuer, "issuer")?),
        &aster::enrollment::Audience::One(audience.into()),
        &grant,
        Validity {
            issued_at: now(),
            not_before: None,
            expires_at: now() + lifetime_secs,
        },
    )
    .map(|chain| chain.to_text().to_string())
    .map_err(|e| PyValueError::new_err(format!("issue_delegation: {e}")))
}

/// **Test-only.** The root as a node: holds the writable fleet namespace,
/// serves enrollment (as a policy replica would) and publishes revocations to
/// a server (as `aster enroll revoke --peer <server>` does).
#[pyclass(frozen, module = "aster._aster", name = "_EnrollmentTestRoot")]
pub struct EnrollmentTestRoot {
    node: aster::Node,
    fleet: aster::Doc,
    fleet_secret: [u8; 32],
    enrollment: aster::enrollment::Enrollment,
    seed: [u8; 32],
    audience: String,
    accept: tokio::task::JoinHandle<()>,
    _files: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[pymethods]
impl EnrollmentTestRoot {
    /// Start the root node for `audience` with the 32-byte `root_seed`.
    #[staticmethod]
    fn start<'py>(
        py: Python<'py>,
        root_seed: Vec<u8>,
        audience: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        use aster::enrollment::{
            Enrollment, EnrollmentOptions, EnrollmentPolicyStore, JwtEnrollmentVerifier,
            PolicyExpressionV1, PolicyPredicate,
        };
        crate::ensure_tokio_runtime();
        let key = root_key(&root_seed)?;
        let seed = key.to_bytes();
        let public = key.public_key().to_bytes();
        let credential = test_credential(
            &root_seed,
            &hex::encode(public),
            &audience,
            None,
            Vec::new(),
            Vec::new(),
            3600,
            None,
        )?;
        future_into_py(py, async move {
            let err = |e: &dyn std::fmt::Display| PyRuntimeError::new_err(e.to_string());
            let mut nonce = [0u8; 8];
            getrandom::fill(&mut nonce).map_err(|e| err(&e))?;
            let dir = std::env::temp_dir().join(format!("aster-test-root-{}", hex::encode(nonce)));
            std::fs::create_dir_all(&dir).map_err(|e| err(&e))?;
            let files = TempDir(dir);
            let config = aster::AsterConfig::builder()
                .relay(aster::RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(key.clone())
                .root_pubkey(key.public_key())
                .hooks(true)
                .hook_failure_mode(aster::HookFailureMode::FailClosed)
                .build();
            let node = aster::Node::start_with_alpns(config, vec![ENROLLMENT_ALPN.to_vec()])
                .await
                .map_err(aster_err_to_py)?;
            for _ in 0..100 {
                if !node.addr().direct_addresses.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let mut fleet_secret = [0u8; 32];
            getrandom::fill(&mut fleet_secret).map_err(|e| err(&e))?;
            let fleet = node
                .docs()
                .open_or_import_write_namespace(aster::NamespaceSecret::from_bytes(fleet_secret))
                .await
                .map_err(aster_err_to_py)?;
            let namespace = hex32(
                &fleet.id().map_err(aster_err_to_py)?.to_string(),
                "namespace",
            )?;
            let store = EnrollmentPolicyStore::open(
                files.0.join("root.json"),
                public,
                namespace,
                audience.clone(),
            )
            .map_err(|e| err(&e))?;
            let enrollment = Enrollment::install(
                &node,
                Arc::new(store),
                JwtEnrollmentVerifier::new(public, audience.clone()).map_err(|e| err(&e))?,
                Some(credential),
                Arc::new(PolicyExpressionV1::Predicate(
                    PolicyPredicate::TolerateStalePolicy,
                )),
                EnrollmentOptions::default(),
            )
            .map_err(|e| err(&e))?;
            enrollment
                .set_policy_document(&fleet)
                .map_err(|e| err(&e))?;
            let served = (node.clone(), enrollment.clone());
            let accept = tokio::spawn(async move {
                let (node, enrollment) = served;
                while let Ok((alpn, connection)) = node.accept().await {
                    if alpn == ENROLLMENT_ALPN {
                        let enrollment = enrollment.clone();
                        tokio::spawn(async move {
                            if enrollment.accept(&connection).await.is_ok() {
                                connection.closed().await;
                            }
                        });
                    }
                }
            });
            Ok(EnrollmentTestRoot {
                node,
                fleet,
                fleet_secret,
                enrollment,
                seed,
                audience,
                accept,
                _files: files,
            })
        })
    }

    /// The root public key (hex): `[trust] root_pubkey`.
    #[getter]
    fn root_pubkey(&self) -> String {
        hex::encode(
            aster::SecretKey::from_bytes(self.seed)
                .public_key()
                .to_bytes(),
        )
    }

    /// The 32-byte root secret seed (for `_enrollment_test_credential`).
    #[getter]
    fn root_seed<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.seed)
    }

    /// The fleet namespace (hex): `[enrollment] fleet_namespace`.
    #[getter]
    fn namespace(&self) -> PyResult<String> {
        Ok(self.fleet.id().map_err(aster_err_to_py)?.to_string())
    }

    /// The root node's `aster1…` ticket: an `[enrollment] replicas` entry.
    #[getter]
    fn ticket(&self) -> PyResult<String> {
        aster::Ticket::from_node_addr(&self.node.addr(), aster::Credential::Open)
            .and_then(|ticket| ticket.to_base58())
            .map_err(aster_err_to_py)
    }

    /// Revoke `subject` (64-hex node id) as the root and deliver the record
    /// to `server` (an `aster.NodeAddr`), whose fleet namespace the root
    /// publishes into.
    fn revoke<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        server: PyRef<'_, NodeAddr>,
        subject: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        use aster::enrollment::{
            node_registration_key, Audience, NodeRegistrationStatus, NodeRegistrationV1,
        };
        let server = to_aster_addr(&server);
        let subject = hex32(&subject, "subject")?;
        future_into_py(py, async move {
            let this = slf.get();
            let err = |e: &dyn std::fmt::Display| PyRuntimeError::new_err(e.to_string());
            this.node
                .add_node_addr(server.clone())
                .map_err(aster_err_to_py)?;
            this.enrollment
                .connect(server.clone())
                .await
                .map_err(|e| err(&e))?;
            let record = NodeRegistrationV1 {
                version: 1,
                issuer: this.root_pubkey(),
                subject: hex::encode(subject),
                audience: Audience::One(this.audience.clone()),
                revision: 1,
                issued_at: now() - 60,
                not_before: None,
                expires_at: None,
                status: NodeRegistrationStatus::Revoked,
                claims: claims(None, Vec::new(), Vec::new()),
                mandatory: vec![],
                reauthorizes_revision: None,
                reason: Some("test".into()),
            };
            let author = this
                .node
                .docs()
                .default_author()
                .await
                .map_err(aster_err_to_py)?;
            this.fleet
                .set_bytes(
                    &author,
                    node_registration_key(&subject),
                    serde_json::to_vec(&record).map_err(|e| err(&e))?,
                )
                .await
                .map_err(aster_err_to_py)?;
            this.fleet
                .sync_with_timeout(
                    std::slice::from_ref(&server.node_id),
                    Duration::from_secs(15),
                )
                .await
                .map_err(aster_err_to_py)
        })
    }

    /// What `[identity.publisher]` on `publisher` (an `aster.Node` enrolled
    /// with a credential granting `aster.identity-publisher`, passed as
    /// `credential`) writes after fetching `jwks_json` for issuer `iss`: the
    /// key-set record in the fleet namespace (opened with the root's write
    /// grant), authored by `publisher`, then synchronized to `server` (an
    /// `aster.NodeAddr`).
    fn publish_issuer_keys<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        publisher: PyRef<'_, Node>,
        credential: String,
        iss: String,
        jwks_json: String,
        server: PyRef<'_, NodeAddr>,
    ) -> PyResult<Bound<'py, PyAny>> {
        use aster::rpc::issuer_keys::{publish_issuer_keys, IssuerKeyPublication};
        let publisher = publisher.inner.clone();
        let server = to_aster_addr(&server);
        future_into_py(py, async move {
            let secret = slf.get().fleet_secret;
            let fleet = publisher
                .docs()
                .open_or_import_write_namespace(aster::NamespaceSecret::from_bytes(secret))
                .await
                .map_err(aster_err_to_py)?;
            let publication = IssuerKeyPublication {
                source_url: format!("{iss}/jwks"),
                iss,
                jwks_json,
                fetched_at: now(),
            };
            publish_issuer_keys(&publisher, &fleet, Some(&credential), &publication)
                .await
                .map_err(aster_err_to_py)?;
            publisher
                .add_node_addr(server.clone())
                .map_err(aster_err_to_py)?;
            fleet
                .sync_with_timeout(
                    std::slice::from_ref(&server.node_id),
                    Duration::from_secs(15),
                )
                .await
                .map_err(aster_err_to_py)
        })
    }

    /// Shut the root node down.
    fn close<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            let this = slf.get();
            this.accept.abort();
            this.node.clone().shutdown().await;
            Ok(())
        })
    }
}

pub fn register(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EnrollmentPlan>()?;
    m.add_class::<Enrollment>()?;
    m.add_class::<EnrollmentTestRoot>()?;
    m.add_function(wrap_pyfunction!(test_credential, m)?)?;
    m.add_function(wrap_pyfunction!(test_delegation, m)?)?;
    m.add("ENROLLMENT_ALPN", PyBytes::new(py, ENROLLMENT_ALPN))?;
    Ok(())
}
