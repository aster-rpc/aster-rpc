#![cfg(feature = "binding-support")]
//! `conformance/vectors/upcalls.json`: the UCv1 upcall payload encodings
//! (`docs/_internal/bindings/upcall-design.md` §3), the kind / status /
//! cancel-reason codes, and malformed inputs every decoder must refuse.
//! Bindings that decode or encode upcall payloads themselves (C ABI
//! consumers) assert against the same file.
//!
//! The placement cases need `coordination-client`; without it they are
//! skipped (not removed). Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster --features binding-support,coordination-client --test upcall_vectors`

use std::path::PathBuf;
use std::time::Duration;

use aster::binding_support::upcall::codec::{
    self, ExposeHttpRequest, ExposeHttpResponse, ExposeRouteSpec, HandshakeDecision, HttpBodyMode,
    TopologyViewParts,
};
use aster::binding_support::upcall::{CancelReason, UpcallKind, UpcallStatus};
use aster::lease::{DenyReason, Fence, FenceRejection, LeaseOp, LeaseSnapshot, TransitionOutcome};
use aster::topology::ClusterView;
use serde_json::{json, Value};

const FILE: &str = "upcalls.json";

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/vectors")
        .join(FILE)
}

/// One valid case: name, message type, a JSON rendering of the value for
/// humans and other languages, and the bytes.
struct Case {
    name: &'static str,
    ty: &'static str,
    value: Value,
    bytes: Vec<u8>,
    /// Decode the bytes and re-encode them (must round-trip exactly).
    reencode: fn(&[u8]) -> Vec<u8>,
}

fn h(b: u8) -> String {
    format!("{b:02x}").repeat(32)
}

fn fence() -> Fence {
    Fence {
        resource: "tree/42".into(),
        epoch: 7,
        holder: h(0xaa),
    }
}

fn lease_cases() -> Vec<Case> {
    let acquire = LeaseOp::Acquire {
        candidate: h(0xaa),
        ttl: Duration::from_millis(10_000),
    };
    let renew = LeaseOp::Renew {
        holder: h(0xaa),
        epoch: 7,
    };
    let release = LeaseOp::Release {
        holder: h(0xaa),
        epoch: 300,
    };
    let revoke = LeaseOp::Revoke {
        authority: "root".into(),
    };
    let snap_held = LeaseSnapshot {
        resource: "tree/42".into(),
        epoch: 7,
        holder: Some(h(0xaa)),
        held: true,
        remaining: Some(Duration::from_millis(4_500)),
    };
    let snap_free = LeaseSnapshot {
        resource: "free".into(),
        epoch: 0,
        holder: None,
        held: false,
        remaining: None,
    };
    let t = |op: &LeaseOp| codec::encode_lease_transition_request("tree/42", op);
    let rt_req = |b: &[u8]| {
        let (r, op) = codec::decode_lease_transition_request(b).unwrap();
        codec::encode_lease_transition_request(&r, &op)
    };
    let rt_out =
        |b: &[u8]| codec::encode_transition_outcome(&codec::decode_transition_outcome(b).unwrap());
    vec![
        Case {
            name: "lease transition: acquire",
            ty: "LeaseTransitionRequest",
            value: json!({"resource": "tree/42", "op": {"acquire": {"candidate": h(0xaa), "ttl_ms": 10000}}}),
            bytes: t(&acquire),
            reencode: rt_req,
        },
        Case {
            name: "lease transition: renew",
            ty: "LeaseTransitionRequest",
            value: json!({"resource": "tree/42", "op": {"renew": {"holder": h(0xaa), "epoch": 7}}}),
            bytes: t(&renew),
            reencode: rt_req,
        },
        Case {
            name: "lease transition: release (multi-byte epoch)",
            ty: "LeaseTransitionRequest",
            value: json!({"resource": "tree/42", "op": {"release": {"holder": h(0xaa), "epoch": 300}}}),
            bytes: t(&release),
            reencode: rt_req,
        },
        Case {
            name: "lease transition: revoke",
            ty: "LeaseTransitionRequest",
            value: json!({"resource": "tree/42", "op": {"revoke": {"authority": "root"}}}),
            bytes: t(&revoke),
            reencode: rt_req,
        },
        Case {
            name: "outcome: granted",
            ty: "TransitionOutcome",
            value: json!({"granted": {"resource": "tree/42", "epoch": 7, "holder": h(0xaa)}}),
            bytes: codec::encode_transition_outcome(&TransitionOutcome::Granted(fence())),
            reencode: rt_out,
        },
        Case {
            name: "outcome: renewed",
            ty: "TransitionOutcome",
            value: json!("renewed"),
            bytes: codec::encode_transition_outcome(&TransitionOutcome::Renewed),
            reencode: rt_out,
        },
        Case {
            name: "outcome: released",
            ty: "TransitionOutcome",
            value: json!("released"),
            bytes: codec::encode_transition_outcome(&TransitionOutcome::Released),
            reencode: rt_out,
        },
        Case {
            name: "outcome: revoked",
            ty: "TransitionOutcome",
            value: json!("revoked"),
            bytes: codec::encode_transition_outcome(&TransitionOutcome::Revoked),
            reencode: rt_out,
        },
        Case {
            name: "outcome: denied held_by_other",
            ty: "TransitionOutcome",
            value: json!({"denied": {"reason": "held_by_other", "snapshot": {"resource": "tree/42", "epoch": 7, "holder": h(0xaa), "held": true, "remaining_ms": 4500}}}),
            bytes: codec::encode_transition_outcome(&TransitionOutcome::Denied(
                DenyReason::HeldByOther,
                snap_held.clone(),
            )),
            reencode: rt_out,
        },
        Case {
            name: "outcome: denied unauthorized (free row)",
            ty: "TransitionOutcome",
            value: json!({"denied": {"reason": "unauthorized", "snapshot": {"resource": "free", "epoch": 0, "holder": null, "held": false, "remaining_ms": null}}}),
            bytes: codec::encode_transition_outcome(&TransitionOutcome::Denied(
                DenyReason::Unauthorized,
                snap_free.clone(),
            )),
            reencode: rt_out,
        },
        Case {
            name: "snapshot request",
            ty: "LeaseSnapshotRequest",
            value: json!({"resource": "tree/42"}),
            bytes: codec::encode_lease_snapshot_request("tree/42"),
            reencode: |b| {
                codec::encode_lease_snapshot_request(
                    &codec::decode_lease_snapshot_request(b).unwrap(),
                )
            },
        },
        Case {
            name: "snapshot: held",
            ty: "LeaseSnapshot",
            value: json!({"resource": "tree/42", "epoch": 7, "holder": h(0xaa), "held": true, "remaining_ms": 4500}),
            bytes: codec::encode_lease_snapshot(&snap_held),
            reencode: |b| codec::encode_lease_snapshot(&codec::decode_lease_snapshot(b).unwrap()),
        },
        Case {
            name: "snapshot: free",
            ty: "LeaseSnapshot",
            value: json!({"resource": "free", "epoch": 0, "holder": null, "held": false, "remaining_ms": null}),
            bytes: codec::encode_lease_snapshot(&snap_free),
            reencode: |b| codec::encode_lease_snapshot(&codec::decode_lease_snapshot(b).unwrap()),
        },
        Case {
            name: "authorize: acquire",
            ty: "LeaseAuthorizeRequest",
            value: json!({"op": {"acquire": {"candidate": h(0xaa), "ttl_ms": 10000}}}),
            bytes: codec::encode_lease_authorize_request(&acquire),
            reencode: |b| {
                codec::encode_lease_authorize_request(
                    &codec::decode_lease_authorize_request(b).unwrap(),
                )
            },
        },
        Case {
            name: "fenced mutate request",
            ty: "FencedMutateRequest",
            value: json!({"fence": {"resource": "tree/42", "epoch": 7, "holder": h(0xaa)}, "writer": h(0xaa), "op_hex": "010203"}),
            bytes: codec::encode_fenced_mutate_request(&fence(), &h(0xaa), &[1, 2, 3]),
            reencode: |b| {
                let (f, w, op) = codec::decode_fenced_mutate_request(b).unwrap();
                codec::encode_fenced_mutate_request(&f, &w, &op)
            },
        },
        Case {
            name: "fenced mutate: applied",
            ty: "FencedMutateResponse",
            value: json!({"applied": {"output_hex": "6f6b"}}),
            bytes: codec::encode_fenced_mutate_response(&Ok(b"ok".to_vec())),
            reencode: |b| {
                codec::encode_fenced_mutate_response(
                    &codec::decode_fenced_mutate_response(b).unwrap(),
                )
            },
        },
        Case {
            name: "fenced mutate: stale epoch",
            ty: "FencedMutateResponse",
            value: json!({"rejected": {"reason": "stale_epoch", "carried": 6, "current": 7}}),
            bytes: codec::encode_fenced_mutate_response(&Err(FenceRejection::StaleEpoch {
                carried: 6,
                current: 7,
            })),
            reencode: |b| {
                codec::encode_fenced_mutate_response(
                    &codec::decode_fenced_mutate_response(b).unwrap(),
                )
            },
        },
        Case {
            name: "fenced mutate: not holder",
            ty: "FencedMutateResponse",
            value: json!({"rejected": {"reason": "not_holder", "carried": 0, "current": 0}}),
            bytes: codec::encode_fenced_mutate_response(&Err(FenceRejection::NotHolder)),
            reencode: |b| {
                codec::encode_fenced_mutate_response(
                    &codec::decode_fenced_mutate_response(b).unwrap(),
                )
            },
        },
        Case {
            name: "fenced mutate: not held",
            ty: "FencedMutateResponse",
            value: json!({"rejected": {"reason": "not_held", "carried": 0, "current": 0}}),
            bytes: codec::encode_fenced_mutate_response(&Err(FenceRejection::NotHeld)),
            reencode: |b| {
                codec::encode_fenced_mutate_response(
                    &codec::decode_fenced_mutate_response(b).unwrap(),
                )
            },
        },
        Case {
            name: "fenced mutate: future epoch",
            ty: "FencedMutateResponse",
            value: json!({"rejected": {"reason": "future_epoch", "carried": 9, "current": 7}}),
            bytes: codec::encode_fenced_mutate_response(&Err(FenceRejection::FutureEpoch {
                carried: 9,
                current: 7,
            })),
            reencode: |b| {
                codec::encode_fenced_mutate_response(
                    &codec::decode_fenced_mutate_response(b).unwrap(),
                )
            },
        },
    ]
}

fn other_cases() -> Vec<Case> {
    let view = TopologyViewParts {
        clusters: vec![
            ClusterView {
                members: vec![h(0x0a), h(0x0b)],
                bridge: h(0x0b),
                witnesses: vec![h(0x0b)],
            },
            ClusterView {
                members: vec![h(0x0c)],
                bridge: h(0x0c),
                witnesses: vec![h(0x0c)],
            },
        ],
        far_pairs: vec![(h(0x0a), h(0x0c))],
        separation_coverage: 1,
    };
    let rt_dec = |b: &[u8]| codec::encode_decision(codec::decode_decision(b).unwrap());
    let rt_hs = |b: &[u8]| {
        codec::encode_admission_handshake_response(
            &codec::decode_admission_handshake_response(b).unwrap(),
        )
    };
    vec![
        Case {
            name: "decision: allow",
            ty: "Decision",
            value: json!(true),
            bytes: codec::encode_decision(true),
            reencode: rt_dec,
        },
        Case {
            name: "decision: deny",
            ty: "Decision",
            value: json!(false),
            bytes: codec::encode_decision(false),
            reencode: rt_dec,
        },
        Case {
            name: "topology admitted request",
            ty: "TopologyAdmittedRequest",
            value: json!({"node_ids": [h(0x0a), h(0x0b)]}),
            bytes: codec::encode_topology_admitted_request(&[h(0x0a), h(0x0b)]),
            reencode: |b| {
                codec::encode_topology_admitted_request(
                    &codec::decode_topology_admitted_request(b).unwrap(),
                )
            },
        },
        Case {
            name: "topology admitted request: empty",
            ty: "TopologyAdmittedRequest",
            value: json!({"node_ids": []}),
            bytes: codec::encode_topology_admitted_request(&[]),
            reencode: |b| {
                codec::encode_topology_admitted_request(
                    &codec::decode_topology_admitted_request(b).unwrap(),
                )
            },
        },
        Case {
            name: "topology admitted response",
            ty: "TopologyAdmittedResponse",
            value: json!({"admitted": [true, false]}),
            bytes: codec::encode_topology_admitted_response(&[true, false]),
            reencode: |b| {
                codec::encode_topology_admitted_response(
                    &codec::decode_topology_admitted_response(b).unwrap(),
                )
            },
        },
        Case {
            name: "topology snapshot request",
            ty: "TopologySnapshotRequest",
            value: json!({}),
            bytes: codec::encode_topology_snapshot_request(),
            reencode: |b| {
                codec::decode_topology_snapshot_request(b).unwrap();
                codec::encode_topology_snapshot_request()
            },
        },
        Case {
            name: "topology snapshot: two clusters",
            ty: "TopologySnapshot",
            value: json!({"clusters": [
                {"members": [h(0x0a), h(0x0b)], "bridge": h(0x0b), "witnesses": [h(0x0b)]},
                {"members": [h(0x0c)], "bridge": h(0x0c), "witnesses": [h(0x0c)]}],
                "far_pairs": [[h(0x0a), h(0x0c)]], "separation_coverage": 1}),
            bytes: codec::encode_topology_snapshot(&view),
            reencode: |b| {
                codec::encode_topology_snapshot(&codec::decode_topology_snapshot(b).unwrap())
            },
        },
        Case {
            name: "expose authorize request",
            ty: "ExposeAuthorizeRequest",
            value: json!({"service_id": "signaling", "peer_id": h(0xaa), "metadata_hex": "746f6b"}),
            bytes: codec::encode_expose_authorize_request("signaling", &h(0xaa), b"tok"),
            reencode: |b| {
                let (s, p, m) = codec::decode_expose_authorize_request(b).unwrap();
                codec::encode_expose_authorize_request(&s, &p, &m)
            },
        },
        Case {
            name: "admission connect request",
            ty: "AdmissionConnectRequest",
            value: json!({"peer": h(0xaa), "alpn": "aster/1"}),
            bytes: codec::encode_admission_connect_request(&h(0xaa), b"aster/1"),
            reencode: |b| {
                let (p, a) = codec::decode_admission_connect_request(b).unwrap();
                codec::encode_admission_connect_request(&p, &a)
            },
        },
        Case {
            name: "admission handshake request",
            ty: "AdmissionHandshakeRequest",
            value: json!({"peer": h(0xaa), "alpn": "aster/1", "is_alive": true}),
            bytes: codec::encode_admission_handshake_request(&h(0xaa), b"aster/1", true),
            reencode: |b| {
                let (p, a, l) = codec::decode_admission_handshake_request(b).unwrap();
                codec::encode_admission_handshake_request(&p, &a, l)
            },
        },
        Case {
            name: "admission handshake: accept",
            ty: "AdmissionHandshakeResponse",
            value: json!({"accept": true, "code": 0, "reason": ""}),
            bytes: codec::encode_admission_handshake_response(&HandshakeDecision::Accept),
            reencode: rt_hs,
        },
        Case {
            name: "admission handshake: reject",
            ty: "AdmissionHandshakeResponse",
            value: json!({"accept": false, "code": 403, "reason": "denied"}),
            bytes: codec::encode_admission_handshake_response(&HandshakeDecision::Reject {
                code: 403,
                reason: b"denied".to_vec(),
            }),
            reencode: rt_hs,
        },
    ]
}

fn auth_cases() -> Vec<Case> {
    let metadata = std::collections::HashMap::from([
        ("authorization".into(), "Bearer token".into()),
        ("aster-session-id".into(), "counter/7".into()),
    ]);
    let response = codec::RpcAuthResponse::Authenticated {
        principal: "user/alice".into(),
        attributes: std::collections::HashMap::from([
            ("aster.role".into(), "operator".into()),
            ("tenant".into(), "one".into()),
        ]),
    };
    let unicode_metadata = std::collections::HashMap::from([
        ("\u{e000}".into(), "BMP".into()),
        ("\u{10000}".into(), "supplementary".into()),
    ]);
    vec![
        Case {
            name: "RPC authentication request",
            ty: "RpcAuthRequest",
            value: json!({"peer_id":"127.0.0.1:1234","service":"Counter","method":"read","session_id":7,"metadata":metadata}),
            bytes: codec::encode_rpc_auth_request(
                "127.0.0.1:1234",
                "Counter",
                "read",
                7,
                &metadata,
            ),
            reencode: |b| {
                let r = codec::decode_rpc_auth_request(b).unwrap();
                codec::encode_rpc_auth_request(
                    &r.peer_id,
                    &r.service,
                    &r.method,
                    r.session_id,
                    &r.metadata,
                )
            },
        },
        Case {
            name: "RPC authentication request: signed session boundary",
            ty: "RpcAuthRequest",
            value: json!({"peer_id":"peer","service":"svc","method":"method","session_id":i32::MIN,"metadata":{}}),
            bytes: codec::encode_rpc_auth_request(
                "peer",
                "svc",
                "method",
                i32::MIN,
                &std::collections::HashMap::new(),
            ),
            reencode: |b| {
                let r = codec::decode_rpc_auth_request(b).unwrap();
                codec::encode_rpc_auth_request(
                    &r.peer_id,
                    &r.service,
                    &r.method,
                    r.session_id,
                    &r.metadata,
                )
            },
        },
        Case {
            name: "RPC authentication: accepted",
            ty: "RpcAuthResponse",
            value: json!({"accepted":true,"principal":"user/alice","attributes":{"aster.role":"operator","tenant":"one"}}),
            bytes: codec::encode_rpc_auth_response(&response).unwrap(),
            reencode: |b| {
                codec::encode_rpc_auth_response(&codec::decode_rpc_auth_response(b).unwrap())
                    .unwrap()
            },
        },
        Case {
            name: "RPC authentication request: UTF-8 map order",
            ty: "RpcAuthRequest",
            value: json!({"peer_id":"peer","service":"svc","method":"method","session_id":0,"metadata":unicode_metadata}),
            bytes: codec::encode_rpc_auth_request("peer", "svc", "method", 0, &unicode_metadata),
            reencode: |b| {
                let r = codec::decode_rpc_auth_request(b).unwrap();
                codec::encode_rpc_auth_request(
                    &r.peer_id,
                    &r.service,
                    &r.method,
                    r.session_id,
                    &r.metadata,
                )
            },
        },
        Case {
            name: "RPC authentication: rejected",
            ty: "RpcAuthResponse",
            value: json!({"accepted":false}),
            bytes: codec::encode_rpc_auth_response(&codec::RpcAuthResponse::Rejected).unwrap(),
            reencode: |b| {
                codec::encode_rpc_auth_response(&codec::decode_rpc_auth_response(b).unwrap())
                    .unwrap()
            },
        },
    ]
}

fn hdrs(v: &[(&str, &[u8])]) -> Vec<codec::HttpHeader> {
    v.iter().map(|(n, x)| (n.to_string(), x.to_vec())).collect()
}

fn http_get() -> ExposeHttpRequest {
    ExposeHttpRequest {
        service_id: "app".into(),
        peer_id: Some(h(0xaa)),
        stream_id: 7,
        version: 2,
        method: "GET".into(),
        uri: "/greet?x=1".into(),
        headers: hdrs(&[("host", b"app.local"), ("accept", b"*/*")]),
        body: HttpBodyMode::Stream,
    }
}

fn http_post() -> ExposeHttpRequest {
    ExposeHttpRequest {
        service_id: "app".into(),
        peer_id: None,
        stream_id: 300,
        version: 3,
        method: "POST".into(),
        uri: "/upload".into(),
        headers: hdrs(&[("content-length", b"5"), ("x-bin", &[0xff, 0x00])]),
        body: HttpBodyMode::Inline(b"hello".to_vec()),
    }
}

fn route_specs(protocol_b: u8) -> Vec<ExposeRouteSpec> {
    vec![
        ExposeRouteSpec {
            host: "a.example".into(),
            port: 443,
            protocol: 0,
            service_id: "web".into(),
        },
        ExposeRouteSpec {
            host: String::new(),
            port: 5432,
            protocol: protocol_b,
            service_id: "db".into(),
        },
    ]
}

/// R9 expose HTTP payloads (kinds 49, 50). Appended after the R6 cases.
fn expose_http_cases() -> Vec<Case> {
    let rt_req = |b: &[u8]| {
        codec::encode_expose_http_request(&codec::decode_expose_http_request(b).unwrap())
    };
    let rt_resp = |b: &[u8]| {
        codec::encode_expose_http_response(&codec::decode_expose_http_response(b).unwrap())
    };
    let rt_trailers = |b: &[u8]| {
        codec::encode_expose_http_trailers(&codec::decode_expose_http_trailers(b).unwrap())
    };
    let rt_grant =
        |b: &[u8]| codec::encode_expose_route_grant(&codec::decode_expose_route_grant(b).unwrap());
    vec![
        Case {
            name: "expose http request: GET, streamed body",
            ty: "ExposeHttpRequest",
            value: json!({"service_id": "app", "peer_id": h(0xaa), "stream_id": 7, "version": 2,
                "method": "GET", "uri": "/greet?x=1",
                "headers": [{"name": "host", "value": "app.local"}, {"name": "accept", "value": "*/*"}],
                "body": "stream"}),
            bytes: codec::encode_expose_http_request(&http_get()),
            reencode: rt_req,
        },
        Case {
            name: "expose http request: POST inline, no peer, HTTP/2, binary header",
            ty: "ExposeHttpRequest",
            value: json!({"service_id": "app", "peer_id": null, "stream_id": 300, "version": 3,
                "method": "POST", "uri": "/upload",
                "headers": [{"name": "content-length", "value": "5"}, {"name": "x-bin", "value_hex": "ff00"}],
                "body": {"inline": "hello"}}),
            bytes: codec::encode_expose_http_request(&http_post()),
            reencode: rt_req,
        },
        Case {
            name: "expose http response: inline, repeated header",
            ty: "ExposeHttpResponse",
            value: json!({"status": 200, "headers": [
                {"name": "content-type", "value": "text/plain"},
                {"name": "set-cookie", "value": "a=1"}, {"name": "set-cookie", "value": "b=2"}],
                "body": {"inline": "hi"}}),
            bytes: codec::encode_expose_http_response(&ExposeHttpResponse {
                status: 200,
                headers: hdrs(&[
                    ("content-type", b"text/plain"),
                    ("set-cookie", b"a=1"),
                    ("set-cookie", b"b=2"),
                ]),
                body: HttpBodyMode::Inline(b"hi".to_vec()),
            }),
            reencode: rt_resp,
        },
        Case {
            name: "expose http response: streamed, no headers",
            ty: "ExposeHttpResponse",
            value: json!({"status": 404, "headers": [], "body": "stream"}),
            bytes: codec::encode_expose_http_response(&ExposeHttpResponse {
                status: 404,
                headers: Vec::new(),
                body: HttpBodyMode::Stream,
            }),
            reencode: rt_resp,
        },
        Case {
            name: "expose http trailers",
            ty: "ExposeHttpTrailers",
            value: json!([{"name": "grpc-status", "value": "0"}, {"name": "grpc-message", "value": "ok"}]),
            bytes: codec::encode_expose_http_trailers(&hdrs(&[
                ("grpc-status", b"0"),
                ("grpc-message", b"ok"),
            ])),
            reencode: rt_trailers,
        },
        Case {
            name: "expose http trailers: empty",
            ty: "ExposeHttpTrailers",
            value: json!([]),
            bytes: codec::encode_expose_http_trailers(&[]),
            reencode: rt_trailers,
        },
        Case {
            name: "expose route policy request",
            ty: "ExposeRoutePolicyRequest",
            value: json!({"peer_id": h(0xaa), "metadata_hex": "746f6b", "specs": [
                {"host": "a.example", "port": 443, "protocol": "http", "service_id": "web"},
                {"host": "", "port": 5432, "protocol": "tcp", "service_id": "db"}]}),
            bytes: codec::encode_expose_route_policy_request(&h(0xaa), b"tok", &route_specs(1)),
            reencode: |b| {
                let (p, m, s) = codec::decode_expose_route_policy_request(b).unwrap();
                codec::encode_expose_route_policy_request(&p, &m, &s)
            },
        },
        Case {
            name: "expose route grant",
            ty: "ExposeRouteGrant",
            value: json!({"granted": [1]}),
            bytes: codec::encode_expose_route_grant(&[1]),
            reencode: rt_grant,
        },
        Case {
            name: "expose route grant: none",
            ty: "ExposeRouteGrant",
            value: json!({"granted": []}),
            bytes: codec::encode_expose_route_grant(&[]),
            reencode: rt_grant,
        },
    ]
}

/// Malformed R9 payloads, built from the (non-validating) encoders.
fn expose_http_invalid() -> Vec<(&'static str, &'static str, String)> {
    let mut v5 = http_get();
    v5.version = 5;
    let mut no_method = http_get();
    no_method.method.clear();
    let resp = |status: u16, name: &str, body: HttpBodyMode| {
        codec::encode_expose_http_response(&ExposeHttpResponse {
            status,
            headers: hdrs(&[(name, b"x")]),
            body,
        })
    };
    let mut bad_mode = resp(200, "x", HttpBodyMode::Stream);
    *bad_mode.last_mut().unwrap() = 0x02;
    vec![
        (
            "unknown HTTP version",
            "ExposeHttpRequest",
            hex::encode(codec::encode_expose_http_request(&v5)),
        ),
        (
            "empty method",
            "ExposeHttpRequest",
            hex::encode(codec::encode_expose_http_request(&no_method)),
        ),
        (
            "uppercase header name",
            "ExposeHttpResponse",
            hex::encode(resp(200, "X-Up", HttpBodyMode::Stream)),
        ),
        (
            "empty header name",
            "ExposeHttpResponse",
            hex::encode(resp(200, "", HttpBodyMode::Stream)),
        ),
        (
            "status below 100",
            "ExposeHttpResponse",
            hex::encode(resp(99, "x", HttpBodyMode::Stream)),
        ),
        (
            "status above 999",
            "ExposeHttpResponse",
            hex::encode(resp(1000, "x", HttpBodyMode::Stream)),
        ),
        (
            "unknown body mode",
            "ExposeHttpResponse",
            hex::encode(&bad_mode),
        ),
        (
            "unknown route protocol",
            "ExposeRoutePolicyRequest",
            hex::encode(codec::encode_expose_route_policy_request(
                "p",
                b"",
                &route_specs(2),
            )),
        ),
        (
            "route grant not ascending",
            "ExposeRouteGrant",
            hex::encode(codec::encode_expose_route_grant(&[1, 0])),
        ),
    ]
}

#[cfg(feature = "coordination-client")]
fn placement_cases() -> Vec<Case> {
    use std::collections::{BTreeMap, BTreeSet};

    use aster::coordination::client::manager::ProvisionRequest;
    use aster::coordination::client::placement::{PlacementCandidate, PlacementPolicy};
    use aster::coordination::contract::{LogIdV1, MemberSeedV1};
    use aster::coordination::ids::{CoordinationGroupId, CoordinationNodeId};

    let id = |b: u8| CoordinationNodeId::from_array([b; 32]);
    let seed = MemberSeedV1 {
        node_id: vec![0x01; 32],
        ticket: "aster1ticket".into(),
        signed_join_permit: Some(vec![0xde, 0xad]),
    };
    let mut eligible = BTreeMap::new();
    eligible.insert(
        id(0x01),
        PlacementCandidate {
            node_id: id(0x01),
            member_seed: Some(seed.clone()),
        },
    );
    eligible.insert(
        id(0x02),
        PlacementCandidate {
            node_id: id(0x02),
            member_seed: None,
        },
    );
    let policy = PlacementPolicy {
        revision: 3,
        digest: [0x11; 32],
        authority_public_key: [0x22; 32],
        authority_domain: "example.org".into(),
        target_voters: 3,
        target_retained_learners: 1,
        eligible,
        pinned_voters: BTreeSet::from([id(0x01)]),
        excluded: BTreeSet::from([id(0x03)]),
        minimum_separated_voter_pairs: 1,
        replacement_grace: Duration::from_millis(600_000),
        allow_healthy_rebalance: false,
    };
    let req = ProvisionRequest {
        group_id: CoordinationGroupId::from_array([0x33; 32]),
        policy_digest: [0x11; 32],
        membership_log_id: Some(LogIdV1 {
            term: 2,
            leader_node_id: vec![0x01; 32],
            index: 130,
        }),
        node_id: id(0x02),
    };
    let req_none = ProvisionRequest {
        membership_log_id: None,
        ..req.clone()
    };
    let rt_prov =
        |b: &[u8]| codec::encode_provision_request(&codec::decode_provision_request(b).unwrap());
    vec![
        Case {
            name: "placement policy request",
            ty: "PlacementPolicyRequest",
            value: json!({}),
            bytes: codec::encode_placement_policy_request(),
            reencode: |b| {
                codec::decode_placement_policy_request(b).unwrap();
                codec::encode_placement_policy_request()
            },
        },
        Case {
            name: "placement policy",
            ty: "PlacementPolicy",
            value: json!({
                "revision": 3, "digest": "11".repeat(32), "authority_public_key": "22".repeat(32),
                "authority_domain": "example.org", "target_voters": 3, "target_retained_learners": 1,
                "eligible": [
                    {"node_id": h(0x01), "member_seed": {"node_id": h(0x01), "ticket": "aster1ticket", "signed_join_permit": "dead"}},
                    {"node_id": h(0x02), "member_seed": null}],
                "pinned_voters": [h(0x01)], "excluded": [h(0x03)],
                "minimum_separated_voter_pairs": 1, "replacement_grace_ms": 600000,
                "allow_healthy_rebalance": false}),
            bytes: codec::encode_placement_policy(&policy),
            reencode: |b| {
                codec::encode_placement_policy(&codec::decode_placement_policy(b).unwrap())
            },
        },
        Case {
            name: "provision request",
            ty: "PlacementProvisionRequest",
            value: json!({"group_id": "33".repeat(32), "policy_digest": "11".repeat(32),
                "membership_log_id": {"term": 2, "leader_node_id": h(0x01), "index": 130}, "node_id": h(0x02)}),
            bytes: codec::encode_provision_request(&req),
            reencode: rt_prov,
        },
        Case {
            name: "provision request: no log id",
            ty: "PlacementProvisionRequest",
            value: json!({"group_id": "33".repeat(32), "policy_digest": "11".repeat(32),
                "membership_log_id": null, "node_id": h(0x02)}),
            bytes: codec::encode_provision_request(&req_none),
            reencode: rt_prov,
        },
        Case {
            name: "member seed",
            ty: "MemberSeed",
            value: json!({"node_id": h(0x01), "ticket": "aster1ticket", "signed_join_permit": "dead"}),
            bytes: codec::encode_member_seed(&seed),
            reencode: |b| codec::encode_member_seed(&codec::decode_member_seed(b).unwrap()),
        },
    ]
}

#[cfg(not(feature = "coordination-client"))]
fn placement_cases() -> Vec<Case> {
    Vec::new()
}

const PLACEMENT_TYPES: [&str; 4] = [
    "PlacementPolicyRequest",
    "PlacementPolicy",
    "PlacementProvisionRequest",
    "MemberSeed",
];

/// Malformed inputs: (name, type, hex, reason).
fn invalid() -> Vec<(&'static str, &'static str, String)> {
    let snap = codec::encode_lease_snapshot_request("r");
    let mut trailing = snap.clone();
    trailing.push(0);
    let mut wrong_version = snap.clone();
    wrong_version[1] = 0x02;
    vec![
        ("empty", "LeaseSnapshotRequest", String::new()),
        (
            "wrong tag (decision bytes)",
            "LeaseSnapshotRequest",
            hex::encode(codec::encode_decision(true)),
        ),
        (
            "unsupported version",
            "LeaseSnapshotRequest",
            hex::encode(&wrong_version),
        ),
        (
            "trailing byte",
            "LeaseSnapshotRequest",
            hex::encode(&trailing),
        ),
        (
            "non-minimal string header",
            "LeaseSnapshotRequest",
            "b301860072".into(),
        ),
        ("bad bool byte", "Decision", "cf0102".into()),
        ("unknown lease op", "LeaseAuthorizeRequest", "b50109".into()),
        ("unknown outcome", "TransitionOutcome", "b20109".into()),
        (
            "non-minimal epoch varint",
            "TransitionOutcome",
            "b201011e747265652f3432870002".into(),
        ),
        (
            "not_holder with epochs",
            "FencedMutateResponse",
            "b70101020100".into(),
        ),
        (
            "accept with a reason",
            "AdmissionHandshakeResponse",
            "c30101000178".into(),
        ),
        (
            "list count beyond payload",
            "TopologyAdmittedResponse",
            "b901050c01".into(),
        ),
        (
            "authentication empty principal",
            "RpcAuthResponse",
            "ca010102000c".into(),
        ),
        (
            "authentication invalid bool",
            "RpcAuthResponse",
            "ca0102".into(),
        ),
        (
            "authentication rejected with trailing fields",
            "RpcAuthResponse",
            "ca010000".into(),
        ),
        (
            "authentication attributes not ascending",
            "RpcAuthResponse",
            "ca01010678020c066202066102".into(),
        ),
        (
            "authentication duplicate attribute",
            "RpcAuthResponse",
            "ca01010678020c066102066102".into(),
        ),
        (
            "authentication session out of range",
            "RpcAuthRequest",
            "c9010202028080808010000c".into(),
        ),
        (
            "authentication nonminimal session",
            "RpcAuthRequest",
            "c9010202028000000c".into(),
        ),
        (
            "authentication metadata not ascending",
            "RpcAuthRequest",
            "c90102020200020c066202066102".into(),
        ),
    ]
    .into_iter()
    .chain(expose_http_invalid())
    .collect()
}

fn decode_by_type(ty: &str, b: &[u8]) -> anyhow::Result<()> {
    match ty {
        "LeaseSnapshotRequest" => codec::decode_lease_snapshot_request(b).map(drop),
        "Decision" => codec::decode_decision(b).map(drop),
        "LeaseAuthorizeRequest" => codec::decode_lease_authorize_request(b).map(drop),
        "TransitionOutcome" => codec::decode_transition_outcome(b).map(drop),
        "FencedMutateResponse" => codec::decode_fenced_mutate_response(b).map(drop),
        "AdmissionHandshakeResponse" => codec::decode_admission_handshake_response(b).map(drop),
        "TopologyAdmittedResponse" => codec::decode_topology_admitted_response(b).map(drop),
        "ExposeHttpRequest" => codec::decode_expose_http_request(b).map(drop),
        "ExposeHttpResponse" => codec::decode_expose_http_response(b).map(drop),
        "ExposeRoutePolicyRequest" => codec::decode_expose_route_policy_request(b).map(drop),
        "ExposeRouteGrant" => codec::decode_expose_route_grant(b).map(drop),
        "RpcAuthRequest" => codec::decode_rpc_auth_request(b).map(drop),
        "RpcAuthResponse" => codec::decode_rpc_auth_response(b).map(drop),
        other => panic!("no decoder for {other}"),
    }
}

fn render() -> Value {
    let mut cases: Vec<Case> = lease_cases();
    cases.extend(other_cases());
    cases.extend(placement_cases());
    cases.extend(expose_http_cases());
    cases.extend(auth_cases());
    let kinds: Vec<Value> = UpcallKind::ALL
        .iter()
        .map(|k| json!({"code": k.code(), "name": k.name(), "decision": k.is_decision()}))
        .collect();
    json!({
        "description": "Aster upcall payloads, format UCv1 (docs/_internal/bindings/upcall-design.md section 3). An upcall event is {invocation_id:u64, kind:u16, deadline_unix_ms:u64, payload}; the binding answers respond(invocation_id, status:u32, payload).",
        "format": {
            "prefix": "tag:u8 version:u8 (0x01)",
            "uint": "LEB128 varint, minimal length, range-checked to the field width",
            "u8enum": "one raw byte",
            "bool": "0x00 | 0x01",
            "string": "varint((utf8_len << 2) | 2) then UTF-8 bytes",
            "bytes": "varint(len) then raw bytes",
            "opt<T>": "0xFD absent | 0x00 then T",
            "list<T>": "varint(count) 0x0C then each T",
            "durations": "whole milliseconds as uint",
            "sets": "lists in strictly ascending byte order, no duplicates",
            "rules": "every field present, fixed order, no trailing bytes"
        },
        "kinds": kinds,
        "statuses": [
            {"code": UpcallStatus::Ok.code(), "name": "ok", "payload": "the kind's UCv1 response"},
            {"code": UpcallStatus::Error.code(), "name": "error", "payload": "UTF-8 message"},
            {"code": UpcallStatus::Unimplemented.code(), "name": "unimplemented", "payload": "ignored"}
        ],
        "cancel_reasons": [
            {"code": CancelReason::Timeout as u8, "name": "timeout"},
            {"code": CancelReason::Abandoned as u8, "name": "abandoned"},
            {"code": CancelReason::Shutdown as u8, "name": "shutdown"}
        ],
        "valid": cases.iter().map(|c| json!({
            "name": c.name, "type": c.ty, "value": c.value, "hex": hex::encode(&c.bytes)
        })).collect::<Vec<_>>(),
        "invalid": invalid().iter().map(|(n, t, x)| json!({"name": n, "type": t, "hex": x})).collect::<Vec<_>>(),
    })
}

#[test]
fn upcall_vectors() {
    let generated = render();
    if std::env::var_os("ASTER_VECTORS_WRITE").is_some() {
        if !cfg!(feature = "coordination-client") {
            panic!("regenerate with coordination-client so the placement cases are included");
        }
        let mut s = serde_json::to_string_pretty(&generated).unwrap();
        s.push('\n');
        std::fs::write(path(), s).unwrap();
        return;
    }
    let file: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    for key in ["format", "kinds", "statuses", "cancel_reasons", "invalid"] {
        assert_eq!(file[key], generated[key], "{key} differs from {FILE}");
    }
    let gen_valid = generated["valid"].as_array().unwrap();
    let file_valid = file["valid"].as_array().unwrap();
    for f in file_valid {
        let ty = f["type"].as_str().unwrap();
        if !cfg!(feature = "coordination-client") && PLACEMENT_TYPES.contains(&ty) {
            continue;
        }
        let g = gen_valid
            .iter()
            .find(|g| g["name"] == f["name"])
            .unwrap_or_else(|| panic!("{} not generated", f["name"]));
        assert_eq!(f, g, "case {} differs", f["name"]);
    }
    assert_eq!(
        gen_valid.len(),
        file_valid
            .iter()
            .filter(|f| cfg!(feature = "coordination-client")
                || !PLACEMENT_TYPES.contains(&f["type"].as_str().unwrap()))
            .count(),
        "generated cases missing from {FILE}"
    );

    // Every valid case decodes and re-encodes to the same bytes.
    let mut cases = lease_cases();
    cases.extend(other_cases());
    cases.extend(placement_cases());
    cases.extend(expose_http_cases());
    cases.extend(auth_cases());
    for c in &cases {
        assert_eq!((c.reencode)(&c.bytes), c.bytes, "{} round trip", c.name);
    }
    // Every invalid case is refused.
    for (name, ty, x) in invalid() {
        let bytes = hex::decode(&x).unwrap();
        assert!(decode_by_type(ty, &bytes).is_err(), "{name} decoded");
    }
}

#[test]
fn set_order_is_canonical() {
    // Placement sets must be strictly ascending; a swapped pair is refused.
    #[cfg(feature = "coordination-client")]
    {
        let cases = placement_cases();
        let policy = cases.iter().find(|c| c.ty == "PlacementPolicy").unwrap();
        // eligible = [01.., 02..]: swap the two 32-byte ids in place.
        let b = &policy.bytes;
        let first = b
            .windows(33)
            .position(|w| w[0] == 32 && w[1..].iter().all(|x| *x == 0x01))
            .unwrap();
        let second = b
            .windows(33)
            .position(|w| w[0] == 32 && w[1..].iter().all(|x| *x == 0x02))
            .unwrap();
        // Rebuild with the ids swapped but seeds left in place: the order
        // check fires before anything else could.
        let mut swapped = b.clone();
        swapped[first + 1..first + 33].fill(0x02);
        swapped[second + 1..second + 33].fill(0x01);
        assert!(codec::decode_placement_policy(&swapped).is_err());
    }
}
