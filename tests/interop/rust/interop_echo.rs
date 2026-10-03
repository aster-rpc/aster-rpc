//! Cross-binding interop smoke: the Rust side of `tests/interop/`.
//!
//! Shared contract (must stay byte-identical in `tests/interop/python/` and
//! `tests/interop/ts/`):
//!
//! ```text
//! service InteropEcho v1
//!   echo(interop/EchoRequest) -> interop/EchoReply           (unary)
//! interop/EchoRequest { message: string, count: varint32, data: binary }
//! interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
//! ```
//!
//! Server semantics: `message` and `data` echoed, `count + 1`, `server_lang`
//! set to the serving language.
//!
//! Usage (built as the `interop_echo` example of the `aster` crate, feature `rpc`):
//!
//! ```text
//! interop_echo contract           # prints CONTRACT_ID <hex>
//! interop_echo server             # prints CONTRACT_ID, then ADDRESS aster1…, serves until stdin closes
//! interop_echo client <aster1…>   # runs the unary cases, prints CONTRACT_ID + one CASE line each, exit 0 on success
//! ```
//!
//! Open gate, relay disabled, loopback bind: works offline.

use std::time::Duration;

use aster::rpc::{async_trait, contract_id, RpcConnection};
use aster::{AsterConfig, Node, RelayMode, Result, Ticket};
use fory_derive::ForyStruct;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "interop/EchoRequest")]
struct EchoRequest {
    message: String,
    count: i32,
    // `#[fory(bytes)]` is REQUIRED for interop: without it fory-derive 1.7.6
    // writes `Vec<u8>` as LIST<UINT8> (type 22) while `AsterType` advertises
    // `binary` and Python `bytes` / TS `Uint8Array` write BINARY (type 41).
    #[fory(bytes)]
    data: Vec<u8>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "interop/EchoReply")]
struct EchoReply {
    message: String,
    count: i32,
    #[fory(bytes)]
    data: Vec<u8>,
    server_lang: String,
}

#[aster::service(name = "InteropEcho", version = 1)]
trait InteropEcho {
    async fn echo(&self, req: EchoRequest) -> Result<EchoReply>;
}

struct EchoImpl;

#[async_trait]
impl InteropEcho for EchoImpl {
    async fn echo(&self, req: EchoRequest) -> Result<EchoReply> {
        Ok(EchoReply {
            message: req.message,
            count: req.count.wrapping_add(1),
            data: req.data,
            server_lang: "rust".into(),
        })
    }
}

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
    node.shutdown().await;
    all_ok
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
