//! Explicit public mode preserves gates without trusting caller identity hints.
use aster::rpc::codec::{decode_rpc_status, encode_stream_header, SerializationMode, StreamHeader};
use aster::rpc::{
    require_role, Call, CapabilityRequirement, PeerProvenance, RpcStatus, Server, ServiceDispatch,
    StatusCode,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{
    decode_frame, encode_frame, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER,
};
use aster_transport_core::test_support::wait_budget;
use aster_transport_salvo::{generate_webtransport_cert, HttpConfig, TlsMaterial};
use salvo::prelude::Service;
use salvo::test::{ResponseExt, TestClient};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use wtransport::{ClientConfig, Endpoint, VarInt};

struct PublicService {
    name: &'static str,
    iroh: bool,
}
#[aster::rpc::async_trait]
impl ServiceDispatch for PublicService {
    fn name(&self) -> &str {
        self.name
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["echo", "secret", "peer"]
    }
    fn iroh_only(&self) -> bool {
        self.iroh
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        (method == "secret").then(|| require_role("operator"))
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        assert_eq!(call.provenance(), PeerProvenance::Other);
        assert!(call.attributes().is_empty());
        let payload = if method == "peer" {
            call.peer().as_bytes().to_vec()
        } else {
            call.recv_request().await.unwrap()
        };
        call.respond(payload, &RpcStatus::ok()).await.unwrap();
    }
}
fn status(bytes: &[u8]) -> i32 {
    let mut bytes = bytes;
    while !bytes.is_empty() {
        let (payload, flags, used) = decode_frame(bytes).unwrap();
        if flags & FLAG_TRAILER != 0 {
            return decode_rpc_status(&payload).unwrap().code;
        }
        bytes = &bytes[used..];
    }
    panic!("missing terminal status")
}

#[tokio::test]
async fn public_http_and_wt_preserve_gates_and_reject_spoofed_sessions_before_factory() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .build(),
    )
    .await
    .unwrap();
    let factories = Arc::new(AtomicUsize::new(0));
    let count = factories.clone();
    let dispatcher = Server::new(&node)
        .register(PublicService {
            name: "Public",
            iroh: false,
        })
        .register(PublicService {
            name: "Iroh",
            iroh: true,
        })
        .register_session(move || {
            count.fetch_add(1, Ordering::SeqCst);
            PublicService {
                name: "Session",
                iroh: false,
            }
        })
        .dispatcher();
    let baseline = factories.load(Ordering::SeqCst);
    let public = dispatcher.for_public_transport();
    // Mutate the exact store retained by this router, using the observed peer
    // hint: public dispatch must ignore cached roles even after startup.
    let in_process = Service::new(aster_transport_salvo::router(public.clone()));
    let mut response = TestClient::post("http://localhost/aster/Public/peer")
        .body(encode_frame(b"", FLAG_END_STREAM).unwrap())
        .send(&in_process)
        .await;
    let bytes = response.take_bytes(None).await.unwrap();
    let (peer, _, _) = decode_frame(&bytes).unwrap();
    public
        .attributes()
        .set_role(String::from_utf8(peer).unwrap(), "operator")
        .unwrap();
    let mut response = TestClient::post("http://localhost/aster/Public/secret")
        .body(encode_frame(b"", FLAG_END_STREAM).unwrap())
        .send(&in_process)
        .await;
    assert_eq!(
        status(&response.take_bytes(None).await.unwrap()),
        StatusCode::PermissionDenied.as_i32()
    );
    let cert = generate_webtransport_cert("public-test", None, &["127.0.0.1".into()]).unwrap();
    let mut listener =
        HttpConfig::new("127.0.0.1:0", TlsMaterial::pem(cert.cert_pem, cert.key_pem))
            .webtransport(true)
            .start_public(public)
            .await
            .unwrap();
    let addr = listener.tcp_addr();
    assert_eq!(Some(addr), listener.quic_addr());
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    for (service, method, wanted) in [
        ("Public", "echo", 0),
        ("Public", "secret", StatusCode::PermissionDenied.as_i32()),
        ("Iroh", "echo", StatusCode::NotFound.as_i32()),
        ("Session", "echo", StatusCode::Unauthenticated.as_i32()),
    ] {
        let bytes = client
            .post(format!("https://{addr}/aster/{service}/{method}"))
            .header("authorization", "Bearer operator")
            .header("aster-session-id", "spoofed")
            .body(encode_frame(b"hello", FLAG_END_STREAM).unwrap())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(status(&bytes), wanted, "{service}/{method}");
    }
    let endpoint = Endpoint::client(
        ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build(),
    )
    .unwrap();
    let conn = endpoint
        .connect(format!("https://{addr}/aster/wt"))
        .await
        .unwrap();
    for (service, method, wanted) in [
        ("Public", "echo", 0),
        ("Public", "secret", StatusCode::PermissionDenied.as_i32()),
        ("Iroh", "echo", StatusCode::NotFound.as_i32()),
        ("Session", "echo", StatusCode::Unauthenticated.as_i32()),
    ] {
        let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
        let header = StreamHeader {
            service: service.into(),
            method: method.into(),
            version: 1,
            call_id: 0,
            deadline: 0,
            serialization_mode: SerializationMode::Xlang.as_i8(),
            metadata_keys: vec!["authorization".into(), "aster-session-id".into()],
            metadata_values: vec!["Bearer operator".into(), "spoofed".into()],
            session_id: if service == "Session" { 77 } else { 0 },
        };
        send.write_all(
            &encode_frame(&encode_stream_header(&header).unwrap(), FLAG_HEADER).unwrap(),
        )
        .await
        .unwrap();
        send.write_all(&encode_frame(b"hello", FLAG_END_STREAM).unwrap())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(wait_budget(), recv.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status(&bytes), wanted, "WT {service}/{method}");
    }
    assert_eq!(
        factories.load(Ordering::SeqCst),
        baseline,
        "caller-provided session ids must never invoke factory"
    );
    conn.close(VarInt::from_u32(0), b"done");
    drop(conn);
    drop(endpoint);
    listener.stop(Duration::ZERO).await.unwrap();
    node.shutdown().await;
}
