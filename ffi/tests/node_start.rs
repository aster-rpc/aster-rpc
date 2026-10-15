//! ABI 3.2: `aster_node_start` + `aster_config_t`, Gate 0 and admission,
//! driven through the C API. Everything is loopback with relays disabled.

use std::ptr;
use std::time::{Duration, Instant};

use aster_transport_ffi::node::*;
use aster_transport_ffi::*;

mod common;

const ALPN: &[u8] = b"aster-test/1";

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

/// Close `rt` and wait for its worker threads: the process must not exit
/// mid-shutdown (LSan reports the scheduler's cores as leaked).
fn close_runtime(rt: aster_runtime_t) {
    assert_eq!(aster_runtime_close_and_wait(rt, Duration::from_secs(10)), 0);
}

unsafe fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    assert_eq!(
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
        0
    );
    rt
}

/// A loopback config: relays off, bound to 127.0.0.1, serving `ALPN`.
struct Loopback {
    cfg: aster_config_t,
    _alpns: Box<[aster_bytes_t; 1]>,
}

const BIND: &[u8] = b"127.0.0.1:0";

fn loopback() -> Loopback {
    let alpns = Box::new([bytes(ALPN)]);
    let mut cfg: aster_config_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { aster_config_init(&mut cfg) }, 0);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(BIND);
    cfg.alpns = aster_bytes_list_t {
        items: alpns.as_ptr(),
        len: 1,
    };
    Loopback { cfg, _alpns: alpns }
}

unsafe fn start(rt: aster_runtime_t, cfg: &aster_config_t, gate: aster_gate_t) -> aster_node_t {
    let mut op = 0;
    assert_eq!(aster_node_start(rt, cfg, gate, 7, &mut op), 0);
    let ev =
        common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_NODE_CREATED);
    assert_eq!(ev.user_data, 7);
    ev.handle
}

unsafe fn node_id(rt: aster_runtime_t, node: aster_node_t) -> String {
    let mut buf = [0u8; 128];
    let mut len = 0usize;
    assert_eq!(
        aster_node_id(rt, node, buf.as_mut_ptr(), buf.len(), &mut len),
        0
    );
    String::from_utf8(buf[..len].to_vec()).unwrap()
}

unsafe fn secret(rt: aster_runtime_t, node: aster_node_t) -> Vec<u8> {
    let mut buf = [0u8; 32];
    let mut len = 0usize;
    assert_eq!(
        aster_node_export_secret_key(rt, node, buf.as_mut_ptr(), 32, &mut len),
        0
    );
    buf[..len].to_vec()
}

/// The node's address via `aster_node_addr_info` (now with direct addresses).
unsafe fn addr(rt: aster_runtime_t, node: aster_node_t) -> aster::NodeAddr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut buf = vec![0u8; 4096];
        let mut out: aster_node_addr_t = std::mem::zeroed();
        assert_eq!(
            aster_node_addr_info(rt, node, buf.as_mut_ptr(), buf.len(), &mut out),
            0
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
            assert!(
                direct.iter().all(|a| a.starts_with("127.0.0.1:")),
                "bind_addr not honoured: {direct:?}"
            );
            return aster::NodeAddr {
                node_id: aster::NodeId::from_hex(s(&out.endpoint_id)),
                relay_url: None,
                direct_addresses: direct,
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

unsafe fn close(rt: aster_runtime_t, node: aster_node_t) {
    let mut op = 0;
    assert_eq!(aster_node_close(rt, node, 0, &mut op), 0);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CLOSED);
}

/// A plain Rust client node (the `aster` crate) on loopback.
fn client(tokio: &tokio::runtime::Runtime) -> aster::Node {
    tokio
        .block_on(aster::Node::start(
            aster::AsterConfig::builder()
                .relay(aster::RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .build(),
        ))
        .unwrap()
}

/// Connect `client` to `server` on `ALPN` and report how the connection ends
/// within `wait`: `Some((code, reason))` if the server closed it, `None` if it
/// stayed open.
fn dial(
    tokio: &tokio::runtime::Runtime,
    client: &aster::Node,
    server: aster::NodeAddr,
    wait: Duration,
) -> Option<(u64, Vec<u8>)> {
    tokio.block_on(async {
        let conn = match client.connect_addr(server, ALPN).await {
            Ok(c) => c,
            Err(e) => panic!("connect failed before admission: {e}"),
        };
        let core = conn.core_connection().clone();
        match tokio::time::timeout(wait, core.closed()).await {
            Ok(info) => Some((
                info.code.unwrap_or(u64::MAX),
                info.reason.unwrap_or_default(),
            )),
            Err(_) => None,
        }
    })
}

/// Poll for the next admission (after-connect) event on `rt`.
unsafe fn next_admission(rt: aster_runtime_t) -> aster_event_t {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let mut events: [aster_event_t; 8] = std::mem::zeroed();
        let n = aster_poll_events(rt, events.as_mut_ptr(), events.len(), 200);
        for ev in events.iter().take(n) {
            if ev.kind == aster_event_kind_t::ASTER_EVENT_HOOK_AFTER_CONNECT as u32 {
                return *ev;
            }
            if ev.buffer != 0 {
                aster_buffer_release(rt, ev.buffer);
            }
        }
    }
    panic!("no admission event");
}

#[test]
fn node_start_loopback() {
    // Transport metrics moved to the OTel pipeline in 4.6
    // (`aster_metrics_prometheus`, tests/health.rs and tests/observability.rs).
    unsafe {
        let rt = runtime();
        let lb = loopback();
        let node = start(rt, &lb.cfg, 0);
        assert_eq!(node_id(rt, node).len(), 64);
        // Without hooks a node can't take a gate.
        let mut gate = 0;
        assert_eq!(aster_gate_new(rt, &mut gate), 0);
        assert_eq!(
            aster_node_set_gate(rt, node, gate),
            aster_status_t::ASTER_STATUS_UNSUPPORTED as i32
        );
        close(rt, node);
        close_runtime(rt);
    }
}

#[test]
fn node_start_rejects_bad_config_synchronously() {
    unsafe {
        let rt = runtime();
        let mut lb = loopback();
        let key = [1u8; 5];
        lb.cfg.secret_key = bytes(&key);
        let mut op = 0;
        assert_eq!(
            aster_node_start(rt, &lb.cfg, 0, 0, &mut op),
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        );
        let mut msg = [0u8; 256];
        let n = aster_last_error_message(msg.as_mut_ptr(), msg.len());
        assert!(String::from_utf8_lossy(&msg[..n]).contains("32 bytes"));
        lb.cfg.secret_key = bytes(&[]);
        lb.cfg.struct_size = 16;
        assert_eq!(
            aster_node_start(rt, &lb.cfg, 0, 0, &mut op),
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        );
        lb.cfg.struct_size = std::mem::size_of::<aster_config_t>() as u32;
        assert_eq!(
            aster_node_start(rt, &lb.cfg, 424_242, 0, &mut op),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        close_runtime(rt);
    }
}

#[test]
fn node_start_with_key_file_and_storage_keeps_identity() {
    unsafe {
        let dir = std::env::temp_dir().join(format!("aster-ffi-node-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key_file = dir.join("keys/node.key").to_string_lossy().into_owned();
        let data = dir.join("data").to_string_lossy().into_owned();
        let rt = runtime();
        let mut lb = loopback();
        lb.cfg.secret_key_file = bytes(key_file.as_bytes());
        lb.cfg.storage_path = bytes(data.as_bytes());
        lb.cfg.gc_interval_ms = 60_000;

        let node = start(rt, &lb.cfg, 0);
        let id = node_id(rt, node);
        let key = secret(rt, node);
        let on_disk = std::fs::read_to_string(&key_file).unwrap();
        assert_eq!(
            on_disk.trim(),
            hex::encode(&key),
            "key file holds the node key"
        );
        assert_eq!(aster::NodeId::from_hex(id.clone()), addr(rt, node).node_id);
        close(rt, node);

        // Restart from the same file: same identity.
        let node = start(rt, &lb.cfg, 0);
        assert_eq!(node_id(rt, node), id);
        close(rt, node);

        // An inline key that disagrees with the file is an error event.
        let other = [9u8; 32];
        lb.cfg.secret_key = bytes(&other);
        let mut op = 0;
        assert_eq!(aster_node_start(rt, &lb.cfg, 0, 0, &mut op), 0);
        let ev = common::wait_for_operation(rt, op);
        assert_eq!(ev.kind, aster_event_kind_t::ASTER_EVENT_ERROR as u32);
        aster_buffer_release(rt, ev.buffer);
        close_runtime(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn gate_admit_revoke_and_settings() {
    unsafe {
        let rt = runtime();
        let mut gate = 0;
        assert_eq!(aster_gate_new(rt, &mut gate), 0);
        let peer = "ab".repeat(32);
        let upper = peer.to_ascii_uppercase();
        let mut out = 9u32;
        assert_eq!(
            aster_gate_is_admitted(rt, gate, bytes(peer.as_bytes()), &mut out),
            0
        );
        assert_eq!(out, 0);
        assert_eq!(aster_gate_admit(rt, gate, bytes(upper.as_bytes())), 0);
        assert_eq!(
            aster_gate_is_admitted(rt, gate, bytes(peer.as_bytes()), &mut out),
            0
        );
        assert_eq!(out, 1, "peer ids are case-insensitive");

        // Decisions: admission ALPN always open, others need admission.
        let mut d = 9u32;
        let other = "cd".repeat(32);
        assert_eq!(
            aster_gate_should_allow(rt, gate, bytes(other.as_bytes()), bytes(ALPN), &mut d),
            0
        );
        assert_eq!(d, aster_hook_decision_t::ASTER_HOOK_DECISION_DENY as u32);
        assert_eq!(
            aster_gate_should_allow(
                rt,
                gate,
                bytes(other.as_bytes()),
                bytes(b"aster.producer_admission"),
                &mut d
            ),
            0
        );
        assert_eq!(d, aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32);
        // The retired consumer admission ALPN is not in the default set (5.1).
        assert_eq!(
            aster_gate_should_allow(
                rt,
                gate,
                bytes(other.as_bytes()),
                bytes(b"aster.consumer_admission"),
                &mut d
            ),
            0
        );
        assert_eq!(d, aster_hook_decision_t::ASTER_HOOK_DECISION_DENY as u32);
        assert_eq!(
            aster_gate_should_allow(rt, gate, bytes(peer.as_bytes()), bytes(ALPN), &mut d),
            0
        );
        assert_eq!(d, aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32);

        // Admission ALPN set is replaceable; admissions survive setters.
        let only = [bytes(ALPN)];
        assert_eq!(
            aster_gate_set_admission_alpns(
                rt,
                gate,
                aster_bytes_list_t {
                    items: only.as_ptr(),
                    len: 1
                }
            ),
            0
        );
        assert_eq!(
            aster_gate_should_allow(rt, gate, bytes(other.as_bytes()), bytes(ALPN), &mut d),
            0
        );
        assert_eq!(d, aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32);
        assert_eq!(aster_gate_set_reject(rt, gate, 451, bytes(b"blocked")), 0);
        assert_eq!(
            aster_gate_is_admitted(rt, gate, bytes(peer.as_bytes()), &mut out),
            0
        );
        assert_eq!(out, 1);

        // Open-dev lets anyone through; revoke drops the admission.
        assert_eq!(
            aster_gate_set_trust_mode(
                rt,
                gate,
                aster_trust_mode_t::ASTER_TRUST_MODE_OPEN_DEV as u32
            ),
            0
        );
        assert_eq!(
            aster_gate_should_allow(rt, gate, bytes(other.as_bytes()), bytes(b"x/1"), &mut d),
            0
        );
        assert_eq!(d, aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32);
        assert_eq!(aster_gate_revoke(rt, gate, bytes(peer.as_bytes())), 0);
        assert_eq!(
            aster_gate_is_admitted(rt, gate, bytes(peer.as_bytes()), &mut out),
            0
        );
        assert_eq!(out, 0);

        // Argument validation.
        let inv = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
        assert_eq!(aster_gate_admit(rt, gate, bytes(b"not-hex")), inv);
        assert_eq!(aster_gate_set_trust_mode(rt, gate, 7), inv);
        assert_eq!(
            aster_gate_admit(rt, 123_456, bytes(peer.as_bytes())),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(aster_gate_free(rt, gate), 0);
        assert_eq!(
            aster_gate_free(rt, gate),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        close_runtime(rt);
    }
}

#[test]
fn gate_on_node_rejects_with_code_then_admits() {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    unsafe {
        let rt = runtime();
        let mut gate = 0;
        assert_eq!(aster_gate_new(rt, &mut gate), 0);
        let lb = loopback();
        let server = start(rt, &lb.cfg, gate);
        let server_addr = addr(rt, server);
        let client = client(&tokio);

        // Not admitted: closed with the default 403.
        let closed = dial(
            &tokio,
            &client,
            server_addr.clone(),
            Duration::from_secs(10),
        );
        assert_eq!(closed, Some((403, b"peer not admitted".to_vec())));

        // Custom reject code / reason.
        assert_eq!(aster_gate_set_reject(rt, gate, 451, bytes(b"blocked")), 0);
        let closed = dial(
            &tokio,
            &client,
            server_addr.clone(),
            Duration::from_secs(10),
        );
        assert_eq!(closed, Some((451, b"blocked".to_vec())));

        // Admitted: the connection reaches the node's accept queue.
        let cid = client.id().as_str().to_string();
        assert_eq!(aster_gate_admit(rt, gate, bytes(cid.as_bytes())), 0);
        let mut op = 0;
        assert_eq!(aster_node_accept_aster(rt, server, 0, &mut op), 0);
        let closed = dial(
            &tokio,
            &client,
            server_addr.clone(),
            Duration::from_millis(500),
        );
        assert_eq!(closed, None, "admitted peer stays connected");
        let ev = common::wait_for_successful_operation(
            rt,
            op,
            aster_event_kind_t::ASTER_EVENT_ASTER_ACCEPTED,
        );
        assert_eq!(std::slice::from_raw_parts(ev.data_ptr, ev.data_len), ALPN);
        aster_buffer_release(rt, ev.buffer);

        // Revoked: rejected again. Detaching the gate (no hooks consumer)
        // falls back to admission events, answered here with a custom reject.
        assert_eq!(aster_gate_revoke(rt, gate, bytes(cid.as_bytes())), 0);
        let closed = dial(
            &tokio,
            &client,
            server_addr.clone(),
            Duration::from_secs(10),
        );
        assert_eq!(closed, Some((451, b"blocked".to_vec())));

        assert_eq!(aster_node_set_gate(rt, server, 0), 0);
        let srv = server_addr.clone();
        let handle = std::thread::spawn(move || {
            let tokio = tokio::runtime::Runtime::new().unwrap();
            let c = client_on(&tokio);
            dial(&tokio, &c, srv, Duration::from_secs(10))
        });
        let ev = next_admission(rt);
        assert_eq!(ev.handle, server);
        assert_eq!(ev.user_data, 7);
        let payload = std::slice::from_raw_parts(ev.data_ptr, ev.data_len).to_vec();
        aster_buffer_release(rt, ev.buffer);
        let id_len = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        assert_eq!(id_len, 64);
        assert_eq!(
            aster_admission_respond(
                rt,
                ev.related,
                aster_hook_decision_t::ASTER_HOOK_DECISION_DENY as u32,
                4242,
                bytes(b"nope"),
            ),
            0
        );
        assert_eq!(
            aster_admission_respond(rt, ev.related, 0, 0, bytes(b"")),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(handle.join().unwrap(), Some((4242, b"nope".to_vec())));

        tokio.block_on(client.shutdown());
        close(rt, server);
        aster_gate_free(rt, gate);
        // The ASTER_ACCEPTED connection is deliberately never freed: closing
        // the runtime must release it, or it keeps the endpoint, the hook
        // drainers and so the whole runtime alive (close_runtime times out).
        close_runtime(rt);
    }
}

fn client_on(tokio: &tokio::runtime::Runtime) -> aster::Node {
    client(tokio)
}

#[test]
fn hooked_node_without_gate_raises_events_and_accepts() {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    unsafe {
        let rt = runtime();
        let mut lb = loopback();
        lb.cfg.enable_hooks = 1;
        lb.cfg.hook_timeout_ms = 10_000;
        lb.cfg.hook_failure_mode = aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED as u32;
        let server = start(rt, &lb.cfg, 0);
        let srv = addr(rt, server);
        let handle = std::thread::spawn(move || {
            let tokio = tokio::runtime::Runtime::new().unwrap();
            let c = client(&tokio);
            dial(&tokio, &c, srv, Duration::from_millis(1500))
        });
        let ev = next_admission(rt);
        aster_buffer_release(rt, ev.buffer);
        assert_eq!(
            aster_admission_respond(
                rt,
                ev.related,
                aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32,
                0,
                bytes(b""),
            ),
            0
        );
        assert_eq!(
            handle.join().unwrap(),
            None,
            "accepted peer stays connected"
        );
        close(rt, server);
        close_runtime(rt);
    }
    drop(tokio);
}

/// ABI 4.0: `aster_accept` on a node takes the connection from the node's
/// aster-ALPN accept queue (it used to race the router on the endpoint) and
/// reports it as `CONNECTION_ACCEPTED` with `related` = the node.
#[test]
fn accept_on_node_reads_the_alpn_queue() {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    unsafe {
        let rt = runtime();
        let lb = loopback();
        let server = start(rt, &lb.cfg, 0);
        let server_addr = addr(rt, server);
        let client = client(&tokio);

        let mut op = 0;
        assert_eq!(aster_accept(rt, server, 9, &mut op), 0);
        let closed = dial(&tokio, &client, server_addr, Duration::from_millis(500));
        assert_eq!(closed, None, "accepted connection stays open");
        let ev = common::wait_for_successful_operation(
            rt,
            op,
            aster_event_kind_t::ASTER_EVENT_CONNECTION_ACCEPTED,
        );
        assert_eq!(ev.related, server);
        assert_eq!(ev.user_data, 9);
        assert_ne!(ev.handle, 0);
        assert_eq!(aster_connection_free(rt, ev.handle), 0);

        // Unknown handle.
        let mut op2 = 0;
        assert_eq!(
            aster_accept(rt, 0xdead_beef, 0, &mut op2),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        tokio.block_on(client.shutdown());
        close(rt, server);
        close_runtime(rt);
    }
}

/// ABI 5.2: `aster_config_t.blob_zstd_level` reaches core. A node with the
/// default level serves the zstd blobs ALPN; with `ASTER_BLOB_ZSTD_LEVEL_OFF`
/// it does not, so a dial on that ALPN fails the handshake.
#[test]
fn blob_zstd_level_off_drops_the_zstd_alpn() {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let dialer = client(&tokio);
    unsafe {
        let rt = runtime();
        for (level, served) in [
            (ASTER_BLOB_ZSTD_LEVEL_DEFAULT, true),
            (5, true),
            (ASTER_BLOB_ZSTD_LEVEL_OFF, false),
        ] {
            let mut lb = loopback();
            lb.cfg.blob_zstd_level = level;
            let node = start(rt, &lb.cfg, 0);
            let to = addr(rt, node);
            let dialed = tokio.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    dialer.connect_addr(to, aster::alpns::BLOBS_ZSTD),
                )
                .await
                .expect("dial timed out")
            });
            assert_eq!(dialed.is_ok(), served, "level {level}: {:?}", dialed.err());
            close(rt, node);
        }
        close_runtime(rt);
    }
    tokio.block_on(dialer.shutdown());
}

/// ABI 5.2: `aster_config_t.reuse_last_port` reaches core. A persistent node
/// records its bound ports in `bind-ports.json` by default and not when port
/// reuse is `ASTER_REUSE_LAST_PORT_DISABLED`.
#[test]
fn reuse_last_port_disabled_records_no_bind_ports() {
    let dir = std::env::temp_dir().join(format!("aster-ffi-reuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    unsafe {
        let rt = runtime();
        for (reuse, recorded) in [
            (aster_reuse_last_port_t::ASTER_REUSE_LAST_PORT_DEFAULT, true),
            (
                aster_reuse_last_port_t::ASTER_REUSE_LAST_PORT_DISABLED,
                false,
            ),
        ] {
            let data = dir.join(format!("{reuse:?}"));
            let data_str = data.to_string_lossy().into_owned();
            let mut lb = loopback();
            lb.cfg.storage_path = bytes(data_str.as_bytes());
            lb.cfg.reuse_last_port = reuse as u32;
            let node = start(rt, &lb.cfg, 0);
            close(rt, node);
            assert_eq!(data.join("bind-ports.json").exists(), recorded, "{reuse:?}");
        }
        close_runtime(rt);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
