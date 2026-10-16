#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! `aster.ops.NodeConfig` (gateway identity, slice 6;
//! `docs/_internal/web_gateway/gateway-identity.md`, "Remote overrides").
//! Everything goes through the high-level path: `AsterServer` and
//! `AsterClient` configured by `AsterConfig`, roles from Enrollment
//! credentials. The root is played with the low-level API.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::rpc::identity::{Issuer, ISSUABLE_ROLES_ATTRIBUTE};
use aster::rpc::node_config::{
    IdentityOverride, NodeConfigAck, NodeConfigAckRequest, NodeConfigClearRequest,
    NodeConfigSetRequest, NodeConfigView, SectionOverride,
};
use aster::rpc::{
    async_trait, require_role, AsterClient, AsterServer, AttrValue, AttributeStore, CallContext,
    CallOptions, NodeConfigClient, StatusCode, CONFIG_ADMIN_ROLE, GATEWAY_ROLE,
};
use aster::{AsterConfig, Doc, EnrollmentConfig, EnrollmentLimits, Node, PublicKey, RelayMode};
use aster::{Result, SecretKey, SETTING_ACCEPT_ON_BEHALF_OF};
use fory_derive::ForyStruct;

const FLEET: &str = "node-config-fleet";

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "nctest/Seen")]
struct Seen {
    principal: String,
    actor: String,
    node_name: String,
    tags: String,
    region: String,
    tier: i64,
}

fn seen(ctx: &CallContext) -> Seen {
    let node = ctx.node_properties();
    Seen {
        principal: ctx.principal().into(),
        actor: ctx.actor().unwrap_or_default().into(),
        node_name: node.name().into(),
        tags: node.tags().join(","),
        region: ctx
            .node_attribute("region")
            .and_then(AttrValue::as_str)
            .unwrap_or_default()
            .into(),
        tier: ctx
            .node_attribute("tier")
            .and_then(AttrValue::as_int)
            .unwrap_or_default(),
    }
}

#[aster::service(name = "NcData", version = 1)]
trait NcData {
    async fn whoami(&self, ctx: &CallContext) -> Result<Seen>;

    #[rpc(requires = require_role("data.admin"))]
    async fn admin(&self, ctx: &CallContext) -> Result<Seen>;
}

struct NcDataImpl;

#[async_trait]
impl NcData for NcDataImpl {
    async fn whoami(&self, ctx: &CallContext) -> Result<Seen> {
        Ok(seen(ctx))
    }
    async fn admin(&self, ctx: &CallContext) -> Result<Seen> {
        Ok(seen(ctx))
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

const ADMIN: u8 = 4;
const EDGE: u8 = 5;
const PLAIN: u8 = 6;

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

/// Wait until the node has a direct address to hand out: a condition wait on
/// the endpoint's address watcher.
async fn wait_for_addr(node: &Node) {
    use aster::native::iroh::Watcher;
    let mut addrs = node.native_endpoint().watch_addr();
    while node.addr().direct_addresses.is_empty() {
        addrs.updated().await.expect("endpoint closed");
    }
}

/// The root's fleet namespace (the server imports it read-only; nothing is
/// published in these tests).
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

/// An enrolled server. `config` adjusts the builder (file, locks, storage).
async fn start_server(
    root: &Root,
    seed: u8,
    state: &std::path::Path,
    config: impl FnOnce(aster::AsterConfigBuilder) -> aster::AsterConfigBuilder,
) -> AsterServer {
    let builder = config(base(seed))
        .enrollment_credential_chain(credential(seed, None, None))
        .enrollment(root.enrollment(Some(state.join(format!("server-{seed}.json")))));
    let server = AsterServer::builder()
        .service(NcDataServer::new(NcDataImpl))
        .config(builder.build())
        .start()
        .await
        .unwrap();
    wait_for_addr(server.node()).await;
    server
}

async fn connect(root: &Root, server: &AsterServer, seed: u8, chain: String) -> AsterClient {
    let config = base(seed)
        .enrollment_credential_chain(chain)
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

fn admin_api(client: &AsterClient) -> NodeConfigClient {
    NodeConfigClient::new(client.rpc()).with_options(options())
}

fn data(client: &AsterClient, token: Option<&str>) -> NcDataClient {
    let options = match token {
        Some(token) => options().on_behalf_of(token),
        None => options(),
    };
    NcDataClient::new(client.rpc()).with_options(options)
}

fn code<T: std::fmt::Debug>(result: Result<T>) -> Option<StatusCode> {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(error) => error.rpc_code(),
    }
}

fn accept(on: bool) -> NodeConfigSetRequest {
    NodeConfigSetRequest::new().identity(IdentityOverride {
        accept_on_behalf_of: Some(on),
        ..Default::default()
    })
}

fn source(view: &NodeConfigView) -> (bool, String) {
    let setting = view.setting(SETTING_ACCEPT_ON_BEHALF_OF).unwrap();
    (
        setting.value.as_ref().and_then(|v| v.as_bool()).unwrap(),
        setting.source.clone(),
    )
}

fn assert_complete(ack: &NodeConfigAck, generation: i64) {
    assert!(ack.complete, "not complete: {ack:?}");
    assert_eq!(ack.generation, generation);
    assert_eq!(ack.applied_generation, generation);
    assert!(ack.outstanding.is_empty());
}

/// The edge, and a token it minted for alice.
async fn edge_and_token(root: &Root, server: &AsterServer) -> (AsterClient, String) {
    let edge = connect(
        root,
        server,
        EDGE,
        credential(EDGE, Some(GATEWAY_ROLE), Some("data.read")),
    )
    .await;
    let token = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap())
        .unwrap()
        .mint_user_token("alice", ["data.read"], ["pwd"], Duration::from_secs(120))
        .unwrap();
    (edge, token)
}

#[tokio::test]
async fn set_answers_once_the_identity_setting_is_live_and_clear_reverts_to_the_file() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let toml = files.path().join("aster.toml");
        std::fs::write(&toml, "[identity]\naccept_on_behalf_of = false\n").unwrap();
        let server = start_server(&root, 2, files.path(), |b| b.load_file(&toml).unwrap()).await;
        let admin = connect(
            &root,
            &server,
            ADMIN,
            credential(ADMIN, Some(CONFIG_ADMIN_ROLE), None),
        )
        .await;
        let (edge, token) = edge_and_token(&root, &server).await;

        // The file says no: the forwarded call is refused.
        let before = admin_api(&admin).get().await.unwrap();
        assert_eq!(source(&before), (false, "file".into()));
        assert_eq!(before.generation, 0);
        assert_eq!(
            code(data(&edge, Some(&token)).whoami().await),
            Some(StatusCode::PermissionDenied)
        );

        // `set` answers complete, so the very next call sees the override.
        let ack = admin_api(&admin).set(accept(true)).await.unwrap();
        assert_complete(&ack, 1);
        let alice = data(&edge, Some(&token)).whoami().await.unwrap();
        assert_eq!(alice.principal, format!("{}|alice", id(EDGE)));
        assert_eq!(alice.actor, id(EDGE));
        assert!(server.node_config().unwrap().identity().accept_on_behalf_of);

        // `ack` agrees, reads the store back, and does not answer complete for
        // a generation that is not there.
        let acked = admin_api(&admin)
            .ack(NodeConfigAckRequest {
                generation: 1,
                timeout_ms: 0,
            })
            .await
            .unwrap();
        assert_complete(&acked, 1);
        let ahead = admin_api(&admin)
            .ack(NodeConfigAckRequest {
                generation: 2,
                timeout_ms: 50,
            })
            .await
            .unwrap();
        assert!(!ahead.complete);
        assert_eq!(ahead.outstanding[0].state, "pending");

        let view = admin_api(&admin).get().await.unwrap();
        assert_eq!(source(&view), (true, "override".into()));
        assert_eq!(view.change, "set");
        assert_eq!(view.changed_by, id(ADMIN));
        assert!(view.changed_at_ms > 0);

        // A stale expected generation is refused; the right one is accepted.
        assert_eq!(
            code(
                admin_api(&admin)
                    .set(accept(false).expected_generation(7))
                    .await
            ),
            Some(StatusCode::Aborted)
        );

        // Clearing reverts to the file, and the call is refused again.
        let cleared = admin_api(&admin)
            .clear(NodeConfigClearRequest {
                expected_generation: 1,
            })
            .await
            .unwrap();
        assert_complete(&cleared, 2);
        assert_eq!(
            code(data(&edge, Some(&token)).whoami().await),
            Some(StatusCode::PermissionDenied)
        );
        let view = admin_api(&admin).get().await.unwrap();
        assert_eq!(source(&view), (false, "file".into()));
        assert_eq!((view.generation, view.change.as_str()), (2, "clear"));
        // Generation 1 was superseded by the clear.
        let old = admin_api(&admin)
            .ack(NodeConfigAckRequest {
                generation: 1,
                timeout_ms: 0,
            })
            .await
            .unwrap();
        assert!(!old.complete);
        assert_eq!(old.outstanding[0].state, "superseded");

        edge.close().await;
        admin.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn only_an_enrolled_config_admin_may_change_it() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(&root, 2, files.path(), |b| b).await;

        // Gate 3: no aster.config-admin.
        let plain = connect(
            &root,
            &server,
            PLAIN,
            credential(PLAIN, Some("data.read"), None),
        )
        .await;
        assert_eq!(
            code(admin_api(&plain).set(accept(true)).await),
            Some(StatusCode::PermissionDenied)
        );
        assert_eq!(
            code(admin_api(&plain).get().await),
            Some(StatusCode::PermissionDenied)
        );

        // An edge cannot forward a user's admin role either: the token's
        // roles are what the edge may issue, and changes refuse forwarded
        // calls outright.
        let edge = connect(
            &root,
            &server,
            EDGE,
            credential(EDGE, Some(GATEWAY_ROLE), Some(CONFIG_ADMIN_ROLE)),
        )
        .await;
        let token = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap())
            .unwrap()
            .mint_user_token(
                "mallory",
                [CONFIG_ADMIN_ROLE],
                ["pwd"],
                Duration::from_secs(60),
            )
            .unwrap();
        let forwarded =
            NodeConfigClient::new(edge.rpc()).with_options(options().on_behalf_of(&token));
        // This server does not accept the entry at all...
        assert_eq!(
            code(forwarded.set(accept(true)).await),
            Some(StatusCode::PermissionDenied)
        );
        assert_eq!(server.node_config().unwrap().generation(), 0);

        edge.close().await;
        plain.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn a_forwarded_admin_role_cannot_change_it_even_when_the_entry_is_accepted() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(&root, 2, files.path(), |b| b.accept_on_behalf_of(true)).await;
        let edge = connect(
            &root,
            &server,
            EDGE,
            credential(EDGE, Some(GATEWAY_ROLE), Some(CONFIG_ADMIN_ROLE)),
        )
        .await;
        let token = Issuer::from_enrollment(edge.node(), edge.enrollment().unwrap())
            .unwrap()
            .mint_user_token(
                "mallory",
                [CONFIG_ADMIN_ROLE],
                ["pwd"],
                Duration::from_secs(60),
            )
            .unwrap();
        let forwarded =
            NodeConfigClient::new(edge.rpc()).with_options(options().on_behalf_of(&token));
        // Gate 3 passes on the token's role; the change is still refused.
        assert!(forwarded.get().await.is_ok());
        assert_eq!(
            code(forwarded.set(accept(false)).await),
            Some(StatusCode::PermissionDenied)
        );
        assert_eq!(server.node_config().unwrap().generation(), 0);
        edge.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn a_node_without_enrollment_refuses_every_change() {
    tokio::time::timeout(Duration::from_secs(60), async {
        // No Enrollment; the application grants the role through the
        // attribute store, which is not a credential.
        let attributes = AttributeStore::new();
        attributes.set_role(id(ADMIN), CONFIG_ADMIN_ROLE).unwrap();
        let server = AsterServer::builder()
            .service(NcDataServer::new(NcDataImpl))
            .config(base(2).build())
            .attributes(attributes)
            .start()
            .await
            .unwrap();
        wait_for_addr(server.node()).await;
        let client = AsterClient::builder()
            .config(base(ADMIN).build())
            .address(server.address().unwrap())
            .connect()
            .await
            .unwrap();
        let api = admin_api(&client);
        assert_eq!(source(&api.get().await.unwrap()), (false, "default".into()));
        assert_eq!(
            code(api.set(accept(true)).await),
            Some(StatusCode::PermissionDenied)
        );
        assert_eq!(
            code(api.clear(NodeConfigClearRequest::default()).await),
            Some(StatusCode::PermissionDenied)
        );
        assert_eq!(server.node_config().unwrap().generation(), 0);
        client.close().await;
        server.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn hardening_refuses_locked_unlisted_invalid_and_reserved_changes_without_storing() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let open = start_server(&root, 2, files.path(), |b| b).await;
        let locked = start_server(&root, 3, files.path(), |b| {
            b.remote_config_lock(["identity"])
        })
        .await;
        let admin_chain = credential(ADMIN, Some(CONFIG_ADMIN_ROLE), None);
        let admin = connect(&root, &open, ADMIN, admin_chain.clone()).await;
        let api = admin_api(&admin);

        // One good generation to compare against.
        let base = NodeConfigSetRequest::new()
            .name("n-1")
            .attribute("region", "eu");
        assert_complete(&api.set(base).await.unwrap(), 1);

        let section = |name: &str, json: &str| SectionOverride {
            section: name.into(),
            settings_json: json.into(),
        };
        let refusals = [
            // Never overridable: no section declares these.
            (
                NodeConfigSetRequest::new().section(section("network", r#"{"bind_port": 1}"#)),
                StatusCode::PermissionDenied,
            ),
            (
                NodeConfigSetRequest::new()
                    .section(section("hooks", r#"{"hook_failure_mode": "fail_open"}"#)),
                StatusCode::PermissionDenied,
            ),
            (
                NodeConfigSetRequest::new()
                    .section(section("remote_config", r#"{"enabled": false}"#)),
                StatusCode::PermissionDenied,
            ),
            // A real `[identity]` key that is not overridable.
            (
                NodeConfigSetRequest::new().section(section("identity", r#"{"file": "/tmp/x"}"#)),
                StatusCode::PermissionDenied,
            ),
            // Invalid as a whole: a good field next to a bad one is refused too.
            (
                NodeConfigSetRequest::new()
                    .name("n-2")
                    .section(section("identity", r#"{"accept_on_behalf_of": "yes"}"#)),
                StatusCode::InvalidArgument,
            ),
            (
                NodeConfigSetRequest::new().section(section("identity", "not json")),
                StatusCode::InvalidArgument,
            ),
            (
                NodeConfigSetRequest::new().section(section("identity", "{}")),
                StatusCode::InvalidArgument,
            ),
            // Reserved attribute keys, any case.
            (
                NodeConfigSetRequest::new().attribute("aster.role", CONFIG_ADMIN_ROLE),
                StatusCode::PermissionDenied,
            ),
            (
                NodeConfigSetRequest::new().attribute("Aster.Gateway", true),
                StatusCode::PermissionDenied,
            ),
        ];
        for (request, expected) in refusals {
            assert_eq!(
                code(api.set(request.clone()).await),
                Some(expected),
                "{request:?}"
            );
        }
        // Nothing was stored or applied.
        let view = api.get().await.unwrap();
        assert_eq!((view.generation, view.applied_generation), (1, 1));
        assert_eq!(view.name, "n-1");
        assert_eq!(source(&view), (false, "default".into()));
        let after = api
            .ack(NodeConfigAckRequest {
                generation: 1,
                timeout_ms: 0,
            })
            .await
            .unwrap();
        assert_complete(&after, 1);

        // A locked section is refused; properties are still accepted there.
        let locked_admin = connect(&root, &locked, ADMIN, admin_chain).await;
        let locked_api = admin_api(&locked_admin);
        assert_eq!(
            code(locked_api.set(accept(true)).await),
            Some(StatusCode::FailedPrecondition)
        );
        assert_complete(
            &locked_api
                .set(NodeConfigSetRequest::new().tag("edge"))
                .await
                .unwrap(),
            1,
        );
        let view = locked_api.get().await.unwrap();
        assert_eq!(view.locked_sections, vec!["identity".to_string()]);
        assert_eq!(source(&view), (false, "default".into()));

        locked_admin.close().await;
        admin.close().await;
        locked.shutdown().await;
        open.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn attributes_reach_handlers_but_never_authorise() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(&root, 2, files.path(), |b| b).await;
        let admin = connect(
            &root,
            &server,
            ADMIN,
            credential(ADMIN, Some(CONFIG_ADMIN_ROLE), None),
        )
        .await;
        let request = NodeConfigSetRequest::new()
            .name("eu-gateway-1")
            .tag("gateway")
            .tag("eu")
            .tag("eu")
            .attribute("region", "eu-west")
            .attribute("tier", 3i64)
            // Shaped like authorisation, and still only data.
            .attribute("role", "data.admin")
            .attribute("data.admin", true);
        assert_complete(&admin_api(&admin).set(request).await.unwrap(), 1);

        let me = data(&admin, None).whoami().await.unwrap();
        assert_eq!(me.node_name, "eu-gateway-1");
        assert_eq!(me.tags, "eu,gateway");
        assert_eq!(me.region, "eu-west");
        assert_eq!(me.tier, 3);
        // `requires(data.admin)` reads the caller's credential, not the node's
        // attributes.
        assert_eq!(
            code(data(&admin, None).admin().await),
            Some(StatusCode::PermissionDenied)
        );
        let view = admin_api(&admin).get().await.unwrap();
        assert_eq!(view.typed_attributes()["tier"], AttrValue::Int(3));
        assert_eq!(
            server
                .node_config()
                .unwrap()
                .properties()
                .attribute("region"),
            Some(&AttrValue::from("eu-west"))
        );

        admin.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn the_record_survives_restart_and_a_lock_added_later_wins() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let storage = files.path().join("storage");
        let persistent = |b: aster::AsterConfigBuilder| b.persistent(&storage);
        let admin_chain = credential(ADMIN, Some(CONFIG_ADMIN_ROLE), None);
        // Each run gets its own Enrollment state file: in-process, the
        // previous instance releases that file's lock asynchronously, and
        // the state under test is the docs store, which is kept.
        let run = |n: u32| files.path().join(format!("run-{n}"));

        let server = start_server(&root, 2, &run(1), persistent).await;
        let admin = connect(&root, &server, ADMIN, admin_chain.clone()).await;
        let request = accept(true).name("kept").attribute("region", "ap");
        assert_complete(&admin_api(&admin).set(request).await.unwrap(), 1);
        admin.close().await;
        server.shutdown().await;

        // Restarted: the override is in force before anything is served.
        let server = start_server(&root, 2, &run(2), persistent).await;
        assert!(server.node_config().unwrap().identity().accept_on_behalf_of);
        let (edge, token) = edge_and_token(&root, &server).await;
        let alice = data(&edge, Some(&token)).whoami().await.unwrap();
        assert_eq!(alice.actor, id(EDGE));
        assert_eq!(
            (alice.node_name.as_str(), alice.region.as_str()),
            ("kept", "ap")
        );
        let admin = connect(&root, &server, ADMIN, admin_chain.clone()).await;
        let view = admin_api(&admin).get().await.unwrap();
        assert_eq!((view.generation, view.applied_generation), (1, 1));
        assert_eq!(source(&view), (true, "override".into()));
        assert_eq!(view.changed_by, id(ADMIN));
        edge.close().await;
        admin.close().await;
        server.shutdown().await;

        // The local file locks [identity] now: the stored override is
        // ignored (and reported), the properties still apply.
        let server = start_server(&root, 2, &run(3), |b| {
            persistent(b).remote_config_lock(["identity"])
        })
        .await;
        let handle = server.node_config().unwrap();
        assert!(!handle.identity().accept_on_behalf_of);
        let view = handle.view();
        assert_eq!(view.ignored_sections, vec!["identity".to_string()]);
        assert_eq!(source(&view), (false, "default".into()));
        assert_eq!(view.name, "kept");
        server.shutdown().await;

        // And with the service off, nothing of the record applies.
        let server = start_server(&root, 2, &run(4), |b| persistent(b).remote_config(false)).await;
        assert!(server.node_config().is_none());
        let admin = connect(&root, &server, ADMIN, admin_chain).await;
        assert!(admin_api(&admin).get().await.is_err());
        assert_eq!(data(&admin, None).whoami().await.unwrap().node_name, "");
        admin.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn an_unknown_lock_fails_the_start() {
    let error = AsterServer::builder()
        .service(NcDataServer::new(NcDataImpl))
        .config(base(2).remote_config_lock(["identty"]).build())
        .start()
        .await
        .err()
        .expect("an unknown lock must fail the start");
    assert!(error.to_string().contains("identty"), "{error}");
}

#[cfg(feature = "audit-outbox")]
#[tokio::test]
async fn every_set_and_clear_reaches_the_audit_outbox_before_it_is_stored() {
    use aster::audit_outbox::{AuditOutbox, Destination, OutboxConfig};
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        // Nothing listens there: events are committed locally and stay queued.
        let outbox = AuditOutbox::open(OutboxConfig::new(
            files.path().join("outbox"),
            Destination {
                endpoint: "http://127.0.0.1:9/audit".into(),
                receiver_identity: None,
                authorization: None,
            },
        ))
        .await
        .unwrap();
        let builder = base(2)
            .enrollment_credential_chain(credential(2, None, None))
            .enrollment(root.enrollment(Some(files.path().join("server.json"))));
        let server = AsterServer::builder()
            .service(NcDataServer::new(NcDataImpl))
            .config(builder.build())
            .audit_outbox(outbox.clone())
            .start()
            .await
            .unwrap();
        wait_for_addr(server.node()).await;
        let admin = connect(
            &root,
            &server,
            ADMIN,
            credential(ADMIN, Some(CONFIG_ADMIN_ROLE), None),
        )
        .await;
        let before = outbox.status().await.unwrap().committed_sequence;
        assert_complete(&admin_api(&admin).set(accept(true)).await.unwrap(), 1);
        assert_complete(
            &admin_api(&admin)
                .clear(NodeConfigClearRequest::default())
                .await
                .unwrap(),
            2,
        );
        // A refused change is not audited.
        assert!(admin_api(&admin)
            .set(NodeConfigSetRequest::new().attribute("aster.x", 1i64))
            .await
            .is_err());
        assert_eq!(
            outbox.status().await.unwrap().committed_sequence,
            before + 2
        );

        // Once the outbox is closed, a change is refused and nothing changes.
        outbox.close().await.unwrap();
        assert_eq!(
            code(admin_api(&admin).set(accept(true)).await),
            Some(StatusCode::Unavailable)
        );
        assert_eq!(server.node_config().unwrap().generation(), 2);
        admin.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .expect("test timed out");
}
