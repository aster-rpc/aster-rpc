//! One coordination voter, in its own process, for the §S3-F process-loss gate.
//!
//! The contract matrix requires "real Iroh loopback subprocess voters proving
//! identity, admission, restart, snapshot catch-up, and **actual process
//! loss**". An in-process `AsterServer::shutdown()` is none of those: it closes
//! the endpoint, releases the storage lock and drains the actors — a polite
//! departure, and the opposite of the event being tested. Only a separate
//! process can be killed.
//!
//! ## The control plane is the filesystem
//!
//! Deliberately, because the parent must be able to observe and steer voters it
//! cannot call into, and any socket-based control channel would be a second
//! network to debug alongside the one under test. Files also survive the thing
//! this gate does: a voter that is `SIGKILL`ed leaves its last status behind,
//! where a connection would simply vanish.
//!
//! Under `<control-dir>`:
//!
//! | File | Written by | Meaning |
//! | --- | --- | --- |
//! | `addr-<i>` | voter `i` | `<node-id-hex> <socket-addr>…`, once bound |
//! | `status-<i>` | voter `i` | `<lifecycle> <quorum> <leader-index> <applied>`, refreshed |
//! | `propose-<i>` | the parent | a key; voter `i` proposes an acquire for it |
//! | `proposed-<i>` | voter `i` | `ok <index>` or `err <reason>` |
//!
//! A status file is rewritten whole and renamed into place, so a parent never
//! reads half a line — the same reason the production snapshot path renames
//! rather than truncating.
//!
//! Usage: `coordination-voter <control-dir> <index> <group-size> <seed>`

#![cfg(feature = "coordination-testing")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use aster::coordination::contract::{
    AcquireActionV1, ActionV1, ClientCommandV1, CommandV1, ResourceKeyV1,
};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;

const DOMAIN: &str = "process-loss.test";

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: coordination-voter <control-dir> <index> <group-size> <seed>");
        std::process::exit(2);
    }
    let control = PathBuf::from(&args[1]);
    let index: usize = args[2].parse().expect("an index");
    let size: usize = args[3].parse().expect("a group size");
    let seed: u8 = args[4].parse().expect("a seed");

    let fixture = GroupFixture::new(DOMAIN, seed, size);

    // The storage directory is derived from the control directory rather than
    // passed, so a restarted voter reopens the *same* store. That is what makes
    // the restart half of this gate a rejoin rather than a second bootstrap.
    let storage = control.join(format!("store-{index}"));
    // Bootstrap founds a group; `Existing` reopens one. A restarted voter must
    // take the second path or §7.2's binding check never runs — and the whole
    // point of restarting it is that the binding survived a kill.
    let returning = storage.exists();
    std::fs::create_dir_all(&storage).expect("a storage directory");

    let server = AsterServer::builder()
        .config(fixture.aster_config(index))
        .builtin_coordination(if returning {
            fixture.existing_config(&storage)
        } else {
            fixture.config(&storage, index)
        })
        .start()
        .await
        .expect("this voter starts");

    // Publish where we are.
    let addr = server.node().addr();
    let mut line = addr.node_id.as_str().to_string();
    for direct in &addr.direct_addresses {
        line.push(' ');
        line.push_str(direct);
    }
    publish(&control, &format!("addr-{index}"), &line);

    // Learn where everyone else is. A voter that gave up here would leave the
    // group unable to form for a reason that has nothing to do with consensus,
    // so it waits as long as the parent's own patience.
    let mut known = vec![false; size];
    known[index] = true;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while known.iter().any(|k| !k) {
        for (i, seen) in known.iter_mut().enumerate() {
            if *seen {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(control.join(format!("addr-{i}"))) else {
                continue;
            };
            let mut parts = text.split_whitespace();
            let Some(node_id) = parts.next() else {
                continue;
            };
            let direct: Vec<String> = parts.map(str::to_string).collect();
            if direct.is_empty() {
                // Written but not yet complete; try again.
                continue;
            }
            server
                .node()
                .add_node_addr(aster::NodeAddr {
                    node_id: aster::NodeId::from_hex(node_id.to_string()),
                    relay_url: None,
                    direct_addresses: direct,
                })
                .expect("register a peer address");
            *seen = true;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "voter {index} never learned every peer address"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let coordination = server
        .coordination()
        .expect("this voter hosts coordination");

    // Report, and take instructions, until killed. There is no exit path: the
    // parent ends this process, which is the point.
    loop {
        let leader = coordination
            .current_leader_for_testing()
            .and_then(|id| fixture.index_of(id.as_bytes()))
            .map(|i| i as i64)
            .unwrap_or(-1);
        publish(
            &control,
            &format!("status-{index}"),
            &format!(
                "{} {} {} {}",
                coordination.lifecycle_code(),
                match coordination.quorum_available() {
                    Some(true) => "yes",
                    Some(false) => "stale",
                    None => "none",
                },
                leader,
                coordination
                    .last_applied_for_testing()
                    .map(|i| i as i64)
                    .unwrap_or(-1),
            ),
        );

        let request = control.join(format!("propose-{index}"));
        if let Ok(key) = std::fs::read_to_string(&request) {
            let _ = std::fs::remove_file(&request);
            let key = key.trim().to_string();
            let action = ActionV1::Acquire(AcquireActionV1 {
                resource: ResourceKeyV1 {
                    namespace: "process-loss".into(),
                    key: key.clone().into_bytes(),
                },
                ttl_ms: 60_000,
            });
            let command = CommandV1::Client(ClientCommandV1 {
                actor_node_id: fixture.node_id(index),
                client_id: aster::coordination::testing::id16(key.as_bytes()),
                sequence: 1,
                request_hash: aster::coordination::hash::request_hash(&action)
                    .expect("a canonical action hashes")
                    .to_vec(),
                action,
            });
            let answer = match coordination
                .propose_for_testing_within(command, Duration::from_secs(10))
                .await
            {
                Ok(committed) => format!("ok {}", committed.index),
                Err(err) => format!("err {err}"),
            };
            publish(&control, &format!("proposed-{index}"), &answer);
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Write `body` to `<dir>/<name>` atomically.
///
/// Rename rather than truncate-and-write, so a parent polling this file never
/// reads a half-written line and concludes the voter reported nonsense.
fn publish(dir: &Path, name: &str, body: &str) {
    let final_path = dir.join(name);
    let temp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&temp, body).expect("write a control file");
    std::fs::rename(&temp, &final_path).expect("publish a control file");
}
