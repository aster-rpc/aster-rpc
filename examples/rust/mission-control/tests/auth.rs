//! The auth chapter end to end: `mission-control --auth` as a test. The server
//! is configured from an `aster.toml` exactly as an operator writes it; the
//! credentials are the files `aster enroll node --attributes` produces; the
//! typed client connects with `AsterClient`. Each role reaches exactly the
//! methods whose `requires` it satisfies, with values identical to Python's
//! `roles.py`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::{
    issue_credential, AdmissionClaims, EnrollmentIssueRequest, JwtEnrollmentIssuer, TypedAttribute,
};
use aster::rpc::{AsterClient, AsterServer, StatusCode};
use aster::{AsterConfig, NamespaceSecret, SecretKey};
use mission_control::auth::{MissionControlClient, MissionControlServer};
use mission_control::{
    roles, Command, MetricPoint, MissionControlImpl, StatusRequest, TailRequest,
};

const AUDIENCE: &str = "mission-control";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// What `aster enroll node --subject <key> --audience mission-control
/// --attributes roles.json --out <file>` writes, with roles.json
/// `{"aster.role": {"type": "string", "value": "<roles>"}}`.
fn enroll(root: &SecretKey, node: &SecretKey, roles: &[&str], out: &Path) {
    let subject = JwtEnrollmentIssuer::from_seed(&node.to_bytes()).public_key();
    let mut request = EnrollmentIssueRequest::new(subject, AUDIENCE, now() + 3600);
    let mut attributes = BTreeMap::new();
    if !roles.is_empty() {
        attributes.insert(
            "aster.role".to_string(),
            TypedAttribute::String(roles.join(",")),
        );
    }
    request.claims = AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    };
    let chain = issue_credential(root, &request, now())
        .unwrap()
        .into_chain();
    std::fs::write(out, format!("{chain}\n")).unwrap();
}

fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn denied<T: std::fmt::Debug>(result: aster::Result<T>) -> bool {
    match result {
        Err(err) => err.rpc_code() == Some(StatusCode::PermissionDenied),
        Ok(value) => panic!("expected PERMISSION_DENIED, got {value:?}"),
    }
}

/// A streaming call refused by Gate 3: at open, or as the stream's first item.
macro_rules! stream_denied {
    ($opened:expr) => {{
        let is_denied =
            |err: &aster::Error| err.rpc_code() == Some(StatusCode::PermissionDenied);
        match $opened {
            Err(err) => is_denied(&err),
            Ok(mut stream) => matches!(stream.recv().await, Some(Err(err)) if is_denied(&err)),
        }
    }};
}

async fn connect(
    dir: &Path,
    root: &SecretKey,
    fleet: &str,
    name: &str,
    roles: &[&str],
    address: &str,
) -> (AsterClient, MissionControlClient) {
    let key = SecretKey::generate();
    let key_file = dir.join(format!("{name}.key"));
    key.write_file(&key_file).unwrap();
    let credential = dir.join(format!("{name}.cred"));
    enroll(root, &key, roles, &credential);
    let config = AsterConfig::builder()
        .relay(aster::RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key_file(key_file)
        .root_pubkey(root.public_key())
        .enrollment_credential_file(credential)
        .enrollment(aster::EnrollmentConfig {
            audience: Some(AUDIENCE.into()),
            fleet_namespace: Some(fleet.into()),
            ..Default::default()
        })
        .build();
    let client = AsterClient::builder()
        .config(config)
        .address(address)
        .connect()
        .await
        .unwrap();
    let typed = MissionControlClient::new(client.rpc());
    (client, typed)
}

#[tokio::test]
async fn roles_in_credentials_gate_each_method() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        // `aster key root generate`, `aster enroll namespace create`,
        // `aster key node generate --out server.key`.
        let root = SecretKey::generate();
        let fleet = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes())
            .id()
            .to_hex();
        let server_key = SecretKey::generate();
        let server_key_file = dir.path().join("server.key");
        server_key.write_file(&server_key_file).unwrap();
        let server_cred = dir.path().join("server.cred");
        enroll(&root, &server_key, &[], &server_cred);

        let config_file = dir.path().join("aster.toml");
        std::fs::write(
            &config_file,
            format!(
                r#"
[trust]
root_pubkey = "{root}"
enrollment_credential = "{cred}"

[network]
secret_key_file = "{key}"
relay_mode = "disabled"
bind_addr = "127.0.0.1:0"

[enrollment]
audience = "{AUDIENCE}"
fleet_namespace = "{fleet}"
state_path = "{state}"
"#,
                root = root.public_key().to_hex(),
                cred = toml_path(&server_cred),
                key = toml_path(&server_key_file),
                state = toml_path(&dir.path().join("server-enrollment.json")),
            ),
        )
        .unwrap();

        let server = AsterServer::builder()
            .service(MissionControlServer::new(MissionControlImpl))
            .config(AsterConfig::from_file(&config_file).unwrap())
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
        let status = || StatusRequest {
            agent_id: "a1".into(),
        };
        let tail = || TailRequest {
            agent_id: "a1".into(),
            level: "info".into(),
        };
        let metrics = || {
            vec![MetricPoint {
                name: "cpu".into(),
                value: 1.0,
                timestamp: 0.0,
            }]
        };
        let command = || {
            vec![Command {
                command: "uptime".into(),
            }]
        };

        // An operator watches the fleet.
        let (operator, mc) = connect(
            dir.path(),
            &root,
            &fleet,
            "operator",
            &[roles::STATUS, roles::LOGS],
            &address,
        )
        .await;
        assert_eq!(mc.get_status(status()).await.unwrap().status, "running");
        let mut logs = mc.tail_logs(tail()).await.unwrap();
        assert!(logs.recv().await.unwrap().is_ok());
        assert!(denied(mc.ingest_metrics(metrics()).await));
        assert!(stream_denied!(mc.run_command(command()).await));

        // An agent reports in.
        let (agent, mc_agent) = connect(
            dir.path(),
            &root,
            &fleet,
            "agent",
            &[roles::STATUS, roles::INGEST],
            &address,
        )
        .await;
        assert_eq!(
            mc_agent.ingest_metrics(metrics()).await.unwrap().accepted,
            1
        );
        assert!(stream_denied!(mc_agent.tail_logs(tail()).await));

        // An administrator runs commands but has no status role.
        let (admin, mc_admin) = connect(
            dir.path(),
            &root,
            &fleet,
            "admin",
            &[roles::ADMIN],
            &address,
        )
        .await;
        let mut results = mc_admin.run_command(command()).await.unwrap();
        assert_eq!(results.recv().await.unwrap().unwrap().stdout, "ran: uptime");
        assert!(mc_admin.tail_logs(tail()).await.is_ok());
        assert!(denied(mc_admin.get_status(status()).await));

        // Every enrolled node may submit logs; no role needed.
        let (plain, mc_plain) = connect(dir.path(), &root, &fleet, "plain", &[], &address).await;
        assert!(
            mc_plain
                .submit_log(mission_control::LogEntry::default())
                .await
                .unwrap()
                .accepted
        );
        assert!(denied(mc_plain.get_status(status()).await));

        for client in [operator, agent, admin, plain] {
            client.close().await;
        }
        server.shutdown().await;
    })
    .await
    .unwrap();
}
