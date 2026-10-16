//! Local measured peer views and owned immutable shared topology observations.
//! JSON is a host DTO only; derivation, admission and Unknown stay native.
use crate::data::{
    bridge_for, in_hex32, in_node_list, in_string, invalid, spawn_op_coded, Done, OpError,
};
use crate::upcall::{admitted_for, aster_topology_admitted_t};
use crate::*;
use aster::topology::{
    ClusterView, PeerBlobStats, PeerView, QuicTraffic, SeparationVerdict, Topology,
    TopologySwarmOptions,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Runtime-local owner of one immutable shared view. Free explicitly, or close
/// its runtime. The observation remains readable after its source node closes.
pub type aster_topology_snapshot_t = u64;

/// Unknown is absence of adequate evidence, never proof of separation.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_topology_separation_t {
    ASTER_TOPOLOGY_UNKNOWN = 0,
    ASTER_TOPOLOGY_CONNECTED = 1,
    ASTER_TOPOLOGY_SEPARATED = 2,
}
macro_rules! attempt {
    ($value:expr) => {
        match $value {
            Ok(value) => value,
            Err(status) => return status,
        }
    };
}
fn topology(bridge: &BridgeRuntime, node: u64) -> Result<Topology, i32> {
    if bridge.closing.load(Ordering::Acquire) {
        return Err(aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32);
    }
    bridge
        .nodes
        .get(node)
        .map(|core| aster::Node::from_core_node((*core).clone()).topology())
        .ok_or(aster_status_t::ASTER_STATUS_NOT_FOUND as i32)
}
fn peer_json(peer: PeerView) -> Value {
    json!({
        "node_id": peer.node_id, "level": peer.level.as_i32(), "level_reason": peer.level_reason,
        "rtt_us": peer.rtt_us, "jitter_us": peer.jitter_us, "loss_ppm": peer.loss_ppm,
        "throughput_bps": peer.throughput_bps, "cwnd_ceiling_bps": peer.cwnd_ceiling_bps,
        "direct": peer.direct, "relay_url": peer.relay_url,
        "remote_addr": peer.remote_addr.map(|address| address.to_string()),
        "congestion_events": peer.congestion_events, "samples": peer.samples,
        "confidence_ppm": peer.confidence_ppm, "last_measured_unix_ms": peer.last_measured_unix_ms,
        "cluster_held_since_unix_ms": peer.cluster_held_since_unix_ms, "is_connected": peer.is_connected,
        "quic_traffic": quic_traffic_json(&peer.quic_traffic),
        "blob_transfer": blob_transfer_json(&peer.blob_transfer),
    })
}
/// ABI 7.4: QUIC bytes per protocol family (`blobs`, `blobs-zstd`, `docs`,
/// `gossip`, `rpc`, `other`), each `{"quic_bytes_sent", "quic_bytes_received"}`,
/// cumulative since the node started and across reconnects.
fn quic_traffic_json(traffic: &QuicTraffic) -> Value {
    Value::Object(
        traffic
            .iter()
            .map(|(family, bytes)| {
                (
                    family.as_str().to_string(),
                    json!({
                        "quic_bytes_sent": bytes.quic_bytes_sent,
                        "quic_bytes_received": bytes.quic_bytes_received,
                    }),
                )
            })
            .collect(),
    )
}
/// ABI 7.4: exact blob payload (and zstd wire) bytes exchanged with the peer.
fn blob_transfer_json(b: &PeerBlobStats) -> Value {
    json!({
        "fetched_payload_bytes": b.fetched_payload_bytes,
        "fetched_zstd_payload_bytes": b.fetched_zstd_payload_bytes,
        "fetched_wire_bytes": b.fetched_wire_bytes,
        "fetched_plain_bytes": b.fetched_plain_bytes,
        "served_payload_bytes": b.served_payload_bytes,
        "served_zstd_payload_bytes": b.served_zstd_payload_bytes,
        "served_wire_bytes": b.served_wire_bytes,
        "served_plain_bytes": b.served_plain_bytes,
        "plain_fallbacks": b.plain_fallbacks,
    })
}
fn cluster_json(cluster: &ClusterView) -> Value {
    json!({"members": cluster.members, "bridge": cluster.bridge, "witnesses": cluster.witnesses})
}
unsafe fn prepare(out: *mut aster_bytes_t) -> Result<(), i32> {
    if out.is_null() {
        return Err(invalid("NULL topology bytes output"));
    }
    unsafe {
        ptr::write(
            out,
            aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
        );
    }
    Ok(())
}
unsafe fn output(out: *mut aster_bytes_t, value: Value) -> i32 {
    unsafe {
        ptr::write(out, alloc_owned_bytes(value.to_string().into_bytes()));
    }
    aster_status_t::ASTER_STATUS_OK as i32
}
fn op_error(error: aster::Error) -> OpError {
    let status = if matches!(error, aster::Error::InvalidArgument(_)) {
        aster_status_t::ASTER_STATUS_INVALID_ARGUMENT
    } else {
        aster_status_t::ASTER_STATUS_INTERNAL
    };
    OpError {
        status,
        code: status as i32,
        flags: 0,
        message: error.to_string(),
    }
}

/// Whether this node has monitoring, the prerequisite for measured local peers.
/// # Safety
/// out must be writable for one u32.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_has_monitoring(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out: *mut u32,
) -> i32 {
    if out.is_null() {
        return invalid("NULL monitoring output");
    }
    unsafe {
        ptr::write(out, 0);
    }
    let bridge = attempt!(bridge_for(runtime));
    let topology = attempt!(topology(&bridge, node));
    unsafe {
        ptr::write(out, u32::from(topology.has_monitoring()));
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Every known local peer, including decaying recently disconnected peers.
/// Output is owned UTF-8 JSON; free with aster_string_release. Level is 0..4,
/// confidence_ppm is native integer confidence; nullable RTT/jitter stay null.
/// remote_addr is local-only and never added to the baseline Topology RPC.
/// ABI 7.4 adds `quic_traffic` (an object keyed by protocol family `blobs`,
/// `blobs-zstd`, `docs`, `gossip`, `rpc`, `other`, each
/// `{quic_bytes_sent, quic_bytes_received}`) and `blob_transfer`
/// (`fetched_payload_bytes`, `fetched_zstd_payload_bytes`,
/// `fetched_wire_bytes`, `fetched_plain_bytes`, `served_payload_bytes`,
/// `served_zstd_payload_bytes`, `served_wire_bytes`, `served_plain_bytes`,
/// `plain_fallbacks`), all cumulative integers.
/// # Safety
/// out must be writable for one byte span.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_peers(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out: *mut aster_bytes_t,
) -> i32 {
    attempt!(unsafe { prepare(out) });
    let bridge = attempt!(bridge_for(runtime));
    let topology = attempt!(topology(&bridge, node));
    unsafe {
        output(
            out,
            Value::Array(topology.peers().into_iter().map(peer_json).collect()),
        )
    }
}

/// One local peer object, or JSON null when unknown/unmeasured/no monitoring.
/// node_id is exactly 64 ASCII hex characters. Release owned JSON with string_release.
/// # Safety
/// node_id must be readable for its length and out writable for one span.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_peer(
    runtime: aster_runtime_t,
    node: aster_node_t,
    node_id: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    attempt!(unsafe { prepare(out) });
    let id = attempt!(unsafe { in_hex32(&node_id, "node_id") });
    let bridge = attempt!(bridge_for(runtime));
    let topology = attempt!(topology(&bridge, node));
    unsafe {
        output(
            out,
            topology
                .peer(&aster::NodeId::from_hex(id))
                .map(peer_json)
                .unwrap_or(Value::Null),
        )
    }
}

/// Capture one shared observation. Requires enable_shared first. Completes with
/// TOPOLOGY_SNAPSHOT, handle = runtime-local snapshot. All queries on that owner
/// use the same immutable native view. Cancellation discards an unseen owner.
/// # Safety
/// out_operation must be writable for one operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_snapshot(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL topology operation output");
    }
    unsafe {
        ptr::write(out_operation, 0);
    }
    let bridge = attempt!(bridge_for(runtime));
    let topology = attempt!(topology(&bridge, node));
    let owner = bridge.clone();
    unsafe {
        spawn_op_coded(&bridge, user_data, out_operation, async move {
            let snapshot = topology.snapshot().await.map_err(op_error)?;
            if owner.closing.load(Ordering::Acquire) {
                return Err(op_error(aster::Error::InvalidArgument(
                    "runtime closing".into(),
                )));
            }
            let handle = owner.topology_snapshots.insert(snapshot);
            if owner.closing.load(Ordering::Acquire) {
                owner.topology_snapshots.remove(handle);
                return Err(op_error(aster::Error::InvalidArgument(
                    "runtime closing".into(),
                )));
            }
            Ok(
                Done::ok(aster_event_kind_t::ASTER_EVENT_TOPOLOGY_SNAPSHOT, handle).on_discard(
                    move || {
                        owner.topology_snapshots.remove(handle);
                    },
                ),
            )
        })
    }
}

/// Free an immutable observation. Repeated free or another runtime returns NOT_FOUND.
#[no_mangle]
pub extern "C" fn aster_topology_snapshot_free(
    runtime: aster_runtime_t,
    snapshot: aster_topology_snapshot_t,
) -> i32 {
    let bridge = attempt!(bridge_for(runtime));
    if bridge.topology_snapshots.remove(snapshot).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Clusters from one captured observation, as owned UTF-8 JSON array.
/// # Safety
/// out must be writable for one byte span.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_snapshot_clusters(
    runtime: aster_runtime_t,
    snapshot: aster_topology_snapshot_t,
    out: *mut aster_bytes_t,
) -> i32 {
    attempt!(unsafe { prepare(out) });
    let bridge = attempt!(bridge_for(runtime));
    let Some(snapshot) = bridge.topology_snapshots.get(snapshot) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    unsafe {
        output(
            out,
            Value::Array(snapshot.clusters().iter().map(cluster_json).collect()),
        )
    }
}

/// Cluster containing node_id from that observation, or owned JSON null.
/// # Safety
/// node_id must be readable for its length and out writable for one span.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_snapshot_cluster_of(
    runtime: aster_runtime_t,
    snapshot: aster_topology_snapshot_t,
    node_id: aster_bytes_t,
    out: *mut aster_bytes_t,
) -> i32 {
    attempt!(unsafe { prepare(out) });
    let node = attempt!(unsafe { in_hex32(&node_id, "node_id") });
    let bridge = attempt!(bridge_for(runtime));
    let Some(snapshot) = bridge.topology_snapshots.get(snapshot) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    unsafe {
        output(
            out,
            snapshot
                .cluster_of(&aster::NodeId::from_hex(node))
                .map(cluster_json)
                .unwrap_or(Value::Null),
        )
    }
}

/// Unknown=0, Connected=1, Separated=2, always from the captured observation.
/// # Safety
/// a and b must be readable for their lengths, and out writable for one u32.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_snapshot_separated(
    runtime: aster_runtime_t,
    snapshot: aster_topology_snapshot_t,
    a: aster_bytes_t,
    b: aster_bytes_t,
    out: *mut u32,
) -> i32 {
    if out.is_null() {
        return invalid("NULL separation output");
    }
    unsafe {
        ptr::write(
            out,
            aster_topology_separation_t::ASTER_TOPOLOGY_UNKNOWN as u32,
        );
    }
    let a = attempt!(unsafe { in_hex32(&a, "a") });
    let b = attempt!(unsafe { in_hex32(&b, "b") });
    let bridge = attempt!(bridge_for(runtime));
    let Some(snapshot) = bridge.topology_snapshots.get(snapshot) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let value = match snapshot.separated_hex(&a, &b) {
        SeparationVerdict::Unknown => aster_topology_separation_t::ASTER_TOPOLOGY_UNKNOWN,
        SeparationVerdict::Connected => aster_topology_separation_t::ASTER_TOPOLOGY_CONNECTED,
        SeparationVerdict::Separated => aster_topology_separation_t::ASTER_TOPOLOGY_SEPARATED,
    };
    unsafe {
        ptr::write(out, value as u32);
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    position_refresh_ms: Option<u32>,
    min_hold_ms: Option<u32>,
    liveness_ttl_ms: Option<u32>,
    edge_ttl_ms: Option<u32>,
    max_skew_ms: Option<u32>,
    separation_coverage: Option<u32>,
    witness_r: Option<u32>,
}
impl Options {
    fn apply(self, options: &mut TopologySwarmOptions) {
        if let Some(value) = self.position_refresh_ms {
            options.position_refresh = Duration::from_millis(value.into());
        }
        if let Some(value) = self.min_hold_ms {
            options.timing.min_hold = Duration::from_millis(value.into());
        }
        if let Some(value) = self.liveness_ttl_ms {
            options.timing.liveness_ttl = Duration::from_millis(value.into());
        }
        if let Some(value) = self.edge_ttl_ms {
            options.timing.edge_ttl = Duration::from_millis(value.into());
        }
        if let Some(value) = self.max_skew_ms {
            options.timing.max_skew = Duration::from_millis(value.into());
        }
        if let Some(value) = self.separation_coverage {
            options.timing.separation_coverage = value as usize;
        }
        if let Some(value) = self.witness_r {
            options.timing.witness_r = value as usize;
        }
    }
}

/// Join shared topology with native timing/coverage options. Missing fields use
/// defaults; explicit zero is retained, except position_refresh_ms=0 means 60s.
/// All options are optional u32 values (milliseconds max ~49.7 days); JSON
/// rejects unknown fields and inputs over 16KiB. An empty span means {}.
/// admitted is runtime-local (0 means every holder
/// of the namespace secret counts). Completes with UNIT_RESULT, handle=node.
/// Like the legacy enable call, cancellation does not roll back a native join
/// already in progress. The publisher remains owned by the node until rejoin/close.
/// # Safety
/// secret must point to 32 readable bytes, input spans/list must be readable for
/// their advertised lengths, and out_operation writable for one operation ID.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_topology_enable_shared_with_options(
    runtime: aster_runtime_t,
    node: aster_node_t,
    secret: *const u8,
    admitted: aster_topology_admitted_t,
    bootstrap: aster_bytes_list_t,
    options_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("NULL topology operation output");
    }
    unsafe {
        ptr::write(out_operation, 0);
    }
    if secret.is_null() {
        return invalid("NULL topology namespace secret");
    }
    if options_json.len > 16 * 1024 {
        return invalid("topology options exceed 16 KiB");
    }
    let json = attempt!(unsafe { in_string(&options_json, "options_json") });
    let decoded: Options =
        attempt!(serde_json::from_str(if json.is_empty() { "{}" } else { &json }).map_err(invalid));
    let mut options = TopologySwarmOptions {
        bootstrap: attempt!(unsafe { in_node_list(&bootstrap, "bootstrap") }),
        ..Default::default()
    };
    decoded.apply(&mut options);
    if admitted != 0 {
        options.admitted = Some(attempt!(admitted_for(runtime, admitted)).admitted_fn());
    }
    let secret = aster::NamespaceSecret::from_bytes(
        unsafe { slice::from_raw_parts(secret, 32) }
            .try_into()
            .expect("fixed length"),
    );
    let bridge = attempt!(bridge_for(runtime));
    let topology = attempt!(topology(&bridge, node));
    if !topology.has_monitoring() {
        return invalid("shared topology requires monitoring");
    }
    unsafe {
        spawn_op_coded(&bridge, user_data, out_operation, async move {
            topology
                .enable_shared(&secret, options)
                .await
                .map_err(op_error)?;
            Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, node))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runtime() -> u64 {
        let mut id = 0;
        assert_eq!(
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut id) },
            0
        );
        id
    }
    fn bytes(value: &str) -> aster_bytes_t {
        aster_bytes_t {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }
    unsafe fn value(out: aster_bytes_t) -> Value {
        let result =
            serde_json::from_slice(unsafe { slice::from_raw_parts(out.ptr, out.len) }).unwrap();
        aster_string_release(out.ptr, out.len);
        result
    }
    fn cluster(members: &[String]) -> ClusterView {
        ClusterView {
            members: members.to_vec(),
            bridge: members[0].clone(),
            witnesses: vec![members[0].clone()],
        }
    }
    #[test]
    fn full_peer_json_preserves_nullable_and_integer_local_measurements() {
        let peer = PeerView {
            node_id: "01".repeat(32),
            level: aster::topology::LadderLevel::SameLan,
            level_reason: "verified private path".into(),
            rtt_us: None,
            jitter_us: None,
            loss_ppm: 123,
            throughput_bps: u64::MAX,
            cwnd_ceiling_bps: 456,
            direct: true,
            relay_url: None,
            remote_addr: Some("127.0.0.1:4567".parse().unwrap()),
            congestion_events: 7,
            samples: 8,
            confidence_ppm: 900_000,
            last_measured_unix_ms: 10,
            cluster_held_since_unix_ms: 11,
            is_connected: false,
            level_source: aster::topology::LevelSource::Measured,
            declared_region: None,
            declared_zone: None,
            quic_traffic: aster::topology::QuicTraffic {
                rpc: aster::topology::QuicBytes {
                    quic_bytes_sent: 12,
                    quic_bytes_received: u64::MAX,
                },
                ..Default::default()
            },
            blob_transfer: aster::topology::PeerBlobStats {
                fetched_wire_bytes: 21,
                served_payload_bytes: 22,
                plain_fallbacks: 23,
                ..Default::default()
            },
        };
        let value = peer_json(peer);
        assert_eq!(value.as_object().unwrap().len(), 19);
        let traffic = value["quic_traffic"].as_object().unwrap();
        assert_eq!(
            traffic.keys().map(String::as_str).collect::<Vec<_>>().len(),
            6
        );
        for family in ["blobs", "blobs-zstd", "docs", "gossip", "rpc", "other"] {
            assert!(traffic.contains_key(family), "{family}");
        }
        assert_eq!(traffic["rpc"]["quic_bytes_sent"], 12);
        assert_eq!(
            traffic["rpc"]["quic_bytes_received"].as_u64(),
            Some(u64::MAX)
        );
        assert_eq!(traffic["blobs-zstd"]["quic_bytes_sent"], 0);
        let blobs = value["blob_transfer"].as_object().unwrap();
        assert_eq!(blobs.len(), 9);
        assert_eq!(blobs["fetched_wire_bytes"], 21);
        assert_eq!(blobs["served_payload_bytes"], 22);
        assert_eq!(blobs["plain_fallbacks"], 23);
        assert_eq!(value["level"], 1);
        assert!(
            value["rtt_us"].is_null()
                && value["jitter_us"].is_null()
                && value["relay_url"].is_null()
        );
        assert_eq!(value["remote_addr"], "127.0.0.1:4567");
        assert_eq!(value["throughput_bps"].as_u64(), Some(u64::MAX));
        assert_eq!(value["confidence_ppm"], 900_000);
        assert_eq!(value["is_connected"], false);
    }
    #[test]
    fn options_preserve_zero_defaults_and_reject_unknown_negative_overflow() {
        let defaults = TopologySwarmOptions::default();
        let mut options = TopologySwarmOptions::default();
        serde_json::from_str::<Options>("{}")
            .unwrap()
            .apply(&mut options);
        assert_eq!(options.timing.min_hold, defaults.timing.min_hold);
        assert_eq!(
            options.timing.separation_coverage,
            defaults.timing.separation_coverage
        );
        serde_json::from_str::<Options>(r#"{"position_refresh_ms":0,"min_hold_ms":0,"liveness_ttl_ms":0,"edge_ttl_ms":0,"max_skew_ms":0,"separation_coverage":0,"witness_r":0}"#).unwrap().apply(&mut options);
        assert!(
            options.position_refresh.is_zero()
                && options.timing.min_hold.is_zero()
                && options.timing.liveness_ttl.is_zero()
                && options.timing.edge_ttl.is_zero()
                && options.timing.max_skew.is_zero()
        );
        assert_eq!(options.timing.separation_coverage, 0);
        assert_eq!(options.timing.witness_r, 0);
        for input in [
            r#"{"unexpected":1}"#,
            r#"{"position_refresh_ms":-1}"#,
            r#"{"min_hold_ms":4294967296}"#,
            r#"{"witness_r":1.5}"#,
        ] {
            assert!(serde_json::from_str::<Options>(input).is_err(), "{input}");
        }
    }
    #[test]
    fn snapshot_owners_keep_one_view_and_preserve_coverage_unknown() {
        let owner = runtime();
        let other = runtime();
        let bridge = load_runtime(owner).unwrap();
        let a = "11".repeat(32);
        let b = "12".repeat(32);
        let c = "13".repeat(32);
        let absent = "14".repeat(32);
        let clusters = vec![
            cluster(&[a.clone(), b.clone()]),
            cluster(std::slice::from_ref(&c)),
        ];
        let first = bridge
            .topology_snapshots
            .insert(aster::topology::TopologySnapshot::from_view(Arc::new(
                CoreTopoSharedView::from_parts(clusters.clone(), vec![(a.clone(), c.clone())], 2),
            )));
        let second =
            bridge
                .topology_snapshots
                .insert(aster::topology::TopologySnapshot::from_view(Arc::new(
                    CoreTopoSharedView::from_parts(
                        clusters,
                        vec![(a.clone(), c.clone()), (b.clone(), c.clone())],
                        2,
                    ),
                )));
        let mut verdict = 99;
        assert_eq!(
            unsafe {
                aster_topology_snapshot_separated(owner, first, bytes(&a), bytes(&b), &mut verdict)
            },
            0
        );
        assert_eq!(verdict, 1);
        assert_eq!(
            unsafe {
                aster_topology_snapshot_separated(owner, first, bytes(&a), bytes(&c), &mut verdict)
            },
            0
        );
        assert_eq!(verdict, 0);
        assert_eq!(
            unsafe {
                aster_topology_snapshot_separated(owner, second, bytes(&a), bytes(&c), &mut verdict)
            },
            0
        );
        assert_eq!(verdict, 2);
        assert_eq!(
            unsafe {
                aster_topology_snapshot_separated(
                    owner,
                    second,
                    bytes(&a),
                    bytes(&absent),
                    &mut verdict,
                )
            },
            0
        );
        assert_eq!(verdict, 0);
        let mut out = aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        };
        assert_eq!(
            unsafe { aster_topology_snapshot_clusters(owner, first, &mut out) },
            0
        );
        assert_eq!(unsafe { value(out) }.as_array().unwrap().len(), 2);
        assert_eq!(
            unsafe { aster_topology_snapshot_cluster_of(owner, first, bytes(&absent), &mut out) },
            0
        );
        assert!(unsafe { value(out) }.is_null());
        assert_eq!(
            unsafe { aster_topology_snapshot_clusters(other, first, &mut out) },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert!(out.ptr.is_null() && out.len == 0);
        assert_eq!(
            aster_topology_snapshot_free(other, first),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            unsafe { aster_topology_snapshot_cluster_of(owner, first, bytes("short"), &mut out) },
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        );
        assert_eq!(aster_topology_snapshot_free(owner, first), 0);
        assert_eq!(
            aster_topology_snapshot_free(owner, first),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(unsafe { aster_runtime_close(owner) }, 0);
        assert_eq!(bridge.topology_snapshots.count(), 0);
        drop(bridge);
        assert_eq!(
            aster_runtime_close_and_wait(other, Duration::from_secs(10)),
            0
        );
    }
    #[test]
    fn admitted_filters_are_runtime_local_for_legacy_and_new_calls_and_teardown() {
        let owner = runtime();
        let other = runtime();
        let bridge = load_runtime(owner).unwrap();
        let mut upcalls = 0;
        let mut filter = 0;
        let mut bad = 99;
        assert_eq!(
            unsafe { crate::upcall::aster_upcalls_new(owner, 16, &mut upcalls) },
            0
        );
        assert_eq!(
            unsafe {
                crate::upcall::aster_topology_admitted_new(other, upcalls, 0, 0, 0, &mut bad)
            },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(bad, 0);
        assert_eq!(
            unsafe {
                crate::upcall::aster_topology_admitted_new(owner, upcalls, 0, 0, 0, &mut filter)
            },
            0
        );
        assert!(admitted_for(owner, filter).is_ok());
        assert!(admitted_for(other, filter).is_err());
        let id = "11".repeat(32);
        assert_eq!(
            unsafe { crate::upcall::aster_topology_admitted_set(other, filter, bytes(&id), 1) },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            unsafe { crate::upcall::aster_topology_admitted_free(other, filter) },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        let mut operation = 99;
        assert_eq!(
            unsafe {
                aster_topology_enable_shared_with_options(
                    other,
                    0,
                    [42; 32].as_ptr(),
                    filter,
                    aster_bytes_list_t {
                        items: ptr::null(),
                        len: 0,
                    },
                    bytes("{}"),
                    0,
                    &mut operation,
                )
            },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(operation, 0);
        assert_eq!(unsafe { aster_runtime_close(owner) }, 0);
        assert_eq!(bridge.topology_admitted.count(), 0);
        drop(bridge);
        assert_eq!(
            aster_runtime_close_and_wait(other, Duration::from_secs(10)),
            0
        );
    }
}
