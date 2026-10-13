//! ABI 5.1: Enrollment through the C ABI, driven by configuration exactly as
//! `AsterServer` / `AsterClient` are (design §15). A Rust root (the CLI's
//! role) owns the fleet namespace and signs credentials and a delegation;
//! everything a server or client does goes through `aster_node_start_enrolled`,
//! `aster_server_*` and `aster_enrollment_*`. Also the lease grantor
//! (`aster_server_add_lease_grantor`, `aster_lease_serializer_grantor_new`).
//! Loopback only.

use std::collections::BTreeSet;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster::delegation::{issue_delegation, DelegationGrant, LinkKind, Validity};
use aster::enrollment::{
    issue_credential, node_registration_key, AdmissionClaims, Audience, Enrollment,
    EnrollmentIssueRequest, EnrollmentOptions, EnrollmentPolicyStore, JwtEnrollmentIssuer,
    JwtEnrollmentVerifier, NodeRegistrationStatus, NodeRegistrationV1, PolicyExpressionV1,
    PolicyPredicate, TypedAttribute, ENROLLMENT_ALPN,
};
use aster::{AsterConfig, Credential, Doc, Node, RelayMode, SecretKey, Ticket};
use aster_transport_ffi::enrollment::*;
use aster_transport_ffi::node::*;
use aster_transport_ffi::server::*;
use aster_transport_ffi::upcall::*;
use aster_transport_ffi::*;
use serde_json::{json, Value};

mod common;

const FLEET: &str = "ffi-enrollment";
const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const SERVER: u32 = aster_enrollment_side_t::ASTER_ENROLLMENT_SIDE_SERVER as u32;
const CLIENT: u32 = aster_enrollment_side_t::ASTER_ENROLLMENT_SIDE_CLIENT as u32;

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
fn bytes(b: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: if b.is_empty() {
            ptr::null()
        } else {
            b.as_ptr()
        },
        len: b.len(),
    }
}
fn role_claims(roles: Option<&str>) -> AdmissionClaims {
    let mut attributes = std::collections::BTreeMap::new();
    if let Some(roles) = roles {
        attributes.insert("aster.role".into(), TypedAttribute::String(roles.into()));
    }
    AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    }
}

/// A one-link credential the root signs.
fn root_credential(seed: u8) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    request.claims = role_claims(None);
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

/// The root (seed 1) delegates enrollment, with `aster.role`, to seed 2.
fn delegation() -> String {
    issue_delegation(
        &key(1),
        None,
        &key(2).public_key(),
        &Audience::One(FLEET.into()),
        &DelegationGrant {
            may_issue: [LinkKind::Enrollment].into(),
            audiences: [FLEET.to_string()].into(),
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
    .unwrap()
    .into_text()
}

/// The root: a Rust node holding the fleet document, serving enrollment.
struct Root {
    tokio: tokio::runtime::Runtime,
    node: Node,
    fleet: Doc,
    _enrollment: Enrollment,
    namespace: [u8; 32],
}

fn root(files: &std::path::Path) -> Root {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let (node, fleet, enrollment, namespace) = tokio.block_on(async {
        let node = Node::start_with_alpns(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(key(1))
                .hooks(true)
                .hook_failure_mode(aster::HookFailureMode::FailClosed)
                .build(),
            vec![ENROLLMENT_ALPN.to_vec()],
        )
        .await
        .unwrap();
        for _ in 0..100 {
            if !node.addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let fleet = node.docs().create().await.unwrap();
        let namespace: [u8; 32] = hex::decode(fleet.id().unwrap().to_string())
            .unwrap()
            .try_into()
            .unwrap();
        let enrollment = Enrollment::install(
            &node,
            Arc::new(
                EnrollmentPolicyStore::open(files.join("root.json"), public(1), namespace, FLEET)
                    .unwrap(),
            ),
            JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
            Some(root_credential(1)),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions::default(),
        )
        .unwrap();
        enrollment.set_policy_document(&fleet).unwrap();
        let (serving, routed) = (node.clone(), enrollment.clone());
        tokio::spawn(async move {
            while let Ok((alpn, connection)) = serving.accept().await {
                if alpn == ENROLLMENT_ALPN {
                    let enrollment = routed.clone();
                    tokio::spawn(async move {
                        let _ = enrollment.accept(&connection).await;
                        connection.closed().await;
                    });
                }
            }
        });
        (node, fleet, enrollment, namespace)
    });
    Root {
        tokio,
        node,
        fleet,
        _enrollment: enrollment,
        namespace,
    }
}

impl Root {
    fn ticket(&self) -> String {
        Ticket::from_node_addr(&self.node.addr(), Credential::Open)
            .unwrap()
            .to_base58()
            .unwrap()
    }

    /// Revoke `subject` in the fleet namespace and push it to `peer`.
    fn revoke(&self, subject: [u8; 32], peer: [u8; 32]) {
        self.tokio.block_on(async {
            let record = NodeRegistrationV1 {
                version: 1,
                issuer: hex::encode(public(1)),
                subject: hex::encode(subject),
                audience: Audience::One(FLEET.into()),
                revision: 1,
                issued_at: now() - 60,
                not_before: None,
                expires_at: None,
                status: NodeRegistrationStatus::Revoked,
                claims: role_claims(None),
                mandatory: vec![],
                reauthorizes_revision: None,
                reason: Some("test".into()),
            };
            let author = self.node.docs().default_author().await.unwrap();
            self.fleet
                .set_bytes(
                    &author,
                    node_registration_key(&subject),
                    serde_json::to_vec(&record).unwrap(),
                )
                .await
                .unwrap();
            self.fleet
                .sync_with_timeout(
                    &[aster::NodeId::from_hex(hex::encode(peer))],
                    Duration::from_secs(10),
                )
                .await
                .unwrap();
        });
    }
}

unsafe fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    assert_eq!(
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
        OK
    );
    rt
}

fn last_error() -> String {
    let mut buf = vec![0u8; 4096];
    let len = unsafe { aster_last_error_message(buf.as_mut_ptr(), buf.len()) };
    String::from_utf8_lossy(&buf[..len.min(buf.len())]).into_owned()
}

/// A k/v list in the C ABI encoding.
fn kv(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut out = (entries.len() as u32).to_le_bytes().to_vec();
    for (key, value) in entries {
        for part in [key, value] {
            out.extend_from_slice(&(part.len() as u32).to_le_bytes());
            out.extend_from_slice(part.as_bytes());
        }
    }
    out
}

/// Start a loopback node (seed, `aster/1`) from `trust`. `Ok((node,
/// enrollment))`; `Err((status, message))` for a refusal before the start
/// or an `ASTER_EVENT_ERROR` from it.
unsafe fn start(
    rt: aster_runtime_t,
    seed: u8,
    trust: &Value,
    side: u32,
) -> Result<(u64, u64), (i32, String)> {
    let alpns = [bytes(b"aster/1")];
    let secret = [seed; 32];
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.secret_key = bytes(&secret);
    cfg.alpns = aster_bytes_list_t {
        items: alpns.as_ptr(),
        len: alpns.len(),
    };
    let text = trust.to_string();
    let mut op = 0;
    let status = aster_node_start_enrolled(rt, &cfg, bytes(text.as_bytes()), side, 0, &mut op);
    if status != OK {
        return Err((status, last_error()));
    }
    let event = common::wait_for_operation(rt, op);
    if event.kind == aster_event_kind_t::ASTER_EVENT_NODE_CREATED as u32 {
        return Ok((event.handle, event.related));
    }
    let message = if event.data_len == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(event.data_ptr, event.data_len))
            .into_owned()
    };
    aster_buffer_release(rt, event.buffer);
    Err((event.status as i32, message))
}

unsafe fn addr_json(rt: aster_runtime_t, node: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut buf = vec![0u8; 4096];
        let mut out: aster_node_addr_t = std::mem::zeroed();
        assert_eq!(
            aster_node_addr_info(rt, node, buf.as_mut_ptr(), buf.len(), &mut out),
            OK
        );
        let s = |b: &aster_bytes_t| {
            String::from_utf8(std::slice::from_raw_parts(b.ptr, b.len).to_vec()).unwrap()
        };
        let direct: Vec<String> = if out.direct_addresses.len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(out.direct_addresses.items, out.direct_addresses.len)
                .iter()
                .map(s)
                .collect()
        };
        if !direct.is_empty() || Instant::now() > deadline {
            return json!({"node_id": s(&out.endpoint_id), "direct_addresses": direct});
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Take an owned JSON output.
unsafe fn take(out: aster_bytes_t) -> Value {
    let text = std::slice::from_raw_parts(out.ptr, out.len).to_vec();
    assert_eq!(aster_string_release(out.ptr, out.len), OK);
    serde_json::from_slice(&text).unwrap()
}

/// Wait for an operation; `Ok(json)` for `ENROLLMENT_RESULT`, else
/// `Err((status, flags, message))`.
unsafe fn wait(rt: aster_runtime_t, op: u64) -> Result<Value, (u32, u32, String)> {
    let event = common::wait_for_operation(rt, op);
    let payload = if event.data_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(event.data_ptr, event.data_len).to_vec()
    };
    aster_buffer_release(rt, event.buffer);
    if event.kind == aster_event_kind_t::ASTER_EVENT_ENROLLMENT_RESULT as u32 {
        assert_eq!(event.status, OK as u32);
        Ok(serde_json::from_slice(&payload).unwrap())
    } else {
        Err((
            event.status,
            event.flags,
            String::from_utf8_lossy(&payload).into_owned(),
        ))
    }
}

unsafe fn claims(rt: aster_runtime_t, enrollment: u64, peer: [u8; 32]) -> Option<Value> {
    let peer = hex::encode(peer);
    let mut out = bytes(b"");
    match aster_enrollment_peer_claims(rt, enrollment, bytes(peer.as_bytes()), &mut out) {
        OK => Some(take(out)),
        status => {
            assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);
            None
        }
    }
}

unsafe fn status(rt: aster_runtime_t, enrollment: u64) -> Value {
    let mut out = bytes(b"");
    assert_eq!(aster_enrollment_status(rt, enrollment, &mut out), OK);
    take(out)
}

unsafe fn check(
    rt: aster_runtime_t,
    enrollment: u64,
    peer: [u8; 32],
) -> Result<Value, (u32, u32, String)> {
    let peer = hex::encode(peer);
    let mut op = 0;
    assert_eq!(
        aster_enrollment_check_peer(rt, enrollment, bytes(peer.as_bytes()), 0, &mut op),
        OK
    );
    wait(rt, op)
}

unsafe fn enroll(
    rt: aster_runtime_t,
    enrollment: u64,
    server: &Value,
) -> Result<Value, (u32, u32, String)> {
    let text = server.to_string();
    let mut op = 0;
    assert_eq!(
        aster_enrollment_enroll(rt, enrollment, bytes(text.as_bytes()), 0, &mut op),
        OK
    );
    wait(rt, op)
}

unsafe fn close_node(rt: aster_runtime_t, node: u64) {
    let mut op = 0;
    assert_eq!(aster_node_close(rt, node, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CLOSED);
}

/// The `[trust]` + `[enrollment]` JSON of a node with `credential`.
fn trust(namespace: [u8; 32], credential: &str, enrollment: Value) -> Value {
    let mut section = json!({"audience": FLEET, "fleet_namespace": hex::encode(namespace)});
    for (k, v) in enrollment.as_object().unwrap() {
        section[k] = v.clone();
    }
    json!({
        "root_pubkey": hex::encode(public(1)),
        "enrollment_credential_chain": credential,
        "enrollment": section,
    })
}

/// A server configured as a delegated issuer with the root as its policy
/// replica; a client enrolls with an issued credential, renews from the
/// server automatically (the server signed it), and is revoked by the root.
#[test]
fn config_driven_server_issues_renews_and_follows_revocation() {
    let files = tempfile::tempdir().unwrap();
    let root = root(files.path());
    let delegation_file = files.path().join("issuer.chain");
    std::fs::write(&delegation_file, format!("{}\n", delegation())).unwrap();
    unsafe {
        let rt = runtime();
        let (server_node, server) = start(
            rt,
            2,
            &trust(
                root.namespace,
                &root_credential(2),
                json!({
                    "state_path": files.path().join("server.json").to_str().unwrap(),
                    "replicas": [root.ticket()],
                    "issuer_delegation_file": delegation_file.to_str().unwrap(),
                    "issuer_lifetime_secs": 6,
                    "limits": {"reconciliation_interval_ms": 200},
                }),
            ),
            SERVER,
        )
        .unwrap();
        assert_ne!(server, 0);
        let mut builder = 0;
        assert_eq!(aster_server_builder_new(rt, server_node, &mut builder), OK);
        let mut srv = 0;
        assert_eq!(aster_server_start(rt, builder, 64, &mut srv), OK);

        let state = status(rt, server);
        assert_eq!(state["side"], "server");
        assert_eq!(state["trusted_root"], hex::encode(public(1)));
        assert_eq!(state["credential"]["subject"], hex::encode(public(2)));
        assert_eq!(state["issuer"], hex::encode(public(2)));
        assert_eq!(state["renewing"], false);
        let mut out = bytes(b"");
        assert_eq!(aster_enrollment_issuer_info(rt, server, &mut out), OK);
        assert_eq!(take(out)["public_key"], hex::encode(public(2)));

        // Claims beyond the delegation are refused with a code.
        let wider = json!({"subject": hex::encode(public(7)),
            "claims": {"scopes": ["deploy"], "tags": [], "attributes": {}}})
        .to_string();
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_issue(rt, server, bytes(wider.as_bytes()), &mut out),
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED as i32
        );
        assert_eq!(take(out)["code"], "invalid_credential");
        let request = json!({"subject": hex::encode(public(7)),
            "claims": {"scopes": [], "tags": [], "attributes": {"aster.role": {"type": "string", "value": "ops.status"}}}})
        .to_string();
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_issue(rt, server, bytes(request.as_bytes()), &mut out),
            OK
        );
        let issued = take(out);
        let chain = issued["chain"].as_str().unwrap().to_owned();
        assert!(issued["expires_at"].as_u64().unwrap() <= now() + 6);

        // The client: enrolls with the server; renews from it (it signed).
        let (client_node, client) =
            start(rt, 7, &trust(root.namespace, &chain, json!({})), CLIENT).unwrap();
        let server_addr = addr_json(rt, server_node);
        let mut op = 0;
        assert_eq!(
            aster_enrollment_enroll(
                rt,
                server,
                bytes(server_addr.to_string().as_bytes()),
                0,
                &mut op
            ),
            INVALID,
            "a server enrollment does not run the client sequence"
        );
        let mut revocation_op = 0;
        assert_eq!(
            aster_enrollment_next_revocation(rt, server, 0, &mut revocation_op),
            OK
        );
        let enrolled = enroll(rt, client, &server_addr).expect("enroll");
        assert_eq!(enrolled["readiness"]["peer"], hex::encode(public(2)));
        assert_eq!(enrolled["renewing"], true);
        assert_eq!(status(rt, client)["side"], "client");

        // The server sees the verified claims (it maps them to roles itself).
        let seen = claims(rt, server, public(7)).expect("session");
        assert_eq!(seen["issuer"], hex::encode(public(2)));
        assert_eq!(seen["attributes"]["aster.role"]["value"], "ops.status");
        assert!(claims(rt, server, public(9)).is_none());
        check(rt, server, public(7)).expect("admitted");

        // The credential sets the maximum roles: runtime code cannot set
        // `aster.role` on the enrolled peer, only restrict it; a peer without
        // a session has nothing to restrict.
        let enrolled_peer = hex::encode(public(7));
        let role_kv = kv(&[("aster.role", "ops.admin")]);
        assert_eq!(
            aster_server_set_peer_attributes(
                srv,
                enrolled_peer.as_ptr(),
                enrolled_peer.len() as u32,
                role_kv.as_ptr(),
                role_kv.len() as u32,
            ),
            INVALID
        );
        assert!(last_error().contains("restrict_roles"), "{}", last_error());
        let tenant_kv = kv(&[("tenant", "t1")]);
        assert_eq!(
            aster_server_set_peer_attributes(
                srv,
                enrolled_peer.as_ptr(),
                enrolled_peer.len() as u32,
                tenant_kv.as_ptr(),
                tenant_kv.len() as u32,
            ),
            OK
        );
        let restrict = |peer: &str, roles: &str| {
            aster_server_restrict_peer_roles(
                srv,
                peer.as_ptr(),
                peer.len() as u32,
                roles.as_ptr(),
                roles.len() as u32,
            )
        };
        assert_eq!(restrict(&enrolled_peer, "ops.status"), OK);
        assert_eq!(restrict(&enrolled_peer, ""), OK);
        assert_eq!(
            restrict(&hex::encode(public(9)), "ops.status"),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_server_clear_peer_restriction(
                srv,
                enrolled_peer.as_ptr(),
                enrolled_peer.len() as u32
            ),
            OK
        );
        assert_eq!(restrict(&enrolled_peer, "ops.status"), OK);

        // Enrolled nodes take no gate, and a client node cannot serve.
        let mut gate = 0;
        assert_eq!(aster_gate_new(rt, &mut gate), OK);
        assert_eq!(
            aster_node_set_gate(rt, client_node, gate),
            aster_status_t::ASTER_STATUS_UNSUPPORTED as i32
        );
        let mut client_builder = 0;
        assert_eq!(
            aster_server_builder_new(rt, client_node, &mut client_builder),
            INVALID
        );

        // Renewal moves the client and the server's session to new
        // credentials across lifetimes.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let current = status(rt, client)["credential"]["credential_id"].clone();
            let seen = claims(rt, server, public(7)).unwrap()["credential_id"].clone();
            if current != issued["credential_id"] && seen == current {
                break;
            }
            assert!(Instant::now() < deadline, "no renewal reached the server");
            std::thread::sleep(Duration::from_millis(200));
        }

        // The root revokes the client: the server revokes the session.
        root.revoke(public(7), public(2));
        let revoked = wait(rt, revocation_op).expect("revocation");
        assert_eq!(revoked["peer"], hex::encode(public(7)));
        assert_eq!(revoked["reason"]["code"], "node_revoked");
        let (code, flags, _) = check(rt, server, public(7)).unwrap_err();
        assert_eq!(code, aster_status_t::ASTER_STATUS_PERMISSION_DENIED as u32);
        assert_eq!(
            flags,
            aster_enrollment_error_t::ASTER_ENROLLMENT_ERROR_DENIED as u32
        );
        assert!(claims(rt, server, public(7)).is_none());
        // The session ended, and the restriction with it.
        assert_eq!(
            restrict(&enrolled_peer, "ops.status"),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        // Releasing an enrollment ends a pending revocation wait.
        let mut op = 0;
        assert_eq!(aster_enrollment_next_revocation(rt, client, 0, &mut op), OK);
        assert_eq!(aster_enrollment_free(rt, client), OK);
        let (code, _, _) = wait(rt, op).unwrap_err();
        assert_eq!(code, aster_status_t::ASTER_STATUS_ALREADY_CLOSED as u32);
        assert_eq!(
            aster_enrollment_free(rt, client),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        assert_eq!(aster_server_destroy(rt, srv), OK);
        close_node(rt, client_node);
        close_node(rt, server_node);
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_status(rt, server, &mut out),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
            "closing the node released its enrollment"
        );
        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
    root.tokio.block_on(root.node.clone().shutdown());
}

/// Misconfiguration fails before anything starts, naming the setting;
/// nothing ever runs open. Without enrollment the call is a plain start.
#[test]
fn misconfiguration_fails_closed() {
    let files = tempfile::tempdir().unwrap();
    let namespace = [3u8; 32];
    let state = files.path().join("s.json");
    let good = json!({"state_path": state.to_str().unwrap()});
    unsafe {
        let rt = runtime();
        let refused = |trust: Value, side: u32, expect: &str| {
            let (code, message) = start(rt, 5, &trust, side).unwrap_err();
            assert_eq!(code, INVALID, "{expect}: {message}");
            assert!(message.contains(expect), "{expect:?} not in {message:?}");
        };
        let credential = root_credential(5);

        let mut no_audience = trust(namespace, &credential, good.clone());
        no_audience["enrollment"]
            .as_object_mut()
            .unwrap()
            .remove("audience");
        refused(no_audience, SERVER, "audience is required");
        let mut no_root = trust(namespace, &credential, good.clone());
        no_root["root_pubkey"] = Value::Null;
        refused(no_root, SERVER, "no trusted root");
        refused(
            trust(namespace, &credential, json!({})),
            SERVER,
            "keeps denials durably",
        );
        let mut no_credential = trust(namespace, &credential, good.clone());
        no_credential["enrollment_credential_chain"] = Value::Null;
        refused(no_credential, SERVER, "no credential");
        refused(
            trust(namespace, "/etc/aster/node.cred", good.clone()),
            SERVER,
            "ASTER_ENROLLMENT_CREDENTIAL)",
        );
        refused(
            trust(namespace, &credential, json!({"policy": "allow_everyone"})),
            CLIENT,
            "policy",
        );
        refused(
            trust(
                namespace,
                &credential,
                json!({"fleet_namespace": "not-hex"}),
            ),
            CLIENT,
            "fleet_namespace",
        );
        refused(
            trust(
                namespace,
                &credential,
                json!({"issuer_delegation_file": "x"}),
            ),
            CLIENT,
            "server-only",
        );
        refused(
            json!({"root_pubkey": hex::encode(public(1)), "enrollment": {"enabled": true}}),
            CLIENT,
            "audience is required",
        );
        refused(json!({"allow_all_consumers": true}), CLIENT, "trust_json");
        let mut op = 0;
        assert_eq!(
            aster_node_start_enrolled(rt, ptr::null(), bytes(b"{}"), 9, 0, &mut op),
            INVALID
        );

        // A credential issued for another key: the node starts, install
        // refuses it, and the node is closed again.
        let (code, message) =
            start(rt, 6, &trust(namespace, &credential, json!({})), CLIENT).unwrap_err();
        assert_eq!(code, INVALID, "{message}");

        // Not enabled (the root alone does not enable it): a plain node.
        let (node, enrollment) = start(
            rt,
            8,
            &json!({"root_pubkey": hex::encode(public(1))}),
            SERVER,
        )
        .unwrap();
        assert_eq!(enrollment, 0);
        close_node(rt, node);
        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
}

/// `[trust]` / `[enrollment]` resolve through `AsterConfig::from_file` into
/// the JSON `aster_node_start_enrolled` takes.
#[test]
fn trust_json_from_toml() {
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("aster.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[trust]
root_pubkey = "{root}"
enrollment_credential = "node.cred"
next_root_commitment = "{commitment}"
root_seq = 1

[enrollment]
audience = "{FLEET}"
fleet_namespace = "{ns}"
policy = {{ any_of = [{{ scope = "ops" }}, "tolerate_stale_policy"] }}

[enrollment.roles.scopes]
ops = "ops.status, ops.logs"

[enrollment.issuer]
lifetime = "2h"
"#,
            root = hex::encode(public(1)),
            commitment = "ab".repeat(32),
            ns = "cd".repeat(32),
        ),
    )
    .unwrap();
    unsafe {
        let text = path.to_str().unwrap();
        let mut out = bytes(b"");
        assert_eq!(
            aster_config_enrollment_from_toml(bytes(text.as_bytes()), &mut out),
            OK
        );
        let json = take(out);
        assert_eq!(json["root_pubkey"], hex::encode(public(1)));
        assert_eq!(json["enrollment_credential_file"], "node.cred");
        assert_eq!(json["root_seq"], 1);
        assert_eq!(json["enrollment"]["audience"], FLEET);
        assert_eq!(
            json["enrollment"]["policy"],
            json!({"any_of": [{"scope": "ops"}, "tolerate_stale_policy"]})
        );
        assert_eq!(
            json["enrollment"]["roles"]["scopes"]["ops"],
            json!(["ops.status", "ops.logs"])
        );
        assert_eq!(json["enrollment"]["roles"]["attribute"], "aster.role");
        assert_eq!(json["enrollment"]["issuer_lifetime_secs"], 7200);

        let missing = files.path().join("missing.toml");
        let mut out = bytes(b"");
        assert_eq!(
            aster_config_enrollment_from_toml(
                bytes(missing.to_str().unwrap().as_bytes()),
                &mut out
            ),
            INVALID
        );
    }
}

unsafe fn connect_rpc(rt: aster_runtime_t, client: u64, server: u64) -> u64 {
    let addr = addr_json(rt, server);
    let id = addr["node_id"].as_str().unwrap().to_owned();
    let direct: Vec<String> = serde_json::from_value(addr["direct_addresses"].clone()).unwrap();
    let direct_b: Vec<aster_bytes_t> = direct.iter().map(|d| bytes(d.as_bytes())).collect();
    let node_addr = aster_node_addr_t {
        endpoint_id: bytes(id.as_bytes()),
        relay_url: bytes(&[]),
        direct_addresses: aster_bytes_list_t {
            items: direct_b.as_ptr(),
            len: direct_b.len(),
        },
    };
    let cfg = aster_connect_config_t {
        struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
        flags: 0,
        node_id: bytes(id.as_bytes()),
        alpn: bytes(b"aster/1"),
        addr: &node_addr,
    };
    let mut op = 0;
    assert_eq!(aster_connect(rt, client, &cfg, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CONNECTED).handle
}

unsafe fn node_id(rt: aster_runtime_t, node: u64) -> String {
    addr_json(rt, node)["node_id"].as_str().unwrap().to_owned()
}

/// Returns `(event kind, handle)` of an acquire.
unsafe fn acquire(rt: aster_runtime_t, ser: u64, resource: &str, holder: &str) -> (u32, u64) {
    let mut op = 0;
    assert_eq!(
        aster_lease_acquire(
            rt,
            ser,
            bytes(resource.as_bytes()),
            bytes(holder.as_bytes()),
            5_000,
            0,
            0,
            0,
            &mut op
        ),
        OK
    );
    let event = common::wait_for_operation(rt, op);
    aster_buffer_release(rt, event.buffer);
    (event.kind, event.handle)
}

/// A server serves `aster.lease.Grantor` over a serializer handle; clients
/// acquire through it, bound to their authenticated node ids.
#[test]
fn lease_grantor_serves_clients_bound_to_their_identity() {
    unsafe {
        let rt = runtime();
        let server_node = common::start_node(rt, &[b"aster/1"]);
        let mut backend = 0;
        assert_eq!(aster_lease_serializer_memory_new(rt, 0, &mut backend), OK);
        let mut builder = 0;
        assert_eq!(aster_server_builder_new(rt, server_node, &mut builder), OK);
        assert_eq!(
            aster_server_add_lease_grantor(builder, backend, bytes(b"{"), bytes(b"")),
            INVALID
        );
        assert_eq!(
            aster_server_add_lease_grantor(builder, 999_999, bytes(b""), bytes(b"")),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_server_add_lease_grantor(builder, backend, bytes(b""), bytes(b"")),
            OK
        );
        let mut srv = 0;
        assert_eq!(aster_server_start(rt, builder, 64, &mut srv), OK);

        let a = common::start_node(rt, &[]);
        let b = common::start_node(rt, &[]);
        let (a_id, b_id) = (node_id(rt, a), node_id(rt, b));
        let mut ser_a = 0;
        let conn_a = connect_rpc(rt, a, server_node);
        assert_eq!(
            aster_lease_serializer_grantor_new(rt, a, conn_a, &mut ser_a),
            OK
        );
        let mut ser_b = 0;
        let conn_b = connect_rpc(rt, b, server_node);
        assert_eq!(
            aster_lease_serializer_grantor_new(rt, b, conn_b, &mut ser_b),
            OK
        );

        let granted = aster_event_kind_t::ASTER_EVENT_LEASE_ACQUIRED as u32;
        let (kind, lease) = acquire(rt, ser_a, "db/primary", &a_id);
        assert_eq!(kind, granted);
        assert_ne!(lease, 0, "a holds the lease");
        // b is denied while a holds it.
        assert_eq!(acquire(rt, ser_b, "db/primary", &b_id), (granted, 0));
        // An op built for another actor never reaches the grantor.
        let (kind, _) = acquire(rt, ser_b, "db/other", &a_id);
        assert_eq!(kind, aster_event_kind_t::ASTER_EVENT_ERROR as u32);
        // The grantor's rows are the backend's.
        let mut op = 0;
        assert_eq!(
            aster_lease_snapshot(rt, backend, bytes(b"db/primary"), 0, &mut op),
            OK
        );
        let event = common::wait_for_operation(rt, op);
        let payload = std::slice::from_raw_parts(event.data_ptr, event.data_len).to_vec();
        aster_buffer_release(rt, event.buffer);
        assert!(
            payload.windows(64).any(|w| w == a_id.as_bytes()),
            "the backend holds a's grant"
        );

        assert_eq!(aster_lease_free(rt, lease), OK);
        for ser in [ser_a, ser_b, backend] {
            assert_eq!(aster_lease_serializer_free(rt, ser), OK);
        }
        assert_eq!(aster_server_destroy(rt, srv), OK);
        for node in [a, b, server_node] {
            close_node(rt, node);
        }
        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
}
