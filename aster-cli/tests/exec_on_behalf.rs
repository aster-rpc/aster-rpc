//! On-behalf-of chains minted by the CLI verify for the named actor and bind
//! the digest `exec run --print-request-digest` reports.
use std::path::PathBuf;
use std::process::{Command, Output};

use aster::delegation::{LeafClaims, LeafKind, NoRevocations, TrustContext, verify_chain};

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("aster-exec-obo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_aster"))
            .arg("--json")
            .args(args)
            .env("ASTER_HOME", &self.0)
            .env("ASTER_KEYSTORE", "file")
            .env_remove("ASTER_PROFILE")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn key(&self, seed: u8) -> (PathBuf, aster::SecretKey) {
        let path = self.0.join(format!("node-{seed}.key"));
        let key = aster::SecretKey::from_bytes([seed; 32]);
        key.write_file(&path).unwrap();
        (path, key)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn principal_delegates_and_orchestrator_authorizes_a_bound_request() {
    let f = Fixture::new("mint");
    let (principal_key, principal) = f.key(1);
    let (orchestrator_key, orchestrator) = f.key(2);
    let actor = aster::SecretKey::from_bytes([3; 32]).public_key();
    let target = "22".repeat(32);
    let request_id = uuid::Uuid::new_v4().to_string();

    let printed = f.run(&[
        "exec",
        "run",
        "--node",
        &target,
        "--target",
        &target,
        "--cwd",
        "/tmp",
        "--request-id",
        &request_id,
        "--print-request-digest",
        "--",
        "/bin/true",
    ]);
    let printed: serde_json::Value = serde_json::from_slice(&printed.stdout).unwrap();
    let mut expected = aster_exec_client::request(&target, "/tmp", "/bin/true", vec![]);
    expected.request_id = request_id.clone();
    let digest = hex::encode(expected.on_behalf_digest().unwrap());
    assert_eq!(printed["request_digest"], digest);

    let delegation = f.0.join("delegation.chain");
    f.run(&[
        "exec",
        "delegate",
        "--signer-key",
        principal_key.to_str().unwrap(),
        "--subject",
        &orchestrator.public_key().to_hex(),
        "--audience",
        "fleet",
        "--max-child-lifetime",
        "1h",
        "--out",
        delegation.to_str().unwrap(),
    ]);
    let action = f.0.join("action.chain");
    f.run(&[
        "exec",
        "authorize",
        "--signer-key",
        orchestrator_key.to_str().unwrap(),
        "--parent",
        delegation.to_str().unwrap(),
        "--subject",
        &actor.to_hex(),
        "--audience",
        "fleet",
        "--resource",
        &format!("exec:target={target}"),
        "--request-digest",
        &digest,
        "--out",
        action.to_str().unwrap(),
    ]);

    let text = std::fs::read_to_string(&action).unwrap();
    let anchors = [principal.public_key().to_bytes()];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let trust = TrustContext::new(&anchors, "fleet", now, &NoRevocations);
    let verified = verify_chain(
        text.trim().as_bytes(),
        LeafKind::Action,
        &actor.to_bytes(),
        &trust,
    )
    .unwrap();
    assert_eq!(verified.jtis().len(), 2);
    let LeafClaims::Action(grant) = verified.leaf() else {
        panic!("not an action leaf");
    };
    assert!(grant.operations.contains("exec.submit"));
    assert_eq!(grant.request.as_deref(), Some(digest.as_str()));
    assert!(grant.resources.contains(&format!("exec:target={target}")));
    // The chain is for this actor only.
    let other = aster::SecretKey::from_bytes([4; 32])
        .public_key()
        .to_bytes();
    assert!(verify_chain(text.trim().as_bytes(), LeafKind::Action, &other, &trust).is_err());
}
