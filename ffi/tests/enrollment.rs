//! ABI 5.1: Enrollment through the C ABI. A Rust root (the CLI's role) owns
//! the fleet namespace and signs a delegation; everything an enrolled node or
//! a delegated issuer does goes through `aster_enrollment_*`. Loopback only.

use std::collections::BTreeSet;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster::delegation::{issue_delegation, DelegationGrant, LinkKind, Validity};
use aster::enrollment::{
    issue_credential, node_registration_key, AdmissionClaims, Audience, Enrollment,
    EnrollmentIssueRequest, EnrollmentOptions, EnrollmentPolicyStore, JwtEnrollmentIssuer,
    JwtEnrollmentVerifier, NodeRegistrationStatus, NodeRegistrationV1, PolicyExpressionV1,
    PolicyPredicate, ENROLLMENT_ALPN,
};
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};
use aster_transport_ffi::enrollment::*;
use aster_transport_ffi::node::*;
use aster_transport_ffi::server::*;
use aster_transport_ffi::*;
use serde_json::{json, Value};

mod common;

const FLEET: &str = "ffi-enrollment";
const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

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
fn build_claims() -> AdmissionClaims {
    AdmissionClaims {
        scopes: Some(["build".to_string()].into()),
        tags: Some(BTreeSet::new()),
        attributes: Some(Default::default()),
    }
}

/// A one-link credential the root signs.
fn root_credential(seed: u8) -> String {
    let request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

/// The root (seed 1) delegates enrollment of `build` nodes to seed 2.
fn delegation() -> String {
    issue_delegation(
        &key(1),
        None,
        &key(2).public_key(),
        &Audience::One(FLEET.into()),
        &DelegationGrant {
            may_issue: [LinkKind::Enrollment].into(),
            audiences: [FLEET.to_string()].into(),
            scopes: ["build".to_string()].into(),
            tags: BTreeSet::new(),
            attribute_keys: ["pod".to_string()].into(),
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
    fn addr_json(&self) -> Value {
        let addr = self.node.addr();
        json!([{"node_id": addr.node_id.to_string(), "direct_addresses": addr.direct_addresses}])
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
                claims: build_claims(),
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

/// A loopback node with fail-closed hooks serving `alpns`.
unsafe fn start(rt: aster_runtime_t, seed: u8, alpns: &[&[u8]], fail_closed: bool) -> u64 {
    let items: Vec<aster_bytes_t> = alpns.iter().map(|a| bytes(a)).collect();
    let secret = [seed; 32];
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.secret_key = bytes(&secret);
    cfg.alpns = aster_bytes_list_t {
        items: items.as_ptr(),
        len: items.len(),
    };
    cfg.enable_hooks = 1;
    cfg.hook_failure_mode = if fail_closed {
        aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED
    } else {
        aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_OPEN
    } as u32;
    let mut op = 0;
    assert_eq!(aster_node_start(rt, &cfg, 0, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_NODE_CREATED)
        .handle
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
            return json!([{"node_id": s(&out.endpoint_id), "direct_addresses": direct}]);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn config(namespace: [u8; 32], path: &std::path::Path, credential: Option<String>) -> Value {
    json!({
        "root": hex::encode(public(1)),
        "audience": FLEET,
        "fleet_namespace": hex::encode(namespace),
        "state_path": path.to_str().unwrap(),
        "credential": credential,
        "policy": {"any_of": [{"scope": "build"}, "tolerate_stale_policy"]},
        "limits": {"reconciliation_interval_ms": 200},
    })
}

unsafe fn install(rt: aster_runtime_t, node: u64, config: &Value) -> Result<u64, (i32, String)> {
    let text = config.to_string();
    let mut handle = 0;
    let status = aster_enrollment_install(rt, node, bytes(text.as_bytes()), &mut handle);
    if status == OK {
        Ok(handle)
    } else {
        Err((status, last_error()))
    }
}

fn last_error() -> String {
    let mut buf = vec![0u8; 4096];
    let len = unsafe { aster_last_error_message(buf.as_mut_ptr(), buf.len()) };
    String::from_utf8_lossy(&buf[..len.min(buf.len())]).into_owned()
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

unsafe fn connect(rt: aster_runtime_t, enrollment: u64, replicas: &Value) -> Value {
    let text = replicas.to_string();
    let mut op = 0;
    assert_eq!(
        aster_enrollment_connect(rt, enrollment, bytes(text.as_bytes()), 0, &mut op),
        OK
    );
    wait(rt, op).expect("connect")
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

#[test]
fn issuer_enrolls_renews_and_a_revocation_closes_the_peer() {
    let files = tempfile::tempdir().unwrap();
    let root = root(files.path());
    unsafe {
        let rt = runtime();

        // The issuer node: enrolled by the root, synchronized, serving
        // enrollment and renewals.
        let issuer_node = start(rt, 2, &[b"aster/1", ENROLLMENT_ALPN], true);
        let issuer_enrollment = install(
            rt,
            issuer_node,
            &config(
                root.namespace,
                &files.path().join("issuer.json"),
                Some(root_credential(2)),
            ),
        )
        .unwrap();
        let joined = connect(rt, issuer_enrollment, &root.addr_json());
        assert_eq!(joined["readiness"]["peer"], hex::encode(public(1)));
        let ticket = joined["policy_ticket"].as_str().unwrap().to_owned();
        let mut op = 0;
        let peer = hex::encode(public(1));
        assert_eq!(
            aster_enrollment_synchronize(
                rt,
                issuer_enrollment,
                bytes(peer.as_bytes()),
                bytes(ticket.as_bytes()),
                20_000,
                0,
                &mut op
            ),
            OK
        );
        assert_eq!(
            wait(rt, op).unwrap()["namespace"],
            hex::encode(root.namespace)
        );
        let state = status(rt, issuer_enrollment);
        assert_eq!(state["trusted_root"], hex::encode(public(1)));
        assert_eq!(state["credential"]["subject"], hex::encode(public(2)));

        // The node cannot take a gate once Enrollment decides admission.
        let mut gate = 0;
        assert_eq!(aster_gate_new(rt, &mut gate), OK);
        assert_eq!(
            aster_node_set_gate(rt, issuer_node, gate),
            aster_status_t::ASTER_STATUS_UNSUPPORTED as i32
        );
        assert_eq!(
            install(
                rt,
                issuer_node,
                &config(root.namespace, &files.path().join("x.json"), None)
            )
            .unwrap_err()
            .0,
            aster_status_t::ASTER_STATUS_ALREADY_EXISTS as i32
        );

        let delegation = delegation();
        let mut issuer = 0;
        assert_eq!(
            aster_enrollment_issuer_new(
                rt,
                issuer_enrollment,
                bytes(delegation.as_bytes()),
                6,
                &mut issuer
            ),
            OK
        );
        let mut out = bytes(b"");
        assert_eq!(aster_enrollment_issuer_info(rt, issuer, &mut out), OK);
        assert_eq!(take(out)["public_key"], hex::encode(public(2)));

        let mut builder = 0;
        assert_eq!(aster_server_builder_new(rt, issuer_node, &mut builder), OK);
        assert_eq!(
            aster_server_set_enrollment(rt, builder, issuer_enrollment, issuer),
            OK
        );
        let mut server = 0;
        assert_eq!(aster_server_start(rt, builder, 64, &mut server), OK);

        // Claims beyond the delegation are refused with a code.
        let wider = json!({"subject": hex::encode(public(7)),
            "claims": {"scopes": ["build", "deploy"], "tags": [], "attributes": {}}});
        let text = wider.to_string();
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_issue(rt, issuer, bytes(text.as_bytes()), &mut out),
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED as i32
        );
        assert_eq!(take(out)["code"], "invalid_credential");

        // The orchestrator authenticated node 7 out of band: issue for it.
        let request = json!({"subject": hex::encode(public(7)),
            "claims": {"scopes": ["build"], "tags": [], "attributes": {"pod": {"type": "string", "value": "p-1"}}},
            "registration": "ephemeral"});
        let text = request.to_string();
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_issue(rt, issuer, bytes(text.as_bytes()), &mut out),
            OK
        );
        let issued = take(out);
        let chain = issued["chain"].as_str().unwrap().to_owned();
        assert!(issued["expires_at"].as_u64().unwrap() <= now() + 6);

        // Node 7 enrolls with the issuer using the issued credential.
        let node = start(rt, 7, &[ENROLLMENT_ALPN], true);
        let enrollment = install(
            rt,
            node,
            &config(
                root.namespace,
                &files.path().join("node.json"),
                Some(chain.clone()),
            ),
        )
        .unwrap();
        let mut op = 0;
        assert_eq!(
            aster_enrollment_next_revocation(rt, issuer_enrollment, 0, &mut op),
            OK
        );
        let revocation_op = op;
        let issuer_addr = addr_json(rt, issuer_node);
        let joined = connect(rt, enrollment, &issuer_addr);
        assert_eq!(joined["readiness"]["peer"], hex::encode(public(2)));

        // The issuer reads the verified claims of its peer.
        let seen = claims(rt, issuer_enrollment, public(7)).expect("session");
        assert_eq!(seen["subject"], hex::encode(public(7)));
        assert_eq!(seen["root"], hex::encode(public(1)));
        assert_eq!(seen["issuer"], hex::encode(public(2)));
        assert_eq!(seen["source"], "chain");
        assert_eq!(seen["scopes"], json!(["build"]));
        assert_eq!(seen["attributes"]["pod"]["value"], "p-1");
        assert_eq!(seen["registration"], "ephemeral");
        assert!(claims(rt, issuer_enrollment, public(9)).is_none());
        check(rt, issuer_enrollment, public(7)).expect("admitted");

        // Offline verification of the chain under the issuer's trust.
        let subject = hex::encode(public(7));
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_verify_credential(
                rt,
                issuer_enrollment,
                bytes(subject.as_bytes()),
                bytes(chain.as_bytes()),
                &mut out
            ),
            OK
        );
        assert_eq!(take(out)["credential_id"], issued["credential_id"]);
        let other = hex::encode(public(9));
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_verify_credential(
                rt,
                issuer_enrollment,
                bytes(other.as_bytes()),
                bytes(chain.as_bytes()),
                &mut out
            ),
            aster_status_t::ASTER_STATUS_VERIFICATION_FAILED as i32
        );

        // Renewal keeps node 7 on fresh credentials from the issuer.
        let renewal_config = json!({"issuer": issuer_addr[0], "retry_ms": 200}).to_string();
        let mut renewal = 0;
        assert_eq!(
            aster_enrollment_start_renewal(
                rt,
                enrollment,
                bytes(renewal_config.as_bytes()),
                &mut renewal
            ),
            OK
        );
        // Node 7 switches to the renewed credential first, then presents it
        // to the issuer: wait for both.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let current = status(rt, enrollment)["credential"]["credential_id"].clone();
            let seen = claims(rt, issuer_enrollment, public(7)).unwrap()["credential_id"].clone();
            if current != issued["credential_id"] && seen == current {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "renewal did not move node 7 and the issuer's session to a new credential"
            );
            std::thread::sleep(Duration::from_millis(200));
        }

        // The root revokes node 7: the issuer revokes the session and reports
        // it; a renewal wait then ends with the issuer's refusal or expiry.
        root.revoke(public(7), public(2));
        let revoked = wait(rt, revocation_op).expect("revocation");
        assert_eq!(revoked["peer"], hex::encode(public(7)));
        assert_eq!(revoked["reason"]["code"], "node_revoked");
        assert_eq!(revoked["reason"]["definite"], true);
        let (status_code, flags, _) = check(rt, issuer_enrollment, public(7)).unwrap_err();
        assert_eq!(
            status_code,
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED as u32
        );
        assert_eq!(
            flags,
            aster_enrollment_error_t::ASTER_ENROLLMENT_ERROR_DENIED as u32
        );
        assert!(claims(rt, issuer_enrollment, public(7)).is_none());
        let mut op = 0;
        assert_eq!(aster_enrollment_renewal_wait(rt, renewal, 0, &mut op), OK);
        let outcome = wait(rt, op).expect("renewal outcome");
        assert!(
            outcome["outcome"] == "refused" || outcome["outcome"] == "expired",
            "{outcome}"
        );
        assert_eq!(aster_enrollment_renewal_free(rt, renewal), OK);

        // A pending revocation wait ends when the enrollment is released.
        let mut op = 0;
        assert_eq!(
            aster_enrollment_next_revocation(rt, enrollment, 0, &mut op),
            OK
        );
        assert_eq!(aster_enrollment_free(rt, enrollment), OK);
        let (status_code, _, _) = wait(rt, op).unwrap_err();
        assert_eq!(
            status_code,
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED as u32
        );
        assert_eq!(
            aster_enrollment_free(rt, enrollment),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        assert_eq!(aster_server_destroy(rt, server), OK);
        assert_eq!(aster_enrollment_issuer_free(rt, issuer), OK);
        assert_eq!(aster_enrollment_free(rt, issuer_enrollment), OK);
        for handle in [node, issuer_node] {
            let mut op = 0;
            assert_eq!(aster_node_close(rt, handle, 0, &mut op), OK);
            common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CLOSED);
        }
        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
    root.tokio.block_on(root.node.clone().shutdown());
}

#[test]
fn install_refuses_unsuitable_nodes_and_configurations() {
    let files = tempfile::tempdir().unwrap();
    let namespace = [3u8; 32];
    unsafe {
        let rt = runtime();
        let path = files.path().join("state.json");

        // No hooks: nothing to install into.
        let plain = common::start_node(rt, &[ENROLLMENT_ALPN]);
        assert_eq!(
            install(rt, plain, &config(namespace, &path, None))
                .unwrap_err()
                .0,
            aster_status_t::ASTER_STATUS_UNSUPPORTED as i32
        );

        // Fail-open hooks are refused by Enrollment itself.
        let open = start(rt, 4, &[ENROLLMENT_ALPN], false);
        let (code, message) = install(rt, open, &config(namespace, &path, None)).unwrap_err();
        assert_eq!(code, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
        assert!(message.contains("fail-closed"), "{message}");

        let node = start(rt, 5, &[ENROLLMENT_ALPN], true);
        for (mutate, expect) in [
            (json!({"policy": {"all_of": []}}), "policy"),
            (
                json!({"policy": {"source_cidr": "10.0.0.0/99"}}),
                "source_cidr",
            ),
            (json!({"policy": "allow_everyone"}), "enrollment config"),
            (json!({"root": "zz"}), "root"),
            (json!({"state_path": ""}), "state_path"),
            (json!({"surprise": true}), "enrollment config"),
            // A credential that does not verify for this node.
            (json!({"credential": root_credential(6)}), "PeerKeyMismatch"),
        ] {
            let mut cfg = config(namespace, &path, None);
            for (k, v) in mutate.as_object().unwrap() {
                cfg[k] = v.clone();
            }
            let (code, message) = install(rt, node, &cfg).unwrap_err();
            assert_eq!(
                code,
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
                "{message}"
            );
            assert!(message.contains(expect), "{expect}: {message}");
        }

        // A gate attached first keeps Enrollment out.
        let mut gate = 0;
        assert_eq!(aster_gate_new(rt, &mut gate), OK);
        assert_eq!(aster_node_set_gate(rt, node, gate), OK);
        assert_eq!(
            install(rt, node, &config(namespace, &path, None))
                .unwrap_err()
                .0,
            aster_status_t::ASTER_STATUS_UNSUPPORTED as i32
        );
        assert_eq!(aster_node_set_gate(rt, node, 0), OK);

        // A pure verifier (no credential) installs; its status has none.
        let enrollment = install(rt, node, &config(namespace, &path, None)).unwrap();
        let state = status(rt, enrollment);
        assert_eq!(state["credential"], Value::Null);
        assert_eq!(state["fleet_namespace"], hex::encode(namespace));

        // Unknown handles and bad arguments.
        let mut out = bytes(b"");
        assert_eq!(
            aster_enrollment_status(rt, 999_999, &mut out),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        let mut op = 0;
        assert_eq!(
            aster_enrollment_connect(rt, enrollment, bytes(b"[]"), 0, &mut op),
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        );
        let mut issuer = 0;
        assert_eq!(
            aster_enrollment_issuer_new(rt, enrollment, bytes(b"not-a-chain"), 0, &mut issuer),
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        );
        // A delegation whose leaf names another key.
        let delegation = delegation();
        assert_eq!(
            aster_enrollment_issuer_new(
                rt,
                enrollment,
                bytes(delegation.as_bytes()),
                0,
                &mut issuer
            ),
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        );

        assert_eq!(aster_enrollment_free(rt, enrollment), OK);
        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
}
