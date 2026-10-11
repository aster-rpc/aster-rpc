//! ABI 3.3 (F3): data-plane parity through the C API -- multi-provider blob
//! fetch + fetch report, store-only and single-provider downloads, ranged
//! reads, tag prefix delete, one-shot GC; docs authority / retire / role-swap
//! shutdown / open / import / durable set / entry_exists / content status;
//! connection paths and send-stream priority. Loopback, relays disabled.

use std::ptr;
use std::slice;

use aster_transport_ffi::data::*;
use aster_transport_ffi::node::*;
use aster_transport_ffi::*;

mod common;

const ALPN: &[u8] = b"aster-f3/1";
const BIND: &[u8] = b"127.0.0.1:0";
const OK: i32 = 0;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;

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

fn list(items: &[aster_bytes_t]) -> aster_bytes_list_t {
    aster_bytes_list_t {
        items: if items.is_empty() {
            ptr::null()
        } else {
            items.as_ptr()
        },
        len: items.len(),
    }
}

/// A finished operation: the event plus a copy of its payload (buffer freed).
struct Ev {
    ev: aster_event_t,
    data: Vec<u8>,
}

unsafe fn finish(rt: aster_runtime_t, op: aster_operation_t) -> Ev {
    let ev = common::wait_for_operation(rt, op);
    let data = if ev.data_len == 0 || ev.data_ptr.is_null() {
        Vec::new()
    } else {
        slice::from_raw_parts(ev.data_ptr, ev.data_len).to_vec()
    };
    if ev.buffer != 0 {
        aster_buffer_release(rt, ev.buffer);
    }
    Ev { ev, data }
}

/// Run an async call (the closure gets `&mut op`) and expect `kind` + OK.
unsafe fn run(
    rt: aster_runtime_t,
    kind: aster_event_kind_t,
    call: impl FnOnce(*mut aster_operation_t) -> i32,
) -> Ev {
    let mut op = 0;
    assert_eq!(call(&mut op), OK, "call refused");
    let e = finish(rt, op);
    assert_eq!(
        (e.ev.kind, e.ev.status),
        (kind as u32, 0),
        "unexpected event (payload {:?})",
        String::from_utf8_lossy(&e.data)
    );
    e
}

unsafe fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    assert_eq!(
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
        0
    );
    rt
}

unsafe fn start_ffi_node(rt: aster_runtime_t) -> aster_node_t {
    let alpns = [bytes(ALPN)];
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), 0);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(BIND);
    cfg.alpns = list(&alpns);
    run(rt, aster_event_kind_t::ASTER_EVENT_NODE_CREATED, |op| {
        aster_node_start(rt, &cfg, 0, 0, op)
    })
    .ev
    .handle
}

/// A plain Rust provider node (the `aster` crate) on loopback serving `ALPN`.
fn provider(tokio: &tokio::runtime::Runtime) -> aster::Node {
    tokio
        .block_on(aster::Node::start_with_alpns(
            aster::AsterConfig::builder()
                .relay(aster::RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .build(),
            vec![ALPN.to_vec()],
        ))
        .unwrap()
}

/// Teach the FFI node where `peer` lives (`aster_add_node_addr` on a node).
unsafe fn add_addr(rt: aster_runtime_t, node: aster_node_t, peer: &aster::NodeAddr) {
    let id = peer.node_id.as_str().as_bytes().to_vec();
    let direct: Vec<Vec<u8>> = peer
        .direct_addresses
        .iter()
        .map(|a| a.as_bytes().to_vec())
        .collect();
    let direct_b: Vec<aster_bytes_t> = direct.iter().map(|d| bytes(d)).collect();
    let addr = aster_node_addr_t {
        endpoint_id: bytes(&id),
        relay_url: bytes(&[]),
        direct_addresses: list(&direct_b),
    };
    assert_eq!(aster_add_node_addr(rt, node, addr), OK);
}

/// Wait until the provider reports a loopback direct address.
fn provider_addr(tokio: &tokio::runtime::Runtime, p: &aster::Node) -> aster::NodeAddr {
    for _ in 0..100 {
        let a = p.addr();
        if !a.direct_addresses.is_empty() {
            return a;
        }
        tokio.block_on(tokio::time::sleep(std::time::Duration::from_millis(50)));
    }
    panic!("provider has no direct address");
}

struct Provider {
    id: [u8; 32],
    attempts: u32,
    failures: u32,
    completed: u32,
}

/// Decode a fetch report v1; returns (providers, bytes_transferred, rest).
fn decode_report(p: &[u8]) -> (Vec<Provider>, u64, &[u8]) {
    assert_eq!(p[0], ASTER_FETCH_REPORT_VERSION);
    let n = u32::from_le_bytes(p[4..8].try_into().unwrap()) as usize;
    let transferred = u64::from_le_bytes(p[8..16].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    let mut out = Vec::new();
    for i in 0..n {
        let o = ASTER_FETCH_REPORT_HEADER_SIZE + i * ASTER_FETCH_PROVIDER_RECORD_SIZE;
        out.push(Provider {
            id: p[o..o + 32].try_into().unwrap(),
            attempts: u32_at(o + 32),
            failures: u32_at(o + 36),
            completed: u32_at(o + 40),
        });
    }
    let end = ASTER_FETCH_REPORT_HEADER_SIZE + n * ASTER_FETCH_PROVIDER_RECORD_SIZE;
    (out, transferred, &p[end..])
}

#[test]
fn blobs_multi_provider_fetch_report_range_tags_gc() {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let prov = provider(&tokio);
    let blobs = prov.blobs();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let h1 = tokio.block_on(blobs.add_bytes(big.clone())).unwrap();
    let h2 = tokio
        .block_on(blobs.add_bytes(b"store-only".to_vec()))
        .unwrap();
    let h3 = tokio.block_on(blobs.add_bytes(b"single".to_vec())).unwrap();
    let paddr = provider_addr(&tokio, &prov);
    let pid = paddr.node_id.as_str().to_string();
    let mut pid_raw = [0u8; 32];
    hex::decode_to_slice(&pid, &mut pid_raw).unwrap();
    // A valid ed25519 key (a namespace id) that no node serves: tried first,
    // fails, and the report records it.
    let dead = aster::NamespaceSecret::from_bytes([9; 32]).id().to_hex();

    unsafe {
        let rt = runtime();
        let node = start_ffi_node(rt);
        add_addr(rt, node, &paddr);

        let h1b = h1.as_str().as_bytes().to_vec();
        let order = [bytes(dead.as_bytes()), bytes(pid.as_bytes())];
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_BLOB_FETCHED, |op| {
            aster_blobs_download_hash_from(
                rt,
                node,
                bytes(&h1b),
                list(&order),
                aster_blob_format_t::ASTER_BLOB_FORMAT_RAW as u32,
                aster_fetch_strategy_t::ASTER_FETCH_STRATEGY_WHOLE as u32,
                0,
                1,
                op,
            )
        });
        assert_eq!(e.ev.handle, node);
        assert_eq!(e.ev.flags, 2);
        let (provs, transferred, data) = decode_report(&e.data);
        assert_eq!(data, &big[..]);
        assert_eq!(provs.len(), 2, "both providers attempted, in order");
        assert_eq!(hex::encode(provs[0].id), dead);
        assert!(provs[0].failures >= 1 && provs[0].completed == 0);
        assert_eq!(provs[1].id, pid_raw);
        assert_eq!((provs[1].attempts, provs[1].completed), (1, 1));
        assert!(transferred >= big.len() as u64);

        // Store-only: report only, blob resident afterwards.
        let h2b = h2.as_str().as_bytes().to_vec();
        let one = [bytes(pid.as_bytes())];
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_BLOB_FETCHED, |op| {
            aster_blobs_download_hash_from(
                rt,
                node,
                bytes(&h2b),
                list(&one),
                0,
                0,
                ASTER_FETCH_TO_STORE,
                0,
                op,
            )
        });
        let (provs, _, rest) = decode_report(&e.data);
        assert!(rest.is_empty() && provs.len() == 1);
        let mut has = 0u32;
        assert_eq!(
            aster_blobs_has(rt, node, h2b.as_ptr(), h2b.len(), &mut has),
            OK
        );
        assert_eq!(has, 1);

        // Ranged read from the local store.
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_BLOB_READ, |op| {
            aster_blobs_read_range(rt, node, bytes(&h1b), 1000, 64, 0, op)
        });
        assert_eq!(e.data, &big[1000..1064]);

        // Single-provider download, bytes and store-only.
        let h3b = h3.as_str().as_bytes().to_vec();
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_BLOB_DOWNLOADED, |op| {
            aster_blobs_download_hash(rt, node, bytes(&h3b), bytes(pid.as_bytes()), 0, 0, 0, op)
        });
        assert_eq!(e.data, b"single");
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_BLOB_DOWNLOADED, |op| {
            aster_blobs_download_hash(
                rt,
                node,
                bytes(&h3b),
                bytes(pid.as_bytes()),
                0,
                ASTER_FETCH_TO_STORE,
                0,
                op,
            )
        });
        assert!(e.data.is_empty());

        // Argument validation happens before anything starts.
        let mut op = 0;
        let split = aster_fetch_strategy_t::ASTER_FETCH_STRATEGY_SPLIT_CHILDREN as u32;
        assert_eq!(
            aster_blobs_download_hash_from(
                rt,
                node,
                bytes(&h1b),
                list(&one),
                0,
                split,
                0,
                0,
                &mut op
            ),
            INVALID,
            "SPLIT_CHILDREN needs HASH_SEQ"
        );
        assert_eq!(
            aster_blobs_download_hash_from(rt, node, bytes(&h1b), list(&[]), 0, 0, 0, 0, &mut op),
            INVALID,
            "empty provider list"
        );
        assert_eq!(
            aster_blobs_download_hash_from(rt, node, bytes(b"zz"), list(&one), 0, 0, 0, 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_blobs_download_hash_from(rt, node, bytes(&h1b), list(&one), 9, 0, 0, 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_blobs_download_hash_from(rt, node, bytes(&h1b), list(&one), 0, 0, 4, 0, &mut op),
            INVALID
        );
        let not_a_key = "ab".repeat(32);
        let bad = [bytes(not_a_key.as_bytes())];
        assert_eq!(
            aster_blobs_download_hash_from(rt, node, bytes(&h1b), list(&bad), 0, 0, 0, 0, &mut op),
            INVALID,
            "provider id must be a valid public key"
        );
        assert_eq!(
            aster_blobs_read_range(rt, 999_999, bytes(&h1b), 0, 1, 0, &mut op),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        // Tag prefix delete: two tags under "f3/", one outside.
        for name in ["f3/a", "f3/b", "keep"] {
            run(rt, aster_event_kind_t::ASTER_EVENT_TAG_SET, |op| {
                aster_tags_set(
                    rt,
                    node,
                    name.as_ptr(),
                    name.len(),
                    h1b.as_ptr(),
                    h1b.len(),
                    0,
                    0,
                    op,
                )
            });
        }
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_TAG_DELETED, |op| {
            aster_tags_delete_prefix(rt, node, bytes(b"f3/"), 0, op)
        });
        assert_eq!(e.ev.flags, 2);
        assert_eq!(e.data, 2u64.to_le_bytes());

        // One-shot GC completes.
        run(rt, aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, |op| {
            aster_blobs_gc_run_once(rt, node, 0, op)
        });

        assert_eq!(aster_node_free(rt, node), OK);
        assert_eq!(aster_runtime_close(rt), OK);
    }
    tokio.block_on(prov.shutdown());
}

unsafe fn doc_id(rt: aster_runtime_t, doc: u64) -> String {
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    assert_eq!(aster_doc_id(rt, doc, buf.as_mut_ptr(), &mut len), OK);
    String::from_utf8(buf[..len].to_vec()).unwrap()
}

#[test]
fn docs_authority_retire_open_import_and_entries() {
    unsafe {
        let rt = runtime();
        let node = start_ffi_node(rt);
        let secret = aster::NamespaceSecret::from_bytes([7; 32]);
        let ns_hex = secret.id().to_hex();
        let ns = bytes(ns_hex.as_bytes());

        let e = run(rt, aster_event_kind_t::ASTER_EVENT_DOC_OPENED, |op| {
            aster_docs_import_write_namespace(rt, node, bytes(&[7; 32]), 0, op)
        });
        assert_eq!(e.ev.related, node);
        let doc = e.ev.handle;
        assert_eq!(doc_id(rt, doc), ns_hex);

        // Authority: WRITE, fingerprint matches the pure function.
        let mut fp = [0u8; 32];
        assert_eq!(
            aster_doc_authority_fingerprint(
                ns,
                aster_local_access_t::ASTER_LOCAL_ACCESS_WRITE as u32,
                fp.as_mut_ptr()
            ),
            OK
        );
        for e in [
            run(
                rt,
                aster_event_kind_t::ASTER_EVENT_DOC_AUTHORITY_REPORT,
                |op| aster_docs_authority_report(rt, node, ns, 0, op),
            ),
            run(
                rt,
                aster_event_kind_t::ASTER_EVENT_DOC_AUTHORITY_REPORT,
                |op| aster_doc_authority_report(rt, node, doc, 0, op),
            ),
        ] {
            assert_eq!(
                e.ev.flags,
                aster_local_access_t::ASTER_LOCAL_ACCESS_WRITE as u32
            );
            assert_eq!(hex::encode(&e.data[..32]), ns_hex);
            assert_eq!(e.data[32..], fp);
        }

        // Entries: durable set, entry_exists, content status, tombstone.
        let author = run(rt, aster_event_kind_t::ASTER_EVENT_AUTHOR_CREATED, |op| {
            aster_docs_create_author(rt, node, 0, op)
        })
        .data;
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_DOC_SET, |op| {
            aster_doc_set_bytes_durable(rt, doc, bytes(&author), bytes(b"k1"), bytes(b"v1"), 0, op)
        });
        let content_hash = e.data;
        assert_eq!(content_hash.len(), 64);
        let exists = |key: &[u8]| {
            run(rt, aster_event_kind_t::ASTER_EVENT_DOC_ENTRY_EXISTS, |op| {
                aster_doc_entry_exists(rt, doc, bytes(&author), bytes(key), 0, op)
            })
            .ev
            .flags
        };
        assert_eq!(exists(b"k1"), 1);
        assert_eq!(exists(b"nope"), 0);
        let e = run(
            rt,
            aster_event_kind_t::ASTER_EVENT_DOC_CONTENT_STATUS,
            |op| aster_doc_entry_content_status(rt, doc, bytes(&content_hash), 0, op),
        );
        assert_eq!(
            e.ev.flags,
            aster_blob_status_t::ASTER_BLOB_STATUS_COMPLETE as u32
        );
        assert_eq!(e.data, 2u64.to_le_bytes());
        run(rt, aster_event_kind_t::ASTER_EVENT_DOC_DEL, |op| {
            aster_doc_del(rt, doc, bytes(&author), bytes(b"k1"), 0, op)
        });
        assert_eq!(exists(b"k1"), 1, "a tombstone still counts");

        // Retire while the doc is open: BUSY (fail-closed), nothing changes.
        let e = run(
            rt,
            aster_event_kind_t::ASTER_EVENT_DOC_RETIRE_OUTCOME,
            |op| aster_docs_retire_local_write_authority(rt, node, ns, 0, op),
        );
        assert_eq!(e.ev.flags, aster_retire_outcome_t::ASTER_RETIRE_BUSY as u32);
        assert_eq!(e.data.len(), 16);
        assert_eq!(&e.data[..4], &0u32.to_le_bytes());
        assert!(u64::from_le_bytes(e.data[8..16].try_into().unwrap()) >= 1);

        // Release the references, then retire: READ is what is stored now.
        run(rt, aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, |op| {
            aster_doc_shutdown_for_role_swap(rt, doc, 5_000, 0, op)
        });
        assert_eq!(
            aster_doc_free(rt, doc),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
            "the role-swap shutdown freed the handle"
        );
        let e = run(
            rt,
            aster_event_kind_t::ASTER_EVENT_DOC_RETIRE_OUTCOME,
            |op| aster_docs_retire_local_write_authority(rt, node, ns, 0, op),
        );
        assert_eq!(
            e.ev.flags,
            aster_retire_outcome_t::ASTER_RETIRE_RETIRED as u32
        );
        assert_eq!(
            &e.data[..4],
            &(aster_local_access_t::ASTER_LOCAL_ACCESS_READ as u32).to_le_bytes()
        );
        let e = run(
            rt,
            aster_event_kind_t::ASTER_EVENT_DOC_AUTHORITY_REPORT,
            |op| aster_docs_authority_report(rt, node, ns, 0, op),
        );
        assert_eq!(
            e.ev.flags,
            aster_local_access_t::ASTER_LOCAL_ACCESS_READ as u32
        );

        // Open (present), import read (idempotent), open (absent).
        let opened = run(rt, aster_event_kind_t::ASTER_EVENT_DOC_OPENED, |op| {
            aster_docs_open(rt, node, ns, 0, op)
        })
        .ev
        .handle;
        assert_eq!(doc_id(rt, opened), ns_hex);
        let read = run(rt, aster_event_kind_t::ASTER_EVENT_DOC_OPENED, |op| {
            aster_docs_import_read_namespace(rt, node, ns, 0, op)
        })
        .ev
        .handle;
        assert_eq!(doc_id(rt, read), ns_hex);
        let other = "cd".repeat(32);
        let mut op = 0;
        assert_eq!(
            aster_docs_open(rt, node, bytes(other.as_bytes()), 0, &mut op),
            OK
        );
        let e = finish(rt, op);
        assert_eq!(e.ev.kind, aster_event_kind_t::ASTER_EVENT_DOC_OPENED as u32);
        assert_eq!(e.ev.status, aster_status_t::ASTER_STATUS_NOT_FOUND as u32);
        assert_eq!(e.ev.handle, 0);
        // Reporting on a namespace the store does not hold is an error.
        let mut op = 0;
        assert_eq!(
            aster_docs_authority_report(rt, node, bytes(other.as_bytes()), 0, &mut op),
            OK
        );
        assert_eq!(
            finish(rt, op).ev.kind,
            aster_event_kind_t::ASTER_EVENT_ERROR as u32
        );

        // Validation.
        assert_eq!(
            aster_docs_import_write_namespace(rt, node, bytes(&[1; 31]), 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_docs_open(rt, node, bytes(b"xyz"), 0, &mut op),
            INVALID
        );
        let mut fp = [0u8; 32];
        assert_eq!(
            aster_doc_authority_fingerprint(ns, 0, fp.as_mut_ptr()),
            INVALID
        );

        aster_doc_free(rt, opened);
        aster_doc_free(rt, read);
        assert_eq!(aster_node_free(rt, node), OK);
        assert_eq!(aster_runtime_close(rt), OK);
    }
}

#[test]
fn connection_paths_and_stream_priority() {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let prov = provider(&tokio);
    let paddr = provider_addr(&tokio, &prov);
    // Hold accepted connections open for the duration of the test.
    let acceptor = prov.clone();
    let accept_task = tokio.spawn(async move {
        let mut held = Vec::new();
        while let Ok((_, conn)) = acceptor.accept().await {
            held.push(conn);
        }
    });

    unsafe {
        let rt = runtime();
        let node = start_ffi_node(rt);
        let id = paddr.node_id.as_str().as_bytes().to_vec();
        let direct: Vec<Vec<u8>> = paddr
            .direct_addresses
            .iter()
            .map(|a| a.as_bytes().to_vec())
            .collect();
        let direct_b: Vec<aster_bytes_t> = direct.iter().map(|d| bytes(d)).collect();
        let addr = aster_node_addr_t {
            endpoint_id: bytes(&id),
            relay_url: bytes(&[]),
            direct_addresses: list(&direct_b),
        };
        let cfg = aster_connect_config_t {
            struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
            flags: 0,
            node_id: bytes(&id),
            alpn: bytes(ALPN),
            addr: &addr,
        };
        let conn = run(rt, aster_event_kind_t::ASTER_EVENT_CONNECTED, |op| {
            aster_connect(rt, node, &cfg, 0, op)
        })
        .ev
        .handle;

        let mut paths: aster_path_list_t = std::mem::zeroed();
        assert_eq!(aster_connection_paths(rt, conn, &mut paths), OK);
        assert_eq!(
            paths.struct_size as usize,
            std::mem::size_of::<aster_path_list_t>()
        );
        assert!(paths.len >= 1);
        let items = slice::from_raw_parts(paths.items, paths.len);
        let direct = items
            .iter()
            .find(|p| p.path_kind == aster_path_kind_t::ASTER_PATH_KIND_DIRECT as u32)
            .expect("a direct loopback path");
        assert_eq!(
            slice::from_raw_parts(direct.peer_host.ptr, direct.peer_host.len),
            b"127.0.0.1"
        );
        assert_ne!(direct.peer_port, 0);
        assert_eq!(aster_path_list_release(&mut paths), OK);
        assert!(paths.storage.is_null() && paths.len == 0);

        let mut sel: aster_path_list_t = std::mem::zeroed();
        assert_eq!(aster_connection_selected_path(rt, conn, &mut sel), OK);
        assert_eq!(sel.len, 1);
        let s = &*sel.items;
        assert_eq!(s.is_selected, 1);
        assert_eq!(
            s.struct_size as usize,
            std::mem::size_of::<aster_path_info_t>()
        );
        assert_eq!(aster_path_list_release(&mut sel), OK);
        assert_eq!(
            aster_connection_paths(rt, 999_999, &mut sel),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(sel.len, 0);

        // Send-stream priority round-trips.
        let send = run(rt, aster_event_kind_t::ASTER_EVENT_STREAM_OPENED, |op| {
            aster_open_bi(rt, conn, 0, op)
        })
        .ev
        .handle;
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_STREAM_PRIORITY, |op| {
            aster_send_stream_priority(rt, send, 0, op)
        });
        assert_eq!(e.data, 0i32.to_le_bytes());
        run(rt, aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, |op| {
            aster_send_stream_set_priority(rt, send, -7, 0, op)
        });
        let e = run(rt, aster_event_kind_t::ASTER_EVENT_STREAM_PRIORITY, |op| {
            aster_send_stream_priority(rt, send, 0, op)
        });
        assert_eq!(e.data, (-7i32).to_le_bytes());
        assert_eq!(e.ev.flags, -7i32 as u32);

        aster_send_stream_free(rt, send);
        aster_connection_free(rt, conn);
        assert_eq!(aster_node_free(rt, node), OK);
        assert_eq!(aster_runtime_close(rt), OK);
    }
    accept_task.abort();
    tokio.block_on(prov.shutdown());
}

#[test]
fn doc_batch_contents_preserves_order_empty_missing_duplicates_and_owned_input() {
    unsafe {
        let rt = runtime();
        let node = start_ffi_node(rt);
        let doc = run(rt, aster_event_kind_t::ASTER_EVENT_DOC_OPENED, |op| {
            aster_docs_import_write_namespace(rt, node, bytes(&[17; 32]), 0, op)
        })
        .ev
        .handle;
        let values = [vec![], b"batch value".to_vec(), vec![0xA5; 128 * 1024]];
        let hashes: Vec<String> = values
            .iter()
            .map(|v| aster::native::iroh_blobs::Hash::new(v).to_hex().to_string())
            .collect();
        for (i, value) in values.iter().enumerate() {
            run(rt, aster_event_kind_t::ASTER_EVENT_BLOB_ADDED, |op| {
                aster_transport_ffi::custody::aster_blobs_add_bytes_verified(
                    rt,
                    node,
                    bytes(value),
                    bytes(hashes[i].as_bytes()),
                    0,
                    bytes(format!("batch-{i}").as_bytes()),
                    0,
                    op,
                )
            });
        }
        let missing = aster::native::iroh_blobs::Hash::new(b"not local")
            .to_hex()
            .to_string();
        let mut owned = vec![
            hashes[1].clone(),
            missing,
            hashes[0].clone(),
            hashes[2].clone(),
            hashes[1].clone(),
        ];
        let spans: Vec<_> = owned.iter().map(|s| bytes(s.as_bytes())).collect();
        let mut op = 0;
        assert_eq!(
            aster_doc_read_entry_contents_if_complete(rt, doc, list(&spans), 42, &mut op),
            OK
        );
        // The implementation copied all hashes before returning admission.
        for hash in &mut owned {
            hash.clear();
        }
        let ev = finish(rt, op);
        assert_eq!(
            ev.ev.kind,
            aster_event_kind_t::ASTER_EVENT_DOC_CONTENTS_BATCH as u32
        );
        assert_eq!(ev.ev.status, 0);
        assert_eq!(ev.ev.user_data, 42);
        assert_eq!(ev.ev.handle, doc);
        assert_eq!(ev.ev.flags, 0);
        let mut expected = 5u32.to_le_bytes().to_vec();
        for value in [
            Some(&values[1]),
            None,
            Some(&values[0]),
            Some(&values[2]),
            Some(&values[1]),
        ] {
            match value {
                None => expected.push(0),
                Some(v) => {
                    expected.push(1);
                    expected.extend_from_slice(&(v.len() as u64).to_le_bytes());
                    expected.extend_from_slice(v);
                }
            }
        }
        assert_eq!(ev.data, expected);
        assert_eq!(
            run(
                rt,
                aster_event_kind_t::ASTER_EVENT_DOC_CONTENTS_BATCH,
                |op| { aster_doc_read_entry_contents_if_complete(rt, doc, list(&[]), 0, op) }
            )
            .data,
            [0, 0, 0, 0]
        );
        // Reads create no retention: removing the only tag then sweeping makes
        // the same hash absent, even though this document remains open.
        run(rt, aster_event_kind_t::ASTER_EVENT_TAG_DELETED, |op| {
            aster_tags_delete(rt, node, b"batch-1".as_ptr(), 7, 0, op)
        });
        run(rt, aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, |op| {
            aster_blobs_gc_run_once(rt, node, 0, op)
        });
        assert_eq!(
            run(
                rt,
                aster_event_kind_t::ASTER_EVENT_DOC_CONTENTS_BATCH,
                |op| {
                    aster_doc_read_entry_contents_if_complete(
                        rt,
                        doc,
                        list(&[bytes(hashes[1].as_bytes())]),
                        0,
                        op,
                    )
                }
            )
            .data,
            [1, 0, 0, 0, 0]
        );
        assert_eq!(
            aster_runtime_close_and_wait(rt, std::time::Duration::from_secs(10)),
            OK
        );
    }
}

#[test]
fn doc_batch_contents_rejects_invalid_input_before_admission() {
    unsafe {
        let rt = runtime();
        let node = start_ffi_node(rt);
        let doc = run(rt, aster_event_kind_t::ASTER_EVENT_DOC_OPENED, |op| {
            aster_docs_import_write_namespace(rt, node, bytes(&[18; 32]), 0, op)
        })
        .ev
        .handle;
        let mut op = 99;
        let bad = [bytes(b"bad")];
        for hashes in [
            list(&bad),
            aster_bytes_list_t {
                items: ptr::null(),
                len: 1,
            },
            aster_bytes_list_t {
                items: ptr::null(),
                len: usize::MAX,
            },
        ] {
            assert_eq!(
                aster_doc_read_entry_contents_if_complete(rt, doc, hashes, 0, &mut op),
                INVALID
            );
            assert_eq!(op, 0);
        }
        assert_eq!(
            aster_doc_read_entry_contents_if_complete(rt, doc, list(&[]), 0, ptr::null_mut()),
            INVALID
        );
        assert_eq!(
            aster_doc_read_entry_contents_if_complete(rt, u64::MAX, list(&[]), 0, &mut op),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(op, 0);
        assert_eq!(
            aster_runtime_close_and_wait(rt, std::time::Duration::from_secs(10)),
            OK
        );
    }
}
