//! The whole fleet in one process: what an operator sets up with `aster
//! enroll …` and three config files, done here in code so the example runs
//! with one command.
//!
//! - **Root.** Holds the fleet namespace's write secret, issues every node's
//!   credential, and seals the namespace's write grant to the edge so the
//!   edge may publish the identity provider's keys (`aster enroll
//!   identity-publisher --seal-to`). Played with the low-level API, as the
//!   CLI does.
//! - **Service** (`AsterServer`, config-driven): `[enrollment]`,
//!   `[identity] accept_on_behalf_of = true`, and one
//!   `[[identity.trusted_issuers]]` for the provider, mapping its `groups` to
//!   the service's roles.
//! - **Edge** (`AsterClient`, config-driven): a credential granting
//!   `aster.gateway` (forward users), `aster.identity-publisher` (publish the
//!   provider's keys) and the issuer delegation
//!   `aster.identity.issuable_roles = "notes.read,notes.write"` (mint tokens
//!   with at most those roles), plus `[identity.publisher]` pointing at the
//!   provider's JWK set.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::{
    issue_credential, publisher_grant_record, AdmissionClaims, Enrollment, EnrollmentIssueRequest,
    EnrollmentOptions, EnrollmentPolicyStore, JwtEnrollmentIssuer, JwtEnrollmentVerifier,
    PolicyExpressionV1, PolicyPredicate, TypedAttribute, ENROLLMENT_ALPN,
};
use aster::rpc::identity::ISSUABLE_ROLES_ATTRIBUTE;
use aster::rpc::issuer_keys::IDENTITY_PUBLISHER_ROLE;
use aster::rpc::{AsterClient, AsterServer, GATEWAY_ROLE};
use aster::{
    AsterConfig, Doc, EnrollmentConfig, EnrollmentLimits, Error, IdentityPublisherConfig,
    NamespaceSecret, Node, PublicKey, RelayMode, Result, SecretKey, TrustedIssuer,
};

use crate::idp::{self, TestIdp};
use crate::service::{roles, NotesImpl, NotesServer};

/// The fleet's enrollment audience.
pub const FLEET: &str = "gateway-example";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn public(key: &SecretKey) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&key.to_bytes()).public_key()
}

fn namespace(doc: &Doc) -> Result<[u8; 32]> {
    let id = doc.id()?.to_string();
    hex::decode(id)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| Error::InvalidArgument("namespace id".into()))
}

async fn wait_for_addr(node: &Node) {
    use aster::native::iroh::Watcher;
    let mut addrs = node.native_endpoint().watch_addr();
    while node.addr().direct_addresses.is_empty() {
        if addrs.updated().await.is_err() {
            return;
        }
    }
}

fn anyhow_error(error: impl std::fmt::Display) -> Error {
    Error::Connection(error.to_string())
}

/// The fleet root (the operator's `aster enroll` key and its policy replica).
pub struct Root {
    key: SecretKey,
    node: Node,
    secret: NamespaceSecret,
    fleet: Doc,
    _enrollment: Enrollment,
    _serve: tokio::task::JoinHandle<()>,
}

impl Drop for Root {
    fn drop(&mut self) {
        self._serve.abort();
    }
}

impl Root {
    pub async fn start(state: &Path) -> Result<Self> {
        let key = SecretKey::generate();
        let node = Node::start_with_alpns(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(key.clone())
                .hooks(true)
                .hook_failure_mode(aster::HookFailureMode::FailClosed)
                .hook_timeout_ms(1000)
                .build(),
            vec![ENROLLMENT_ALPN.to_vec()],
        )
        .await?;
        wait_for_addr(&node).await;
        let secret = NamespaceSecret::from_bytes(crate::webauthn::new_challenge());
        let fleet = node
            .docs()
            .open_or_import_write_namespace(secret.clone())
            .await?;
        let store = EnrollmentPolicyStore::open(
            state.join("root-policy.json"),
            public(&key),
            namespace(&fleet)?,
            FLEET,
        )
        .map_err(anyhow_error)?;
        let root = Self::credential_for(&key, &public(&key), None, None);
        let enrollment = Enrollment::install(
            &node,
            Arc::new(store),
            JwtEnrollmentVerifier::new(public(&key), FLEET).map_err(anyhow_error)?,
            Some(root),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions {
                reconciliation_interval: Duration::from_millis(200),
                ..Default::default()
            },
        )
        .map_err(anyhow_error)?;
        enrollment
            .set_policy_document(&fleet)
            .map_err(anyhow_error)?;
        let served = (node.clone(), enrollment.clone());
        let serve = tokio::spawn(async move {
            let (node, enrollment) = served;
            while let Ok((alpn, connection)) = node.accept().await {
                if alpn == ENROLLMENT_ALPN {
                    let enrollment = enrollment.clone();
                    tokio::spawn(async move {
                        let _ = enrollment.accept(&connection).await;
                        connection.closed().await;
                    });
                }
            }
        });
        Ok(Self {
            key,
            node,
            secret,
            fleet,
            _enrollment: enrollment,
            _serve: serve,
        })
    }

    fn credential_for(
        root: &SecretKey,
        subject: &[u8; 32],
        roles: Option<&str>,
        issuable: Option<&str>,
    ) -> String {
        let mut attributes = BTreeMap::new();
        if let Some(roles) = roles {
            attributes.insert("aster.role".into(), TypedAttribute::String(roles.into()));
        }
        if let Some(issuable) = issuable {
            attributes.insert(
                ISSUABLE_ROLES_ATTRIBUTE.into(),
                TypedAttribute::String(issuable.into()),
            );
        }
        let mut request = EnrollmentIssueRequest::new(*subject, FLEET, now() + 24 * 3600);
        request.claims = AdmissionClaims {
            scopes: Some(BTreeSet::new()),
            tags: Some(BTreeSet::new()),
            attributes: Some(attributes),
        };
        issue_credential(root, &request, now())
            .expect("issuing a credential")
            .into_chain()
    }

    /// `aster enroll node --subject <node> --attributes …`.
    pub fn credential(
        &self,
        node: &SecretKey,
        roles: Option<&str>,
        issuable: Option<&str>,
    ) -> String {
        Self::credential_for(&self.key, &public(node), roles, issuable)
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey::from_bytes(public(&self.key))
    }

    /// What every node's `[enrollment]` section says.
    pub fn enrollment(&self, state: Option<std::path::PathBuf>) -> Result<EnrollmentConfig> {
        Ok(EnrollmentConfig {
            audience: Some(FLEET.into()),
            fleet_namespace: Some(hex::encode(namespace(&self.fleet)?)),
            state_path: state,
            limits: EnrollmentLimits {
                reconciliation_interval_ms: Some(500),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// `aster enroll identity-publisher --seal-to <edge>`, then deliver the
    /// root's records to the edge (in production the CLI delivers to the
    /// policy replicas, and nodes sync from them).
    pub async fn grant_publisher(&self, edge: &AsterClient) -> Result<()> {
        let edge_key = edge.node().export_secret_key()?;
        let (key, record) = publisher_grant_record(
            &public(&self.key),
            &public(&edge_key),
            &self.secret.to_bytes(),
        )
        .map_err(|e| Error::InvalidArgument(e.to_string()))?;
        let author = self.node.docs().default_author().await?;
        self.fleet
            .set_bytes(&author, key.into_bytes(), record)
            .await?;
        let enrollment = edge
            .enrollment()
            .ok_or_else(|| Error::InvalidArgument("the edge has no enrollment".into()))?;
        let (ready, ticket) = enrollment
            .connect(self.node.addr())
            .await
            .map_err(|e| Error::Connection(e.to_string()))?;
        let ticket =
            ticket.ok_or_else(|| Error::Connection("the root offered no fleet ticket".into()))?;
        enrollment
            .synchronize(ready.peer, ticket, Duration::from_secs(15))
            .await
            .map_err(|e| Error::Connection(e.to_string()))?;
        Ok(())
    }
}

fn base(key: &SecretKey, root: &Root) -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key.clone())
        .root_pubkey(root.public_key())
}

/// The roles the edge may mint into users' tokens (its issuer delegation).
pub const EDGE_ISSUABLE_ROLES: &str = "notes.read,notes.write";

/// The running fleet: root, identity provider, service and edge.
pub struct Fleet {
    pub root: Root,
    pub idp: Arc<TestIdp>,
    pub service: AsterServer,
    pub edge: AsterClient,
}

impl Fleet {
    /// Start everything, with state under `state`, and wait until the service
    /// holds the provider's keys.
    pub async fn start(state: &Path) -> Result<Self> {
        let root = Root::start(state).await?;
        let idp = Arc::new(
            TestIdp::start(state)
                .await
                .map_err(|e| Error::Connection(e.to_string()))?,
        );

        // The service: [enrollment] + [identity].
        let service_key = SecretKey::generate();
        let provider = TrustedIssuer::new(idp::ISSUER, [idp::AUDIENCE])
            .roles_claim("groups")
            .map_role("readers", roles::READ)
            .map_role("writers", roles::WRITE);
        let service = AsterServer::builder()
            .service(NotesServer::new(NotesImpl))
            .config(
                base(&service_key, &root)
                    .enrollment_credential_chain(root.credential(&service_key, None, None))
                    .enrollment(root.enrollment(Some(state.join("service.json")))?)
                    .accept_on_behalf_of(true)
                    .trusted_issuer(provider)
                    .build(),
            )
            .start()
            .await?;
        wait_for_addr(service.node()).await;

        // The edge: [enrollment] + [identity.publisher].
        let edge_key = SecretKey::generate();
        let edge_roles = format!("{GATEWAY_ROLE},{IDENTITY_PUBLISHER_ROLE}");
        let publisher = IdentityPublisherConfig::new([(idp::ISSUER, idp.jwks_url())])
            .min_refresh_secs(1)
            .ca_cert(idp.ca_pem())
            .ca_only(true);
        let edge = AsterClient::builder()
            .config(
                base(&edge_key, &root)
                    .enrollment_credential_chain(root.credential(
                        &edge_key,
                        Some(&edge_roles),
                        Some(EDGE_ISSUABLE_ROLES),
                    ))
                    .enrollment(root.enrollment(None)?)
                    .identity_publisher(publisher)
                    .build(),
            )
            .address(service.address()?)
            .connect()
            .await?;

        // The edge publishes the provider's keys once it holds the grant.
        root.grant_publisher(&edge).await?;
        let keys = service
            .issuer_keys()
            .ok_or_else(|| Error::InvalidArgument("the service holds no issuer keys".into()))?;
        let publisher = edge
            .identity_publisher()
            .ok_or_else(|| Error::InvalidArgument("the edge runs no publisher".into()))?;
        let mut changes = keys.subscribe();
        let wait = async {
            publisher.notice_kid(idp::ISSUER, idp::KID);
            while keys.key_set(idp::ISSUER).is_none() {
                if changes.changed().await.is_err() {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .map_err(|_| {
                Error::Connection("the provider's keys never reached the service".into())
            })?;
        Ok(Self {
            root,
            idp,
            service,
            edge,
        })
    }
}
