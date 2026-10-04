//! `conformance/vectors/coordination.json`: the UCv1 `D0`-`DE` messages of
//! the C coordination client (ABI 4.4,
//! `docs/_internal/bindings/coordination-abi-design.md` §2), with a JSON
//! rendering of each value, and malformed inputs every decoder must refuse.
//! Java / .NET / Go codecs assert against the same file.
//!
//! Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster_transport_ffi --test coordination_vectors`

use std::path::PathBuf;

use aster::coordination::client::placement::{PlacementReport, SeparationSummary};
use aster::coordination::contract::{
    FenceV1, LogIdV1, MemberSeedV1, NodeIdV1, ResourceKeyV1, ResourceSnapshotV1,
};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::raft::RecoveredFromV1;
use aster::coordination::store::wire::{
    AclGrantWireV1, AclSnapshotWireV1, AclStatusReplyWireV1, AdminSnapshotWireV1,
    AdminStatusReplyWireV1, AuditRecordWireV1, GroupStatusReplyWireV1, MembershipStatusReplyWireV1,
    MutationReplyWireV1, ProbeClientReplyWireV1, ReadReplyWireV1, RecoveryExportManifestWireV1,
    RetireClientReplyWireV1,
};
use aster_transport_ffi::coordination_codec::{self as codec, MutationRequest, ReadRequest};
use serde_json::{json, Value};

const FILE: &str = "coordination.json";

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/vectors")
        .join(FILE)
}

struct Case {
    name: &'static str,
    ty: &'static str,
    value: Value,
    bytes: Vec<u8>,
}

fn x(b: &[u8]) -> String {
    hex::encode(b)
}
fn h32(b: u8) -> [u8; 32] {
    [b; 32]
}
fn rk(r: &ResourceKeyV1) -> Value {
    json!({"namespace": r.namespace, "key": x(&r.key)})
}
fn lid(l: &LogIdV1) -> Value {
    json!({"term": l.term, "leader_node_id": x(&l.leader_node_id), "index": l.index})
}
fn olid(l: &Option<LogIdV1>) -> Value {
    l.as_ref().map_or(Value::Null, lid)
}
fn ob(b: &Option<Vec<u8>>) -> Value {
    b.as_ref().map_or(Value::Null, |b| json!(x(b)))
}
fn fence_v(f: &FenceV1) -> Value {
    json!({"group_id": x(&f.group_id), "resource": rk(&f.resource), "epoch": f.epoch, "holder": x(&f.holder)})
}
fn snap_v(s: &ResourceSnapshotV1) -> Value {
    json!({"group_id": x(&s.group_id), "resource": rk(&s.resource), "epoch": s.epoch,
        "holder": ob(&s.holder), "held": s.held, "ttl_ms": s.ttl_ms,
        "lease_revision": s.lease_revision, "value_revision": s.value_revision,
        "acl_revision": s.acl_revision, "value": ob(&s.value), "value_redacted": s.value_redacted})
}
fn ids_v(ids: &[NodeIdV1]) -> Value {
    json!(ids.iter().map(|n| x(&n.node_id)).collect::<Vec<_>>())
}
fn cids_v(ids: &[CoordinationNodeId]) -> Value {
    json!(ids.iter().map(|n| x(n.as_bytes())).collect::<Vec<_>>())
}
fn seed_v(s: &MemberSeedV1) -> Value {
    json!({"node_id": x(&s.node_id), "ticket": s.ticket, "signed_join_permit": ob(&s.signed_join_permit)})
}
fn nid(b: u8) -> NodeIdV1 {
    NodeIdV1 {
        node_id: vec![b; 32],
    }
}

fn res() -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: "jobs".into(),
        key: b"tree/42".to_vec(),
    }
}
fn log() -> LogIdV1 {
    LogIdV1 {
        term: 3,
        leader_node_id: vec![0x01; 32],
        index: 130,
    }
}
fn fence() -> FenceV1 {
    FenceV1 {
        group_id: vec![0x33; 32],
        resource: res(),
        epoch: 7,
        holder: vec![0xaa; 32],
    }
}
fn snapshot() -> ResourceSnapshotV1 {
    ResourceSnapshotV1 {
        group_id: vec![0x33; 32],
        resource: res(),
        epoch: 7,
        holder: Some(vec![0xaa; 32]),
        held: true,
        ttl_ms: 30_000,
        lease_revision: 4,
        value_revision: 2,
        acl_revision: 1,
        value: Some(b"v1".to_vec()),
        value_redacted: false,
    }
}
fn seed() -> MemberSeedV1 {
    MemberSeedV1 {
        node_id: vec![0x02; 32],
        ticket: "aster1ticket".into(),
        signed_join_permit: Some(vec![0xde, 0xad]),
    }
}

fn mutation_cases() -> Vec<Case> {
    let m = |name, value: Value, req: MutationRequest| Case {
        name,
        ty: "MutationRequest",
        value,
        bytes: codec::encode_mutation_request(&req),
    };
    let f = fence();
    vec![
        m(
            "mutate: acquire",
            json!({"op": "acquire", "resource": rk(&res()), "ttl_ms": 30000}),
            MutationRequest::Acquire {
                resource: res(),
                ttl_ms: 30_000,
            },
        ),
        m(
            "mutate: renew",
            json!({"op": "renew", "fence": fence_v(&f)}),
            MutationRequest::Renew { fence: f.clone() },
        ),
        m(
            "mutate: compare_and_set",
            json!({"op": "compare_and_set", "fence": fence_v(&f), "expected_value_revision": 2, "new_value": "7631"}),
            MutationRequest::CompareAndSet {
                fence: f.clone(),
                expected_value_revision: 2,
                new_value: b"v1".to_vec(),
            },
        ),
        m(
            "mutate: release",
            json!({"op": "release", "fence": fence_v(&f)}),
            MutationRequest::Release { fence: f.clone() },
        ),
        m(
            "mutate: release_with_value (empty value)",
            json!({"op": "release_with_value", "fence": fence_v(&f), "expected_value_revision": 0, "final_value": ""}),
            MutationRequest::ReleaseWithValue {
                fence: f.clone(),
                expected_value_revision: 0,
                final_value: vec![],
            },
        ),
        m(
            "mutate: revoke",
            json!({"op": "revoke", "resource": rk(&res()), "expected_epoch": 7}),
            MutationRequest::Revoke {
                resource: res(),
                expected_epoch: Some(7),
            },
        ),
        m(
            "mutate: revoke, any epoch",
            json!({"op": "revoke", "resource": rk(&res()), "expected_epoch": null}),
            MutationRequest::Revoke {
                resource: res(),
                expected_epoch: None,
            },
        ),
        m(
            "mutate: set_grant",
            json!({"op": "set_grant", "resource": rk(&res()), "target_node_id": x(&h32(0xbb)), "rights": 31, "expected_acl_revision": 1}),
            MutationRequest::SetGrant {
                resource: res(),
                target_node_id: h32(0xbb),
                rights: 31,
                expected_acl_revision: 1,
            },
        ),
        m(
            "mutate: set_admin",
            json!({"op": "set_admin", "target_node_id": x(&h32(0xbb)), "enabled": true, "expected_admin_revision": 0}),
            MutationRequest::SetAdmin {
                target_node_id: h32(0xbb),
                enabled: true,
                expected_admin_revision: 0,
            },
        ),
        m(
            "mutate: change_membership",
            json!({"op": "change_membership", "operation_id": x(&[0x0c; 16]),
                "expected_membership_log_id": lid(&log()), "desired_voters": [x(&h32(1)), x(&h32(2))],
                "retained_learners": [x(&h32(3))], "member_seeds": [seed_v(&seed())]}),
            MutationRequest::ChangeMembership {
                operation_id: [0x0c; 16],
                expected_membership_log_id: Some(log()),
                desired_voters: vec![h32(1), h32(2)],
                retained_learners: vec![h32(3)],
                member_seeds: vec![seed()],
            },
        ),
        m(
            "mutate: change_membership, no fence",
            json!({"op": "change_membership", "operation_id": x(&[0x0c; 16]),
                "expected_membership_log_id": null, "desired_voters": [x(&h32(1))], "retained_learners": [], "member_seeds": []}),
            MutationRequest::ChangeMembership {
                operation_id: [0x0c; 16],
                expected_membership_log_id: None,
                desired_voters: vec![h32(1)],
                retained_learners: vec![],
                member_seeds: vec![],
            },
        ),
        m(
            "mutate: cancel_membership",
            json!({"op": "cancel_membership", "operation_id": x(&[0x0c; 16]), "expected_phase_code": 2}),
            MutationRequest::CancelMembership {
                operation_id: [0x0c; 16],
                expected_phase_code: 2,
            },
        ),
    ]
}

fn read_cases() -> Vec<Case> {
    let r = |name, value: Value, req: ReadRequest| Case {
        name,
        ty: "ReadRequest",
        value,
        bytes: codec::encode_read_request(&req),
    };
    vec![
        r(
            "read: read_linearizable",
            json!({"op": "read_linearizable", "resource": rk(&res())}),
            ReadRequest::ReadLinearizable(res()),
        ),
        r(
            "read: inspect_acl",
            json!({"op": "inspect_acl", "resource": rk(&res())}),
            ReadRequest::InspectAcl(res()),
        ),
        r(
            "read: inspect_admins",
            json!({"op": "inspect_admins"}),
            ReadRequest::InspectAdmins,
        ),
        r(
            "read: membership_status",
            json!({"op": "membership_status", "operation_id": x(&[0x0c; 16])}),
            ReadRequest::MembershipStatus(Some([0x0c; 16])),
        ),
        r(
            "read: membership_status, current",
            json!({"op": "membership_status", "operation_id": null}),
            ReadRequest::MembershipStatus(None),
        ),
        r(
            "read: group_status",
            json!({"op": "group_status"}),
            ReadRequest::GroupStatus,
        ),
        r(
            "read: probe_client",
            json!({"op": "probe_client", "target_node_id": x(&h32(0xaa)), "target_client_id": x(&[0x0d; 16])}),
            ReadRequest::ProbeClient {
                target_node_id: h32(0xaa),
                target_client_id: [0x0d; 16],
            },
        ),
        r(
            "read: audit_stream",
            json!({"op": "audit_stream", "after": lid(&log()), "limit": 10000}),
            ReadRequest::AuditStream {
                after: Some(log()),
                limit: 10_000,
            },
        ),
        r(
            "read: audit_stream, from the floor",
            json!({"op": "audit_stream", "after": null, "limit": 1}),
            ReadRequest::AuditStream {
                after: None,
                limit: 1,
            },
        ),
        r(
            "read: recovery_export",
            json!({"op": "recovery_export"}),
            ReadRequest::RecoveryExport,
        ),
    ]
}

fn result_cases() -> Vec<Case> {
    let reply = MutationReplyWireV1 {
        client_id: vec![0x0d; 16],
        sequence: 9,
        request_hash: vec![0x44; 32],
        disposition_code: 1,
        outcome_code: 3,
        reason_code: 0,
        snapshot: Some(snapshot()),
        acl_revision: 1,
        admin_revision: 0,
        operation_id: Some(vec![0x0c; 16]),
        membership_log_id: Some(log()),
        ..Default::default()
    };
    let rejected = MutationReplyWireV1 {
        client_id: vec![0x0d; 16],
        sequence: 10,
        request_hash: vec![0x45; 32],
        disposition_code: 1,
        outcome_code: 100,
        reason_code: 4,
        ..Default::default()
    };
    let reply_v = |m: &MutationReplyWireV1| {
        json!({"client_id": x(&m.client_id), "sequence": m.sequence, "request_hash": x(&m.request_hash),
            "disposition_code": m.disposition_code, "outcome_code": m.outcome_code, "reason_code": m.reason_code,
            "snapshot": m.snapshot.as_ref().map_or(Value::Null, snap_v), "acl_revision": m.acl_revision,
            "admin_revision": m.admin_revision, "operation_id": ob(&m.operation_id),
            "membership_log_id": olid(&m.membership_log_id)})
    };
    let unheld = ResourceSnapshotV1 {
        holder: None,
        held: false,
        ttl_ms: 0,
        value: None,
        value_redacted: true,
        ..snapshot()
    };
    let acl = AclStatusReplyWireV1 {
        acl_snapshot: AclSnapshotWireV1 {
            resource: res(),
            acl_revision: 2,
            grants: vec![
                AclGrantWireV1 {
                    subject_node_id: vec![0xaa; 32],
                    rights: 31,
                },
                AclGrantWireV1 {
                    subject_node_id: vec![0xbb; 32],
                    rights: 1,
                },
            ],
        },
        ..Default::default()
    };
    let admins = AdminStatusReplyWireV1 {
        admin_snapshot: AdminSnapshotWireV1 {
            admin_revision: 1,
            administrators: vec![nid(0xbb)],
        },
        ..Default::default()
    };
    let membership = MembershipStatusReplyWireV1 {
        operation_id: Some(vec![0x0c; 16]),
        status_code: 2,
        expected_membership_log_id: Some(log()),
        observed_membership_log_id: None,
        desired_voters: vec![nid(1), nid(2)],
        retained_learners: vec![],
        error_code: 0,
        ..Default::default()
    };
    let group = GroupStatusReplyWireV1 {
        lifecycle_code: 3,
        local_node_id: vec![0x01; 32],
        local_role_code: 1,
        leader_node_id: Some(vec![0x01; 32]),
        term: 3,
        last_log_id: Some(log()),
        committed_log_id: Some(log()),
        applied_log_id: Some(log()),
        deployment_profile_code: 1,
        membership_log_id: Some(LogIdV1 { index: 1, ..log() }),
        membership_joint: false,
        voters: vec![nid(1)],
        learners: vec![],
        pending_operation_id: None,
        quorum_available: Some(true),
        fatal_code: 0,
        audit_floor: None,
        genesis_hash: vec![0x55; 32],
        authority_public_key: vec![0x22; 32],
        authority_domain: "example.org".into(),
        recovered_from: None,
        ..Default::default()
    };
    let recovered = GroupStatusReplyWireV1 {
        quorum_available: None,
        recovered_from: Some(RecoveredFromV1 {
            source_group_id: vec![0x66; 32],
            source_genesis_hash: vec![0x67; 32],
            source_authority_public_key: vec![0x68; 32],
            source_authority_domain: "old.example.org".into(),
            export_hash: vec![0x69; 32],
            source_last_applied: Some(log()),
        }),
        ..group.clone()
    };
    let group_v = |g: &GroupStatusReplyWireV1| {
        json!({"lifecycle_code": g.lifecycle_code, "local_node_id": x(&g.local_node_id),
            "local_role_code": g.local_role_code, "leader_node_id": ob(&g.leader_node_id), "term": g.term,
            "last_log_id": olid(&g.last_log_id), "committed_log_id": olid(&g.committed_log_id),
            "applied_log_id": olid(&g.applied_log_id), "deployment_profile_code": g.deployment_profile_code,
            "membership_log_id": olid(&g.membership_log_id), "membership_joint": g.membership_joint,
            "voters": ids_v(&g.voters), "learners": ids_v(&g.learners),
            "pending_operation_id": ob(&g.pending_operation_id),
            "quorum_available": g.quorum_available, "fatal_code": g.fatal_code,
            "audit_floor": olid(&g.audit_floor), "genesis_hash": x(&g.genesis_hash),
            "authority_public_key": x(&g.authority_public_key), "authority_domain": g.authority_domain,
            "recovered_from": g.recovered_from.as_ref().map_or(Value::Null, |f| json!({
                "source_group_id": x(&f.source_group_id), "source_genesis_hash": x(&f.source_genesis_hash),
                "source_authority_public_key": x(&f.source_authority_public_key),
                "source_authority_domain": f.source_authority_domain, "export_hash": x(&f.export_hash),
                "source_last_applied": olid(&f.source_last_applied)}))})
    };
    let probe = ProbeClientReplyWireV1 {
        target_node_id: vec![0xaa; 32],
        target_client_id: vec![0x0d; 16],
        exists: true,
        last_sequence: 9,
        request_hash: vec![0x44; 32],
        disposition_code: 1,
        retired: false,
        ..Default::default()
    };
    let audit = vec![
        AuditRecordWireV1 {
            log_id: log(),
            actor_node_id: Some(vec![0xaa; 32]),
            operation_code: 1,
            resource: Some(res()),
            result_code: 1,
            epoch: 7,
            value_revision: 0,
            ..Default::default()
        },
        AuditRecordWireV1 {
            log_id: LogIdV1 {
                index: 131,
                ..log()
            },
            actor_node_id: None,
            operation_code: 8,
            resource: None,
            result_code: 100,
            epoch: 0,
            value_revision: 0,
            ..Default::default()
        },
    ];
    let manifest = RecoveryExportManifestWireV1 {
        total_len: 3,
        blake3_checksum: vec![0x77; 32],
        applied_log_id: Some(log()),
        ..Default::default()
    };
    let manifest_v =
        json!({"total_len": 3, "blake3_checksum": x(&[0x77; 32]), "applied_log_id": lid(&log())});
    let retire = RetireClientReplyWireV1 {
        outcome_code: 9,
        reason_code: 0,
        ..Default::default()
    };
    let report = PlacementReport {
        policy_revision: 3,
        policy_digest: [0x11; 32],
        membership_log_id: Some(log()),
        current_voters: vec![CoordinationNodeId::from_array(h32(1))],
        current_learners: vec![],
        selected_voters: vec![
            CoordinationNodeId::from_array(h32(1)),
            CoordinationNodeId::from_array(h32(2)),
        ],
        selected_learners: vec![],
        grace_crossed_members: vec![],
        separation: Some(SeparationSummary {
            separated_pairs: 1,
            connected_pairs: 0,
            unknown_pairs: 0,
            total_pairs: 1,
            represented_clusters: 2,
            diversity_proved: true,
        }),
        pending_operation_id: Some([0x0c; 16]),
        reason: "submitted".into(),
    };
    let stub = PlacementReport {
        policy_revision: 0,
        policy_digest: [0; 32],
        membership_log_id: None,
        current_voters: vec![],
        current_learners: vec![],
        selected_voters: vec![],
        selected_learners: vec![],
        grace_crossed_members: vec![],
        separation: None,
        pending_operation_id: None,
        reason: "no leader".into(),
    };
    let report_v = |p: &PlacementReport| {
        json!({"policy_revision": p.policy_revision, "policy_digest": x(&p.policy_digest),
            "membership_log_id": olid(&p.membership_log_id), "current_voters": cids_v(&p.current_voters),
            "current_learners": cids_v(&p.current_learners), "selected_voters": cids_v(&p.selected_voters),
            "selected_learners": cids_v(&p.selected_learners), "grace_crossed_members": cids_v(&p.grace_crossed_members),
            "separation": p.separation.as_ref().map_or(Value::Null, |s| json!({
                "separated_pairs": s.separated_pairs, "connected_pairs": s.connected_pairs,
                "unknown_pairs": s.unknown_pairs, "total_pairs": s.total_pairs,
                "represented_clusters": s.represented_clusters, "diversity_proved": s.diversity_proved})),
            "pending_operation_id": p.pending_operation_id.map_or(Value::Null, |o| json!(x(&o))),
            "reason": p.reason})
    };
    vec![
        Case {
            name: "mutation reply: mutated",
            ty: "MutationReply",
            value: reply_v(&reply),
            bytes: codec::encode_mutation_reply(&reply),
        },
        Case {
            name: "mutation reply: rejected",
            ty: "MutationReply",
            value: reply_v(&rejected),
            bytes: codec::encode_mutation_reply(&rejected),
        },
        Case {
            name: "read reply: held",
            ty: "ReadReply",
            value: snap_v(&snapshot()),
            bytes: codec::encode_read_reply(&ReadReplyWireV1 {
                snapshot: snapshot(),
                ..Default::default()
            }),
        },
        Case {
            name: "read reply: free, redacted",
            ty: "ReadReply",
            value: snap_v(&unheld),
            bytes: codec::encode_read_reply(&ReadReplyWireV1 {
                snapshot: unheld.clone(),
                ..Default::default()
            }),
        },
        Case {
            name: "acl status",
            ty: "AclStatus",
            value: json!({"resource": rk(&res()), "acl_revision": 2, "grants": [
                {"subject_node_id": x(&h32(0xaa)), "rights": 31}, {"subject_node_id": x(&h32(0xbb)), "rights": 1}]}),
            bytes: codec::encode_acl_status(&acl),
        },
        Case {
            name: "admin status",
            ty: "AdminStatus",
            value: json!({"admin_revision": 1, "administrators": [x(&h32(0xbb))]}),
            bytes: codec::encode_admin_status(&admins),
        },
        Case {
            name: "membership status",
            ty: "MembershipStatus",
            value: json!({"operation_id": x(&[0x0c; 16]), "status_code": 2, "expected_membership_log_id": lid(&log()),
                "observed_membership_log_id": null, "desired_voters": [x(&h32(1)), x(&h32(2))], "retained_learners": [], "error_code": 0}),
            bytes: codec::encode_membership_status(&membership),
        },
        Case {
            name: "group status",
            ty: "GroupStatus",
            value: group_v(&group),
            bytes: codec::encode_group_status(&group),
        },
        Case {
            name: "group status: recovered",
            ty: "GroupStatus",
            value: group_v(&recovered),
            bytes: codec::encode_group_status(&recovered),
        },
        Case {
            name: "client probe",
            ty: "ClientProbe",
            value: json!({"target_node_id": x(&h32(0xaa)), "target_client_id": x(&[0x0d; 16]), "exists": true,
                "last_sequence": 9, "request_hash": x(&[0x44; 32]), "disposition_code": 1, "retired": false}),
            bytes: codec::encode_client_probe(&probe),
        },
        Case {
            name: "audit records",
            ty: "AuditRecords",
            value: json!([
                {"log_id": lid(&log()), "actor_node_id": x(&h32(0xaa)), "operation_code": 1, "resource": rk(&res()),
                 "result_code": 1, "epoch": 7, "value_revision": 0},
                {"log_id": lid(&LogIdV1 { index: 131, ..log() }), "actor_node_id": null, "operation_code": 8, "resource": null,
                 "result_code": 100, "epoch": 0, "value_revision": 0}]),
            bytes: codec::encode_audit_records(&audit),
        },
        Case {
            name: "audit records: none",
            ty: "AuditRecords",
            value: json!([]),
            bytes: codec::encode_audit_records(&[]),
        },
        Case {
            name: "recovery export",
            ty: "RecoveryExport",
            value: json!({"total_len": 3, "blake3_checksum": x(&[0x77; 32]), "applied_log_id": lid(&log()), "body": "616263"}),
            bytes: codec::encode_recovery_export(&manifest, b"abc"),
        },
        Case {
            name: "recovery export manifest",
            ty: "RecoveryExportManifest",
            value: manifest_v,
            bytes: codec::encode_recovery_export_manifest(&manifest),
        },
        Case {
            name: "retire reply",
            ty: "RetireReply",
            value: json!({"outcome_code": 9, "reason_code": 0}),
            bytes: codec::encode_retire_reply(&retire),
        },
        Case {
            name: "coordination fence",
            ty: "CoordinationFence",
            value: fence_v(&fence()),
            bytes: codec::encode_coordination_fence(&fence()),
        },
        Case {
            name: "placement report: submitted",
            ty: "PlacementReport",
            value: report_v(&report),
            bytes: codec::encode_placement_report(&report),
        },
        Case {
            name: "placement report: stopped",
            ty: "PlacementReport",
            value: report_v(&stub),
            bytes: codec::encode_placement_report(&stub),
        },
    ]
}

fn all_cases() -> Vec<Case> {
    let mut v = mutation_cases();
    v.extend(read_cases());
    v.extend(result_cases());
    v
}

/// Decode by message type, then re-encode (`Err` for a refused input).
fn roundtrip(ty: &str, b: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(match ty {
        "MutationRequest" => codec::encode_mutation_request(&codec::decode_mutation_request(b)?),
        "ReadRequest" => codec::encode_read_request(&codec::decode_read_request(b)?),
        "MutationReply" => codec::encode_mutation_reply(&codec::decode_mutation_reply(b)?),
        "ReadReply" => codec::encode_read_reply(&ReadReplyWireV1 {
            snapshot: codec::decode_read_reply(b)?,
            ..Default::default()
        }),
        "AclStatus" => codec::encode_acl_status(&AclStatusReplyWireV1 {
            acl_snapshot: codec::decode_acl_status(b)?,
            ..Default::default()
        }),
        "AdminStatus" => codec::encode_admin_status(&AdminStatusReplyWireV1 {
            admin_snapshot: codec::decode_admin_status(b)?,
            ..Default::default()
        }),
        "MembershipStatus" => codec::encode_membership_status(&codec::decode_membership_status(b)?),
        "GroupStatus" => codec::encode_group_status(&codec::decode_group_status(b)?),
        "ClientProbe" => codec::encode_client_probe(&codec::decode_client_probe(b)?),
        "AuditRecords" => codec::encode_audit_records(&codec::decode_audit_records(b)?),
        "RecoveryExport" => {
            let (m, body) = codec::decode_recovery_export(b)?;
            codec::encode_recovery_export(&m, &body)
        }
        "RecoveryExportManifest" => {
            codec::encode_recovery_export_manifest(&codec::decode_recovery_export_manifest(b)?)
        }
        "RetireReply" => codec::encode_retire_reply(&codec::decode_retire_reply(b)?),
        "CoordinationFence" => {
            codec::encode_coordination_fence(&codec::decode_coordination_fence(b)?)
        }
        "PlacementReport" => {
            let p = codec::decode_placement_report(b)?;
            let id = |v: &Vec<Vec<u8>>| {
                v.iter()
                    .map(|b| CoordinationNodeId::from_array(b.as_slice().try_into().unwrap()))
                    .collect::<Vec<_>>()
            };
            codec::encode_placement_report(&PlacementReport {
                policy_revision: p.policy_revision,
                policy_digest: p.policy_digest,
                membership_log_id: p.membership_log_id,
                current_voters: id(&p.current_voters),
                current_learners: id(&p.current_learners),
                selected_voters: id(&p.selected_voters),
                selected_learners: id(&p.selected_learners),
                grace_crossed_members: id(&p.grace_crossed_members),
                separation: p.separation,
                pending_operation_id: p
                    .pending_operation_id
                    .map(|o| <[u8; 16]>::try_from(o.as_slice()).unwrap()),
                reason: p.reason,
            })
        }
        other => panic!("no decoder for {other}"),
    })
}

/// Malformed inputs: (name, type, bytes).
fn invalid() -> Vec<(&'static str, &'static str, Vec<u8>)> {
    let renew = codec::encode_mutation_request(&MutationRequest::Renew { fence: fence() });
    let with = |mut b: Vec<u8>, f: &dyn Fn(&mut Vec<u8>)| {
        f(&mut b);
        b
    };
    let short_holder = {
        let mut f = fence();
        f.holder.pop();
        codec::encode_mutation_request(&MutationRequest::Renew { fence: f })
    };
    let short_group = {
        let mut f = fence();
        f.group_id.push(0);
        codec::encode_coordination_fence(&f)
    };
    let set_admin = codec::encode_mutation_request(&MutationRequest::SetAdmin {
        target_node_id: h32(0xbb),
        enabled: true,
        expected_admin_revision: 0,
    });
    let membership = codec::encode_read_request(&ReadRequest::MembershipStatus(Some([0x0c; 16])));
    let probe = codec::encode_read_request(&ReadRequest::ProbeClient {
        target_node_id: h32(0xaa),
        target_client_id: [0x0d; 16],
    });
    let revoke_none = codec::encode_mutation_request(&MutationRequest::Revoke {
        resource: res(),
        expected_epoch: None,
    });
    let acquire = codec::encode_mutation_request(&MutationRequest::Acquire {
        resource: res(),
        ttl_ms: 5,
    });
    let reply = codec::encode_retire_reply(&RetireClientReplyWireV1 {
        outcome_code: 9,
        ..Default::default()
    });
    let report = codec::encode_placement_report(&PlacementReport {
        policy_revision: 0,
        policy_digest: [0; 32],
        membership_log_id: None,
        current_voters: vec![],
        current_learners: vec![],
        selected_voters: vec![],
        selected_learners: vec![],
        grace_crossed_members: vec![],
        separation: None,
        pending_operation_id: None,
        reason: String::new(),
    });
    let audit = |limit: u32| {
        // Encode by hand: the encoder does not range-check.
        codec::encode_read_request(&ReadRequest::AuditStream { after: None, limit })
    };
    let set_admin_bool_at = set_admin.len() - 2; // enabled, then one-byte revision
    vec![
        ("empty", "MutationRequest", vec![]),
        ("tag only", "MutationRequest", vec![0xD0]),
        (
            "wrong tag (a read request)",
            "MutationRequest",
            with(renew.clone(), &|b| b[0] = 0xD1),
        ),
        ("unknown tag", "ReadRequest", vec![0xDF, 0x01, 0x05]),
        (
            "unknown version",
            "MutationRequest",
            with(renew.clone(), &|b| b[1] = 0x02),
        ),
        (
            "unknown mutation op 0",
            "MutationRequest",
            vec![0xD0, 0x01, 0x00],
        ),
        (
            "unknown mutation op 11",
            "MutationRequest",
            vec![0xD0, 0x01, 0x0B],
        ),
        ("unknown read op 9", "ReadRequest", vec![0xD1, 0x01, 0x09]),
        (
            "trailing byte",
            "MutationRequest",
            with(renew.clone(), &|b| b.push(0)),
        ),
        (
            "truncated",
            "MutationRequest",
            renew[..renew.len() - 1].to_vec(),
        ),
        ("31-byte fence holder", "MutationRequest", short_holder),
        ("33-byte fence group id", "CoordinationFence", short_group),
        (
            "non-minimal varint (ttl 5 as 0x85 0x00)",
            "MutationRequest",
            with(acquire.clone(), &|b| {
                b.pop();
                b.extend([0x85, 0x00]);
            }),
        ),
        (
            "bad bool byte",
            "MutationRequest",
            with(set_admin.clone(), &|b| b[set_admin_bool_at] = 0x02),
        ),
        (
            "bad optional marker",
            "MutationRequest",
            with(revoke_none.clone(), &|b| *b.last_mut().unwrap() = 0x01),
        ),
        (
            "15-byte operation id",
            "ReadRequest",
            with(membership.clone(), &|b| {
                b.pop();
                b[4] = 15;
            }),
        ),
        (
            "17-byte client id",
            "ReadRequest",
            with(probe.clone(), &|b| {
                b.push(0);
                let n = b.len();
                b[n - 18] = 17;
            }),
        ),
        ("audit limit 0", "ReadRequest", audit(0)),
        ("audit limit 10001", "ReadRequest", audit(10_001)),
        ("code above u32", "RetireReply", {
            let mut b = vec![reply[0], reply[1]];
            b.extend([0x80, 0x80, 0x80, 0x80, 0x10]);
            b.push(0);
            b
        }),
        (
            "31-byte policy digest",
            "PlacementReport",
            with(report.clone(), &|b| {
                b.remove(4);
                b[3] = 31;
            }),
        ),
        (
            "bad list header",
            "AuditRecords",
            vec![0xD9, 0x01, 0x00, 0x0B],
        ),
    ]
}

const MUTATION_OPS: [&str; 10] = [
    "acquire",
    "renew",
    "compare_and_set",
    "release",
    "release_with_value",
    "revoke",
    "set_grant",
    "set_admin",
    "change_membership",
    "cancel_membership",
];
const READ_OPS: [&str; 8] = [
    "read_linearizable",
    "inspect_acl",
    "inspect_admins",
    "membership_status",
    "group_status",
    "probe_client",
    "audit_stream",
    "recovery_export",
];

fn ops(names: &[&str]) -> Vec<Value> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| json!({"code": i + 1, "name": n}))
        .collect()
}

fn render() -> Value {
    json!({
        "description": "Aster C ABI 4.4 coordination client payloads, UCv1 tags D0-DE (docs/_internal/bindings/coordination-abi-design.md section 2). D0 / D1 are the binding's requests (aster_coordination_mutate / _read); D2-DE are results. Byte strings are hex.",
        "format": {
            "prefix": "tag:u8 version:u8 (0x01)",
            "uint": "LEB128 varint, minimal length",
            "code": "uint(u32): an i32 code-space value as its u32 bit pattern",
            "u8enum": "one raw byte",
            "bool": "0x00 | 0x01",
            "string": "varint((utf8_len << 2) | 2) then UTF-8 bytes",
            "bytes": "varint(len) then raw bytes; request ids are exact lengths (node 32, client / operation 16)",
            "opt<T>": "0xFD absent | 0x00 then T",
            "list<T>": "varint(count) 0x0C then each T",
            "rules": "every field present, fixed order, no trailing bytes"
        },
        "tags": [
            {"tag": 0xD0, "name": "MutationRequest"}, {"tag": 0xD1, "name": "ReadRequest"},
            {"tag": 0xD2, "name": "MutationReply"}, {"tag": 0xD3, "name": "ReadReply"},
            {"tag": 0xD4, "name": "AclStatus"}, {"tag": 0xD5, "name": "AdminStatus"},
            {"tag": 0xD6, "name": "MembershipStatus"}, {"tag": 0xD7, "name": "GroupStatus"},
            {"tag": 0xD8, "name": "ClientProbe"}, {"tag": 0xD9, "name": "AuditRecords"},
            {"tag": 0xDA, "name": "RecoveryExport"}, {"tag": 0xDB, "name": "RecoveryExportManifest"},
            {"tag": 0xDC, "name": "RetireReply"}, {"tag": 0xDD, "name": "CoordinationFence"},
            {"tag": 0xDE, "name": "PlacementReport"}
        ],
        "mutation_ops": ops(&MUTATION_OPS),
        "read_ops": ops(&READ_OPS),
        "valid": all_cases().iter().map(|c| json!({
            "name": c.name, "type": c.ty, "value": c.value, "hex": hex::encode(&c.bytes)
        })).collect::<Vec<_>>(),
        "invalid": invalid().iter().map(|(n, t, b)| json!({"name": n, "type": t, "hex": hex::encode(b)})).collect::<Vec<_>>(),
    })
}

#[test]
fn coordination_vectors() {
    let generated = render();
    if std::env::var_os("ASTER_VECTORS_WRITE").is_some() {
        let mut s = serde_json::to_string_pretty(&generated).unwrap();
        s.push('\n');
        std::fs::write(path(), s).unwrap();
        return;
    }
    let file: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(file, generated, "{FILE} differs from the generated vectors");
    for c in all_cases() {
        assert_eq!(
            roundtrip(c.ty, &c.bytes).unwrap_or_else(|e| panic!("{}: {e:#}", c.name)),
            c.bytes,
            "{} round trip",
            c.name
        );
    }
    for (name, ty, b) in invalid() {
        assert!(roundtrip(ty, &b).is_err(), "{name} decoded");
    }
}

#[test]
fn op_codes_match_the_header() {
    use aster_transport_ffi::coordination::{
        aster_coordination_mutation_op_t as M, aster_coordination_read_op_t as R,
    };
    let v = render();
    let m = &v["mutation_ops"];
    assert_eq!(m[0]["code"], M::ASTER_COORDINATION_MUTATE_ACQUIRE as u32);
    assert_eq!(
        m[9]["code"],
        M::ASTER_COORDINATION_MUTATE_CANCEL_MEMBERSHIP as u32
    );
    let r = &v["read_ops"];
    assert_eq!(
        r[7]["code"],
        R::ASTER_COORDINATION_READ_RECOVERY_EXPORT as u32
    );
    for c in mutation_cases() {
        let op = c.bytes[2] as usize;
        assert_eq!(m[op - 1]["name"], c.value["op"], "{}", c.name);
    }
    for c in read_cases() {
        let op = c.bytes[2] as usize;
        assert_eq!(r[op - 1]["name"], c.value["op"], "{}", c.name);
    }
}
