//! Real listener ownership, bounded blocked accepts, cancellation and reuse.
use aster_transport_ffi::{raw_webtransport::*, *};
use aster_transport_salvo::generate_webtransport_cert;
use std::{mem, ptr};
mod common;
fn span(b: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: b.as_ptr(),
        len: b.len(),
    }
}
#[test]
fn listener_blocked_accept_cancel_and_stop() {
    let cert = generate_webtransport_cert("raw-ffi", None, &["127.0.0.1".into()]).unwrap();
    unsafe {
        let mut runtime = 0;
        let mut config = common::default_runtime_config();
        config.max_inflight_operations = 1;
        assert_eq!(aster_runtime_new(4, &config, &mut runtime), 0);
        let c = aster_raw_wt_config_t {
            struct_size: mem::size_of::<aster_raw_wt_config_t>() as u32,
            flags: 0,
            bind_addr: span(b"127.0.0.1:0"),
            certificate_pem: span(&cert.cert_pem),
            private_key_pem: span(&cert.key_pem),
            path: span(b"/raw"),
            max_sessions: 1,
            max_streams: 1,
            max_datagrams: 1,
            max_datagram_bytes: 4096,
            max_read_bytes: 4096,
            max_pending_auth: 1,
            max_auth_payload_bytes: 4096,
            reserved: 0,
        };
        let mut listener = 0;
        assert_eq!(
            aster_raw_wt_listener_start_public(runtime, &c, &mut listener),
            0
        );
        let mut addr = mem::zeroed();
        assert_eq!(aster_raw_wt_listener_addr(runtime, listener, &mut addr), 0);
        let info: serde_json::Value =
            serde_json::from_slice(std::slice::from_raw_parts(addr.ptr, addr.len)).unwrap();
        assert_eq!(info["version"], 1);
        assert!(info["quic_addr"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:"));
        aster_string_release(addr.ptr, addr.len);
        let mut op = 0;
        assert_eq!(
            aster_raw_wt_listener_accept(runtime, listener, 17, &mut op),
            0
        );
        let mut blocked = 99;
        assert_eq!(
            aster_raw_wt_listener_accept(runtime, listener, 18, &mut blocked),
            aster_status_t::ASTER_STATUS_OPERATION_LIMIT as i32
        );
        assert_eq!(blocked, 0);
        assert_eq!(aster_operation_cancel(runtime, op), 0);
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(e.status, aster_status_t::ASTER_STATUS_CANCELLED as u32);
        assert_eq!(e.user_data, 17);
        let mut next = 0;
        assert_eq!(
            aster_raw_wt_listener_accept(runtime, listener, 19, &mut next),
            0
        );
        assert_eq!(aster_raw_wt_listener_stop(runtime, listener, 0), 0);
        let e = common::wait_for_operation(runtime, next);
        assert_ne!(e.status, aster_status_t::ASTER_STATUS_OK as u32);
        assert_eq!(
            aster_raw_wt_listener_accept(runtime, listener, 0, &mut op),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_raw_wt_listener_start_public(runtime, ptr::null(), &mut listener),
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        );
        assert_eq!(aster_runtime_close(runtime), 0);
    }
}

unsafe fn payload(runtime: u64, e: aster_event_t) -> Vec<u8> {
    let bytes = if e.data_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(e.data_ptr, e.data_len) }.to_vec()
    };
    if e.buffer != 0 {
        assert_eq!(unsafe { aster_buffer_release(runtime, e.buffer) }, 0);
    }
    bytes
}
#[test]
fn real_peer_raw_bytes_fin_ack_priority_and_datagrams() {
    let cert = generate_webtransport_cert("raw-ffi", None, &["127.0.0.1".into()]).unwrap();
    unsafe {
        let mut runtime = 0;
        assert_eq!(
            aster_runtime_new(4, &common::default_runtime_config(), &mut runtime),
            0
        );
        let c = aster_raw_wt_config_t {
            struct_size: mem::size_of::<aster_raw_wt_config_t>() as u32,
            flags: 0,
            bind_addr: span(b"127.0.0.1:0"),
            certificate_pem: span(&cert.cert_pem),
            private_key_pem: span(&cert.key_pem),
            path: span(b"/raw"),
            max_sessions: 2,
            max_streams: 8,
            max_datagrams: 2,
            max_datagram_bytes: 4096,
            max_read_bytes: 4096,
            max_pending_auth: 1,
            max_auth_payload_bytes: 4096,
            reserved: 0,
        };
        let mut listener = 0;
        assert_eq!(
            aster_raw_wt_listener_start_public(runtime, &c, &mut listener),
            0
        );
        let mut addr = mem::zeroed();
        assert_eq!(aster_raw_wt_listener_addr(runtime, listener, &mut addr), 0);
        let info: serde_json::Value =
            serde_json::from_slice(std::slice::from_raw_parts(addr.ptr, addr.len)).unwrap();
        aster_string_release(addr.ptr, addr.len);
        let url = format!("https://{}/raw", info["quic_addr"].as_str().unwrap());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    tokio::time::timeout(std::time::Duration::from_secs(20), async move {
                        let endpoint = wtransport::Endpoint::client(
                            wtransport::ClientConfig::builder()
                                .with_bind_default()
                                .with_no_cert_validation()
                                .build(),
                        )
                        .unwrap();
                        let peer = endpoint.connect(url).await.unwrap();
                        let (mut send, mut recv) = peer.open_bi().await.unwrap().await.unwrap();
                        send.write_all(b"raw\0input").await.unwrap();
                        send.finish().await.unwrap();
                        let mut reply = [0; 5];
                        recv.read_exact(&mut reply).await.unwrap();
                        assert_eq!(&reply, b"reply");
                        assert!(recv.read(&mut reply).await.unwrap().is_none());
                        peer.send_datagram(b"ping").unwrap();
                        assert_eq!(&*peer.receive_datagram().await.unwrap(), b"pong");
                        done_tx.send(()).unwrap();
                        let _ = peer.closed().await;
                    })
                    .await
                    .unwrap();
                })
        });
        let mut op = 0;
        assert_eq!(
            aster_raw_wt_listener_accept(runtime, listener, 0, &mut op),
            0
        );
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(e.status, 0);
        let session = e.handle;
        assert_eq!(
            aster_raw_wt_session_accept_bi(runtime, session, 0, &mut op),
            0
        );
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(e.flags, 3);
        let stream = e.handle;
        assert_eq!(
            aster_raw_wt_stream_read(runtime, stream, 128, 0, &mut op),
            0
        );
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(e.status, 0);
        assert_eq!(payload(runtime, e), b"raw\0input");
        assert_eq!(
            aster_raw_wt_stream_read(runtime, stream, 128, 0, &mut op),
            0
        );
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(e.flags, 1);
        assert!(payload(runtime, e).is_empty());
        assert_eq!(
            aster_raw_wt_stream_set_priority(runtime, stream, -42, 0, &mut op),
            0
        );
        assert_eq!(common::wait_for_operation(runtime, op).status, 0);
        assert_eq!(aster_raw_wt_stream_priority(runtime, stream, 0, &mut op), 0);
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(e.flags, 1);
        assert_eq!(payload(runtime, e), (-42i32).to_le_bytes());
        assert_eq!(
            aster_raw_wt_stream_write(runtime, stream, span(b"reply"), 0, &mut op),
            0
        );
        assert_eq!(common::wait_for_operation(runtime, op).status, 0);
        assert_eq!(aster_raw_wt_stream_finish(runtime, stream, 0, &mut op), 0);
        assert_eq!(common::wait_for_operation(runtime, op).status, 0);
        assert_eq!(
            aster_raw_wt_stream_transport_acked(runtime, stream, 0, &mut op),
            0
        );
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(e.status, 0);
        assert_eq!(e.flags, 1);
        assert_eq!(
            aster_raw_wt_session_receive_datagram(runtime, session, 0, &mut op),
            0
        );
        let e = common::wait_for_operation(runtime, op);
        assert_eq!(payload(runtime, e), b"ping");
        assert_eq!(
            aster_raw_wt_session_send_datagram(runtime, session, span(b"pong")),
            0
        );
        done_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
        assert_eq!(
            aster_raw_wt_session_close(runtime, session, 73, span(b"done")),
            0
        );
        peer.join().unwrap();
        assert_eq!(aster_raw_wt_stream_free(runtime, stream), 0);
        assert_eq!(aster_raw_wt_session_free(runtime, session), 0);
        assert_eq!(aster_raw_wt_listener_stop(runtime, listener, 0), 0);
        assert_eq!(aster_runtime_close(runtime), 0);
    }
}
