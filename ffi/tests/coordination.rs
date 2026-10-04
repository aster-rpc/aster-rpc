//! ABI 4.4 (plan F7): the coordination client and placement manager through
//! the C functions, against a **real** in-process voter group
//! (`aster::coordination::testing::GroupFixture`, the fixture the Rust
//! client gates use). The test plays the binding: it encodes `D0` / `D1`
//! requests, decodes the `D2`-`DE` results, and answers the placement
//! upcalls (kinds 17 / 32 / 33) by hand over `aster_upcall_poll` /
//! `aster_upcall_respond`.
//!
//! All tests share one single-voter group (started once per binary); each
//! uses its own actor identities, journal directories and resource keys.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aster::binding_support::upcall::codec as ucv1;
use aster::coordination::client::placement::{PlacementCandidate, PlacementPolicy};
use aster::coordination::contract::{FenceV1, LogIdV1, MemberSeedV1, ResourceKeyV1};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW,
};
use aster::coordination::store::wire::{
    MutationReplyWireV1, RecoveryExportManifestWireV1, RetireClientReplyWireV1,
};
use aster::coordination::testing::GroupFixture;
use aster::lease::{DenyReason, TransitionOutcome};
use aster::rpc::AsterServer;
use aster::topology::ClusterView;
use aster_transport_core::ticket::{AsterTicket, TicketCredential};
use aster_transport_ffi::coordination::*;
use aster_transport_ffi::coordination_codec::{self as codec, MutationRequest, ReadRequest};
use aster_transport_ffi::node::{aster_config_init, aster_config_t, aster_node_start};
use aster_transport_ffi::upcall::*;
use aster_transport_ffi::*;

const OK: i32 = 0;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const NOT_FOUND: i32 = aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
const ALREADY_CLOSED: i32 = aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
const UNSUPPORTED: i32 = aster_status_t::ASTER_STATUS_UNSUPPORTED as i32;
const TOO_SMALL: i32 = aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;

const E_RESOLVED: u32 = aster_event_kind_t::ASTER_EVENT_COORDINATION_RESOLVED as u32;
const E_RESULT: u32 = aster_event_kind_t::ASTER_EVENT_COORDINATION_RESULT as u32;
const E_LEASE: u32 = aster_event_kind_t::ASTER_EVENT_COORDINATION_LEASE_ACQUIRED as u32;
const E_OPENED: u32 = aster_event_kind_t::ASTER_EVENT_COORDINATION_CLIENT_OPENED as u32;
const E_UNIT: u32 = aster_event_kind_t::ASTER_EVENT_UNIT_RESULT as u32;
const E_BYTES: u32 = aster_event_kind_t::ASTER_EVENT_BYTES_RESULT as u32;
const E_ERROR: u32 = aster_event_kind_t::ASTER_EVENT_ERROR as u32;

const K_CANCEL: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_CANCEL as u16;
const K_TOPOLOGY: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_TOPOLOGY_SNAPSHOT as u16;
const K_POLICY: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_PLACEMENT_POLICY as u16;
const K_PROVISION: u16 = aster_upcall_kind_t::ASTER_UPCALL_KIND_PLACEMENT_PROVISION as u16;
const S_OK: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_OK as u32;
const S_ERROR: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_ERROR as u32;
const S_UNIMPLEMENTED: u32 = aster_upcall_status_t::ASTER_UPCALL_STATUS_UNIMPLEMENTED as u32;

const SEED: u8 = 0xF4;
const DOMAIN: &str = "ffi.coordination.test";

// ── the voter group ──────────────────────────────────────────────────────────

struct Group {
    _rt: tokio::runtime::Runtime,
    _voter: AsterServer,
    fixture: GroupFixture,
    ticket: String,
}

/// An Open ticket with the voter's direct addresses: the routing hint.
fn routed_ticket(server: &AsterServer) -> String {
    let addr = server.node().addr();
    let id: [u8; 32] = hex::decode(addr.node_id.as_str())
        .unwrap()
        .try_into()
        .unwrap();
    AsterTicket {
        endpoint_id: id,
        relay: None,
        direct_addrs: addr
            .direct_addresses
            .iter()
            .filter_map(|a| a.parse().ok())
            .collect(),
        credential: Some(TicketCredential::Open),
    }
    .to_base58_string()
    .unwrap()
}

fn group() -> &'static Group {
    static GROUP: OnceLock<Group> = OnceLock::new();
    GROUP.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let fixture = GroupFixture::new(DOMAIN, SEED, 1);
        let dir = tempfile::tempdir().unwrap().keep();
        let voter = rt.block_on(async {
            let voter = AsterServer::builder()
                .config(fixture.aster_config(0))
                .builtin_coordination(fixture.config(&dir, 0))
                .start()
                .await
                .expect("the voter starts");
            for _ in 0..200 {
                if !voter.node().addr().direct_addresses.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::timeout(
                Duration::from_secs(60),
                voter.coordination().expect("hosted").wait_ready(),
            )
            .await
            .expect("ready in time")
            .expect("ready");
            voter
        });
        let ticket = routed_ticket(&voter);
        Group {
            _rt: rt,
            _voter: voter,
            fixture,
            ticket,
        }
    })
}

fn root_secret() -> [u8; 32] {
    [SEED ^ 0xFF; 32]
}

/// A fresh actor identity per call (distinct journals / ACL rows).
fn actor_secret() -> [u8; 32] {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut s = [0x5Au8; 32];
    s[..8].copy_from_slice(&n.to_le_bytes());
    s[8] = std::process::id() as u8;
    s
}

fn res(ns: &str, key: &str) -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: ns.into(),
        key: key.as_bytes().to_vec(),
    }
}

// ── C ABI helpers ────────────────────────────────────────────────────────────

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

/// One ffi runtime and one ROOT node shared by every test: two endpoints
/// with the same identity would compete for the voter's connection.
#[derive(Clone, Copy)]
struct Shared {
    rt: aster_runtime_t,
    root_node: aster_node_t,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| {
        let rt = runtime();
        let root_node = unsafe { start_node(rt, root_secret()) };
        Shared { rt, root_node }
    })
}

fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    let cfg = aster_runtime_config_t {
        struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
        worker_threads: 4,
        event_queue_capacity: 256,
        reserved: 0,
    };
    assert_eq!(
        unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &cfg, &mut rt) },
        OK
    );
    rt
}

struct Ev {
    ev: aster_event_t,
    data: Vec<u8>,
}

// SAFETY: `data_ptr` is never dereferenced after `finish` copied the payload
// into `data` and released the buffer.
unsafe impl Send for Ev {}

impl Ev {
    fn kind(&self) -> u32 {
        self.ev.kind
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
    fn expect(self, kind: u32) -> Self {
        assert_eq!(
            self.ev.kind,
            kind,
            "event kind {} (status {}, error_code {}, flags {}): {}",
            self.ev.kind,
            self.ev.status,
            self.ev.error_code,
            self.ev.flags,
            self.text()
        );
        assert_eq!(self.ev.status, 0);
        self
    }
    fn expect_error(self, code: aster_coordination_error_t) -> Self {
        assert_eq!(
            (self.ev.kind, self.ev.error_code),
            (E_ERROR, code as i32),
            "expected coordination error {code:?}: kind {}, status {}, payload {}",
            self.ev.kind,
            self.ev.status,
            self.text()
        );
        self
    }
}

/// Wait for `op`'s event. The completion queue is shared by every test in
/// the binary, so whoever polls parks other operations' events for them.
unsafe fn finish(rt: aster_runtime_t, op: aster_operation_t) -> Ev {
    static PARKED: OnceLock<Mutex<HashMap<u64, Ev>>> = OnceLock::new();
    static POLL: Mutex<()> = Mutex::new(());
    let parked = PARKED.get_or_init(|| Mutex::new(HashMap::new()));
    let started = Instant::now();
    loop {
        if let Some(ev) = parked.lock().unwrap().remove(&op) {
            return ev;
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "operation {op}: no event"
        );
        let _poller = POLL.lock().unwrap();
        let mut events: [aster_event_t; 8] = std::mem::zeroed();
        let n = aster_poll_events(rt, events.as_mut_ptr(), events.len(), 20);
        let mut parked = parked.lock().unwrap();
        for ev in events.iter().take(n) {
            let data = if ev.data_len == 0 || ev.data_ptr.is_null() {
                Vec::new()
            } else {
                slice::from_raw_parts(ev.data_ptr, ev.data_len).to_vec()
            };
            if ev.buffer != 0 {
                aster_buffer_release(rt, ev.buffer);
            }
            parked.insert(ev.operation, Ev { ev: *ev, data });
        }
    }
}

unsafe fn start_node(rt: aster_runtime_t, secret: [u8; 32]) -> aster_node_t {
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.secret_key = bytes(&secret);
    let mut op = 0;
    assert_eq!(aster_node_start(rt, &cfg, 0, 0, &mut op), OK);
    let ev = finish(rt, op).expect(aster_event_kind_t::ASTER_EVENT_NODE_CREATED as u32);
    ev.ev.handle
}

/// Open a client; the raw event (opened or error).
unsafe fn open_raw(
    rt: aster_runtime_t,
    node: aster_node_t,
    journal: Option<&std::path::Path>,
) -> Ev {
    let g = group();
    let group_id = *g.fixture.group_id().as_bytes();
    let tickets = [bytes(g.ticket.as_bytes())];
    let dir = journal.map(|p| p.to_str().unwrap().to_owned());
    let mut o: aster_coordination_client_options_t = std::mem::zeroed();
    assert_eq!(aster_coordination_client_options_init(&mut o), OK);
    o.group_id = group_id.as_ptr();
    o.seed_tickets = aster_bytes_list_t {
        items: tickets.as_ptr(),
        len: 1,
    };
    match &dir {
        Some(d) => o.journal_dir = bytes(d.as_bytes()),
        None => o.flags = ASTER_COORDINATION_CLIENT_READ_ONLY,
    }
    let mut op = 0;
    assert_eq!(aster_coordination_client_open(rt, node, &o, 7, &mut op), OK);
    finish(rt, op)
}

unsafe fn open(
    rt: aster_runtime_t,
    node: aster_node_t,
    journal: Option<&std::path::Path>,
) -> aster_coordination_client_t {
    let ev = open_raw(rt, node, journal).expect(E_OPENED);
    assert_eq!(ev.ev.related, node);
    assert_eq!(ev.ev.user_data, 7);
    ev.ev.handle
}

unsafe fn mutate(rt: aster_runtime_t, c: aster_coordination_client_t, m: &MutationRequest) -> Ev {
    let req = codec::encode_mutation_request(m);
    let mut op = 0;
    assert_eq!(
        aster_coordination_mutate(rt, c, bytes(&req), 0, &mut op),
        OK
    );
    finish(rt, op)
}

/// Mutate, decode the reply, acknowledge it.
unsafe fn mutate_ack(
    rt: aster_runtime_t,
    c: aster_coordination_client_t,
    m: &MutationRequest,
) -> MutationReplyWireV1 {
    let ev = mutate(rt, c, m).expect(E_RESOLVED);
    assert_ne!(ev.ev.handle, 0);
    let reply = codec::decode_mutation_reply(&ev.data).unwrap();
    let mut op = 0;
    assert_eq!(
        aster_coordination_resolved_acknowledge(rt, ev.ev.handle, 0, &mut op),
        OK
    );
    finish(rt, op).expect(E_UNIT);
    reply
}

unsafe fn read_raw(rt: aster_runtime_t, c: aster_coordination_client_t, q: &ReadRequest) -> Ev {
    let req = codec::encode_read_request(q);
    let mut op = 0;
    assert_eq!(aster_coordination_read(rt, c, bytes(&req), 0, &mut op), OK);
    finish(rt, op)
}

unsafe fn read(rt: aster_runtime_t, c: aster_coordination_client_t, q: &ReadRequest) -> Vec<u8> {
    read_raw(rt, c, q).expect(E_RESULT).data
}

unsafe fn local_id(rt: aster_runtime_t, c: aster_coordination_client_t) -> [u8; 32] {
    let mut g = [0u8; 32];
    let mut l = [0u8; 32];
    assert_eq!(
        aster_coordination_client_ids(rt, c, g.as_mut_ptr(), l.as_mut_ptr()),
        OK
    );
    assert_eq!(&g, group().fixture.group_id().as_bytes());
    l
}

/// ROOT grants `actor` the lease rights on `resource`.
unsafe fn grant(
    rt: aster_runtime_t,
    root: aster_coordination_client_t,
    r: &ResourceKeyV1,
    actor: [u8; 32],
) {
    let acl =
        codec::decode_acl_status(&read(rt, root, &ReadRequest::InspectAcl(r.clone()))).unwrap();
    let reply = mutate_ack(
        rt,
        root,
        &MutationRequest::SetGrant {
            resource: r.clone(),
            target_node_id: actor,
            rights: RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
            expected_acl_revision: acl.acl_revision,
        },
    );
    assert_eq!(reply.outcome_code, 6, "grant updated: {reply:?}");
}

// ── codec ────────────────────────────────────────────────────────────────────

fn fence() -> FenceV1 {
    FenceV1 {
        group_id: vec![0x11; 32],
        resource: res("ns", "k"),
        epoch: 300,
        holder: vec![0x22; 32],
    }
}

#[test]
fn codec_round_trips_every_request() {
    let log = LogIdV1 {
        term: 3,
        leader_node_id: vec![9; 32],
        index: 1 << 40,
    };
    let muts = vec![
        MutationRequest::Acquire {
            resource: res("a", ""),
            ttl_ms: 30_000,
        },
        MutationRequest::Renew { fence: fence() },
        MutationRequest::CompareAndSet {
            fence: fence(),
            expected_value_revision: 0,
            new_value: vec![1, 2, 3],
        },
        MutationRequest::Release { fence: fence() },
        MutationRequest::ReleaseWithValue {
            fence: fence(),
            expected_value_revision: 7,
            final_value: vec![],
        },
        MutationRequest::Revoke {
            resource: res("a", "b"),
            expected_epoch: Some(4),
        },
        MutationRequest::Revoke {
            resource: res("a", "b"),
            expected_epoch: None,
        },
        MutationRequest::SetGrant {
            resource: res("a", "b"),
            target_node_id: [3; 32],
            rights: 0x1f,
            expected_acl_revision: 2,
        },
        MutationRequest::SetAdmin {
            target_node_id: [4; 32],
            enabled: true,
            expected_admin_revision: 0,
        },
        MutationRequest::ChangeMembership {
            operation_id: [5; 16],
            expected_membership_log_id: Some(log.clone()),
            desired_voters: vec![[1; 32], [2; 32]],
            retained_learners: vec![],
            member_seeds: vec![MemberSeedV1 {
                node_id: vec![2; 32],
                ticket: "t".into(),
                signed_join_permit: Some(vec![8; 10]),
            }],
        },
        MutationRequest::CancelMembership {
            operation_id: [6; 16],
            expected_phase_code: 2,
        },
    ];
    for m in &muts {
        let b = codec::encode_mutation_request(m);
        assert_eq!(b[0], 0xD0);
        assert_eq!(b[1], 0x01);
        assert_eq!(&codec::decode_mutation_request(&b).unwrap(), m);
        // No trailing bytes, no truncation.
        let mut long = b.clone();
        long.push(0);
        assert!(codec::decode_mutation_request(&long).is_err());
        assert!(codec::decode_mutation_request(&b[..b.len() - 1]).is_err());
    }
    let reads = vec![
        ReadRequest::ReadLinearizable(res("x", "y")),
        ReadRequest::InspectAcl(res("x", "y")),
        ReadRequest::InspectAdmins,
        ReadRequest::MembershipStatus(None),
        ReadRequest::MembershipStatus(Some([1; 16])),
        ReadRequest::GroupStatus,
        ReadRequest::ProbeClient {
            target_node_id: [1; 32],
            target_client_id: [2; 16],
        },
        ReadRequest::AuditStream {
            after: Some(log),
            limit: 10_000,
        },
        ReadRequest::RecoveryExport,
    ];
    for q in &reads {
        let b = codec::encode_read_request(q);
        assert_eq!(b[0], 0xD1);
        assert_eq!(&codec::decode_read_request(&b).unwrap(), q);
    }
    // The op bytes are the header's enums.
    assert_eq!(
        codec::encode_mutation_request(&muts[10])[2],
        aster_coordination_mutation_op_t::ASTER_COORDINATION_MUTATE_CANCEL_MEMBERSHIP as u8
    );
    assert_eq!(
        codec::encode_read_request(&reads[8])[2],
        aster_coordination_read_op_t::ASTER_COORDINATION_READ_RECOVERY_EXPORT as u8
    );
}

#[test]
fn codec_refuses_malformed_requests() {
    let good = codec::encode_mutation_request(&MutationRequest::Renew { fence: fence() });
    let bad: Vec<Vec<u8>> = vec![
        vec![],
        vec![0xD0],
        vec![0xD1, 0x01, 1],  // wrong tag for a mutation
        vec![0xD0, 0x02, 2],  // unknown version
        vec![0xD0, 0x01, 0],  // unknown op
        vec![0xD0, 0x01, 11], // unknown op
        {
            // a 31-byte holder
            let mut f = fence();
            f.holder.pop();
            codec::encode_mutation_request(&MutationRequest::Renew { fence: f })
        },
        {
            // non-minimal varint for the ttl
            let mut b = vec![0xD0, 0x01, 1, 0x06, b'a', 0x00];
            b.extend([0x80, 0x00]);
            b
        },
        {
            let mut b = good.clone();
            b.push(1);
            b
        },
    ];
    for b in &bad {
        assert!(codec::decode_mutation_request(b).is_err(), "{b:02x?}");
    }
    for limit in [0u32, 10_001] {
        let b = codec::encode_read_request(&ReadRequest::AuditStream { after: None, limit });
        assert!(codec::decode_read_request(&b).is_err());
    }
    assert!(codec::decode_read_request(&[0xD1, 0x01, 9]).is_err());
    // A short operation id.
    let b = vec![0xD1, 0x01, 4, 0x00, 0x02, 1, 2];
    assert!(codec::decode_read_request(&b).is_err());
}

#[test]
fn codec_round_trips_results() {
    let reply = MutationReplyWireV1 {
        client_id: vec![1; 16],
        sequence: 9,
        request_hash: vec![2; 32],
        disposition_code: 1,
        outcome_code: 100,
        reason_code: 3,
        snapshot: Some(aster::coordination::contract::ResourceSnapshotV1 {
            group_id: vec![3; 32],
            resource: res("n", "k"),
            epoch: 5,
            holder: None,
            held: false,
            ttl_ms: 0,
            lease_revision: 1,
            value_revision: 2,
            acl_revision: 3,
            value: Some(vec![]),
            value_redacted: false,
        }),
        acl_revision: 4,
        admin_revision: 0,
        operation_id: None,
        membership_log_id: Some(LogIdV1 {
            term: 1,
            leader_node_id: vec![4; 32],
            index: 2,
        }),
        ..Default::default()
    };
    assert_eq!(
        codec::decode_mutation_reply(&codec::encode_mutation_reply(&reply)).unwrap(),
        reply
    );
    let retire = RetireClientReplyWireV1 {
        outcome_code: 9,
        reason_code: 0,
        ..Default::default()
    };
    assert_eq!(
        codec::decode_retire_reply(&codec::encode_retire_reply(&retire)).unwrap(),
        retire
    );
    let manifest = RecoveryExportManifestWireV1 {
        total_len: 3,
        blake3_checksum: vec![7; 32],
        applied_log_id: None,
        ..Default::default()
    };
    let (m, body) =
        codec::decode_recovery_export(&codec::encode_recovery_export(&manifest, b"abc")).unwrap();
    assert_eq!((m, body), (manifest.clone(), b"abc".to_vec()));
    assert_eq!(
        codec::decode_recovery_export_manifest(&codec::encode_recovery_export_manifest(&manifest))
            .unwrap(),
        manifest
    );
    assert_eq!(
        codec::decode_coordination_fence(&codec::encode_coordination_fence(&fence())).unwrap(),
        fence()
    );
    // A code keeps its i32 bit pattern.
    let neg = RetireClientReplyWireV1 {
        outcome_code: -1,
        ..Default::default()
    };
    assert_eq!(
        codec::decode_retire_reply(&codec::encode_retire_reply(&neg))
            .unwrap()
            .outcome_code,
        -1
    );
}

// ── the durable client ───────────────────────────────────────────────────────

#[test]
fn durable_client_mutations_reads_and_redelivery() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        let root_id = local_id(rt, root);

        // A fresh journal has nothing to redeliver.
        let mut op = 0;
        assert_eq!(aster_coordination_unacknowledged(rt, root, 0, &mut op), OK);
        let ev = finish(rt, op).expect(E_RESOLVED);
        assert_eq!((ev.ev.handle, ev.data.len()), (0, 0));
        assert_eq!(aster_coordination_resume(rt, root, 0, &mut op), OK);
        assert_eq!(finish(rt, op).expect(E_RESOLVED).ev.handle, 0);

        let mut op = 0;
        assert_eq!(aster_coordination_client_id(rt, root, 0, &mut op), OK);
        let client_id = finish(rt, op).expect(E_BYTES).data;
        assert_eq!(client_id.len(), 16);

        // One mutation, not acknowledged: the journal blocks the next one.
        let r = res("ffi.durable", "head");
        let worker = [0x33u8; 32];
        let first = mutate(
            rt,
            root,
            &MutationRequest::SetGrant {
                resource: r.clone(),
                target_node_id: worker,
                rights: RIGHT_READ,
                expected_acl_revision: 0,
            },
        )
        .expect(E_RESOLVED);
        assert_ne!(first.ev.handle, 0);
        let reply = codec::decode_mutation_reply(&first.data).unwrap();
        assert_eq!(reply.outcome_code, 6);
        assert_eq!(reply.client_id, client_id);
        let blocked = mutate(
            rt,
            root,
            &MutationRequest::SetAdmin {
                target_node_id: worker,
                enabled: false,
                expected_admin_revision: 0,
            },
        )
        .expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_AWAITING_ACK);
        assert_eq!(blocked.ev.status, INVALID as u32);

        // Drop it unacknowledged, "restart", and get the exact reply back.
        assert_eq!(aster_coordination_resolved_free(rt, first.ev.handle), OK);
        assert_eq!(
            aster_coordination_resolved_free(rt, first.ev.handle),
            NOT_FOUND
        );
        assert_eq!(aster_coordination_client_free(rt, root), OK);
        let root = open(rt, root_node, Some(dir.path()));
        // A second open over the same journal is refused.
        open_raw(rt, root_node, Some(dir.path()))
            .expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_JOURNAL_LOCKED);
        assert_eq!(aster_coordination_unacknowledged(rt, root, 0, &mut op), OK);
        let again = finish(rt, op).expect(E_RESOLVED);
        assert_ne!(again.ev.handle, 0);
        assert_eq!(again.data, first.data, "the exact saved reply");
        assert_eq!(
            aster_coordination_resolved_acknowledge(rt, again.ev.handle, 0, &mut op),
            OK
        );
        finish(rt, op).expect(E_UNIT);
        assert_eq!(
            aster_coordination_resolved_acknowledge(rt, again.ev.handle, 0, &mut op),
            NOT_FOUND
        );

        // The next sequence proceeds.
        let next = mutate_ack(
            rt,
            root,
            &MutationRequest::SetGrant {
                resource: r.clone(),
                target_node_id: worker,
                rights: RIGHT_READ | RIGHT_ACQUIRE,
                expected_acl_revision: reply.acl_revision,
            },
        );
        assert_eq!(next.sequence, reply.sequence + 1);

        // Reads.
        let status =
            codec::decode_group_status(&read(rt, root, &ReadRequest::GroupStatus)).unwrap();
        assert_eq!(status.voters.len(), 1);
        assert_eq!(status.voters[0].node_id, group().fixture.node_id(0));
        assert_eq!(status.authority_domain, DOMAIN);
        let acl =
            codec::decode_acl_status(&read(rt, root, &ReadRequest::InspectAcl(r.clone()))).unwrap();
        assert_eq!(acl.grants.len(), 1);
        assert_eq!(acl.grants[0].subject_node_id, worker.to_vec());
        assert_eq!(acl.grants[0].rights, RIGHT_READ | RIGHT_ACQUIRE);
        // ROOT administers the group but holds no READ grant on the resource:
        // the replicated ACL refuses (deny by default).
        let denied = read_raw(rt, root, &ReadRequest::ReadLinearizable(r.clone()))
            .expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_RPC);
        assert_eq!(
            denied.ev.status,
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED as u32
        );
        assert_eq!(denied.ev.flags, 7);
        let admins =
            codec::decode_admin_status(&read(rt, root, &ReadRequest::InspectAdmins)).unwrap();
        assert!(admins.administrators.iter().all(|a| a.node_id.len() == 32));
        codec::decode_membership_status(&read(rt, root, &ReadRequest::MembershipStatus(None)))
            .unwrap();
        let probe = codec::decode_client_probe(&read(
            rt,
            root,
            &ReadRequest::ProbeClient {
                target_node_id: root_id,
                target_client_id: client_id.clone().try_into().unwrap(),
            },
        ))
        .unwrap();
        assert!(probe.exists);
        assert_eq!(probe.last_sequence, next.sequence);
        let audit = codec::decode_audit_records(&read(
            rt,
            root,
            &ReadRequest::AuditStream {
                after: None,
                limit: 100,
            },
        ))
        .unwrap();
        assert!(!audit.is_empty());
        let (manifest, body) =
            codec::decode_recovery_export(&read(rt, root, &ReadRequest::RecoveryExport)).unwrap();
        assert_eq!(manifest.total_len, body.len() as u64);
        let path = dir.path().join("export.bin");
        let p = path.to_str().unwrap();
        assert_eq!(
            aster_coordination_recovery_export_to_file(rt, root, bytes(p.as_bytes()), 0, &mut op),
            OK
        );
        let m =
            codec::decode_recovery_export_manifest(&finish(rt, op).expect(E_RESULT).data).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), m.total_len);

        // Retiring the journal's own client id is refused (only its closing
        // corridor may).
        let own: [u8; 16] = client_id.clone().try_into().unwrap();
        assert_eq!(
            aster_coordination_retire_other_client(rt, root, own.as_ptr(), 0, &mut op),
            OK
        );
        finish(rt, op).expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_INVALID);

        // Malformed requests never reach the group.
        assert_eq!(
            aster_coordination_mutate(rt, root, bytes(&[0xD0, 1, 0]), 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_coordination_read(rt, root, bytes(&[0xD0, 1, 5]), 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_coordination_mutate(rt, 0xdead, bytes(&[0xD0]), 0, &mut op),
            NOT_FOUND
        );

        assert_eq!(aster_coordination_client_free(rt, root), OK);
        assert_eq!(aster_coordination_client_free(rt, root), NOT_FOUND);
    }
}

#[test]
fn read_only_client_reads_and_refuses_the_journal() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        // An ungranted actor's read-only client: status needs the
        // administrator predicate, so the group refuses.
        let node = start_node(rt, actor_secret());
        let stranger = open(rt, node, None);
        let ev = read_raw(rt, stranger, &ReadRequest::GroupStatus)
            .expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_RPC);
        assert_eq!(
            ev.ev.status,
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED as u32
        );
        assert_eq!(aster_coordination_client_free(rt, stranger), OK);
        // ROOT's read-only client (beside any durable ROOT client: no journal).
        let c = open(rt, root_node, None);
        let status = codec::decode_group_status(&read(rt, c, &ReadRequest::GroupStatus)).unwrap();
        assert_eq!(status.voters.len(), 1);
        let req = codec::encode_mutation_request(&MutationRequest::Acquire {
            resource: res("ro", "k"),
            ttl_ms: 10_000,
        });
        let mut op = 0;
        assert_eq!(
            aster_coordination_mutate(rt, c, bytes(&req), 0, &mut op),
            UNSUPPORTED
        );
        assert_eq!(aster_coordination_resume(rt, c, 0, &mut op), UNSUPPORTED);
        assert_eq!(aster_coordination_client_id(rt, c, 0, &mut op), UNSUPPORTED);
        assert_eq!(
            aster_coordination_lease_acquire(rt, c, bytes(b"ro"), bytes(b"k"), 0, 0, 0, 0, &mut op),
            UNSUPPORTED
        );
        let u = {
            let mut u = 0;
            assert_eq!(aster_upcalls_new(rt, 0, &mut u), OK);
            u
        };
        let mut m = 0;
        assert_eq!(
            aster_coordination_placement_manager_new(rt, c, u, 0, &mut m),
            UNSUPPORTED
        );
        // Options are validated before anything starts.
        let mut o: aster_coordination_client_options_t = std::mem::zeroed();
        assert_eq!(
            aster_coordination_client_open(rt, node, &o, 0, &mut op),
            INVALID
        );
        assert_eq!(aster_coordination_client_options_init(&mut o), OK);
        assert_eq!(
            aster_coordination_client_open(rt, node, &o, 0, &mut op),
            INVALID
        ); // no group
        let gid = [1u8; 32];
        o.group_id = gid.as_ptr();
        assert_eq!(
            aster_coordination_client_open(rt, node, &o, 0, &mut op),
            INVALID
        ); // no journal
        o.flags = 0x80;
        assert_eq!(
            aster_coordination_client_open(rt, node, &o, 0, &mut op),
            INVALID
        );
        o.flags = ASTER_COORDINATION_CLIENT_READ_ONLY;
        assert_eq!(
            aster_coordination_client_open(rt, 0xbad, &o, 0, &mut op),
            NOT_FOUND
        );
        assert_eq!(aster_coordination_client_close(rt, c), OK);
        assert_eq!(aster_coordination_client_close(rt, c), OK);
        let q = codec::encode_read_request(&ReadRequest::GroupStatus);
        assert_eq!(
            aster_coordination_read(rt, c, bytes(&q), 0, &mut op),
            ALREADY_CLOSED
        );
        assert_eq!(aster_coordination_client_free(rt, c), OK);
        assert_eq!(
            aster_coordination_client_ids(rt, c, ptr::null_mut(), ptr::null_mut()),
            NOT_FOUND
        );
        aster_upcalls_free(rt, u);
    }
}

#[test]
fn an_acl_refusal_is_permission_denied() {
    unsafe {
        let rt = shared().rt;
        let node = start_node(rt, actor_secret());
        let dir = tempfile::tempdir().unwrap();
        let c = open(rt, node, Some(dir.path()));
        let ev = mutate(
            rt,
            c,
            &MutationRequest::Acquire {
                resource: res("ffi.denied", "k"),
                ttl_ms: 10_000,
            },
        )
        .expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_RPC);
        assert_eq!(
            ev.ev.status,
            aster_status_t::ASTER_STATUS_PERMISSION_DENIED as u32
        );
        assert_eq!(ev.ev.flags, 7);
        // Plan Q2: the denied request stays journaled as unresolved.
        mutate(
            rt,
            c,
            &MutationRequest::Acquire {
                resource: res("ffi.denied", "k2"),
                ttl_ms: 10_000,
            },
        )
        .expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_UNRESOLVED);
        assert_eq!(aster_coordination_client_free(rt, c), OK);
    }
}

// ── leases ───────────────────────────────────────────────────────────────────

#[test]
fn coordination_lease_lifecycle() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let root_dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(root_dir.path()));
        let (w1_node, w2_node) = (
            start_node(rt, actor_secret()),
            start_node(rt, actor_secret()),
        );
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (w1, w2) = (
            open(rt, w1_node, Some(d1.path())),
            open(rt, w2_node, Some(d2.path())),
        );
        let r = res("ffi.lease", "leader");
        grant(rt, root, &r, local_id(rt, w1));
        grant(rt, root, &r, local_id(rt, w2));

        let mut op = 0;
        assert_eq!(
            aster_coordination_lease_acquire(
                rt,
                w1,
                bytes(b"ffi.lease"),
                bytes(b"leader"),
                10_000,
                0,
                0,
                0,
                &mut op
            ),
            OK
        );
        let ev = finish(rt, op).expect(E_LEASE);
        let lease = ev.ev.handle;
        assert_ne!(lease, 0);
        let f = codec::decode_coordination_fence(&ev.data).unwrap();
        assert_eq!(f.holder, local_id(rt, w1).to_vec());
        assert_eq!(f.resource, r);
        assert_eq!(f.group_id, group().fixture.group_id().as_bytes().to_vec());

        // fence: ask for the size first.
        let mut len = 0usize;
        assert_eq!(
            aster_coordination_lease_fence(rt, lease, ptr::null_mut(), 0, &mut len),
            TOO_SMALL
        );
        let mut buf = vec![0u8; len];
        assert_eq!(
            aster_coordination_lease_fence(rt, lease, buf.as_mut_ptr(), buf.len(), &mut len),
            OK
        );
        assert_eq!(
            codec::decode_coordination_fence(&buf).unwrap().epoch,
            f.epoch
        );
        let mut st = 9;
        assert_eq!(aster_coordination_lease_status(rt, lease, &mut st), OK);
        assert_eq!(st, aster_lease_status_t::ASTER_LEASE_STATUS_HELD as u32);

        // A competing acquire is denied (B2), not an error.
        assert_eq!(
            aster_coordination_lease_acquire(
                rt,
                w2,
                bytes(b"ffi.lease"),
                bytes(b"leader"),
                10_000,
                0,
                0,
                0,
                &mut op
            ),
            OK
        );
        let denied = finish(rt, op).expect(E_LEASE);
        assert_eq!(denied.ev.handle, 0);
        match ucv1::decode_transition_outcome(&denied.data).unwrap() {
            TransitionOutcome::Denied(reason, snap) => {
                assert_eq!(reason, DenyReason::HeldByOther);
                assert!(snap.held);
            }
            other => panic!("expected a denial, got {other:?}"),
        }

        // Register mutation under the fence, then the atomic final handoff.
        assert_eq!(
            aster_coordination_lease_compare_and_set(rt, lease, 0, bytes(b"v1"), 0, &mut op),
            OK
        );
        let cas = codec::decode_mutation_reply(&finish(rt, op).expect(E_RESULT).data).unwrap();
        assert_eq!(cas.outcome_code, 3, "mutated: {cas:?}");
        let rev = cas.snapshot.as_ref().unwrap().value_revision;
        assert_eq!(
            aster_coordination_lease_release_with_value(
                rt,
                lease,
                rev,
                bytes(b"final"),
                0,
                &mut op
            ),
            OK
        );
        let done = codec::decode_mutation_reply(&finish(rt, op).expect(E_RESULT).data).unwrap();
        assert_eq!(done.outcome_code, 4, "released: {done:?}");
        assert_eq!(
            aster_coordination_lease_status(rt, lease, &mut st),
            NOT_FOUND
        );
        assert_eq!(
            aster_coordination_lease_release(rt, lease, 0, &mut op),
            NOT_FOUND
        );

        let snap =
            codec::decode_read_reply(&read(rt, w2, &ReadRequest::ReadLinearizable(r.clone())))
                .unwrap();
        assert!(!snap.held);
        assert_eq!(snap.value.as_deref(), Some(&b"final"[..]));

        // w2 takes it now; a plain release returns it; the lease journal
        // acknowledged itself.
        assert_eq!(
            aster_coordination_lease_acquire(
                rt,
                w2,
                bytes(b"ffi.lease"),
                bytes(b"leader"),
                10_000,
                0,
                0,
                0,
                &mut op
            ),
            OK
        );
        let l2 = finish(rt, op).expect(E_LEASE).ev.handle;
        assert_ne!(l2, 0);
        assert_eq!(aster_coordination_lease_release(rt, l2, 0, &mut op), OK);
        finish(rt, op).expect(E_UNIT);
        assert_eq!(aster_coordination_unacknowledged(rt, w2, 0, &mut op), OK);
        assert_eq!(finish(rt, op).expect(E_RESOLVED).ev.handle, 0);

        // Bad timing is refused before anything starts.
        assert_eq!(
            aster_coordination_lease_acquire(
                rt,
                w2,
                bytes(b"ffi.lease"),
                bytes(b"leader"),
                1_000,
                0,
                5_000,
                0,
                &mut op
            ),
            INVALID
        );

        for c in [root, w1, w2] {
            assert_eq!(aster_coordination_client_free(rt, c), OK);
        }
    }
}

// ── close with handles and operations outstanding ───────────────────────────

#[test]
fn close_retires_handles_and_keeps_the_delivery_contract() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        let w_node = start_node(rt, actor_secret());
        let wdir = tempfile::tempdir().unwrap();
        let w = open(rt, w_node, Some(wdir.path()));
        let r = res("ffi.close", "k");
        grant(rt, root, &r, local_id(rt, w));

        // A lease and an unacknowledged result outstanding on `w`.
        let mut op = 0;
        assert_eq!(
            aster_coordination_lease_acquire(
                rt,
                w,
                bytes(b"ffi.close"),
                bytes(b"k"),
                10_000,
                0,
                0,
                0,
                &mut op
            ),
            OK
        );
        let lease = finish(rt, op).expect(E_LEASE).ev.handle;
        let other = res("ffi.close", "other");
        let pending = {
            // ROOT holds an unacknowledged result while a second op is in flight.
            let req = codec::encode_mutation_request(&MutationRequest::SetGrant {
                resource: other.clone(),
                target_node_id: [0x44; 32],
                rights: RIGHT_READ,
                expected_acl_revision: 0,
            });
            assert_eq!(
                aster_coordination_mutate(rt, root, bytes(&req), 0, &mut op),
                OK
            );
            // Close while it is in flight.
            assert_eq!(aster_coordination_client_close(rt, root), OK);
            finish(rt, op)
        };
        // Racy on purpose (a stress case; the deterministic halves are
        // `close_after_resolution_*` and `resolution_after_close_*`): either
        // outcome must keep the delivery contract.
        match pending.kind() {
            E_RESOLVED => {
                // Resolved before the close: the handle is already inert.
                let mut op2 = 0;
                assert_eq!(
                    aster_coordination_resolved_acknowledge(rt, pending.ev.handle, 0, &mut op2),
                    ALREADY_CLOSED
                );
            }
            E_ERROR => {
                assert_eq!(
                    pending.ev.error_code,
                    aster_coordination_error_t::ASTER_COORDINATION_ERROR_CLOSED as i32
                );
                assert_eq!(pending.ev.status, ALREADY_CLOSED as u32);
            }
            k => panic!("unexpected event {k}"),
        }
        // The closed client refuses new work at once.
        let q = codec::encode_read_request(&ReadRequest::GroupStatus);
        assert_eq!(
            aster_coordination_read(rt, root, bytes(&q), 0, &mut op),
            ALREADY_CLOSED
        );
        assert_eq!(
            aster_coordination_resume(rt, root, 0, &mut op),
            ALREADY_CLOSED
        );
        // Reopen: the result is redelivered (the delivery contract).
        assert_eq!(aster_coordination_client_free(rt, root), OK);
        let root = open(rt, root_node, Some(dir.path()));
        assert_eq!(aster_coordination_unacknowledged(rt, root, 0, &mut op), OK);
        let again = finish(rt, op).expect(E_RESOLVED);
        assert_ne!(again.ev.handle, 0);
        let reply = codec::decode_mutation_reply(&again.data).unwrap();
        assert_eq!(reply.outcome_code, 6);
        assert_eq!(
            aster_coordination_resolved_acknowledge(rt, again.ev.handle, 0, &mut op),
            OK
        );
        finish(rt, op).expect(E_UNIT);

        // Closing `w` makes its lease inert: no fence, released, release fails.
        assert_eq!(aster_coordination_client_close(rt, w), OK);
        let mut len = 0usize;
        assert_eq!(
            aster_coordination_lease_fence(rt, lease, ptr::null_mut(), 0, &mut len),
            ALREADY_CLOSED
        );
        let mut st = 9;
        assert_eq!(aster_coordination_lease_status(rt, lease, &mut st), OK);
        assert_eq!(st, aster_lease_status_t::ASTER_LEASE_STATUS_RELEASED as u32);
        assert_eq!(
            aster_coordination_lease_compare_and_set(rt, lease, 0, bytes(b"x"), 0, &mut op),
            ALREADY_CLOSED
        );
        assert_eq!(aster_coordination_lease_release(rt, lease, 0, &mut op), OK);
        finish(rt, op).expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_CLOSED);
        assert_eq!(aster_coordination_lease_free(rt, lease), NOT_FOUND);

        // The journal lock is free again for `w`'s identity.
        assert_eq!(aster_coordination_client_free(rt, w), OK);
        let w = open(rt, w_node, Some(wdir.path()));
        assert_eq!(aster_coordination_client_free(rt, w), OK);
        assert_eq!(aster_coordination_client_free(rt, root), OK);
    }
}

// ── placement over upcalls ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Seen {
    kind: u16,
    cancel_reason: u8,
    payload: Vec<u8>,
}

enum Reply {
    Answer(u32, Vec<u8>),
    Ignore,
}

/// The "binding": a thread draining the upcall queue and answering inline.
struct Foreign {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<Seen>>>,
    thread: Option<JoinHandle<()>>,
}

impl Foreign {
    fn start(
        rt: aster_runtime_t,
        upcalls: aster_upcalls_t,
        handler: impl Fn(u16, &[u8]) -> Reply + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s, sn) = (stop.clone(), seen.clone());
        let thread = std::thread::spawn(move || {
            let mut events: [aster_upcall_event_t; 8] = unsafe { std::mem::zeroed() };
            while !s.load(Ordering::Acquire) {
                let n = unsafe { aster_upcall_poll(rt, upcalls, events.as_mut_ptr(), 8, 50) };
                for ev in events.iter().take(n as usize) {
                    let payload = if ev.payload_len == 0 {
                        Vec::new()
                    } else {
                        unsafe { slice::from_raw_parts(ev.payload_ptr, ev.payload_len as usize) }
                            .to_vec()
                    };
                    unsafe { aster_upcall_buffer_release(rt, upcalls, ev.buffer) };
                    sn.lock().unwrap().push(Seen {
                        kind: ev.kind,
                        cancel_reason: ev.cancel_reason,
                        payload: payload.clone(),
                    });
                    if ev.kind == K_CANCEL {
                        continue;
                    }
                    if let Reply::Answer(status, body) = handler(ev.kind, &payload) {
                        unsafe {
                            aster_upcall_respond(
                                rt,
                                upcalls,
                                ev.invocation_id,
                                status,
                                if body.is_empty() {
                                    ptr::null()
                                } else {
                                    body.as_ptr()
                                },
                                body.len() as u32,
                            )
                        };
                    }
                }
            }
        });
        Foreign {
            stop,
            seen,
            thread: Some(thread),
        }
    }

    fn kinds(&self) -> Vec<u16> {
        self.seen.lock().unwrap().iter().map(|s| s.kind).collect()
    }
}

impl Drop for Foreign {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn nid(b: &[u8]) -> CoordinationNodeId {
    CoordinationNodeId::from_array(b.try_into().unwrap())
}

/// One cluster per node, every pair corroborated far apart.
fn view(ids: &[Vec<u8>]) -> Vec<u8> {
    let mut hex: Vec<String> = ids.iter().map(hex::encode).collect();
    hex.sort();
    let mut far = Vec::new();
    for (i, a) in hex.iter().enumerate() {
        for b in &hex[i + 1..] {
            far.push((a.clone(), b.clone()));
        }
    }
    ucv1::encode_topology_snapshot(&ucv1::TopologyViewParts {
        clusters: hex
            .iter()
            .map(|id| ClusterView {
                members: vec![id.clone()],
                bridge: id.clone(),
                witnesses: vec![id.clone()],
            })
            .collect(),
        far_pairs: far,
        separation_coverage: 1,
    })
}

fn policy(
    status: &aster::coordination::store::wire::GroupStatusReplyWireV1,
    eligible: &[Vec<u8>],
    excluded: &[Vec<u8>],
) -> Vec<u8> {
    ucv1::encode_placement_policy(&PlacementPolicy {
        revision: 1,
        digest: [0x44; 32],
        authority_public_key: status.authority_public_key.clone().try_into().unwrap(),
        authority_domain: status.authority_domain.clone(),
        target_voters: 1,
        target_retained_learners: 0,
        eligible: eligible
            .iter()
            .map(|id| {
                (
                    nid(id),
                    PlacementCandidate {
                        node_id: nid(id),
                        member_seed: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        pinned_voters: BTreeSet::new(),
        excluded: excluded.iter().map(|id| nid(id)).collect(),
        minimum_separated_voter_pairs: 0,
        replacement_grace: Duration::from_secs(60),
        allow_healthy_rebalance: false,
    })
}

#[test]
fn placement_manager_over_foreign_providers() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        let status =
            codec::decode_group_status(&read(rt, root, &ReadRequest::GroupStatus)).unwrap();
        let voter = status.voters[0].node_id.clone();
        let newcomer = vec![0x7a; 32];
        let mut upcalls = 0;
        assert_eq!(aster_upcalls_new(rt, 0, &mut upcalls), OK);

        let reconcile = |m: aster_coordination_placement_manager_t| {
            let mut op = 0;
            assert_eq!(
                aster_coordination_placement_reconcile(rt, m, 0, &mut op),
                OK
            );
            finish(rt, op)
        };
        let manager = || {
            let mut m = 0;
            assert_eq!(
                aster_coordination_placement_manager_new(rt, root, upcalls, 0, &mut m),
                OK
            );
            assert_ne!(m, 0);
            m
        };

        // 1. Roles satisfied: the binding answers policy (32) and topology (17).
        {
            let (st, v) = (status.clone(), voter.clone());
            let foreign = Foreign::start(rt, upcalls, move |kind, _| match kind {
                K_POLICY => Reply::Answer(S_OK, policy(&st, std::slice::from_ref(&v), &[])),
                K_TOPOLOGY => Reply::Answer(S_OK, view(std::slice::from_ref(&v))),
                _ => Reply::Answer(S_ERROR, b"not expected".to_vec()),
            });
            let m = manager();
            let ev = reconcile(m).expect(E_RESULT);
            let report = codec::decode_placement_report(&ev.data).unwrap();
            assert_eq!(report.reason, "current roles already satisfy policy");
            assert_eq!(report.policy_revision, 1);
            assert_eq!(report.policy_digest, [0x44; 32]);
            assert_eq!(report.current_voters, vec![voter.clone()]);
            assert!(foreign.kinds().contains(&K_POLICY));
            assert!(foreign.kinds().contains(&K_TOPOLOGY));
            assert!(!foreign.kinds().contains(&K_PROVISION));
            assert_eq!(aster_coordination_placement_manager_free(rt, m), OK);
            assert_eq!(aster_coordination_placement_manager_free(rt, m), NOT_FOUND);
        }

        // 2. A candidate without seed material goes to the provisioner (33)
        //    with the idempotency tuple; its failure fails the pass.
        {
            let (st, v, n) = (status.clone(), voter.clone(), newcomer.clone());
            let foreign = Foreign::start(rt, upcalls, move |kind, _| match kind {
                K_POLICY => Reply::Answer(
                    S_OK,
                    policy(&st, &[v.clone(), n.clone()], std::slice::from_ref(&v)),
                ),
                K_TOPOLOGY => Reply::Answer(S_OK, view(&[v.clone(), n.clone()])),
                K_PROVISION => Reply::Answer(S_ERROR, b"no capacity in that zone".to_vec()),
                _ => Reply::Answer(S_UNIMPLEMENTED, vec![]),
            });
            let m = manager();
            let ev = reconcile(m);
            assert_eq!(ev.kind(), E_ERROR);
            assert!(
                ev.text().contains("no capacity in that zone"),
                "{}",
                ev.text()
            );
            let seen = foreign.seen.lock().unwrap().clone();
            let prov: Vec<_> = seen.iter().filter(|s| s.kind == K_PROVISION).collect();
            assert_eq!(prov.len(), 1);
            let req = ucv1::decode_provision_request(&prov[0].payload).unwrap();
            assert_eq!(req.node_id.as_bytes().to_vec(), newcomer);
            assert_eq!(req.policy_digest, [0x44; 32]);
            assert_eq!(req.group_id, group().fixture.group_id());
            assert_eq!(req.membership_log_id, status.membership_log_id);
            assert_eq!(aster_coordination_placement_manager_free(rt, m), OK);
        }

        // 3. A provisioner answering for a different node fails the pass.
        {
            let (st, v, n) = (status.clone(), voter.clone(), newcomer.clone());
            let _foreign = Foreign::start(rt, upcalls, move |kind, _| match kind {
                K_POLICY => Reply::Answer(
                    S_OK,
                    policy(&st, &[v.clone(), n.clone()], std::slice::from_ref(&v)),
                ),
                K_TOPOLOGY => Reply::Answer(S_OK, view(&[v.clone(), n.clone()])),
                K_PROVISION => Reply::Answer(
                    S_OK,
                    ucv1::encode_member_seed(&MemberSeedV1 {
                        node_id: vec![0x7b; 32],
                        ticket: "t".into(),
                        signed_join_permit: None,
                    }),
                ),
                _ => Reply::Answer(S_UNIMPLEMENTED, vec![]),
            });
            let m = manager();
            let ev = reconcile(m);
            assert_eq!(ev.kind(), E_ERROR);
            assert!(ev.text().contains("different node"), "{}", ev.text());
            aster_coordination_placement_manager_free(rt, m);
        }

        // 4. Failure modes of the providers: no fail-open for placement.
        for (case, kind_answer) in [
            ("topology error", 0u8),
            ("policy unimplemented", 1),
            ("policy malformed", 2),
            ("policy timeout", 3),
        ] {
            assert_eq!(
                aster_upcall_set_policy(rt, upcalls, K_POLICY as u32, 300, 1),
                OK
            );
            let (st, v) = (status.clone(), voter.clone());
            let foreign = Foreign::start(rt, upcalls, move |kind, _| match (kind, kind_answer) {
                (K_TOPOLOGY, 0) => Reply::Answer(S_ERROR, b"topology offline".to_vec()),
                (K_TOPOLOGY, _) => Reply::Answer(S_OK, view(std::slice::from_ref(&v))),
                (K_POLICY, 1) => Reply::Answer(S_UNIMPLEMENTED, vec![]),
                (K_POLICY, 2) => Reply::Answer(S_OK, vec![0xBD, 0x01, 0xFF]),
                (K_POLICY, 3) => Reply::Ignore,
                (K_POLICY, _) => Reply::Answer(S_OK, policy(&st, std::slice::from_ref(&v), &[])),
                _ => Reply::Answer(S_UNIMPLEMENTED, vec![]),
            });
            let m = manager();
            let ev = reconcile(m);
            assert_eq!(ev.kind(), E_ERROR, "{case}: {}", ev.text());
            if kind_answer == 0 {
                assert!(
                    ev.text().contains("topology offline"),
                    "{case}: {}",
                    ev.text()
                );
            }
            if kind_answer == 3 {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !foreign
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|s| s.kind == K_CANCEL && s.cancel_reason == 1)
                {
                    assert!(Instant::now() < deadline, "{case}: no CANCEL{{timeout}}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            aster_coordination_placement_manager_free(rt, m);
        }

        // 5. A node topology needs a known node; a closed client retires
        //    its manager.
        let mut m = 0;
        assert_eq!(
            aster_coordination_placement_manager_new(rt, root, upcalls, 0xbad, &mut m),
            NOT_FOUND
        );
        assert_eq!(
            aster_coordination_placement_manager_new(rt, root, 0xbad, 0, &mut m),
            NOT_FOUND
        );
        let m = manager();
        assert_eq!(aster_coordination_client_close(rt, root), OK);
        let mut op = 0;
        assert_eq!(
            aster_coordination_placement_reconcile(rt, m, 0, &mut op),
            ALREADY_CLOSED
        );
        assert_eq!(aster_coordination_placement_manager_free(rt, m), OK);
        let mut m2 = 0;
        assert_eq!(
            aster_coordination_placement_manager_new(rt, root, upcalls, 0, &mut m2),
            ALREADY_CLOSED
        );

        assert_eq!(aster_upcalls_free(rt, upcalls), OK);
        assert_eq!(aster_coordination_client_free(rt, root), OK);
    }
}

// ── concurrency ──────────────────────────────────────────────────────────────

#[test]
fn a_release_waits_for_an_in_flight_compare_and_set() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        let w_node = start_node(rt, actor_secret());
        let wdir = tempfile::tempdir().unwrap();
        let w = open(rt, w_node, Some(wdir.path()));
        let r = res("ffi.race", "k");
        grant(rt, root, &r, local_id(rt, w));
        for round in 0..3u64 {
            let mut op = 0;
            assert_eq!(
                aster_coordination_lease_acquire(
                    rt,
                    w,
                    bytes(b"ffi.race"),
                    bytes(b"k"),
                    10_000,
                    0,
                    0,
                    0,
                    &mut op
                ),
                OK
            );
            let lease = finish(rt, op).expect(E_LEASE).ev.handle;
            assert_ne!(lease, 0, "round {round}");
            let snap =
                codec::decode_read_reply(&read(rt, w, &ReadRequest::ReadLinearizable(r.clone())))
                    .unwrap();
            let (mut cas, mut rel) = (0, 0);
            let value = round.to_le_bytes();
            assert_eq!(
                aster_coordination_lease_compare_and_set(
                    rt,
                    lease,
                    snap.value_revision,
                    bytes(&value),
                    0,
                    &mut cas
                ),
                OK
            );
            assert_eq!(aster_coordination_lease_release(rt, lease, 0, &mut rel), OK);
            // The CAS ran under the fence; the release waited for it.
            let reply =
                codec::decode_mutation_reply(&finish(rt, cas).expect(E_RESULT).data).unwrap();
            assert_eq!(reply.outcome_code, 3, "round {round}: {reply:?}");
            finish(rt, rel).expect(E_UNIT);
            let after =
                codec::decode_read_reply(&read(rt, w, &ReadRequest::ReadLinearizable(r.clone())))
                    .unwrap();
            assert!(!after.held);
            assert_eq!(after.value.as_deref(), Some(&value[..]));
        }
        for c in [root, w] {
            assert_eq!(aster_coordination_client_free(rt, c), OK);
        }
    }
}

#[test]
fn concurrent_reads_share_one_client() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let c = open(rt, root_node, None);
        let req = codec::encode_read_request(&ReadRequest::GroupStatus);
        let ops: Vec<u64> = (0..16)
            .map(|i| {
                let mut op = 0;
                assert_eq!(aster_coordination_read(rt, c, bytes(&req), i, &mut op), OK);
                op
            })
            .collect();
        // Free the handle with the reads in flight: they complete anyway.
        assert_eq!(aster_coordination_client_free(rt, c), OK);
        for (i, op) in ops.into_iter().enumerate() {
            let ev = finish(rt, op).expect(E_RESULT);
            assert_eq!(ev.ev.user_data, i as u64);
            assert_eq!(
                codec::decode_group_status(&ev.data).unwrap().voters.len(),
                1
            );
        }
    }
}

// ── close: the deterministic halves ──────────────────────────────────────────

fn grant_req(key: &str, target: u8) -> MutationRequest {
    MutationRequest::SetGrant {
        resource: res("ffi.window", key),
        target_node_id: [target; 32],
        rights: RIGHT_READ,
        expected_acl_revision: 0,
    }
}

/// Reopen `dir` and require the unacknowledged result to be `expected`.
unsafe fn reopen_and_redeliver(
    rt: aster_runtime_t,
    node: aster_node_t,
    dir: &std::path::Path,
    expected: Option<&[u8]>,
) -> MutationReplyWireV1 {
    let c = open(rt, node, Some(dir));
    let mut op = 0;
    assert_eq!(aster_coordination_unacknowledged(rt, c, 0, &mut op), OK);
    let again = finish(rt, op).expect(E_RESOLVED);
    assert_ne!(again.ev.handle, 0, "the result was redelivered");
    if let Some(expected) = expected {
        assert_eq!(again.data, expected, "the exact saved reply");
    }
    assert_eq!(
        aster_coordination_resolved_acknowledge(rt, again.ev.handle, 0, &mut op),
        OK
    );
    finish(rt, op).expect(E_UNIT);
    assert_eq!(aster_coordination_client_free(rt, c), OK);
    codec::decode_mutation_reply(&again.data).unwrap()
}

#[test]
fn close_after_resolution_makes_the_handle_inert_and_redelivers() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        let ev = mutate(rt, root, &grant_req("after", 0x51)).expect(E_RESOLVED);
        assert_ne!(ev.ev.handle, 0);
        assert_eq!(aster_coordination_client_close(rt, root), OK);
        let mut op = 0;
        // Consumed even though it fails: a later free is NOT_FOUND.
        assert_eq!(
            aster_coordination_resolved_acknowledge(rt, ev.ev.handle, 0, &mut op),
            ALREADY_CLOSED
        );
        assert_eq!(
            aster_coordination_resolved_free(rt, ev.ev.handle),
            NOT_FOUND
        );
        assert_eq!(aster_coordination_client_free(rt, root), OK);
        let reply = reopen_and_redeliver(rt, root_node, dir.path(), Some(&ev.data));
        assert_eq!(reply.outcome_code, 6);
    }
}

#[test]
fn resolution_after_close_is_an_error_and_redelivered() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        // Hold the mutation between "resolved and fsynced" and "handed out".
        let gate = aster_transport_ffi::coordination::test_hooks::arm(root);
        let req = codec::encode_mutation_request(&grant_req("before", 0x52));
        let mut op = 0;
        assert_eq!(
            aster_coordination_mutate(rt, root, bytes(&req), 0, &mut op),
            OK
        );
        assert!(
            gate.wait_reached(Duration::from_secs(60)),
            "the mutation resolved"
        );
        assert_eq!(aster_coordination_client_close(rt, root), OK);
        gate.release();
        let ev = finish(rt, op)
            .expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_CLOSED);
        assert_eq!(ev.ev.status, ALREADY_CLOSED as u32);
        assert_eq!(aster_coordination_client_free(rt, root), OK);
        let reply = reopen_and_redeliver(rt, root_node, dir.path(), None);
        assert_eq!(reply.outcome_code, 6);
        assert_eq!(reply.sequence, 1);
    }
}

#[test]
fn nothing_new_starts_after_close_even_with_a_take_pending() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        let w_node = start_node(rt, actor_secret());
        let wdir = tempfile::tempdir().unwrap();
        let w = open(rt, w_node, Some(wdir.path()));
        let r = res("ffi.window", "lease");
        grant(rt, root, &r, local_id(rt, w));
        let mut op = 0;
        assert_eq!(
            aster_coordination_lease_acquire(
                rt,
                w,
                bytes(b"ffi.window"),
                bytes(b"lease"),
                10_000,
                0,
                0,
                0,
                &mut op
            ),
            OK
        );
        let lease = finish(rt, op).expect(E_LEASE).ev.handle;
        // A CAS holds the lease's read guard; close must defer its take.
        let mut cas = 0;
        assert_eq!(
            aster_coordination_lease_compare_and_set(rt, lease, 0, bytes(b"v"), 0, &mut cas),
            OK
        );
        assert_eq!(aster_coordination_client_close(rt, w), OK);
        // Synchronously refused, whether or not the deferred take ran.
        assert_eq!(
            aster_coordination_lease_compare_and_set(rt, lease, 0, bytes(b"w"), 0, &mut op),
            ALREADY_CLOSED
        );
        let mut len = 0usize;
        assert_eq!(
            aster_coordination_lease_fence(rt, lease, ptr::null_mut(), 0, &mut len),
            ALREADY_CLOSED
        );
        let mut st = 9;
        assert_eq!(aster_coordination_lease_status(rt, lease, &mut st), OK);
        assert_eq!(st, aster_lease_status_t::ASTER_LEASE_STATUS_RELEASED as u32);
        // The CAS that started before the close completes.
        let first = finish(rt, cas).expect(E_RESULT);
        assert_eq!(
            codec::decode_mutation_reply(&first.data)
                .unwrap()
                .outcome_code,
            3
        );
        assert_eq!(aster_coordination_lease_release(rt, lease, 0, &mut op), OK);
        finish(rt, op).expect_error(aster_coordination_error_t::ASTER_COORDINATION_ERROR_CLOSED);

        // A placement pass parked on its policy upcall; close; a new pass is
        // refused at once.
        let status =
            codec::decode_group_status(&read(rt, root, &ReadRequest::GroupStatus)).unwrap();
        let mut upcalls = 0;
        assert_eq!(aster_upcalls_new(rt, 0, &mut upcalls), OK);
        assert_eq!(
            aster_upcall_set_policy(rt, upcalls, K_POLICY as u32, 30_000, 1),
            OK
        );
        let release = Arc::new(AtomicBool::new(false));
        let (st2, v, rel) = (
            status.clone(),
            status.voters[0].node_id.clone(),
            release.clone(),
        );
        let _foreign = Foreign::start(rt, upcalls, move |kind, _| match kind {
            K_TOPOLOGY => Reply::Answer(S_OK, view(std::slice::from_ref(&v))),
            K_POLICY => {
                while !rel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Reply::Answer(S_OK, policy(&st2, std::slice::from_ref(&v), &[]))
            }
            _ => Reply::Answer(S_UNIMPLEMENTED, vec![]),
        });
        let mut m = 0;
        assert_eq!(
            aster_coordination_placement_manager_new(rt, root, upcalls, 0, &mut m),
            OK
        );
        let mut pass = 0;
        assert_eq!(
            aster_coordination_placement_reconcile(rt, m, 0, &mut pass),
            OK
        );
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(aster_coordination_client_close(rt, root), OK);
        assert_eq!(
            aster_coordination_placement_reconcile(rt, m, 0, &mut op),
            ALREADY_CLOSED
        );
        release.store(true, Ordering::Release);
        let ev = finish(rt, pass);
        assert!(ev.kind() == E_RESULT || ev.kind() == E_ERROR);
        assert_eq!(aster_coordination_placement_manager_free(rt, m), OK);
        drop(_foreign);
        assert_eq!(aster_upcalls_free(rt, upcalls), OK);
        for c in [root, w] {
            assert_eq!(aster_coordination_client_free(rt, c), OK);
        }
    }
}

// ── placement with the node's own topology ───────────────────────────────────

unsafe fn start_monitoring_node(rt: aster_runtime_t) -> aster_node_t {
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.enable_monitoring = 1;
    let mut op = 0;
    assert_eq!(aster_node_start(rt, &cfg, 0, 0, &mut op), OK);
    finish(rt, op)
        .expect(aster_event_kind_t::ASTER_EVENT_NODE_CREATED as u32)
        .ev
        .handle
}

#[test]
fn placement_reads_a_node_topology() {
    unsafe {
        let Shared { rt, root_node } = *shared();
        let dir = tempfile::tempdir().unwrap();
        let root = open(rt, root_node, Some(dir.path()));
        let status =
            codec::decode_group_status(&read(rt, root, &ReadRequest::GroupStatus)).unwrap();
        let voter = status.voters[0].node_id.clone();
        let mut upcalls = 0;
        assert_eq!(aster_upcalls_new(rt, 0, &mut upcalls), OK);
        let (st, v) = (status.clone(), voter.clone());
        let foreign = Foreign::start(rt, upcalls, move |kind, _| match kind {
            K_POLICY => Reply::Answer(S_OK, policy(&st, std::slice::from_ref(&v), &[])),
            _ => Reply::Answer(S_UNIMPLEMENTED, vec![]),
        });
        let topo = start_monitoring_node(rt);

        // Not in a swarm: the node has no shared view, the pass fails.
        let mut m = 0;
        assert_eq!(
            aster_coordination_placement_manager_new(rt, root, upcalls, topo, &mut m),
            OK
        );
        let mut op = 0;
        assert_eq!(
            aster_coordination_placement_reconcile(rt, m, 0, &mut op),
            OK
        );
        let ev = finish(rt, op);
        assert_eq!(ev.kind(), E_ERROR, "no shared view: {}", ev.text());
        assert_eq!(aster_coordination_placement_manager_free(rt, m), OK);

        // Joined (alone): the pass runs on the node's view. The voter is no
        // vertex of it, so the planner reports missing evidence -- a result.
        let secret = [0x3c; 32];
        assert_eq!(
            aster_topology_enable_shared(
                rt,
                topo,
                secret.as_ptr(),
                0,
                aster_bytes_list_t {
                    items: ptr::null(),
                    len: 0
                },
                0,
                0,
                &mut op
            ),
            OK
        );
        finish(rt, op).expect(E_UNIT);
        assert_eq!(
            aster_coordination_placement_manager_new(rt, root, upcalls, topo, &mut m),
            OK
        );
        assert_eq!(
            aster_coordination_placement_reconcile(rt, m, 0, &mut op),
            OK
        );
        let ev = finish(rt, op).expect(E_RESULT);
        let report = codec::decode_placement_report(&ev.data).unwrap();
        assert_eq!(report.policy_revision, 1);
        assert_eq!(report.current_voters, vec![voter]);
        assert!(!report.reason.is_empty());
        assert!(
            !foreign.kinds().contains(&K_TOPOLOGY),
            "the node's view, not kind 17"
        );
        assert_eq!(aster_coordination_placement_manager_free(rt, m), OK);
        drop(foreign);
        assert_eq!(aster_upcalls_free(rt, upcalls), OK);
        assert_eq!(aster_coordination_client_free(rt, root), OK);
        assert_eq!(aster_node_free(rt, topo), OK);
    }
}
