#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! Gateway identity, slice 2 (`docs/_internal/web_gateway/gateway-identity.md`):
//! an edge holding `aster.gateway` forwards an identity provider's own JWT in
//! `aster-on-behalf-of`, and the core verifies it with issuer keys a node
//! holding `aster.identity-publisher` wrote into the fleet namespace.
//! Everything goes through the high-level path: `AsterServer` and
//! `AsterClient` configured by `AsterConfig` (`[enrollment]`, `[identity]`,
//! `[[identity.trusted_issuers]]`), roles from the credentials, overrides
//! through `aster.ops.NodeConfig`. The root is played with the low-level API.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::rpc::issuer_keys::{
    issuer_keys_key, publish_issuer_keys, IssuerKeyPublication, IssuerKeysHandle,
    IDENTITY_PUBLISHER_ROLE,
};
use aster::rpc::node_config::{IdentityOverride, NodeConfigSetRequest};
use aster::rpc::{
    async_trait, require_role, AsterClient, AsterServer, CallContext, CallOptions,
    NodeConfigClient, StatusCode, CONFIG_ADMIN_ROLE, GATEWAY_ROLE,
};
use aster::{
    AsterConfig, Doc, EnrollmentConfig, EnrollmentLimits, Node, PublicKey, RelayMode, ShareMode,
    TrustedIssuer,
};
use aster::{Result, SecretKey, SETTING_TRUSTED_ISSUERS};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, RsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fory_derive::ForyStruct;

const FLEET: &str = "provider-token-fleet";
const ISS_A: &str = "https://idp-a.example.com";
const ISS_B: &str = "https://idp-b.example.com";
const AUD: &str = "odata-core";

const SERVER: u8 = 2;
const ADMIN: u8 = 4;
const EDGE: u8 = 5;
const PUBLISHER: u8 = 7;
const ROGUE: u8 = 8;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "pttest/Who")]
struct Who {
    principal: String,
    actor: String,
    amr: String,
    roles: String,
    jti: String,
}

fn who(ctx: &CallContext) -> Who {
    Who {
        principal: ctx.principal().into(),
        actor: ctx.actor().unwrap_or_default().into(),
        amr: ctx.auth_methods().join(","),
        roles: ctx.attribute("aster.role").unwrap_or_default().into(),
        jti: ctx
            .on_behalf_of()
            .map(|identity| identity.jti().to_string())
            .unwrap_or_default(),
    }
}

#[aster::service(name = "PtData", version = 1)]
trait PtData {
    async fn whoami(&self, ctx: &CallContext) -> Result<Who>;

    #[rpc(requires = require_role("data.read"))]
    async fn read(&self, ctx: &CallContext) -> Result<Who>;

    #[rpc(requires = require_role("data.admin"))]
    async fn admin(&self, ctx: &CallContext) -> Result<Who>;
}

struct PtDataImpl;

#[async_trait]
impl PtData for PtDataImpl {
    async fn whoami(&self, ctx: &CallContext) -> Result<Who> {
        Ok(who(ctx))
    }
    async fn read(&self, ctx: &CallContext) -> Result<Who> {
        Ok(who(ctx))
    }
    async fn admin(&self, ctx: &CallContext) -> Result<Who> {
        Ok(who(ctx))
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
fn id(seed: u8) -> String {
    hex::encode(public(seed))
}

/// A root-issued credential for `seed` with `roles` in `aster.role`.
fn credential(seed: u8, roles: Option<&str>) -> String {
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
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

fn base(seed: u8) -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key(seed))
        .root_pubkey(PublicKey::from_bytes(public(1)))
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

/// The root's fleet namespace. Its node serves the write ticket the
/// publishers join with; it needs no Enrollment of its own.
struct Root {
    _node: Node,
    fleet: Doc,
}

impl Root {
    async fn start() -> Self {
        let node = Node::start(base(1).build()).await.unwrap();
        wait_for_addr(&node).await;
        let fleet = node.docs().create().await.unwrap();
        Self { _node: node, fleet }
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
}

fn issuer_a() -> TrustedIssuer {
    TrustedIssuer::new(ISS_A, [AUD])
        .roles_claim("groups")
        .map_role("data-readers", "data.read")
        .map_role("data-admins", "data.admin")
}

fn issuer_b() -> TrustedIssuer {
    TrustedIssuer::new(ISS_B, [AUD])
        .roles_claim("scope")
        .map_role("data:read", "data.read")
}

async fn start_server(root: &Root, state: &std::path::Path) -> AsterServer {
    let config = base(SERVER)
        .enrollment_credential_chain(credential(SERVER, None))
        .enrollment(root.enrollment(Some(state.join("server.json"))))
        .accept_on_behalf_of(true)
        .trusted_issuer(issuer_a())
        .build();
    let server = AsterServer::builder()
        .service(PtDataServer::new(PtDataImpl))
        .config(config)
        .start()
        .await
        .unwrap();
    wait_for_addr(server.node()).await;
    server
}

async fn connect(root: &Root, server: &AsterServer, seed: u8, roles: Option<&str>) -> AsterClient {
    let config = base(seed)
        .enrollment_credential_chain(credential(seed, roles))
        .enrollment(root.enrollment(None))
        .build();
    AsterClient::builder()
        .config(config)
        .address(server.address().unwrap())
        .connect()
        .await
        .unwrap()
}

fn options() -> CallOptions {
    CallOptions::new().deadline(Duration::from_secs(20))
}

fn data(client: &AsterClient, token: &str) -> PtDataClient {
    PtDataClient::new(client.rpc()).with_options(options().on_behalf_of(token))
}

fn code<T: std::fmt::Debug>(result: Result<T>) -> Option<StatusCode> {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(error) => error.rpc_code(),
    }
}

// ── An identity provider ────────────────────────────────────────────────────

enum IdpKey {
    Rsa(RsaKeyPair, bool),
    Ec(EcdsaKeyPair),
}

impl IdpKey {
    fn rs256() -> Self {
        Self::Rsa(
            RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap(),
            false,
        )
    }
    fn ps256() -> Self {
        Self::Rsa(
            RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap(),
            true,
        )
    }
    fn es256() -> Self {
        Self::Ec(EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap())
    }

    fn alg(&self) -> &'static str {
        match self {
            Self::Rsa(_, false) => "RS256",
            Self::Rsa(_, true) => "PS256",
            Self::Ec(_) => "ES256",
        }
    }

    fn jwk(&self, kid: &str) -> serde_json::Value {
        let b64 = |bytes: &[u8]| URL_SAFE_NO_PAD.encode(bytes);
        let mut jwk = match self {
            Self::Rsa(pair, _) => serde_json::json!({
                "kty": "RSA",
                "n": b64(pair.public_key().modulus().big_endian_without_leading_zero()),
                "e": b64(pair.public_key().exponent().big_endian_without_leading_zero()),
                // Providers publish certificates too; they never reach the namespace.
                "x5c": ["MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA"],
            }),
            Self::Ec(pair) => {
                let point = pair.public_key().as_ref();
                serde_json::json!({
                    "kty": "EC", "crv": "P-256",
                    "x": b64(&point[1..33]), "y": b64(&point[33..65]),
                })
            }
        };
        jwk["kid"] = kid.into();
        jwk["alg"] = self.alg().into();
        jwk["use"] = "sig".into();
        jwk
    }

    fn sign_with(&self, header: serde_json::Value, claims: serde_json::Value) -> String {
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let rng = SystemRandom::new();
        let signature = match self {
            Self::Rsa(pair, pss) => {
                let mut signature = vec![0; pair.public_modulus_len()];
                let encoding = if *pss {
                    &aws_lc_rs::signature::RSA_PSS_SHA256
                } else {
                    &aws_lc_rs::signature::RSA_PKCS1_SHA256
                };
                pair.sign(encoding, &rng, input.as_bytes(), &mut signature)
                    .unwrap();
                signature
            }
            Self::Ec(pair) => pair.sign(&rng, input.as_bytes()).unwrap().as_ref().to_vec(),
        };
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn token(&self, kid: &str, claims: serde_json::Value) -> String {
        self.sign_with(
            serde_json::json!({"alg": self.alg(), "kid": kid, "typ": "JWT"}),
            claims,
        )
    }
}

fn user_claims(iss: &str) -> serde_json::Value {
    serde_json::json!({
        "iss": iss,
        "sub": "alice",
        "aud": [AUD, "another-client"],
        "iat": now() - 5,
        "exp": now() + 300,
        "jti": "provider-jti-1",
        "amr": ["pwd", "mfa"],
        "groups": ["data-readers", "not-mapped"],
        "scope": "openid data:read",
    })
}

fn jwks(keys: &[(&IdpKey, &str)]) -> String {
    serde_json::json!({ "keys": keys.iter().map(|(key, kid)| key.jwk(kid)).collect::<Vec<_>>() })
        .to_string()
}

/// A publisher: an enrolled client that joins the fleet namespace with write
/// access, writes a key set as itself and delivers it to the server.
struct Publisher {
    client: AsterClient,
    fleet: Doc,
    chain: String,
}

impl Publisher {
    async fn start(root: &Root, server: &AsterServer, seed: u8, roles: Option<&str>) -> Self {
        let client = connect(root, server, seed, roles).await;
        let ticket = root.fleet.share_with_addr(ShareMode::Write).await.unwrap();
        let fleet = client.node().docs().join(ticket).await.unwrap();
        let chain = client.enrollment().unwrap().credential().unwrap().0;
        Self {
            client,
            fleet,
            chain,
        }
    }

    async fn publish(&self, server: &AsterServer, iss: &str, jwks: String, fetched_at: u64) {
        publish_issuer_keys(
            self.client.node(),
            &self.fleet,
            Some(&self.chain),
            &IssuerKeyPublication {
                iss: iss.into(),
                source_url: format!("{iss}/.well-known/jwks.json"),
                jwks_json: jwks,
                fetched_at,
            },
        )
        .await
        .unwrap();
        self.fleet
            .sync_with_timeout(&[server.id()], Duration::from_secs(15))
            .await
            .unwrap();
    }
}

/// Wait until `keys` satisfies `condition`: a condition wait on the rebuild
/// generation.
async fn wait_keys(keys: &IssuerKeysHandle, condition: impl Fn(&IssuerKeysHandle) -> bool) {
    let mut changes = keys.subscribe();
    while !condition(keys) {
        changes.changed().await.expect("issuer keys dropped");
    }
}

/// Wait until the server holds the complete content of `author`'s key set
/// for `iss`: a condition wait on the server's fleet-namespace events.
async fn wait_entry(server: &AsterServer, fleet: &Doc, author: u8, iss: &str) {
    let doc = server
        .node()
        .docs()
        .open(fleet.id().unwrap())
        .await
        .unwrap()
        .expect("the server holds the fleet namespace");
    let events = doc.subscribe().await.unwrap();
    let key = issuer_keys_key(iss);
    loop {
        let entries = doc.query_key_exact(key.clone(), None).await.unwrap();
        if let Some(entry) = entries.iter().find(|e| e.author.as_str() == id(author)) {
            let content = doc
                .read_entry_contents_if_complete(std::slice::from_ref(&entry.content_hash))
                .await
                .unwrap();
            if content[0].is_some() {
                return;
            }
        }
        events.recv().await.unwrap().expect("subscription ended");
    }
}

fn kids(keys: &IssuerKeysHandle, iss: &str) -> Vec<String> {
    keys.key_set(iss).map(|set| set.kids).unwrap_or_default()
}

#[tokio::test]
async fn provider_tokens_verify_with_published_keys_rotate_and_follow_node_config() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(&root, files.path()).await;
        let keys = server
            .issuer_keys()
            .expect("an enrolled server holds issuer keys");
        let edge = connect(&root, &server, EDGE, Some(GATEWAY_ROLE)).await;
        let publisher =
            Publisher::start(&root, &server, PUBLISHER, Some(IDENTITY_PUBLISHER_ROLE)).await;
        // The rogue holds a valid credential, without the publisher role.
        let rogue = Publisher::start(&root, &server, ROGUE, Some("data.read")).await;

        let a_rs = IdpKey::rs256();
        let a_es = IdpKey::es256();
        let a_ps = IdpKey::ps256();
        let evil = IdpKey::rs256();
        let b_es = IdpKey::es256();

        // The rogue's key set for A is the newest, and arrives first. It must
        // count for nothing.
        rogue
            .publish(&server, ISS_A, jwks(&[(&evil, "evil")]), now())
            .await;
        wait_entry(&server, &root.fleet, ROGUE, ISS_A).await;
        publisher
            .publish(
                &server,
                ISS_A,
                jwks(&[(&a_rs, "a-rs"), (&a_es, "a-es"), (&a_ps, "a-ps")]),
                now() - 10,
            )
            .await;
        publisher
            .publish(&server, ISS_B, jwks(&[(&b_es, "b-es")]), now() - 10)
            .await;
        wait_keys(&keys, |k| {
            k.key_set(ISS_A)
                .is_some_and(|s| s.publisher == id(PUBLISHER))
                && k.key_set(ISS_B).is_some()
        })
        .await;
        let set = keys.key_set(ISS_A).unwrap();
        assert_eq!(set.kids, ["a-rs", "a-es", "a-ps"]);
        assert_eq!(set.source_url, format!("{ISS_A}/.well-known/jwks.json"));
        assert!(set.valid_until.is_some());

        // RS256, ES256 and PS256 tokens from A: alice, through the edge,
        // with only the mapped group as a role.
        for (key, kid) in [(&a_rs, "a-rs"), (&a_es, "a-es"), (&a_ps, "a-ps")] {
            let token = key.token(kid, user_claims(ISS_A));
            let alice = data(&edge, &token).read().await.unwrap();
            assert_eq!(alice.principal, format!("{ISS_A}|alice"));
            assert_eq!(alice.actor, id(EDGE));
            assert_eq!(alice.amr, "pwd,mfa");
            assert_eq!(alice.roles, "data.read");
            assert_eq!(alice.jti, "provider-jti-1");
            assert_eq!(
                code(data(&edge, &token).admin().await),
                Some(StatusCode::PermissionDenied),
                "an unmapped group grants nothing"
            );
        }
        let mut admin_claims = user_claims(ISS_A);
        admin_claims["groups"] = serde_json::json!(["data-admins"]);
        let admin_token = a_es.token("a-es", admin_claims);
        assert_eq!(
            data(&edge, &admin_token).admin().await.unwrap().roles,
            "data.admin"
        );

        // Refusals, all UNAUTHENTICATED: the rogue's key, an unknown kid (no
        // fetch happens), alg none, an algorithm the key does not use, the
        // wrong audience, expiry, a lifetime above the issuer's maximum, an
        // issuer that is not trusted although its keys are held.
        let unauthenticated = |token: String, what: &'static str| {
            let edge = &edge;
            async move {
                assert_eq!(
                    code(data(edge, &token).whoami().await),
                    Some(StatusCode::Unauthenticated),
                    "{what}"
                );
            }
        };
        unauthenticated(evil.token("evil", user_claims(ISS_A)), "rogue key").await;
        unauthenticated(a_rs.token("a-unknown", user_claims(ISS_A)), "unknown kid").await;
        let unsigned = format!(
            "{}.{}.AAAA",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none","kid":"a-rs"}"#),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&user_claims(ISS_A)).unwrap())
        );
        unauthenticated(unsigned, "alg none").await;
        unauthenticated(
            a_rs.sign_with(
                serde_json::json!({"alg": "PS256", "kid": "a-rs"}),
                user_claims(ISS_A),
            ),
            "alg not the key's",
        )
        .await;
        unauthenticated(
            a_rs.sign_with(
                serde_json::json!({"alg": "HS256", "kid": "a-rs"}),
                user_claims(ISS_A),
            ),
            "HS256",
        )
        .await;
        let mut claims = user_claims(ISS_A);
        claims["aud"] = "someone-else".into();
        unauthenticated(a_rs.token("a-rs", claims), "audience").await;
        let mut claims = user_claims(ISS_A);
        claims["iat"] = (now() - 400).into();
        claims["exp"] = (now() - 1).into();
        unauthenticated(a_rs.token("a-rs", claims), "expired").await;
        let mut claims = user_claims(ISS_A);
        claims["nbf"] = (now() + 120).into();
        claims["exp"] = (now() + 600).into();
        unauthenticated(a_rs.token("a-rs", claims), "nbf beyond leeway").await;
        let mut claims = user_claims(ISS_A);
        claims["exp"] = (now() + 3600).into();
        unauthenticated(a_rs.token("a-rs", claims), "lifetime above 900 s").await;
        unauthenticated(b_es.token("b-es", user_claims(ISS_B)), "B not trusted").await;

        // A peer without aster.gateway may not forward even a valid token.
        let valid = a_rs.token("a-rs", user_claims(ISS_A));
        assert_eq!(
            code(data(&rogue.client, &valid).whoami().await),
            Some(StatusCode::PermissionDenied)
        );

        // Rotation: A's newest set replaces the old one, without a restart.
        let a_new = IdpKey::rs256();
        publisher
            .publish(&server, ISS_A, jwks(&[(&a_new, "a-new")]), now())
            .await;
        wait_keys(&keys, |k| kids(k, ISS_A) == ["a-new"]).await;
        data(&edge, &a_new.token("a-new", user_claims(ISS_A)))
            .read()
            .await
            .unwrap();
        assert_eq!(
            code(data(&edge, &valid).whoami().await),
            Some(StatusCode::Unauthenticated),
            "a key removed from the newest set stops verifying"
        );

        // NodeConfig: trusting B takes effect once `set` acknowledges.
        let admin = connect(&root, &server, ADMIN, Some(CONFIG_ADMIN_ROLE)).await;
        let api = NodeConfigClient::new(admin.rpc()).with_options(options());
        let refused = api
            .set(NodeConfigSetRequest::new().identity(IdentityOverride {
                trusted_issuers: Some(vec![TrustedIssuer::new(ISS_B, Vec::<String>::new())]),
                ..Default::default()
            }))
            .await;
        assert_eq!(code(refused), Some(StatusCode::InvalidArgument));
        let ack = api
            .set(NodeConfigSetRequest::new().identity(IdentityOverride {
                trusted_issuers: Some(vec![issuer_a(), issuer_b()]),
                ..Default::default()
            }))
            .await
            .unwrap();
        assert!(ack.complete, "{ack:?}");
        let bob = data(&edge, &b_es.token("b-es", user_claims(ISS_B)))
            .read()
            .await
            .unwrap();
        assert_eq!(bob.principal, format!("{ISS_B}|alice"));
        assert_eq!(bob.roles, "data.read", "from the space-separated scope");
        let view = api.get().await.unwrap();
        let setting = view.setting(SETTING_TRUSTED_ISSUERS).unwrap();
        assert_eq!(setting.source, "override");
        assert!(setting
            .value
            .as_ref()
            .and_then(|v| v.as_text())
            .unwrap()
            .contains(ISS_B));
        assert_eq!(
            server
                .node_config()
                .unwrap()
                .identity()
                .trusted_issuers
                .len(),
            2
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn an_issuer_without_audiences_or_enrollment_fails_start_up() {
    let root = Root::start().await;
    let files = tempfile::tempdir().unwrap();
    let config = base(SERVER)
        .enrollment_credential_chain(credential(SERVER, None))
        .enrollment(root.enrollment(Some(files.path().join("server.json"))))
        .trusted_issuer(TrustedIssuer::new(ISS_A, Vec::<String>::new()))
        .build();
    let error = AsterServer::builder()
        .service(PtDataServer::new(PtDataImpl))
        .config(config)
        .start()
        .await
        .err()
        .expect("an issuer without audiences must fail start-up");
    assert!(error.to_string().contains("audiences"), "{error}");

    let no_enrollment = base(SERVER).trusted_issuer(issuer_a()).build();
    let error = AsterServer::builder()
        .service(PtDataServer::new(PtDataImpl))
        .config(no_enrollment)
        .start()
        .await
        .err()
        .expect("trusted issuers need Enrollment");
    assert!(error.to_string().contains("enrollment"), "{error}");
}
