//! Test-only (`test-hooks`): an enrollment root for the TypeScript tests.
//!
//! Root operations belong to the Rust CLI (`aster enroll node`, `delegate`,
//! `revoke --peer`), never to a binding. The TypeScript suite still needs a
//! root to issue credentials and publish revocations, exactly as the Rust
//! `aster/tests/enrollment_aster_server.rs` plays one with the low-level API;
//! this is that `Root`, built only into the test-hooks addon.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::delegation::{issue_delegation, DelegationGrant, LinkKind, Validity};
use aster::enrollment::{
    issue_credential, node_registration_key, AdmissionClaims, Audience, Enrollment,
    EnrollmentIssueRequest, EnrollmentOptions, EnrollmentPolicyStore, JwtEnrollmentVerifier,
    NodeRegistrationStatus, NodeRegistrationV1, PolicyExpressionV1, PolicyPredicate,
    TypedAttribute, ENROLLMENT_ALPN,
};
use aster::{AsterConfig, Credential, Doc, Node, RelayMode, SecretKey, Ticket};

use crate::config::aster_err;
use crate::rpc::{DispatchServer, Running};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn reason(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

pub(crate) fn key32(hex_key: &str) -> Result<[u8; 32]> {
    hex::decode(hex_key.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| reason(format!("expected 64 hex characters, got {hex_key:?}")))
}

pub(crate) fn role_claims(roles: Option<&str>) -> AdmissionClaims {
    let mut attributes = BTreeMap::new();
    if let Some(roles) = roles.filter(|r| !r.is_empty()) {
        attributes.insert("aster.role".into(), TypedAttribute::String(roles.into()));
    }
    AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    }
}

/// A root holding the writable fleet namespace and serving enrollment, as a
/// CLI publication session or a policy replica would.
#[napi]
pub struct TestEnrollmentRoot {
    key: SecretKey,
    audience: String,
    node: Node,
    fleet: Doc,
    /// The fleet namespace's write secret (gateway-identity tests write
    /// issuer key sets with it as a publisher node, as the sealed grant
    /// `aster enroll identity-publisher` hands one would).
    fleet_secret: [u8; 32],
    enrollment: Enrollment,
    _files: Arc<TempDir>,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[napi]
impl TestEnrollmentRoot {
    /// Start a root for `audience` on loopback with a fresh key.
    #[napi(ts_return_type = "Promise<TestEnrollmentRoot>")]
    pub fn start(env: &Env, audience: String) -> Result<PromiseRaw<'_, TestEnrollmentRoot>> {
        env.spawn_future(async move {
            let key = SecretKey::generate();
            let dir = std::env::temp_dir().join(format!(
                "aster-ts-root-{}",
                hex::encode(SecretKey::generate().to_bytes())
            ));
            std::fs::create_dir_all(&dir).map_err(reason)?;
            let files = Arc::new(TempDir(dir));
            let config = AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(key.clone())
                .root_pubkey(key.public_key())
                .hooks(true)
                .hook_failure_mode(aster::HookFailureMode::FailClosed)
                .build();
            let node = Node::start_with_alpns(config, vec![ENROLLMENT_ALPN.to_vec()])
                .await
                .map_err(aster_err)?;
            for _ in 0..100 {
                if !node.addr().direct_addresses.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let fleet_secret = SecretKey::generate().to_bytes();
            let fleet = node
                .docs()
                .open_or_import_write_namespace(aster::NamespaceSecret::from_bytes(fleet_secret))
                .await
                .map_err(aster_err)?;
            let namespace = key32(&fleet.id().map_err(aster_err)?.to_string())?;
            let root = key.public_key().to_bytes();
            let mut request = EnrollmentIssueRequest::new(root, &audience, now() + 3600);
            request.claims = role_claims(None);
            let own = issue_credential(&key, &request, now())
                .map_err(reason)?
                .into_chain();
            let enrollment = Enrollment::install(
                &node,
                Arc::new(
                    EnrollmentPolicyStore::open(
                        files.0.join("root.json"),
                        root,
                        namespace,
                        audience.clone(),
                    )
                    .map_err(reason)?,
                ),
                JwtEnrollmentVerifier::new(root, audience.clone()).map_err(reason)?,
                Some(own),
                Arc::new(PolicyExpressionV1::Predicate(
                    PolicyPredicate::TolerateStalePolicy,
                )),
                EnrollmentOptions::default(),
            )
            .map_err(reason)?;
            enrollment.set_policy_document(&fleet).map_err(reason)?;
            let served = (node.clone(), enrollment.clone());
            tokio::spawn(async move {
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
            Ok(TestEnrollmentRoot {
                key,
                audience,
                node,
                fleet,
                fleet_secret,
                enrollment,
                _files: files,
            })
        })
    }

    /// The root public key (hex): `[trust] root_pubkey`.
    #[napi(getter)]
    pub fn root_pubkey(&self) -> String {
        self.key.public_key().to_hex()
    }

    /// The fleet namespace (hex): `[enrollment] fleet_namespace`.
    #[napi(getter)]
    pub fn fleet_namespace(&self) -> Result<String> {
        Ok(self.fleet.id().map_err(aster_err)?.to_string())
    }

    /// The fleet namespace's write secret (hex).
    #[napi(getter)]
    pub fn fleet_secret(&self) -> String {
        hex::encode(self.fleet_secret)
    }

    /// The root node's `aster1…` ticket (a policy replica).
    #[napi(getter)]
    pub fn ticket(&self) -> Result<String> {
        Ticket::from_node_addr(&self.node.addr(), Credential::Open)
            .and_then(|t| t.to_base58())
            .map_err(aster_err)
    }

    /// What `aster enroll node --subject <subject> --audience <audience>
    /// --attributes roles.json` writes: a root-issued credential for
    /// `subject` (hex node id) carrying `roles` (comma-separated) in
    /// `aster.role`. `lifetimeSecs` defaults to one hour. `issuableRoles`
    /// adds the gateway issuer delegation (`aster enroll node
    /// --attribute-key aster.identity.issuable_roles`): the roles the node
    /// may put in edge-minted tokens (comma-separated).
    #[napi]
    pub fn issue(
        &self,
        subject: String,
        roles: Option<String>,
        lifetime_secs: Option<u32>,
        issuable_roles: Option<String>,
    ) -> Result<String> {
        let lifetime = u64::from(lifetime_secs.unwrap_or(3600));
        let mut request =
            EnrollmentIssueRequest::new(key32(&subject)?, &self.audience, now() + lifetime);
        request.claims = role_claims(roles.as_deref());
        if let (Some(issuable), Some(attributes)) =
            (issuable_roles, request.claims.attributes.as_mut())
        {
            attributes.insert(
                aster::rpc::identity::ISSUABLE_ROLES_ATTRIBUTE.into(),
                TypedAttribute::String(issuable),
            );
        }
        Ok(issue_credential(&self.key, &request, now())
            .map_err(reason)?
            .into_chain())
    }

    /// What `aster enroll delegate --subject <issuer> --attribute-key
    /// aster.role` writes: a delegation letting `issuer` (hex node id) sign
    /// enrollment credentials for this audience carrying `aster.role`.
    #[napi]
    pub fn delegate(&self, issuer: String) -> Result<String> {
        let subject = aster::PublicKey::from_hex(issuer.trim()).map_err(aster_err)?;
        let chain = issue_delegation(
            &self.key,
            None,
            &subject,
            &Audience::One(self.audience.clone()),
            &DelegationGrant {
                may_issue: [LinkKind::Enrollment].into(),
                audiences: [self.audience.clone()].into(),
                scopes: BTreeSet::new(),
                tags: BTreeSet::new(),
                attribute_keys: ["aster.role".to_string()].into(),
                max_child_lifetime: 3600,
                max_depth: 0,
                subjects: None,
                operations: BTreeSet::new(),
                mandatory: BTreeSet::new(),
                may_register: false,
            },
            Validity {
                issued_at: now(),
                not_before: None,
                expires_at: now() + 3600,
            },
        )
        .map_err(reason)?;
        Ok(chain.to_text().to_string())
    }

    /// What `aster enroll revoke --subject <subject> --fleet-only --peer
    /// <server>` does: enroll with the server, publish a revoked registration
    /// for `subject` into the fleet namespace and synchronize it to the
    /// server (an `aster1…` ticket).
    #[napi]
    pub async fn revoke(&self, subject: String, server: String) -> Result<()> {
        let subject = key32(&subject)?;
        let server = Ticket::from_base58(&server)
            .and_then(|t| t.to_node_addr())
            .map_err(aster_err)?;
        let server_id = server.node_id.clone();
        self.enrollment.connect(server).await.map_err(reason)?;
        let record = NodeRegistrationV1 {
            version: 1,
            issuer: self.key.public_key().to_hex(),
            subject: hex::encode(subject),
            audience: Audience::One(self.audience.clone()),
            revision: 1,
            issued_at: now() - 60,
            not_before: None,
            expires_at: None,
            status: NodeRegistrationStatus::Revoked,
            claims: role_claims(None),
            mandatory: vec![],
            reauthorizes_revision: None,
            reason: Some("revoked by the test root".into()),
        };
        let author = self.node.docs().default_author().await.map_err(aster_err)?;
        self.fleet
            .set_bytes(
                &author,
                node_registration_key(&subject),
                serde_json::to_vec(&record).map_err(reason)?,
            )
            .await
            .map_err(aster_err)?;
        self.fleet
            .sync_with_timeout(&[server_id], Duration::from_secs(15))
            .await
            .map_err(aster_err)?;
        Ok(())
    }

    /// Shut the root down.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.node.clone().shutdown().await;
        Ok(())
    }
}

#[napi]
impl DispatchServer {
    /// Test-only (`test-hooks`): have a `start` server's configured issuer
    /// sign an ephemeral credential for `subject` (hex node id) carrying
    /// `roles` in `aster.role`, as an orchestrator's provisioning channel
    /// would (`Issuer::issue_for`).
    #[napi]
    pub async fn test_issue_credential(
        &self,
        subject: String,
        roles: Option<String>,
    ) -> Result<String> {
        let guard = self.running.lock().await;
        let Some(Running::Own(server)) = guard.as_ref() else {
            return Err(reason("not a running start() server"));
        };
        let issuer = server
            .issuer()
            .ok_or_else(|| reason("no issuer configured"))?;
        Ok(issuer
            .issue_for(
                &key32(&subject)?,
                role_claims(roles.as_deref()),
                aster::enrollment::Registration::Ephemeral,
            )
            .map_err(reason)?
            .into_chain())
    }
}
