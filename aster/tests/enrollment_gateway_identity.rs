#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! Gateway identity, slice 1 (`docs/_internal/web_gateway/gateway-identity.md`):
//! an edge holding `aster.gateway` and an issuer delegation calls a service
//! on behalf of a user with an edge-minted token in `aster-on-behalf-of`.
//! Everything goes through the high-level path: `AsterServer` and
//! `AsterClient` configured by `AsterConfig` (`[enrollment]`,
//! `[identity] accept_on_behalf_of`), roles and the delegation from the
//! credentials. The root is played with the low-level API.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::rpc::identity::{Issuer, ISSUABLE_ROLES_ATTRIBUTE, MAX_EDGE_TOKEN_LIFETIME};
use aster::rpc::{
    async_trait, require_role, AsterClient, AsterServer, CallContext, CallOptions, RequestStream,
    ResponseSink, StatusCode, GATEWAY_ROLE, ON_BEHALF_OF_METADATA,
};
use aster::{AsterConfig, Doc, EnrollmentConfig, EnrollmentLimits, Node, PublicKey, RelayMode};
use aster::{Result, SecretKey};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fory_derive::ForyStruct;

const FLEET: &str = "gateway-fleet";

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "gwtest/Num")]
struct Num {
    n: i32,
}

/// What a handler saw in its context.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "gwtest/Who")]
struct Who {
    principal: String,
    peer: String,
    actor: String,
    amr: String,
    roles: String,
    jti: String,
}

fn who(ctx: &CallContext) -> Who {
    Who {
        principal: ctx.principal().into(),
        peer: ctx.peer().into(),
        actor: ctx.actor().unwrap_or_default().into(),
        amr: ctx.auth_methods().join(","),
        roles: ctx.attribute("aster.role").unwrap_or_default().into(),
        jti: ctx
            .on_behalf_of()
            .map(|identity| identity.jti().to_string())
            .unwrap_or_default(),
    }
}

#[aster::service(name = "Data", version = 1)]
trait Data {
    async fn whoami(&self, ctx: &CallContext) -> Result<Who>;

    #[rpc(requires = require_role("data.read"))]
    async fn read(&self, req: Num, ctx: &CallContext) -> Result<Who>;

    #[rpc(requires = require_role("data.admin"))]
    async fn admin(&self, req: Num, ctx: &CallContext) -> Result<Who>;

    #[rpc(server_stream, requires = require_role("data.read"))]
    async fn watch(&self, req: Num, out: ResponseSink<Who>, ctx: &CallContext) -> Result<()>;

    #[rpc(client_stream, requires = require_role("data.read"))]
    async fn sum(&self, reqs: RequestStream<Num>, ctx: &CallContext) -> Result<Who>;

    #[rpc(bidi_stream, requires = require_role("data.read"))]
    async fn chat(
        &self,
        reqs: RequestStream<Num>,
        out: ResponseSink<Who>,
        ctx: &CallContext,
    ) -> Result<()>;
}

struct DataImpl;

#[async_trait]
impl Data for DataImpl {
    async fn whoami(&self, ctx: &CallContext) -> Result<Who> {
        Ok(who(ctx))
    }
    async fn read(&self, _req: Num, ctx: &CallContext) -> Result<Who> {
        Ok(who(ctx))
    }
    async fn admin(&self, _req: Num, ctx: &CallContext) -> Result<Who> {
        Ok(who(ctx))
    }
    async fn watch(&self, _req: Num, out: ResponseSink<Who>, ctx: &CallContext) -> Result<()> {
        out.send(&who(ctx)).await
    }
    async fn sum(&self, mut reqs: RequestStream<Num>, ctx: &CallContext) -> Result<Who> {
        while reqs.recv().await?.is_some() {}
        Ok(who(ctx))
    }
    async fn chat(
        &self,
        mut reqs: RequestStream<Num>,
        out: ResponseSink<Who>,
        ctx: &CallContext,
    ) -> Result<()> {
        while reqs.recv().await?.is_some() {
            out.send(&who(ctx)).await?;
        }
        Ok(())
    }
}

/// A session-scoped running total: each session's instance is its opener's.
#[aster::service(name = "Cart", version = 1)]
trait Cart {
    async fn add(&self, req: Num, ctx: &CallContext) -> Result<Num>;
}

#[derive(Default)]
struct CartImpl {
    total: std::sync::Mutex<i32>,
}

#[async_trait]
impl Cart for CartImpl {
    async fn add(&self, req: Num, _ctx: &CallContext) -> Result<Num> {
        let mut total = self.total.lock().unwrap();
        *total += req.n;
        Ok(Num { n: *total })
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

/// A root-issued credential for `seed` with `roles` in `aster.role` and,
/// when given, an issuer delegation of `issuable` roles.
fn credential(seed: u8, roles: Option<&str>, issuable: Option<&str>) -> String {
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

/// Wait until the node has a direct address to hand out (polled: the node
/// exposes no address watcher).
async fn wait_for_addr(node: &Node) {
    for _ in 0..100 {
        if !node.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The root's fleet namespace (no node serving it is needed: the server
/// imports it read-only and nothing is published in these tests).
struct Root {
    node: Node,
    fleet: Doc,
}

impl Root {
    async fn start() -> Self {
        let node = Node::start(base(1).build()).await.unwrap();
        let fleet = node.docs().create().await.unwrap();
        Self { node, fleet }
    }
}

fn server_config(root: &Root, seed: u8, accept: bool, state: &std::path::Path) -> AsterConfig {
    base(seed)
        .enrollment_credential_chain(credential(seed, None, None))
        .enrollment(EnrollmentConfig {
            audience: Some(FLEET.into()),
            fleet_namespace: Some(hex::encode(namespace(&root.fleet))),
            state_path: Some(state.join(format!("server-{seed}.json"))),
            limits: EnrollmentLimits {
                reconciliation_interval_ms: Some(200),
                ..Default::default()
            },
            ..Default::default()
        })
        .accept_on_behalf_of(accept)
        .build()
}

async fn start_server(root: &Root, seed: u8, accept: bool, state: &std::path::Path) -> AsterServer {
    let server = AsterServer::builder()
        .service(DataServer::new(DataImpl))
        .config(server_config(root, seed, accept, state))
        .start()
        .await
        .unwrap();
    wait_for_addr(server.node()).await;
    server
}

/// An enrolled client (an edge, or any peer) with the given credential.
async fn connect(root: &Root, server: &AsterServer, seed: u8, chain: String) -> AsterClient {
    let config = base(seed)
        .enrollment_credential_chain(chain)
        .enrollment(EnrollmentConfig {
            audience: Some(FLEET.into()),
            fleet_namespace: Some(hex::encode(namespace(&root.fleet))),
            ..Default::default()
        })
        .build();
    AsterClient::builder()
        .config(config)
        .address(server.address().unwrap())
        .connect()
        .await
        .unwrap()
}

fn api(client: &AsterClient, token: Option<&str>) -> DataClient {
    let options = CallOptions::new().deadline(Duration::from_secs(20));
    let options = match token {
        Some(token) => options.on_behalf_of(token),
        None => options,
    };
    DataClient::new(client.rpc()).with_options(options)
}

fn code<T: std::fmt::Debug>(result: Result<T>) -> Option<StatusCode> {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(error) => error.rpc_code(),
    }
}

/// Sign an arbitrary edge-token payload with `seed`, bypassing the issuer's
/// checks, as a hostile or buggy edge could.
fn forge(seed: u8, payload: serde_json::Value) -> String {
    use ed25519_dalek::Signer;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"aster-on-behalf-of-v1+jwt"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    let input = format!("{header}.{body}");
    let signature = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

fn claims(iss: u8, iat: u64, exp: u64) -> serde_json::Value {
    serde_json::json!({
        "iss": id(iss),
        "sub": "mallory",
        "roles": ["data.read"],
        "amr": ["pwd"],
        "iat": iat,
        "exp": exp,
        "jti": "forged-token-0000001",
    })
}

fn jti_of(token: &str) -> String {
    let payload = token.split('.').nth(1).unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    json["jti"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn forwarded_identity_reaches_handlers_and_every_refusal_fails_closed() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(&root, 2, true, files.path()).await;

        // The edge: a gateway that may issue data.read and data.write. Its
        // own roles include data.admin, which forwarded calls must not get.
        let edge = connect(
            &root,
            &server,
            5,
            credential(
                5,
                Some(&format!("{GATEWAY_ROLE},data.admin")),
                Some("data.read,data.write"),
            ),
        )
        .await;
        let edge_id = edge.id().as_str().to_string();
        assert_eq!(edge_id, id(5));

        // No entry: the edge calls as itself, unchanged.
        let me = api(&edge, None).whoami().await.unwrap();
        assert_eq!(me.principal, edge_id);
        assert_eq!(me.peer, edge_id);
        assert_eq!(me.actor, "");
        assert!(me.roles.contains("data.admin"));
        assert_eq!(
            api(&edge, None).admin(Num { n: 1 }).await.unwrap().actor,
            ""
        );
        assert_eq!(
            code(api(&edge, None).read(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied),
            "the edge itself lacks data.read"
        );

        // The edge mints for alice, who signed in with a passkey.
        let issuer = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap()).unwrap();
        assert_eq!(issuer.issuer_id(), edge_id);
        let token = issuer
            .mint_user_token(
                "alice",
                ["data.read"],
                ["webauthn"],
                Duration::from_secs(120),
            )
            .unwrap();
        let expect = Who {
            principal: format!("{edge_id}|alice"),
            peer: edge_id.clone(),
            actor: edge_id.clone(),
            amr: "webauthn".into(),
            roles: "data.read".into(),
            jti: jti_of(&token),
        };
        let as_alice = api(&edge, Some(&token));
        assert_eq!(as_alice.read(Num { n: 1 }).await.unwrap(), expect, "unary");
        assert_eq!(as_alice.whoami().await.unwrap(), expect, "no-request unary");
        let mut stream = as_alice.watch(Num { n: 1 }).await.unwrap();
        assert_eq!(
            stream.recv().await.unwrap().unwrap(),
            expect,
            "server stream"
        );
        assert_eq!(
            as_alice
                .sum(vec![Num { n: 1 }, Num { n: 2 }])
                .await
                .unwrap(),
            expect,
            "client stream"
        );
        let mut stream = as_alice.chat(vec![Num { n: 1 }]).await.unwrap();
        assert_eq!(stream.recv().await.unwrap().unwrap(), expect, "bidi");

        // `requires` checks the token's roles, not the edge's.
        assert_eq!(
            code(as_alice.admin(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied),
            "token lacks data.admin though the edge has it"
        );
        let writer = issuer
            .mint_user_token("bob", ["data.write"], ["apikey"], Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            code(api(&edge, Some(&writer)).read(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied),
            "data.write does not satisfy data.read"
        );
        assert_eq!(
            code(api(&edge, Some(&writer)).sum(vec![Num { n: 1 }]).await),
            Some(StatusCode::PermissionDenied),
            "client stream"
        );

        // The issuer helper refuses what the core would.
        assert!(issuer
            .mint_user_token("alice", ["data.admin"], ["pwd"], Duration::from_secs(60))
            .is_err());
        assert!(issuer
            .mint_user_token(
                "alice",
                ["data.read"],
                ["pwd"],
                MAX_EDGE_TOKEN_LIFETIME + Duration::from_secs(1)
            )
            .is_err());

        // A token claiming a role outside the edge's delegation: an issuer
        // configured more broadly than the credential allows.
        let broad = Issuer::new(key(5), ["data.read", "data.admin"]);
        let admin = broad
            .mint_user_token("alice", ["data.admin"], ["pwd"], Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            code(api(&edge, Some(&admin)).admin(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied)
        );

        // Expired, not yet valid, and over the 5-minute lifetime.
        let t = now();
        for (what, token) in [
            ("expired", forge(5, claims(5, t - 200, t - 100))),
            ("not yet valid", forge(5, claims(5, t + 120, t + 240))),
            ("over lifetime", forge(5, claims(5, t, t + 301))),
        ] {
            assert_eq!(
                code(api(&edge, Some(&token)).read(Num { n: 1 }).await),
                Some(StatusCode::Unauthenticated),
                "{what}"
            );
        }
        // A well-formed forged token is accepted: the signature is the edge's.
        api(&edge, Some(&forge(5, claims(5, t, t + 60))))
            .read(Num { n: 1 })
            .await
            .unwrap();

        // Signed by a node other than the transport peer: one that names
        // itself, and one that names the edge.
        let stranger = Issuer::new(key(9), ["data.read"])
            .mint_user_token("alice", ["data.read"], ["pwd"], Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            code(api(&edge, Some(&stranger)).read(Num { n: 1 }).await),
            Some(StatusCode::Unauthenticated),
            "iss is another node"
        );
        let impostor = forge(9, claims(5, t, t + 60));
        assert_eq!(
            code(api(&edge, Some(&impostor)).read(Num { n: 1 }).await),
            Some(StatusCode::Unauthenticated),
            "iss is the edge, signature is not"
        );

        // The entry twice.
        let twice = DataClient::new(edge.rpc()).with_options(
            CallOptions::new()
                .metadata(ON_BEHALF_OF_METADATA, token.clone())
                .metadata("Aster-On-Behalf-Of", token.clone()),
        );
        assert_eq!(
            code(twice.read(Num { n: 1 }).await),
            Some(StatusCode::Unauthenticated)
        );

        // A peer without aster.gateway (it does hold a delegation).
        let plain = connect(
            &root,
            &server,
            6,
            credential(6, Some("data.read"), Some("data.read")),
        )
        .await;
        let own = Issuer::from_enrollment(plain.node(), plain.enrollment().unwrap())
            .unwrap()
            .mint_user_token("alice", ["data.read"], ["pwd"], Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            code(api(&plain, Some(&own)).read(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied)
        );
        // Unchanged as itself.
        assert_eq!(
            api(&plain, None).read(Num { n: 1 }).await.unwrap().actor,
            ""
        );

        // A gateway with no issuer delegation cannot mint at all.
        let undelegated = connect(&root, &server, 7, credential(7, Some(GATEWAY_ROLE), None)).await;
        let unbounded = Issuer::new(key(7), ["data.read"])
            .mint_user_token("alice", ["data.read"], ["pwd"], Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            code(api(&undelegated, Some(&unbounded)).read(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied)
        );

        // Another gateway replaying the edge's token.
        let other = connect(
            &root,
            &server,
            8,
            credential(8, Some(GATEWAY_ROLE), Some("data.read")),
        )
        .await;
        assert_eq!(
            code(api(&other, Some(&token)).read(Num { n: 1 }).await),
            Some(StatusCode::Unauthenticated)
        );
        // Its own token works.
        let mine = Issuer::from_enrollment(other.node(), other.enrollment().unwrap())
            .unwrap()
            .mint_user_token("carol", ["data.read"], ["pwd"], Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            api(&other, Some(&mine))
                .read(Num { n: 1 })
                .await
                .unwrap()
                .principal,
            format!("{}|carol", id(8))
        );

        for client in [edge, plain, undelegated, other] {
            client.close().await;
        }
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

/// GI-1: an edge multiplexes users over one connection, so a session it
/// opens for one user must not be reachable as another user, or as the edge.
#[tokio::test]
async fn a_session_opened_for_one_user_refuses_every_other_principal() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = AsterServer::builder()
            .service(DataServer::new(DataImpl))
            .register_session(|| CartServer::new(CartImpl::default()))
            .config(server_config(&root, 11, true, files.path()))
            .start()
            .await
            .unwrap();
        wait_for_addr(server.node()).await;
        let edge = connect(
            &root,
            &server,
            5,
            credential(5, Some(GATEWAY_ROLE), Some("data.read")),
        )
        .await;
        let issuer = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap()).unwrap();
        let mint = |user: &str| {
            issuer
                .mint_user_token(user, ["data.read"], ["pwd"], Duration::from_secs(120))
                .unwrap()
        };
        let (alice, bob) = (mint("alice"), mint("bob"));
        let cart = |session: &aster::rpc::RpcSession, token: Option<&str>| {
            let options = session.options().deadline(Duration::from_secs(20));
            let options = match token {
                Some(token) => options.on_behalf_of(token),
                None => options,
            };
            CartClient::new(edge.rpc()).with_options(options)
        };

        let session = edge.rpc().open_session().unwrap();
        assert_eq!(
            cart(&session, Some(&alice))
                .add(Num { n: 2 })
                .await
                .unwrap(),
            Num { n: 2 }
        );
        assert_eq!(
            code(cart(&session, Some(&bob)).add(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied),
            "another user on the same connection"
        );
        assert_eq!(
            code(cart(&session, None).add(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied),
            "the edge as itself"
        );
        assert_eq!(
            cart(&session, Some(&alice))
                .add(Num { n: 3 })
                .await
                .unwrap(),
            Num { n: 5 },
            "the opener continues, untouched by the refused calls"
        );

        // Bob's own session, and the edge's, are their own.
        let other = edge.rpc().open_session().unwrap();
        assert_eq!(
            cart(&other, Some(&bob)).add(Num { n: 1 }).await.unwrap(),
            Num { n: 1 }
        );
        let own = edge.rpc().open_session().unwrap();
        assert_eq!(
            cart(&own, None).add(Num { n: 7 }).await.unwrap(),
            Num { n: 7 }
        );
        assert_eq!(
            code(cart(&own, Some(&alice)).add(Num { n: 1 }).await),
            Some(StatusCode::PermissionDenied),
            "a user on the edge's own session"
        );

        edge.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

/// GI-4: framework (`aster.`) roles are never issuable, as with `role_map`:
/// not minted, and refused on verification even when the delegation lists one.
#[tokio::test]
async fn framework_roles_are_never_issuable() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(&root, 12, true, files.path()).await;
        let edge = connect(
            &root,
            &server,
            5,
            credential(
                5,
                Some(GATEWAY_ROLE),
                Some(&format!("data.read,{GATEWAY_ROLE},Aster.Config-Admin")),
            ),
        )
        .await;
        let issuer = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap()).unwrap();
        assert_eq!(
            issuer.issuable_roles(),
            BTreeSet::from(["data.read".to_string()])
        );
        for role in [GATEWAY_ROLE, "Aster.Config-Admin"] {
            assert!(
                issuer
                    .mint_user_token("alice", [role], ["pwd"], Duration::from_secs(60))
                    .is_err(),
                "{role}"
            );
            assert!(
                Issuer::new(key(5), [role])
                    .mint_user_token("alice", [role], ["pwd"], Duration::from_secs(60))
                    .is_err(),
                "{role}, fixed issuer"
            );
        }
        // A token the edge signed itself, claiming a role its delegation lists.
        let t = now();
        for role in [GATEWAY_ROLE, "Aster.Config-Admin"] {
            let mut payload = claims(5, t, t + 60);
            payload["roles"] = serde_json::json!([role]);
            assert_eq!(
                code(api(&edge, Some(&forge(5, payload))).whoami().await),
                Some(StatusCode::PermissionDenied),
                "{role}"
            );
        }
        // An ordinary role still works.
        api(&edge, Some(&forge(5, claims(5, t, t + 60))))
            .read(Num { n: 1 })
            .await
            .unwrap();
        edge.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn a_server_that_does_not_accept_on_behalf_of_refuses_the_entry() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(&root, 3, false, files.path()).await;
        let edge = connect(
            &root,
            &server,
            5,
            credential(5, Some(GATEWAY_ROLE), Some("data.read")),
        )
        .await;
        let token = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap())
            .unwrap()
            .mint_user_token("alice", ["data.read"], ["pwd"], Duration::from_secs(60))
            .unwrap();
        // Refused, not run as the edge.
        assert_eq!(
            code(api(&edge, Some(&token)).whoami().await),
            Some(StatusCode::PermissionDenied)
        );
        assert_eq!(api(&edge, None).whoami().await.unwrap().principal, id(5));
        edge.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

/// A foreign (binding-implemented) handler sees the same identity: the C ABI
/// reads `CallRef::actor` for `aster_dispatch_identity`'s actor slot.
#[cfg(feature = "binding-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_handlers_see_the_forwarded_identity() {
    use aster::binding_support::dispatch::{DispatchEvent, RingSink};
    use aster::rpc::{DynamicService, MethodDesc, MethodPattern, RpcStatus, StreamHeader, TypeRef};

    tokio::time::timeout(Duration::from_secs(60), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let (sink, mut consumer) = RingSink::new_with_limit(1, 4);
        let mut svc = DynamicService::new("test.Who", 1);
        let mut who = MethodDesc::new(
            "who",
            MethodPattern::Unary,
            TypeRef::new("a1".repeat(32)),
            TypeRef::new("b2".repeat(32)),
        );
        who.requires = Some(require_role("data.read"));
        svc.methods = vec![who];
        let server = AsterServer::builder()
            .config(server_config(&root, 4, true, files.path()))
            .dispatch_sink(sink)
            .dynamic_service(svc)
            .unwrap()
            .start()
            .await
            .unwrap();
        wait_for_addr(server.node()).await;
        let table = server.call_table();
        let edge = connect(
            &root,
            &server,
            5,
            credential(5, Some(GATEWAY_ROLE), Some("data.read")),
        )
        .await;
        let token = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap())
            .unwrap()
            .mint_user_token(
                "alice",
                ["data.read"],
                ["webauthn"],
                Duration::from_secs(60),
            )
            .unwrap();
        let rpc = edge.rpc();
        let pending = tokio::spawn(async move {
            let header = StreamHeader {
                service: "test.Who".into(),
                method: "who".into(),
                version: 1,
                ..Default::default()
            };
            rpc.unary_with(&header, vec![1], &CallOptions::new().on_behalf_of(token))
                .await
        });
        consumer.ready().await;
        let mut seen = None;
        consumer.drain(1, |event| match event {
            DispatchEvent::Call(call) => {
                let call_id = call.handle.detach();
                let entry = table.get(call_id).unwrap();
                seen = Some((
                    call.peer_id.to_string(),
                    call.transport_peer_id.as_deref().map(str::to_string),
                    entry.actor().map(str::to_string),
                    call.attributes.get("aster.role").map(str::to_string),
                ));
                table.try_respond(call_id, b"ok", &RpcStatus::ok()).unwrap();
            }
            other => panic!("expected a call, got {other:?}"),
        });
        assert_eq!(pending.await.unwrap().unwrap(), b"ok");
        let edge_id = id(5);
        assert_eq!(
            seen,
            Some((
                format!("{edge_id}|alice"),
                Some(edge_id.clone()),
                Some(edge_id),
                Some("data.read".to_string()),
            ))
        );
        edge.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}
