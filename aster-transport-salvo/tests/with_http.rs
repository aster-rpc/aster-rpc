//! `AsterServer::builder().with_http(...)` — the high-level producer serves the
//! same services over Iroh AND HTTPS from one builder. Drives the HTTP side with
//! a real TLS client (reqwest).

use std::time::Duration;

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{AsterServer, Call, RpcStatus, ServiceDispatch};
use aster::{AsterConfig, RelayMode};
use aster_transport_core::framing::{decode_frame, encode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use aster_transport_salvo::{HttpConfig, TlsMaterial};

struct Echo;

#[aster::rpc::async_trait]
impl ServiceDispatch for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["unary"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        let resp: Vec<u8> = req.into_iter().rev().collect();
        let _ = call.respond(resp, &RpcStatus::ok());
    }
}

#[tokio::test]
async fn aster_server_with_http() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let srv = AsterServer::builder()
        .config(cfg)
        .service(Echo)
        .with_http(HttpConfig::new(
            format!("127.0.0.1:{port}"),
            TlsMaterial::self_signed(["localhost".into()]),
        ))
        .start()
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let url = format!("https://127.0.0.1:{port}/aster/Echo/unary");
    let body = encode_frame(&[1, 2, 3], FLAG_END_STREAM).unwrap();

    let mut resp = None;
    for _ in 0..40 {
        match client
            .post(&url)
            .header("content-type", "application/aster-frames")
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let bytes = resp.expect("HTTP did not come up").bytes().await.unwrap();

    let (payload, _f, consumed) = decode_frame(&bytes).unwrap();
    assert_eq!(payload, vec![3, 2, 1]);
    let (trailer, tflags, _) = decode_frame(&bytes[consumed..]).unwrap();
    assert!(tflags & FLAG_TRAILER != 0);
    assert_eq!(decode_rpc_status(&trailer).unwrap().code, 0);

    srv.shutdown().await;
}
