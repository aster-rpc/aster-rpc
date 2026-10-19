//! Authenticated listener ownership and foreign dispatch through the C ABI.

use aster_transport_core::test_support::wait_budget;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use aster::binding_support::upcall::codec::{self, RpcAuthResponse};
use aster::rpc::{
    require_role, DynamicService, MethodDesc, MethodPattern, StatusCode, StreamHeader, TypeRef,
};
use aster_transport_core::framing::{
    decode_frame, encode_frame, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER,
};
use aster_transport_ffi::http_server::*;
use aster_transport_ffi::server::*;
use aster_transport_ffi::upcall::*;
use aster_transport_ffi::*;
use aster_transport_salvo::generate_webtransport_cert;
use tokio::io::AsyncReadExt;
use wtransport::{ClientConfig, Endpoint, VarInt};

mod common;

const NOT_FOUND: i32 = aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
const CLOSED: i32 = aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;

fn span(bytes: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: bytes.as_ptr(),
        len: bytes.len(),
    }
}

struct Config {
    bind: String,
    cert: Vec<u8>,
    key: Vec<u8>,
    wt: bool,
}

impl Config {
    fn new(wt: bool) -> Self {
        let cert = generate_webtransport_cert("ffi-auth", None, &["127.0.0.1".into()]).unwrap();
        Self {
            bind: "127.0.0.1:0".into(),
            cert: cert.cert_pem,
            key: cert.key_pem,
            wt,
        }
    }
    fn raw(&self) -> aster_http_server_config_t {
        aster_http_server_config_t {
            struct_size: std::mem::size_of::<aster_http_server_config_t>() as u32,
            flags: if self.wt {
                ASTER_HTTP_SERVER_WEBTRANSPORT
            } else {
                0
            },
            bind_addr: span(self.bind.as_bytes()),
            cert_pem: span(&self.cert),
            key_pem: span(&self.key),
            max_pending_auth: 2,
            max_auth_payload_bytes: 65536,
        }
    }
}

struct Fixture {
    rt: u64,
    server: u64,
    upcalls: u64,
    stop: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    auth_started: mpsc::Receiver<u64>,
    auth_cancelled: mpsc::Receiver<u64>,
    threads: Vec<JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        unsafe {
            let (mut rt, mut server, mut upcalls, mut builder) = (0, 0, 0, 0);
            assert_eq!(
                aster_runtime_new(
                    aster_transport_ffi::ASTER_ABI_VERSION_MAJOR,
                    &common::default_runtime_config(),
                    &mut rt
                ),
                0
            );
            let node = common::start_node(rt, &[b"aster/1"]);
            assert_eq!(aster_server_builder_new(rt, node, &mut builder), 0);
            let mut service = DynamicService::new("AuthEcho", 1);
            let mut method = MethodDesc::new(
                "echo",
                MethodPattern::Unary,
                TypeRef::new("a1".repeat(32)),
                TypeRef::new("b2".repeat(32)),
            );
            method.requires = Some(require_role("operator"));
            service.methods.push(method);
            let json = service.to_json().unwrap();
            let mut index = 0;
            assert_eq!(
                aster_server_add_service_json(
                    builder,
                    json.as_ptr(),
                    json.len() as u32,
                    &mut index
                ),
                0
            );
            assert_eq!(aster_server_start(rt, builder, 32, &mut server), 0);
            assert_eq!(aster_upcalls_new(rt, 8, &mut upcalls), 0);
            let stop = Arc::new(AtomicBool::new(false));
            let pending = Arc::new(AtomicBool::new(false));
            let calls = Arc::new(AtomicUsize::new(0));
            let (started_tx, auth_started) = mpsc::channel();
            let (cancel_tx, auth_cancelled) = mpsc::channel();
            let auth_thread = std::thread::spawn({
                let stop = stop.clone();
                let pending = pending.clone();
                move || {
                    while !stop.load(Ordering::Acquire) {
                        let mut events: [aster_upcall_event_t; 4] = std::mem::zeroed();
                        let mut count = 0;
                        let status =
                            aster_upcall_poll(rt, upcalls, events.as_mut_ptr(), 4, 25, &mut count);
                        if status != 0 {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        for event in &events[..count as usize] {
                            if event.kind == 0 {
                                let _ = cancel_tx.send(event.invocation_id);
                                continue;
                            }
                            assert_eq!(event.kind, 80);
                            let request =
                                codec::decode_rpc_auth_request(std::slice::from_raw_parts(
                                    event.payload_ptr,
                                    event.payload_len as usize,
                                ))
                                .unwrap();
                            assert_eq!(request.service, "AuthEcho");
                            assert_eq!(request.method, "echo");
                            if event.buffer != 0 {
                                assert_eq!(
                                    aster_upcall_buffer_release(rt, upcalls, event.buffer),
                                    0
                                );
                            }
                            if pending.load(Ordering::Acquire) {
                                let _ = started_tx.send(event.invocation_id);
                                continue;
                            }
                            let accepted =
                                request.metadata.get("authorization").map(String::as_str)
                                    == Some("Bearer secret");
                            let answer = if accepted {
                                RpcAuthResponse::Authenticated {
                                    principal: "user/alice".into(),
                                    attributes: [("aster.role".into(), "operator".into())].into(),
                                }
                            } else {
                                RpcAuthResponse::Rejected
                            };
                            let answer = codec::encode_rpc_auth_response(&answer).unwrap();
                            let status = aster_upcall_respond(
                                rt,
                                upcalls,
                                event.invocation_id,
                                0,
                                answer.as_ptr(),
                                answer.len() as u32,
                            );
                            assert!(status == 0 || status == NOT_FOUND);
                        }
                    }
                }
            });
            let dispatch_thread = std::thread::spawn({
                let stop = stop.clone();
                let calls = calls.clone();
                move || {
                    while !stop.load(Ordering::Acquire) {
                        let mut events: [aster_dispatch_event_t; 4] = std::mem::zeroed();
                        let count = aster_server_poll(rt, server, events.as_mut_ptr(), 4, 25);
                        if count == 0 {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        for event in &events[..count as usize] {
                            if event.kind == 0 {
                                assert_eq!(event.flags & ASTER_DISPATCH_FLAG_IROH_AUTHENTICATED, 0);
                                assert_ne!(event.flags & ASTER_DISPATCH_FLAG_HAS_ATTRIBUTES, 0);
                                assert_eq!(
                                    std::slice::from_raw_parts(
                                        event.peer_ptr,
                                        event.peer_len as usize
                                    ),
                                    b"user/alice"
                                );
                                // ABI 7.5: the authenticator's principal differs
                                // from the transport peer, and both are reported.
                                assert_ne!(event.flags & ASTER_DISPATCH_FLAG_HAS_PRINCIPAL, 0);
                                let mut ident: aster_dispatch_identity_t = std::mem::zeroed();
                                ident.struct_size =
                                    std::mem::size_of::<aster_dispatch_identity_t>() as u32;
                                assert_eq!(
                                    aster_dispatch_identity(rt, server, event.call_id, &mut ident),
                                    0
                                );
                                assert_eq!(ident.flags, ASTER_DISPATCH_IDENTITY_PRINCIPAL);
                                let principal = std::slice::from_raw_parts(
                                    ident.principal_ptr,
                                    ident.principal_len as usize,
                                );
                                assert_eq!(principal, b"user/alice");
                                assert!(ident.peer_len > 0);
                                let peer = std::slice::from_raw_parts(
                                    ident.peer_ptr,
                                    ident.peer_len as usize,
                                );
                                assert_ne!(peer, principal);
                                assert!(ident.actor_ptr.is_null() && ident.actor_len == 0);
                                assert_eq!(
                                    aster_dispatch_buffer_release(rt, server, ident.buffer),
                                    0
                                );
                                // The authenticator's attributes are what Gate 3 saw.
                                let (mut p, mut l, mut b) = (ptr::null(), 0u32, 0u64);
                                assert_eq!(
                                    aster_dispatch_attributes(
                                        rt,
                                        server,
                                        event.call_id,
                                        &mut p,
                                        &mut l,
                                        &mut b
                                    ),
                                    0
                                );
                                let attrs = std::slice::from_raw_parts(p, l as usize);
                                assert!(attrs.windows(8).any(|w| w == b"operator"));
                                assert_eq!(aster_dispatch_buffer_release(rt, server, b), 0);
                                assert_eq!(
                                    aster_dispatch_is_cancelled(rt, server, event.call_id),
                                    0
                                );
                                let mut needed = 0;
                                assert_eq!(
                                    aster_dispatch_respond(
                                        rt,
                                        server,
                                        event.call_id,
                                        event.request_ptr,
                                        event.request_len,
                                        ptr::null(),
                                        0,
                                        &mut needed
                                    ),
                                    0
                                );
                                calls.fetch_add(1, Ordering::SeqCst);
                            }
                            if event.buffer != 0 {
                                assert_eq!(
                                    aster_dispatch_buffer_release(rt, server, event.buffer),
                                    0
                                );
                            }
                        }
                    }
                }
            });
            Self {
                rt,
                server,
                upcalls,
                stop,
                pending,
                calls,
                auth_started,
                auth_cancelled,
                threads: vec![auth_thread, dispatch_thread],
            }
        }
    }

    fn start(&self, cfg: &Config) -> u64 {
        let mut listener = 0;
        assert_eq!(
            unsafe {
                aster_server_http_start(
                    self.rt,
                    self.server,
                    self.upcalls,
                    &cfg.raw(),
                    &mut listener,
                )
            },
            0
        );
        listener
    }
    fn addr(&self, listener: u64, quic: bool) -> String {
        unsafe {
            let (mut buffer, mut p, mut len) = (0, ptr::null(), 0);
            let status = if quic {
                aster_http_listener_quic_addr(self.rt, listener, &mut buffer, &mut p, &mut len)
            } else {
                aster_http_listener_tcp_addr(self.rt, listener, &mut buffer, &mut p, &mut len)
            };
            assert_eq!(status, 0);
            let out =
                String::from_utf8(std::slice::from_raw_parts(p, len as usize).to_vec()).unwrap();
            assert_eq!(aster_buffer_release(self.rt, buffer), 0);
            out
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            thread.join().unwrap();
        }
        unsafe {
            if self.rt != 0 {
                let _ = aster_server_destroy(self.rt, self.server);
                let _ = aster_upcalls_free(self.rt, self.upcalls);
                assert_eq!(aster_runtime_close_and_wait(self.rt, wait_budget()), 0);
            }
        }
    }
}

fn async_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn status(mut bytes: &[u8]) -> (Vec<u8>, i32) {
    let (mut data, mut code) = (Vec::new(), None);
    while !bytes.is_empty() {
        let (payload, flags, n) = decode_frame(bytes).unwrap();
        if flags & FLAG_TRAILER != 0 {
            code = Some(aster::rpc::decode_rpc_status(&payload).unwrap().code);
        } else {
            data.extend(payload);
        }
        bytes = &bytes[n..];
    }
    (data, code.expect("missing trailer"))
}

fn released(addr: &str, quic: bool) {
    let _tcp = std::net::TcpListener::bind(addr).expect("TCP still bound");
    if quic {
        let _udp = std::net::UdpSocket::bind(addr).expect("UDP still bound");
    }
}

#[test]
fn foreign_auth_and_dispatch_work_over_ready_https_and_webtransport() {
    let fixture = Fixture::new();
    let listener = fixture.start(&Config::new(true));
    let addr = fixture.addr(listener, false);
    assert_eq!(fixture.addr(listener, true), addr);
    async_runtime().block_on(async {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let url = format!("https://{addr}/aster/AuthEcho/echo");
        let body = encode_frame(&[1, 2, 255], FLAG_END_STREAM).unwrap();
        let denied = client
            .post(&url)
            .body(body.clone())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(
            status(&denied),
            (vec![], StatusCode::Unauthenticated.as_i32())
        );
        let accepted = client
            .post(&url)
            .header("authorization", "Bearer secret")
            .body(body)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(status(&accepted), (vec![1, 2, 255], 0));
        let endpoint = Endpoint::client(
            ClientConfig::builder()
                .with_bind_default()
                .with_no_cert_validation()
                .build(),
        )
        .unwrap();
        let connection = endpoint
            .connect(format!("https://{addr}/aster/wt"))
            .await
            .unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap().await.unwrap();
        let header = aster::rpc::encode_stream_header(&StreamHeader {
            service: "AuthEcho".into(),
            method: "echo".into(),
            version: 1,
            metadata_keys: vec!["authorization".into()],
            metadata_values: vec!["Bearer secret".into()],
            ..Default::default()
        })
        .unwrap();
        send.write_all(&encode_frame(&header, FLAG_HEADER).unwrap())
            .await
            .unwrap();
        send.write_all(&encode_frame(&[9, 8], FLAG_END_STREAM).unwrap())
            .await
            .unwrap();
        let mut reply = Vec::new();
        tokio::time::timeout(wait_budget(), recv.read_to_end(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status(&reply), (vec![9, 8], 0));
        connection.close(VarInt::from_u32(0), b"done");
    });
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        2,
        "denied request reached foreign dispatch"
    );
    assert_eq!(aster_http_listener_stop(fixture.rt, listener, 100), 0);
    assert_eq!(aster_http_listener_stop(fixture.rt, listener, 0), NOT_FOUND);
    released(&addr, true);
}

#[test]
fn wrong_runtime_and_invalid_inputs_preserve_owners() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let cfg = Config::new(false);
    let listener = fixture.start(&cfg);
    let addr = fixture.addr(listener, false);
    assert_eq!(aster_http_listener_stop(other.rt, listener, 0), NOT_FOUND);
    assert_eq!(aster_server_destroy(other.rt, fixture.server), NOT_FOUND);
    assert_eq!(fixture.addr(listener, false), addr);
    let mut output = 123;
    assert_eq!(
        unsafe {
            aster_server_http_start(
                other.rt,
                fixture.server,
                other.upcalls,
                &cfg.raw(),
                &mut output,
            )
        },
        NOT_FOUND
    );
    assert_eq!(output, 0);
    assert_eq!(
        unsafe {
            aster_server_http_start(
                fixture.rt,
                fixture.server,
                other.upcalls,
                &cfg.raw(),
                &mut output,
            )
        },
        NOT_FOUND
    );
    let mut bad = cfg.raw();
    bad.flags = 0x80;
    assert_eq!(
        unsafe {
            aster_server_http_start(
                fixture.rt,
                fixture.server,
                fixture.upcalls,
                &bad,
                &mut output,
            )
        },
        INVALID
    );
    let mut bad = cfg.raw();
    bad.cert_pem = span(b"bad PEM");
    assert_eq!(
        unsafe {
            aster_server_http_start(
                fixture.rt,
                fixture.server,
                fixture.upcalls,
                &bad,
                &mut output,
            )
        },
        INVALID
    );
    assert_eq!(
        unsafe { aster_upcalls_close(fixture.rt, fixture.upcalls) },
        0
    );
    assert_eq!(
        unsafe {
            aster_server_http_start(
                fixture.rt,
                fixture.server,
                fixture.upcalls,
                &cfg.raw(),
                &mut output,
            )
        },
        CLOSED
    );
    assert_eq!(aster_http_listener_stop(fixture.rt, listener, 0), 0);
    released(&addr, false);
}

#[test]
fn server_destroy_cancels_pending_auth_and_joins_child_ports() {
    let fixture = Fixture::new();
    let listener = fixture.start(&Config::new(false));
    let addr = fixture.addr(listener, false);
    fixture.pending.store(true, Ordering::Release);
    let client_addr = addr.clone();
    let client = std::thread::spawn(move || {
        async_runtime().block_on(async {
            reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap()
                .post(format!("https://{client_addr}/aster/AuthEcho/echo"))
                .body(encode_frame(&[1], FLAG_END_STREAM).unwrap())
                .send()
                .await?
                .bytes()
                .await
        })
    });
    let id = fixture.auth_started.recv_timeout(wait_budget()).unwrap();
    assert_eq!(aster_server_destroy(fixture.rt, fixture.server), 0);
    assert_eq!(
        fixture.auth_cancelled.recv_timeout(wait_budget()).unwrap(),
        id
    );
    if let Ok(bytes) = client.join().unwrap() {
        assert!(
            bytes.is_empty(),
            "cancelled authentication produced a response"
        );
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        unsafe {
            aster_upcall_respond(fixture.rt, fixture.upcalls, id, 0, [0xca, 1, 0].as_ptr(), 3)
        },
        NOT_FOUND
    );
    assert_eq!(aster_http_listener_stop(fixture.rt, listener, 0), NOT_FOUND);
    released(&addr, false);
}

#[test]
fn runtime_close_joins_live_listener_and_start_race_does_not_publish_late_owner() {
    let mut fixture = Fixture::new();
    let live = fixture.start(&Config::new(true));
    let live_addr = fixture.addr(live, false);
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let race_addr = reserved.local_addr().unwrap().to_string();
    drop(reserved);
    let mut cfg = Config::new(true);
    cfg.bind = race_addr.clone();
    let (rt, server, upcalls) = (fixture.rt, fixture.server, fixture.upcalls);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let start = std::thread::spawn({
        let barrier = barrier.clone();
        move || {
            barrier.wait();
            let mut listener = 0;
            let status =
                unsafe { aster_server_http_start(rt, server, upcalls, &cfg.raw(), &mut listener) };
            assert!(status == 0 || status == CLOSED || status == NOT_FOUND);
        }
    });
    barrier.wait();
    assert_eq!(aster_runtime_close_and_wait(rt, wait_budget()), 0);
    fixture.rt = 0;
    start.join().unwrap();
    released(&live_addr, true);
    released(&race_addr, true);
}

#[test]
fn public_start_is_explicit_and_does_not_bypass_method_permissions() {
    let fixture = Fixture::new();
    let config = Config::new(true);
    let mut raw = config.raw();
    let mut listener = 123;
    // Public mode must not silently discard authentication options.
    assert_eq!(
        unsafe { aster_server_http_start_public(fixture.rt, fixture.server, &raw, &mut listener) },
        INVALID
    );
    assert_eq!(listener, 0);
    raw.max_pending_auth = 0;
    raw.max_auth_payload_bytes = 0;
    assert_eq!(
        unsafe { aster_server_http_start_public(fixture.rt, fixture.server, &raw, &mut listener) },
        0
    );
    let addr = fixture.addr(listener, false);
    assert_eq!(fixture.addr(listener, true), addr);
    async_runtime().block_on(async {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let reply = client
            .post(format!("https://{addr}/aster/AuthEcho/echo"))
            .header("authorization", "Bearer secret")
            .body(encode_frame(&[1], FLAG_END_STREAM).unwrap())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(
            status(&reply),
            (vec![], StatusCode::PermissionDenied.as_i32())
        );
    });
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.auth_started.try_recv().is_err());
    assert_eq!(aster_http_listener_stop(fixture.rt, listener, 0), 0);
    released(&addr, true);
}
