//! A root publication session (`aster enroll revoke --peer …`) reaching a
//! direct-only server: relay disabled, bound to a loopback port, so it is
//! reachable only at its direct address. The CLI must dial the address the
//! `--peer` argument carries (`aster1…` or `<id>@<ip:port>`); a bare node id
//! with no configured route fails fast with a hint, not a handshake timeout.
//! CLI state uses an isolated file keystore, never the developer's Keychain.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use aster::enrollment::JwtEnrollmentIssuer;
use aster::rpc::{AsterServer, Call, RpcStatus, ServiceDispatch};
use aster::{AsterConfig, EnrollmentConfig, PublicKey, RelayMode, SecretKey};

const AUDIENCE: &str = "mission-control";

struct Home(PathBuf);
impl Home {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aster-enroll-publish-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    /// Run the CLI as a user would, with direct-only node variables exported
    /// in the shell: the CLI must not depend on them either way.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .arg("--json")
            .env("ASTER_HOME", self.path("home"))
            .env("ASTER_KEYSTORE", "file")
            .env("ASTER_RELAY_MODE", "disabled")
            .env("ASTER_BIND_ADDR", "127.0.0.1:0")
            .env_remove("ASTER_PROFILE")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server with one open method; the test only needs it to hold the fleet
/// namespace and serve enrollment.
struct Ping;
#[async_trait::async_trait]
impl ServiceDispatch for Ping {
    fn name(&self) -> &str {
        "Ping"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        let _ = call.respond(b"pong".to_vec(), &RpcStatus::ok()).await;
    }
}

fn node_id(seed: u8) -> String {
    hex::encode(JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key())
}

fn write_key(path: &Path, seed: u8) {
    SecretKey::from_bytes([seed; 32]).write_file(path).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_reaches_a_direct_only_server_by_the_address_peer_carries() {
    let h = Home::new("direct");
    // The root, the fleet namespace and credentials, as the README's chapter.
    let root = h.ok(&["key", "root", "generate"])["root_pubkey"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            h.ok(&["key", "root", "show"])["root_pubkey"]
                .as_str()
                .unwrap()
                .to_owned()
        });
    let secret = h.path("fleet.secret");
    let fleet = h.ok(&[
        "enroll",
        "namespace",
        "create",
        "--out-secret",
        secret.to_str().unwrap(),
    ])["namespace"]
        .as_str()
        .unwrap()
        .to_owned();
    let server_cred = h.path("server.cred");
    h.ok(&[
        "enroll",
        "node",
        "--subject",
        &node_id(2),
        "--audience",
        AUDIENCE,
        "--out",
        server_cred.to_str().unwrap(),
    ]);
    // The profile's enrolled publisher identity.
    std::fs::create_dir_all(h.path("home/keys")).unwrap();
    write_key(&h.path("home/keys/default.node.key"), 3);
    let publisher_cred = h.path("publisher.cred");
    h.ok(&[
        "enroll",
        "node",
        "--subject",
        &node_id(3),
        "--audience",
        AUDIENCE,
        "--out",
        publisher_cred.to_str().unwrap(),
    ]);
    let config = h.path("home/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "exec_credential_file = \"{}\"\n",
        publisher_cred.display()
    ));
    std::fs::write(&config, text).unwrap();

    // The direct-only server.
    let server = AsterServer::builder()
        .service(Ping)
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes([2; 32]))
                .root_pubkey(PublicKey::from_hex(&root).unwrap())
                .enrollment_credential_file(server_cred)
                .enrollment(EnrollmentConfig {
                    audience: Some(AUDIENCE.into()),
                    fleet_namespace: Some(fleet.clone()),
                    state_path: Some(h.path("server-enrollment.json")),
                    ..Default::default()
                })
                .build(),
        )
        .start()
        .await
        .unwrap();
    for _ in 0..100 {
        if !server.node().addr().direct_addresses.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let address = server.address().unwrap();
    let direct = format!(
        "{}@{}",
        node_id(2),
        server.node().addr().direct_addresses.join(",")
    );

    let revoke = |peer: String| {
        let secret = secret.clone();
        let fleet = fleet.clone();
        let home = h.path("home");
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_aster"))
                .args([
                    "enroll",
                    "revoke",
                    "--subject",
                    &node_id(9),
                    "--fleet-only",
                    "--fleet-namespace",
                    &fleet,
                    "--fleet-secret-file",
                    secret.to_str().unwrap(),
                    "--audience",
                    AUDIENCE,
                    "--peer",
                    &peer,
                    "--json",
                ])
                .env("ASTER_HOME", home)
                .env("ASTER_KEYSTORE", "file")
                .env("ASTER_RELAY_MODE", "disabled")
                .env("ASTER_BIND_ADDR", "127.0.0.1:0")
                .env_remove("ASTER_PROFILE")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
        })
    };

    // The server's printed aster1 address carries its direct address.
    let out = revoke(address).await.unwrap();
    assert!(
        out.status.success(),
        "aster1 peer: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // <id>@<ip:port> works too (an idempotent replay of the same record).
    let out = revoke(direct).await.unwrap();
    assert!(
        out.status.success(),
        "id@addr peer: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A bare id with no configured route cannot be found by discovery: it
    // fails promptly, naming the fix, rather than timing out a handshake.
    let started = std::time::Instant::now();
    let out = revoke(node_id(2)).await.unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("policy replica") && stderr.contains("aster1"),
        "{stderr}"
    );
    assert!(started.elapsed() < Duration::from_secs(30), "{stderr}");

    server.shutdown().await;
}
