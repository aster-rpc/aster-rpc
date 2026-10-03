//! Cross-binding interop smoke: the Rust side of `tests/interop/`.
//!
//! Shared contract (must stay byte-identical in `tests/interop/python/` and
//! `tests/interop/ts/`):
//!
//! ```text
//! service InteropEcho v1
//!   echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//!   generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//!   ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//! interop/EchoRequest { message: string, count: varint32, data: binary }
//! interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
//! ```
//!
//! Types, service and server semantics live in `contract.rs` (shared with
//! `benchmarks/dispatch/driver.rs`).
//!
//! Usage (built as the `interop_echo` example of the `aster` crate, feature `rpc`):
//!
//! ```text
//! interop_echo contract           # prints CONTRACT_ID <hex>
//! interop_echo server             # prints CONTRACT_ID, then ADDRESS aster1…, serves until stdin closes
//! interop_echo client <aster1…>   # runs the cases (unary + streaming), prints CONTRACT_ID + one CASE line each, exit 0 on success
//! ```
//!
//! Open gate, relay disabled, loopback bind: works offline.

use std::time::Duration;

use aster::rpc::{contract_id, RpcConnection};
use aster::{AsterConfig, Node, RelayMode, Result, Ticket};

#[path = "contract.rs"]
mod contract;
use contract::{EchoImpl, EchoReply, EchoRequest, InteropEchoClient, InteropEchoServer};

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

fn contract_hex() -> String {
    contract_id(&InteropEchoClient::contract())
}

/// The shared cases. Must match `cases()` in the Python and TS drivers.
fn cases(client_lang: &str) -> Vec<(&'static str, EchoRequest)> {
    let small = EchoRequest {
        message: format!("hello from {client_lang}"),
        count: 41,
        data: vec![0, 1, 2, 255],
    };
    // > 4096 bytes on the wire and highly compressible: exercises zstd.
    let big = EchoRequest {
        message: "héllo ✓ ".repeat(200),
        count: -123_456,
        data: (0..20_000u32).map(|i| (i % 251) as u8).collect(),
    };
    // A single string field longer than 4095 bytes (Fory string-length edge).
    let long_string = EchoRequest {
        message: "x".repeat(5000),
        count: i32::MAX - 1,
        data: vec![],
    };
    vec![
        ("small", small),
        ("zstd", big),
        ("long_string", long_string),
    ]
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..100 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn serve() -> Result<()> {
    let srv = aster::rpc::AsterServer::builder()
        .config(cfg())
        .service(InteropEchoServer::new(EchoImpl))
        .builtin_node_info(false)
        .start()
        .await?;
    wait_for_addr(srv.node()).await;
    println!("ADDRESS {}", srv.address()?);
    // Serve until the driver closes our stdin (or kills us).
    tokio::task::spawn_blocking(|| {
        let mut sink = String::new();
        while std::io::stdin()
            .read_line(&mut sink)
            .map(|n| n > 0)
            .unwrap_or(false)
        {
            sink.clear();
        }
    })
    .await
    .ok();
    srv.shutdown().await;
    Ok(())
}

async fn run_client(ticket: &str) -> bool {
    let node = match Node::start(cfg()).await {
        Ok(n) => n,
        Err(e) => {
            println!("CASE connect FAIL node start: {e}");
            return false;
        }
    };
    let conn: RpcConnection = match async {
        let t = Ticket::from_base58(ticket)?;
        let id = node.add_ticket_addr(&t)?;
        node.rpc_connect(&id).await
    }
    .await
    {
        Ok(c) => c,
        Err(e) => {
            println!("CASE connect FAIL {e}");
            return false;
        }
    };
    let client = InteropEchoClient::new(conn);
    let mut all_ok = true;
    for (name, req) in cases("rust") {
        let res = tokio::time::timeout(Duration::from_secs(15), client.echo(req.clone())).await;
        let verdict = match res {
            Err(_) => Err("timeout".to_string()),
            Ok(Err(e)) => Err(format!("rpc error: {e}")),
            Ok(Ok(reply)) => check(&req, &reply),
        };
        match verdict {
            Ok(server_lang) => println!("CASE {name} OK server_lang={server_lang}"),
            Err(why) => {
                all_ok = false;
                println!("CASE {name} FAIL {why}");
            }
        }
    }
    for (name, verdict) in [
        ("server_stream", stream_case(&client).await),
        ("client_stream", ingest_case(&client).await),
    ] {
        match verdict {
            Ok(server_lang) => println!("CASE {name} OK server_lang={server_lang}"),
            Err(why) => {
                all_ok = false;
                println!("CASE {name} FAIL {why}");
            }
        }
    }
    node.shutdown().await;
    all_ok
}

/// `generate` with count 3: three replies, `count` 0..3, message/data echoed.
/// Must match the Python and TS drivers.
async fn stream_case(client: &InteropEchoClient) -> std::result::Result<String, String> {
    let req = EchoRequest {
        message: "stream".into(),
        count: 3,
        data: vec![7, 8, 9],
    };
    let fut = async {
        let mut s = client.generate(req.clone()).await?;
        s.collect().await
    };
    let replies = match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Err(_) => return Err("timeout".into()),
        Ok(Err(e)) => return Err(format!("rpc error: {e}")),
        Ok(Ok(r)) => r,
    };
    if replies.len() != 3 {
        return Err(format!("{} replies != 3", replies.len()));
    }
    for (i, r) in replies.iter().enumerate() {
        if r.count != i as i32 || r.message != req.message || r.data != req.data {
            return Err(format!("reply {i} mismatch: {replies:?}"));
        }
    }
    Ok(replies[0].server_lang.clone())
}

/// `ingest` with three requests: one reply, count 3, last message echoed.
async fn ingest_case(client: &InteropEchoClient) -> std::result::Result<String, String> {
    let reqs: Vec<EchoRequest> = (0..3)
        .map(|i| EchoRequest {
            message: format!("m{i}"),
            count: i,
            data: vec![i as u8],
        })
        .collect();
    let reply = match tokio::time::timeout(Duration::from_secs(15), client.ingest(reqs)).await {
        Err(_) => return Err("timeout".into()),
        Ok(Err(e)) => return Err(format!("rpc error: {e}")),
        Ok(Ok(r)) => r,
    };
    if reply.count != 3 || reply.message != "m2" {
        return Err(format!("reply mismatch: {reply:?}"));
    }
    if reply.server_lang.is_empty() {
        return Err("empty server_lang".into());
    }
    Ok(reply.server_lang)
}

fn check(req: &EchoRequest, reply: &EchoReply) -> std::result::Result<String, String> {
    if reply.message != req.message {
        return Err(format!(
            "message mismatch (len {} vs {})",
            reply.message.len(),
            req.message.len()
        ));
    }
    if reply.count != req.count.wrapping_add(1) {
        return Err(format!(
            "count {} != {}",
            reply.count,
            req.count.wrapping_add(1)
        ));
    }
    if reply.data != req.data {
        return Err(format!(
            "data mismatch (len {} vs {})",
            reply.data.len(),
            req.data.len()
        ));
    }
    if reply.server_lang.is_empty() {
        return Err("empty server_lang".into());
    }
    Ok(reply.server_lang.clone())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("CONTRACT_ID {}", contract_hex());
    match args.first().map(String::as_str) {
        Some("contract") => {}
        Some("server") => {
            if let Err(e) = serve().await {
                eprintln!("server error: {e}");
                std::process::exit(1);
            }
        }
        Some("client") if args.len() == 2 => {
            if !run_client(&args[1]).await {
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: interop_echo contract | server | client <aster1-ticket>");
            std::process::exit(2);
        }
    }
}
