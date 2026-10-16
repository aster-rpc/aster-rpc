#![cfg(feature = "identity-publisher")]
//! Gateway identity, slice 3 (`docs/_internal/web_gateway/gateway-identity.md`):
//! the issuer-key publisher. A gateway (`AsterClient` with
//! `[identity.publisher]`, holding `aster.gateway` and
//! `aster.identity-publisher`) fetches an identity provider's JWK set over
//! validated HTTPS from an in-process server with a throwaway CA, writes it
//! into the fleet namespace under the root's sealed write grant, and a core
//! (`AsterServer`) verifies the provider's tokens with it. Rotation at the
//! provider is picked up when a forwarded token names an unknown `kid`; bad
//! fetches never replace the last good set; stale sets stop verifying; and a
//! root rotation moves the core's watcher to the new fleet namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::delegation::{issue_root_rotation, next_root_commitment, RotationReason};
use aster::enrollment::*;
use aster::rpc::issuer_keys::{
    publish_issuer_keys, IssuerKeyPublication, IssuerKeysHandle, IDENTITY_PUBLISHER_ROLE,
};
use aster::rpc::jwks_publisher::{IssuerPublishStatus, PublishError, PublisherHandle};
use aster::rpc::node_config::{IdentityOverride, NodeConfigSetRequest};
use aster::rpc::{
    async_trait, AsterClient, AsterServer, CallContext, CallOptions, NodeConfigClient, StatusCode,
    CONFIG_ADMIN_ROLE, GATEWAY_ROLE,
};
use aster::{
    AsterConfig, Doc, EnrollmentConfig, EnrollmentLimits, IdentityPublisherConfig, NamespaceSecret,
    Node, PublicKey, RelayMode, Result, SecretKey, TrustedIssuer,
};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fory_derive::ForyStruct;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyUsagePurpose};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FLEET: &str = "identity-publisher-fleet";
const ISS: &str = "https://idp.test.example";
const ISS_B: &str = "https://idp-b.test.example";
const AUD: &str = "odata-core";

const ROOT: u8 = 1;
const SERVER: u8 = 2;
const ADMIN: u8 = 4;
const GATEWAY: u8 = 5;
const UNTRUSTING: u8 = 6;
const NEW_ROOT: u8 = 11;
const NEXT_ROOT: u8 = 12;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "iptest/Who")]
struct Who {
    principal: String,
}

#[aster::service(name = "IpData", version = 1)]
trait IpData {
    async fn whoami(&self, ctx: &CallContext) -> Result<Who>;
}

struct IpDataImpl;

#[async_trait]
impl IpData for IpDataImpl {
    async fn whoami(&self, ctx: &CallContext) -> Result<Who> {
        Ok(Who {
            principal: ctx.principal().into(),
        })
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes([seed; 32])
}
fn public(seed: u8) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
}

/// A credential for `seed` issued by `root`, with `roles` in `aster.role`.
fn credential_from(root: u8, seed: u8, roles: Option<&str>) -> String {
    let mut attributes = BTreeMap::new();
    if let Some(roles) = roles {
        attributes.insert("aster.role".into(), TypedAttribute::String(roles.into()));
    }
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    request.claims = AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    };
    issue_credential(&key(root), &request, now())
        .unwrap()
        .into_chain()
}
fn credential(seed: u8, roles: Option<&str>) -> String {
    credential_from(ROOT, seed, roles)
}

fn base(seed: u8) -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key(seed))
        .root_pubkey(PublicKey::from_bytes(public(ROOT)))
}

fn namespace(doc: &Doc) -> [u8; 32] {
    hex::decode(doc.id().unwrap().to_string())
        .unwrap()
        .try_into()
        .unwrap()
}

async fn wait_for_addr(node: &Node) {
    use aster::native::iroh::Watcher;
    let mut addrs = node.native_endpoint().watch_addr();
    while node.addr().direct_addresses.is_empty() {
        addrs.updated().await.expect("endpoint closed");
    }
}

/// A root, played with the low-level API: its node holds the fleet
/// namespace's write secret (as `aster enroll identity-publisher` does) and
/// serves it as a policy replica, so enrolled nodes can sync from it.
struct Root {
    node: Node,
    seed: u8,
    secret: NamespaceSecret,
    fleet: Doc,
    _enrollment: Enrollment,
}

impl Root {
    /// `seed`'s root, pinned to `seed` with `next` as its next root.
    async fn start(seed: u8, next: u8, state: &std::path::Path) -> Self {
        let node = Node::start_with_alpns(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(key(seed))
                .hooks(true)
                .hook_failure_mode(aster::HookFailureMode::FailClosed)
                .hook_timeout_ms(1000)
                .build(),
            vec![ENROLLMENT_ALPN.to_vec()],
        )
        .await
        .unwrap();
        wait_for_addr(&node).await;
        let secret = NamespaceSecret::from_bytes([seed.wrapping_add(100); 32]);
        let fleet = node
            .docs()
            .open_or_import_write_namespace(secret.clone())
            .await
            .unwrap();
        let enrollment = Enrollment::install(
            &node,
            Arc::new(
                EnrollmentPolicyStore::open(
                    state.join(format!("root-{seed}.json")),
                    public(seed),
                    namespace(&fleet),
                    FLEET,
                )
                .unwrap(),
            ),
            JwtEnrollmentVerifier::new(public(seed), FLEET).unwrap(),
            Some(credential_from(seed, seed, None)),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions {
                reconciliation_interval: Duration::from_millis(50),
                rotation: Some(RotationPin {
                    next_commitment: next_root_commitment(&public(next)),
                    seq: 0,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        enrollment.set_policy_document(&fleet).unwrap();
        let served = (node.clone(), enrollment.clone());
        tokio::spawn(async move {
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
        Self {
            node,
            seed,
            secret,
            fleet,
            _enrollment: enrollment,
        }
    }

    fn enrollment(&self, state: Option<std::path::PathBuf>) -> EnrollmentConfig {
        EnrollmentConfig {
            audience: Some(FLEET.into()),
            fleet_namespace: Some(hex::encode(namespace(&self.fleet))),
            state_path: state,
            limits: EnrollmentLimits {
                reconciliation_interval_ms: Some(200),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    async fn put(&self, key: impl Into<Vec<u8>>, value: Vec<u8>) {
        let author = self.node.docs().default_author().await.unwrap();
        self.fleet
            .set_bytes(&author, key.into(), value)
            .await
            .unwrap();
    }

    /// Seal the fleet write grant to `publisher`.
    async fn grant(&self, publisher: u8) {
        let (key, record) = publisher_grant_record(
            &public(self.seed),
            &public(publisher),
            &self.secret.to_bytes(),
        )
        .unwrap();
        self.put(key.into_bytes(), record).await;
    }

    async fn withdraw(&self, publisher: u8) {
        self.put(
            publisher_grant_key(&public(publisher)).into_bytes(),
            publisher_grant_withdrawal("test"),
        )
        .await;
    }

    /// Deliver the root's records to the node running `enrollment`: it
    /// enrolls with the root and syncs the fleet namespace (production: the
    /// root's CLI session delivers to the policy replicas, and nodes sync
    /// from them).
    async fn deliver_to(&self, enrollment: &Enrollment) -> Doc {
        let (ready, ticket) = enrollment.connect(self.node.addr()).await.unwrap();
        enrollment
            .synchronize(
                ready.peer,
                ticket.expect("the root offers its fleet namespace"),
                Duration::from_secs(15),
            )
            .await
            .unwrap()
    }

    /// Write a key set for `ISS` as the root's own node, carrying a
    /// self-issued credential that grants the publisher role.
    async fn publish(&self, key: &IdpKey, kid: &str) {
        publish_issuer_keys(
            &self.node,
            &self.fleet,
            Some(&credential_from(
                self.seed,
                self.seed,
                Some(IDENTITY_PUBLISHER_ROLE),
            )),
            &IssuerKeyPublication {
                iss: ISS.into(),
                source_url: format!("{ISS}/jwks"),
                jwks_json: jwks(&[(key, kid)]),
                fetched_at: now(),
            },
        )
        .await
        .unwrap();
    }
}

// ── An identity provider, served over HTTPS from a throwaway CA ────────────

struct IdpKey(EcdsaKeyPair);

impl IdpKey {
    fn new() -> Self {
        Self(EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap())
    }

    fn jwk(&self, kid: &str) -> serde_json::Value {
        let point = self.0.public_key().as_ref();
        serde_json::json!({
            "kty": "EC", "crv": "P-256", "kid": kid, "alg": "ES256", "use": "sig",
            "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
            "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
        })
    }

    fn token(&self, iss: &str, kid: &str) -> String {
        let header = serde_json::json!({"alg": "ES256", "kid": kid, "typ": "JWT"});
        let claims = serde_json::json!({
            "iss": iss, "sub": "alice", "aud": AUD,
            "iat": now() - 5, "exp": now() + 300,
        });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let signature = self.0.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

fn jwks(keys: &[(&IdpKey, &str)]) -> String {
    serde_json::json!({ "keys": keys.iter().map(|(key, kid)| key.jwk(kid)).collect::<Vec<_>>() })
        .to_string()
}

/// A minimal HTTPS server answering every request with the current
/// `(status, body)`.
struct Idp {
    addr: SocketAddr,
    ca_pem: std::path::PathBuf,
    response: Arc<Mutex<(u16, String)>>,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Idp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Idp {
    async fn start(dir: &std::path::Path) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "Aster test identity provider CA");
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();
        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone(), ca_cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
                ),
            )
            .unwrap();
        let ca_pem = dir.join("idp-ca.pem");
        std::fs::write(&ca_pem, ca_cert.pem()).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let response = Arc::new(Mutex::new((404, String::new())));
        let requests = Arc::new(AtomicUsize::new(0));
        let (served, counted) = (response.clone(), requests.clone());
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let served = served.clone();
                let counted = counted.clone();
                tokio::spawn(async move {
                    // A client that refuses the certificate never gets here.
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    counted.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = served.lock().unwrap().clone();
                    let head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = tls.write_all(head.as_bytes()).await;
                    let _ = tls.write_all(body.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        Self {
            addr,
            ca_pem,
            response,
            requests,
            task,
        }
    }

    fn serve(&self, status: u16, body: impl Into<String>) {
        *self.response.lock().unwrap() = (status, body.into());
    }

    fn url(&self) -> String {
        format!("https://{}/.well-known/jwks.json", self.addr)
    }

    /// The gateway's `[identity.publisher]`, trusting only this CA.
    fn publisher(&self) -> IdentityPublisherConfig {
        IdentityPublisherConfig::new([(ISS, self.url())])
            .min_refresh_secs(1)
            .ca_cert(&self.ca_pem)
            .ca_only(true)
    }
}

// ── Waits ───────────────────────────────────────────────────────────────────

async fn wait_keys(keys: &IssuerKeysHandle, condition: impl Fn(&IssuerKeysHandle) -> bool) {
    let mut changes = keys.subscribe();
    while !condition(keys) {
        changes.changed().await.expect("issuer keys dropped");
    }
}

/// Wait until the publisher's status for `iss` satisfies `condition`.
async fn wait_status(
    publisher: &PublisherHandle,
    iss: &str,
    condition: impl Fn(&IssuerPublishStatus) -> bool,
) -> IssuerPublishStatus {
    let mut changes = publisher.subscribe();
    loop {
        let status = publisher.issuer_status(iss).expect("a configured issuer");
        if condition(&status) {
            return status;
        }
        changes.changed().await.expect("publisher stopped");
    }
}

/// Notice an unknown kid and wait for the refresh it forces to finish.
async fn refresh_now(publisher: &PublisherHandle) -> IssuerPublishStatus {
    let attempts = publisher.issuer_status(ISS).unwrap().attempts;
    publisher.notice_kid(ISS, "not-a-published-kid");
    wait_status(publisher, ISS, |s| s.attempts > attempts).await
}

fn kids(keys: &IssuerKeysHandle, iss: &str) -> Vec<String> {
    keys.key_set(iss).map(|set| set.kids).unwrap_or_default()
}

fn data(client: &AsterClient, token: &str) -> IpDataClient {
    IpDataClient::new(client.rpc()).with_options(
        CallOptions::new()
            .deadline(Duration::from_secs(20))
            .on_behalf_of(token),
    )
}

fn refused<T: std::fmt::Debug>(result: Result<T>) -> Option<StatusCode> {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(error) => error.rpc_code(),
    }
}

async fn start_server(root: &Root, state: &std::path::Path) -> AsterServer {
    let config = base(SERVER)
        .enrollment_credential_chain(credential(SERVER, None))
        .enrollment(root.enrollment(Some(state.join("server.json"))))
        .accept_on_behalf_of(true)
        .trusted_issuer(TrustedIssuer::new(ISS, [AUD]))
        .trusted_issuer(TrustedIssuer::new(ISS_B, [AUD]))
        .build();
    let server = AsterServer::builder()
        .service(IpDataServer::new(IpDataImpl))
        .config(config)
        .start()
        .await
        .unwrap();
    wait_for_addr(server.node()).await;
    server
}

async fn connect(
    root: &Root,
    server: &AsterServer,
    seed: u8,
    roles: Option<&str>,
    publisher: Option<IdentityPublisherConfig>,
) -> AsterClient {
    let mut config = base(seed)
        .enrollment_credential_chain(credential(seed, roles))
        .enrollment(root.enrollment(None));
    if let Some(publisher) = publisher {
        config = config.identity_publisher(publisher);
    }
    AsterClient::builder()
        .config(config.build())
        .address(server.address().unwrap())
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_publisher_fetches_publishes_and_follows_the_provider() {
    tokio::time::timeout(Duration::from_secs(240), async {
        let files = tempfile::tempdir().unwrap();
        let root = Root::start(ROOT, NEW_ROOT, files.path()).await;
        let server = start_server(&root, files.path()).await;
        let keys = server.issuer_keys().unwrap();
        let idp = Idp::start(files.path()).await;
        let k1 = IdpKey::new();
        idp.serve(200, jwks(&[(&k1, "k1")]));

        // The gateway forwards tokens and publishes keys.
        let roles = format!("{GATEWAY_ROLE},{IDENTITY_PUBLISHER_ROLE}");
        let gateway = connect(
            &root,
            &server,
            GATEWAY,
            Some(&roles),
            Some(idp.publisher()),
        )
        .await;
        let publisher = gateway.identity_publisher().expect("a publisher");

        // No write grant yet: the first fetch succeeds, nothing is written.
        let status = wait_status(&publisher, ISS, |s| s.attempts >= 1).await;
        assert_eq!(status.last_error, Some(PublishError::NoWriteGrant));
        assert_eq!(status.publications, 0);
        assert!(idp.requests.load(Ordering::SeqCst) >= 1);
        assert!(keys.key_set(ISS).is_none());

        // The root seals the fleet write grant to the gateway.
        root.grant(GATEWAY).await;
        let gateway_fleet = root.deliver_to(gateway.enrollment().unwrap()).await;
        let status = refresh_now(&publisher).await;
        assert_eq!(status.last_error, None, "{status:?}");
        assert_eq!(status.kids, ["k1"]);
        wait_keys(&keys, |k| kids(k, ISS) == ["k1"]).await;
        let set = keys.key_set(ISS).unwrap();
        assert_eq!(set.publisher, hex::encode(public(GATEWAY)));
        assert_eq!(set.source_url, idp.url());
        let alice = data(&gateway, &k1.token(ISS, "k1")).whoami().await.unwrap();
        assert_eq!(alice.principal, format!("{ISS}|alice"));

        // The provider rotates to k2. The gateway forwards a k2 token; its
        // unknown kid refreshes at once, and the core then verifies it.
        let k2 = IdpKey::new();
        idp.serve(200, jwks(&[(&k1, "k1"), (&k2, "k2")]));
        let k2_token = k2.token(ISS, "k2");
        let _ = data(&gateway, &k2_token).whoami().await;
        wait_keys(&keys, |k| kids(k, ISS) == ["k1", "k2"]).await;
        data(&gateway, &k2_token).whoami().await.unwrap();
        // The core can hold the set before the publisher records it.
        wait_status(&publisher, ISS, |s| s.kids == ["k1", "k2"]).await;

        // Failures never replace the last good set.
        let k3 = IdpKey::new();
        let bad = [
            (500, "{}".to_string(), PublishError::Status(500)),
            (302, String::new(), PublishError::Status(302)),
            (200, "x".repeat(1024 * 1024 + 1), PublishError::TooLarge),
            (200, "not json".to_string(), PublishError::Invalid),
            (
                200,
                r#"{"keys": [{"kty": "oct", "kid": "k3", "k": "AAAA"}]}"#.to_string(),
                PublishError::NoUsableKeys,
            ),
            (
                200,
                // An unsupported key alongside nothing usable.
                serde_json::json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": "k3",
                    "x": URL_SAFE_NO_PAD.encode([7u8; 32])}]})
                .to_string(),
                PublishError::NoUsableKeys,
            ),
        ];
        for (status, body, expected) in bad {
            idp.serve(status, body);
            let outcome = refresh_now(&publisher).await;
            assert_eq!(outcome.last_error, Some(expected));
            assert_eq!(outcome.kids, ["k1", "k2"]);
        }
        assert_eq!(kids(&keys, ISS), ["k1", "k2"]);
        data(&gateway, &k2_token).whoami().await.unwrap();
        // An unsupported key is dropped; the usable one is published.
        idp.serve(
            200,
            serde_json::json!({"keys": [
                k3.jwk("k3"),
                {"kty": "OKP", "crv": "Ed25519", "kid": "ed", "x": URL_SAFE_NO_PAD.encode([7u8; 32])},
            ]})
            .to_string(),
        );
        let outcome = refresh_now(&publisher).await;
        assert_eq!(outcome.kids, ["k3"]);
        wait_keys(&keys, |k| kids(k, ISS) == ["k3"]).await;
        assert_eq!(
            refused(data(&gateway, &k2_token).whoami().await),
            Some(StatusCode::Unauthenticated),
            "a key the provider removed stops verifying"
        );

        // A stale set (older than max_key_set_age_secs, 48 h by default) is
        // held but verifies nothing, until NodeConfig raises the limit.
        let b1 = IdpKey::new();
        let chain = gateway.enrollment().unwrap().credential().unwrap().0;
        let writable = gateway
            .node()
            .docs()
            .open(gateway_fleet.id().unwrap())
            .await
            .unwrap()
            .unwrap();
        publish_issuer_keys(
            gateway.node(),
            &writable,
            Some(&chain),
            &IssuerKeyPublication {
                iss: ISS_B.into(),
                source_url: format!("{ISS_B}/jwks"),
                jwks_json: jwks(&[(&b1, "b1")]),
                fetched_at: now() - 172_801,
            },
        )
        .await
        .unwrap();
        writable
            .sync_with_timeout(&[server.id()], Duration::from_secs(15))
            .await
            .unwrap();
        wait_keys(&keys, |k| k.key_set(ISS_B).is_some()).await;
        let b_token = b1.token(ISS_B, "b1");
        assert_eq!(
            refused(data(&gateway, &b_token).whoami().await),
            Some(StatusCode::Unauthenticated),
            "a stale key set verifies nothing"
        );
        let admin = connect(&root, &server, ADMIN, Some(CONFIG_ADMIN_ROLE), None).await;
        let api = NodeConfigClient::new(admin.rpc())
            .with_options(CallOptions::new().deadline(Duration::from_secs(20)));
        let too_short = api
            .set(NodeConfigSetRequest::new().identity(IdentityOverride {
                max_key_set_age_secs: Some(10),
                ..Default::default()
            }))
            .await;
        assert_eq!(refused(too_short), Some(StatusCode::InvalidArgument));
        let ack = api
            .set(NodeConfigSetRequest::new().identity(IdentityOverride {
                max_key_set_age_secs: Some(30 * 86_400),
                ..Default::default()
            }))
            .await
            .unwrap();
        assert!(ack.complete, "{ack:?}");
        data(&gateway, &b_token).whoami().await.unwrap();

        // The root withdraws the grant: the gateway stops writing.
        root.withdraw(GATEWAY).await;
        root.deliver_to(gateway.enrollment().unwrap()).await;
        let outcome = refresh_now(&publisher).await;
        assert_eq!(outcome.last_error, Some(PublishError::GrantWithdrawn));
        assert_eq!(kids(&keys, ISS), ["k3"]);

        // TLS is validated: a publisher that does not trust the provider's
        // CA never reads its keys.
        let mut untrusting = IdentityPublisherConfig::new([(ISS, idp.url())]);
        untrusting.min_refresh_secs = 1;
        let untrusting = connect(
            &root,
            &server,
            UNTRUSTING,
            Some(IDENTITY_PUBLISHER_ROLE),
            Some(untrusting),
        )
        .await;
        let status = wait_status(&untrusting.identity_publisher().unwrap(), ISS, |s| {
            s.attempts >= 1
        })
        .await;
        assert_eq!(status.last_error, Some(PublishError::Fetch));
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn a_core_that_publishes_refreshes_on_a_refused_unknown_kid() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let files = tempfile::tempdir().unwrap();
        let root = Root::start(ROOT, NEW_ROOT, files.path()).await;
        let idp = Idp::start(files.path()).await;
        let k1 = IdpKey::new();
        idp.serve(200, jwks(&[(&k1, "k1")]));
        // The core publishes for itself: its own credential holds the role.
        let config = base(SERVER)
            .enrollment_credential_chain(credential(SERVER, Some(IDENTITY_PUBLISHER_ROLE)))
            .enrollment(root.enrollment(Some(files.path().join("server.json"))))
            .accept_on_behalf_of(true)
            .trusted_issuer(TrustedIssuer::new(ISS, [AUD]))
            .identity_publisher(idp.publisher())
            .build();
        let server = AsterServer::builder()
            .service(IpDataServer::new(IpDataImpl))
            .config(config)
            .start()
            .await
            .unwrap();
        wait_for_addr(server.node()).await;
        let keys = server.issuer_keys().unwrap();
        let publisher = server.identity_publisher().expect("a publisher");
        let edge = connect(&root, &server, GATEWAY, Some(GATEWAY_ROLE), None).await;
        // Two attempts without a grant: the next scheduled retry is at least
        // 5 s (the second backoff step) after the second.
        let status = wait_status(&publisher, ISS, |s| s.attempts >= 2).await;
        assert_eq!(status.last_error, Some(PublishError::NoWriteGrant));
        let second_attempt = std::time::Instant::now();

        root.grant(SERVER).await;
        root.deliver_to(server.enrollment().unwrap()).await;
        // Refused for the unknown kid, which makes the core's publisher
        // fetch at once (well before its retry); then it verifies.
        let token = k1.token(ISS, "k1");
        assert_eq!(
            refused(data(&edge, &token).whoami().await),
            Some(StatusCode::Unauthenticated)
        );
        wait_keys(&keys, |k| kids(k, ISS) == ["k1"]).await;
        assert!(
            second_attempt.elapsed() < Duration::from_secs(5),
            "published by the notice, not the scheduled retry"
        );
        assert_eq!(
            data(&edge, &token).whoami().await.unwrap().principal,
            format!("{ISS}|alice")
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn publisher_config_is_checked_at_start_up() {
    let files = tempfile::tempdir().unwrap();
    let root = Root::start(ROOT, NEW_ROOT, files.path()).await;
    let server = start_server(&root, files.path()).await;
    let bad = [
        IdentityPublisherConfig::new([(ISS, "http://idp/jwks")]),
        IdentityPublisherConfig::new([(ISS, "https://idp/jwks")]).refresh_secs(10),
        IdentityPublisherConfig::new([(ISS, "https://idp/jwks")])
            .ca_cert(files.path().join("missing.pem")),
    ];
    for publisher in bad {
        let config = base(GATEWAY)
            .enrollment_credential_chain(credential(GATEWAY, None))
            .enrollment(root.enrollment(None))
            .identity_publisher(publisher)
            .build();
        assert!(AsterClient::builder()
            .config(config)
            .address(server.address().unwrap())
            .connect()
            .await
            .is_err());
    }
    // A publisher needs Enrollment.
    let config = base(GATEWAY)
        .identity_publisher(IdentityPublisherConfig::new([(ISS, "https://idp/jwks")]))
        .build();
    let error = AsterServer::builder()
        .service(IpDataServer::new(IpDataImpl))
        .config(config)
        .start()
        .await
        .err()
        .expect("a publisher without enrollment must fail start-up");
    assert!(error.to_string().contains("[enrollment]"), "{error}");
}

#[tokio::test]
async fn root_rotation_moves_the_issuer_key_watcher_to_the_new_fleet() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let files = tempfile::tempdir().unwrap();
        let old_root = Root::start(ROOT, NEW_ROOT, files.path()).await;
        let config = base(SERVER)
            .enrollment_credential_chain(credential(SERVER, None))
            .enrollment(old_root.enrollment(Some(files.path().join("server.json"))))
            .root_rotation(
                hex::encode(next_root_commitment(&public(NEW_ROOT))),
                0,
                Vec::new(),
            )
            .accept_on_behalf_of(true)
            .trusted_issuer(TrustedIssuer::new(ISS, [AUD]))
            .build();
        let server = AsterServer::builder()
            .service(IpDataServer::new(IpDataImpl))
            .config(config)
            .start()
            .await
            .unwrap();
        wait_for_addr(server.node()).await;
        let keys = server.issuer_keys().unwrap();
        let enrollment = server.enrollment().unwrap().clone();

        // A key set under the old root reaches the core.
        old_root.publish(&IdpKey::new(), "old").await;
        old_root.deliver_to(&enrollment).await;
        wait_keys(&keys, |k| kids(k, ISS) == ["old"]).await;

        // The pre-committed new root rotates, into a fresh fleet namespace.
        let new_root = Root::start(NEW_ROOT, NEXT_ROOT, files.path()).await;
        let statement = issue_root_rotation(
            &key(NEW_ROOT),
            &PublicKey::from_bytes(public(ROOT)),
            1,
            &next_root_commitment(&public(NEXT_ROOT)),
            &[namespace(&new_root.fleet)],
            now(),
            RotationReason::Routine,
        )
        .unwrap();
        assert_eq!(enrollment.apply_root_rotations(&[statement]).unwrap(), 1);
        assert_eq!(enrollment.fleet_namespace(), namespace(&new_root.fleet));
        // The old root's key sets count for nothing now.
        wait_keys(&keys, |k| k.key_set(ISS).is_none()).await;

        // A key set in the new fleet namespace is picked up: the watcher
        // moved there.
        enrollment
            .replace_credential(credential_from(NEW_ROOT, SERVER, None))
            .await
            .unwrap();
        new_root.publish(&IdpKey::new(), "new").await;
        new_root.deliver_to(&enrollment).await;
        wait_keys(&keys, |k| kids(k, ISS) == ["new"]).await;
        let set = keys.key_set(ISS).unwrap();
        assert_eq!(set.publisher, hex::encode(public(NEW_ROOT)));
    })
    .await
    .expect("test timed out");
}
