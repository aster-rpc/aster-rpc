//! Test-only coordination voter group for the TypeScript suite.
//!
//! The coordination **voter** is Rust-only by design (plan D7): the binding
//! wraps the client and never hosts a voter. The TypeScript integration tests
//! still need a real group to talk to, so this small binary starts one from
//! `aster::coordination::testing::GroupFixture` — the same fixture the Rust
//! client gates use — prints what a client needs as one JSON line on stdout,
//! and runs until its stdin closes (the parent test process exiting closes it).
//!
//! Built only with the `coordination-fixture` feature, never by `napi build`:
//!
//! ```text
//! cargo build -p aster-transport-napi --features coordination-fixture \
//!     --bin aster-coordination-fixture
//! ```
//!
//! Usage: `aster-coordination-fixture <storage-dir> [voters=1] [seed=0xC7]`.

use std::path::PathBuf;
use std::time::Duration;

use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;
use aster_transport_core::ticket::{AsterTicket, TicketCredential};
use tokio::io::AsyncReadExt;

const DOMAIN: &str = "aster.typescript.fixture";

fn parse_u8(arg: Option<String>, default: u8) -> u8 {
    match arg {
        None => default,
        Some(s) => {
            let s = s.trim();
            match s.strip_prefix("0x") {
                Some(hex) => u8::from_str_radix(hex, 16),
                None => s.parse(),
            }
            .unwrap_or_else(|_| panic!("not a byte: {s:?}"))
        }
    }
}

/// An Open ticket carrying the voter's current direct addresses: the routing
/// hint a client seeds with (it registers the addresses, the handshake
/// authenticates the identity).
fn routed_ticket(server: &AsterServer) -> String {
    let addr = server.node().addr();
    let id: [u8; 32] = hex::decode(addr.node_id.as_str())
        .expect("hex node id")
        .try_into()
        .expect("32 bytes");
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
    .expect("a ticket")
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("usage: <storage-dir> [voters] [seed]"));
    let voters = usize::from(parse_u8(args.next(), 1));
    let seed = parse_u8(args.next(), 0xC7);
    let fixture = GroupFixture::new(DOMAIN, seed, voters);

    let mut servers: Vec<AsterServer> = Vec::with_capacity(voters);
    for index in 0..voters {
        let store = dir.join(format!("voter-{index}"));
        std::fs::create_dir_all(&store).expect("a voter directory");
        let server = AsterServer::builder()
            .config(fixture.aster_config(index))
            .builtin_coordination(fixture.config(&store, index))
            .start()
            .await
            .expect("the voter starts");
        // Wait for a bound socket so the ticket carries a route.
        for _ in 0..200 {
            if !server.node().addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        servers.push(server);
    }
    // Voters must know each other's routes before the group can form.
    for a in &servers {
        for b in &servers {
            if a.node().id() != b.node().id() {
                a.node()
                    .add_node_addr(b.node().addr())
                    .expect("register a peer voter");
            }
        }
    }
    for server in &servers {
        let coordination = server.coordination().expect("hosted");
        tokio::time::timeout(Duration::from_secs(60), coordination.wait_ready())
            .await
            .expect("the group becomes ready in time")
            .expect("the group becomes ready");
    }

    let voters_json: Vec<serde_json::Value> = servers
        .iter()
        .map(|s| {
            serde_json::json!({
                "nodeId": s.node().id().as_str(),
                "ticket": routed_ticket(s),
                "directAddresses": s.node().addr().direct_addresses,
            })
        })
        .collect();
    let info = serde_json::json!({
        "groupId": hex::encode(fixture.group_id().as_bytes()),
        "genesisHash": hex::encode(fixture.genesis_hash()),
        "authorityDomain": DOMAIN,
        "authorityKey": hex::encode(fixture.authority_key()),
        // GroupFixture derives its ROOT signing key from the seed; a client
        // whose endpoint secret is this key is the group's ROOT.
        "rootSecret": hex::encode([seed ^ 0xFF; 32]),
        "voters": voters_json,
    });
    println!("{info}");

    // Run until the parent closes stdin.
    let mut sink = Vec::new();
    let _ = tokio::io::stdin().read_to_end(&mut sink).await;
    for server in servers {
        server.shutdown().await;
    }
}
