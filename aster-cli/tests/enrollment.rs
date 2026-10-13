//! CLI issuance uses isolated file keystores; never the developer's Keychain.
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use aster::enrollment::{
    AdmissionEvidenceSource, JwtEnrollmentIssuer, JwtEnrollmentVerifier, Registration,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("aster-enroll-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .arg("--json")
            .env("ASTER_HOME", &self.0)
            .env("ASTER_KEYSTORE", "file")
            .env_remove("ASTER_PROFILE")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }
    fn pin_root(&self, seed: u8) {
        self.pin_root_with(seed, "");
    }
    fn pin_root_with(&self, seed: u8, extra: &str) {
        std::fs::write(
            self.0.join("config.toml"),
            format!(
                "version = 1\n[profiles.default]\nroot_pubkey = \"{}\"\n{extra}",
                hex::encode(JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key())
            ),
        )
        .unwrap();
    }
    fn key(&self, seed: u8) -> PathBuf {
        let path = self.0.join(format!("root-{seed}.key"));
        aster::SecretKey::from_bytes([seed; 32])
            .write_file(&path)
            .unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn issues_bound_chain_from_explicit_key_and_never_overwrites() {
    let f = Fixture::new("issue");
    f.pin_root(31);
    let key = f.key(31);
    let out = f.0.join("node.jwt");
    let issuer = JwtEnrollmentIssuer::from_seed(&[31; 32]);
    let subject = JwtEnrollmentIssuer::from_seed(&[32; 32]).public_key();
    let subject_hex = hex::encode(subject);
    let args = [
        "enroll",
        "node",
        "--subject",
        &subject_hex,
        "--audience",
        "fleet",
        "--root-key",
        key.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--expires-in",
        "24h",
        "--scope",
        "build-worker",
        "--tag",
        "linux",
    ];
    let result = f.run(&args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["registered"], false);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        report["expires_at"]
            .as_u64()
            .unwrap()
            .abs_diff(now + 86_400)
            <= 2
    );
    assert_eq!(report["registration"], "ephemeral");
    let jwt = std::fs::read_to_string(&out).unwrap();
    let evidence = JwtEnrollmentVerifier::new(issuer.public_key(), "fleet")
        .unwrap()
        .verify(jwt.trim(), &subject, now)
        .unwrap();
    assert!(
        evidence
            .claims()
            .scopes
            .as_ref()
            .unwrap()
            .contains("build-worker")
    );
    assert!(evidence.claims().tags.as_ref().unwrap().contains("linux"));
    assert!(!f.run(&args).status.success());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), jwt);
    assert!(!String::from_utf8_lossy(&result.stdout).contains(&"1f".repeat(32)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(out).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn selected_profile_keystore_signs_with_a_default_lifetime() {
    let f = Fixture::new("profile");
    assert!(f.run(&["key", "root", "generate"]).status.success());
    let subject = hex::encode(JwtEnrollmentIssuer::from_seed(&[33; 32]).public_key());
    let out = f.0.join("node.jwt");
    let base = [
        "enroll",
        "node",
        "--subject",
        &subject,
        "--audience",
        "fleet",
        "--out",
        out.to_str().unwrap(),
    ];
    // An ephemeral credential defaults to a day; it never has no expiry.
    let result = f.run(&base);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(report["expires_at"].as_u64().is_some());
    assert_eq!(
        f.run(&[base.as_slice(), &["--no-expiry"]].concat())
            .status
            .code(),
        Some(2)
    );
    let mismatched = f.key(34);
    let rejected_out = f.0.join("rejected.jwt");
    let result = f.run(&[
        "enroll",
        "node",
        "--subject",
        &subject,
        "--audience",
        "fleet",
        "--out",
        rejected_out.to_str().unwrap(),
        "--root-key",
        mismatched.to_str().unwrap(),
    ]);
    assert_eq!(result.status.code(), Some(4));
    assert!(!rejected_out.exists());
}

#[test]
fn persistent_lifetimes_follow_the_profile_root_cadence() {
    let f = Fixture::new("cadence");
    let key = f.key(40);
    let subject = hex::encode(JwtEnrollmentIssuer::from_seed(&[41; 32]).public_key());
    let issue = |name: &str, extra: &[&str]| {
        let out = f.0.join(name);
        let args = [
            "enroll",
            "node",
            "--subject",
            &subject,
            "--audience",
            "fleet",
            "--root-key",
            key.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--persistent",
        ];
        (f.run(&[args.as_slice(), extra].concat()), out)
    };
    // With no declared cadence a persistent root credential needs 7 days.
    f.pin_root(40);
    let (result, out) = issue("unset-short", &["--expires-in", "1d"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("root_cadence"));
    assert!(!out.exists());
    // A daily root with a 1 h replication allowance needs 2C + R = 49 h.
    f.pin_root_with(
        40,
        "root_cadence = \"1d\"\nreplication_allowance = \"1h\"\n",
    );
    let (result, out) = issue("short", &["--expires-in", "48h"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(!out.exists());
    let (result, _) = issue("forced", &["--expires-in", "48h", "--allow-short-lifetime"]);
    assert!(result.status.success());
    let (result, out) = issue("default", &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["registration"], "persistent");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        report["expires_at"]
            .as_u64()
            .unwrap()
            .abs_diff(now + 49 * 3600)
            <= 2
    );
    let chain = std::fs::read_to_string(out).unwrap();
    let evidence = JwtEnrollmentVerifier::new(
        JwtEnrollmentIssuer::from_seed(&[40; 32]).public_key(),
        "fleet",
    )
    .unwrap()
    .verify(
        chain.trim(),
        &JwtEnrollmentIssuer::from_seed(&[41; 32]).public_key(),
        now,
    )
    .unwrap();
    assert_eq!(evidence.registration(), Some(Registration::Persistent));
    // Beyond the profile's 30 day maximum no link is issued at all.
    let (result, _) = issue("too-long", &["--expires-in", "31d"]);
    assert_eq!(result.status.code(), Some(2));
}

#[test]
fn delegated_issuer_signs_chained_credentials_within_its_delegation() {
    let f = Fixture::new("delegate");
    f.pin_root(42);
    let root = f.key(42);
    let issuer_key = f.key(43);
    let issuer = hex::encode(JwtEnrollmentIssuer::from_seed(&[43; 32]).public_key());
    let node = JwtEnrollmentIssuer::from_seed(&[44; 32]).public_key();
    let node_hex = hex::encode(node);
    let delegation = f.0.join("issuer.chain");
    let delegate = |may_register: bool, out: &std::path::Path| {
        let mut args = vec![
            "enroll",
            "delegate",
            "--subject",
            &issuer,
            "--audience",
            "fleet",
            "--root-key",
            root.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--max-child-lifetime",
            "1d",
            "--scope",
            "build",
        ];
        if may_register {
            args.push("--may-register");
        }
        f.run(&args)
    };
    let result = delegate(false, &delegation);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let node_args = |out: &std::path::Path, parent: &std::path::Path, extra: &[&str]| {
        let args = [
            "enroll",
            "node",
            "--subject",
            &node_hex,
            "--audience",
            "fleet",
            "--signer",
            issuer_key.to_str().unwrap(),
            "--parent",
            parent.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ];
        f.run(&[args.as_slice(), extra].concat())
    };
    let out = f.0.join("node.chain");
    let result = node_args(&out, &delegation, &["--scope", "build"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let evidence = JwtEnrollmentVerifier::new(
        JwtEnrollmentIssuer::from_seed(&[42; 32]).public_key(),
        "fleet",
    )
    .unwrap()
    .verify(std::fs::read_to_string(&out).unwrap().trim(), &node, now)
    .unwrap();
    assert!(matches!(
        evidence.source(),
        AdmissionEvidenceSource::PresentedChain { issuers, .. } if issuers.len() == 2
    ));
    // Beyond the delegation: another scope, a longer life, or persistence
    // without --may-register.
    for (name, extra) in [
        ("scope.chain", vec!["--scope", "deploy"]),
        ("long.chain", vec!["--expires-in", "2d"]),
        ("persistent.chain", vec!["--persistent"]),
    ] {
        let out = f.0.join(name);
        assert_eq!(
            node_args(&out, &delegation, &extra).status.code(),
            Some(2),
            "{name}"
        );
        assert!(!out.exists());
    }
    let registering = f.0.join("registering.chain");
    assert!(delegate(true, &registering).status.success());
    let out = f.0.join("persistent-ok.chain");
    let result = node_args(&out, &registering, &["--persistent"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    // A delegation from a key that is not the profile root is refused.
    let foreign = f.key(45);
    let result = f.run(&[
        "enroll",
        "delegate",
        "--subject",
        &issuer,
        "--audience",
        "fleet",
        "--root-key",
        foreign.to_str().unwrap(),
        "--out",
        f.0.join("foreign.chain").to_str().unwrap(),
        "--max-child-lifetime",
        "1d",
    ]);
    assert_eq!(result.status.code(), Some(4));
}

#[test]
fn expired_or_invalid_claims_leave_no_credential_file() {
    let f = Fixture::new("invalid");
    f.pin_root(35);
    let key = f.key(35);
    let subject = hex::encode(JwtEnrollmentIssuer::from_seed(&[36; 32]).public_key());
    let out = f.0.join("node.jwt");
    let args = [
        "enroll",
        "node",
        "--subject",
        &subject,
        "--audience",
        "fleet",
        "--root-key",
        key.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ];
    let result = f.run(&[args.as_slice(), &["--expires-at", "1"]].concat());
    assert_eq!(result.status.code(), Some(2));
    assert!(!out.exists());
    let attrs = f.0.join("attrs.json");
    std::fs::write(&attrs, r#"{"role":{"type":"not-a-type","value":"admin"}}"#).unwrap();
    let result = f.run(&[args.as_slice(), &["--attributes", attrs.to_str().unwrap()]].concat());
    assert_eq!(result.status.code(), Some(2));
    assert!(!out.exists());
    std::fs::write(
        &attrs,
        r#"{"role":{"type":"string","value":"reader"},"role":{"type":"string","value":"admin"}}"#,
    )
    .unwrap();
    let result = f.run(&[args.as_slice(), &["--attributes", attrs.to_str().unwrap()]].concat());
    assert_eq!(result.status.code(), Some(2));
    assert!(!out.exists());
}

#[cfg(unix)]
#[test]
fn fifo_key_input_is_rejected_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    use std::time::{Duration, Instant};
    let f = Fixture::new("fifo");
    f.pin_root(37);
    let fifo = f.0.join("root.fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // The C string is valid for the duration of the call; no pointers escape.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let out = f.0.join("node.jwt");
    let subject = hex::encode(JwtEnrollmentIssuer::from_seed(&[37; 32]).public_key());
    let mut child = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args([
            "enroll",
            "node",
            "--subject",
            &subject,
            "--audience",
            "fleet",
            "--json",
            "--root-key",
        ])
        .arg(&fifo)
        .arg("--out")
        .arg(&out)
        .env("ASTER_HOME", &f.0)
        .env("ASTER_KEYSTORE", "file")
        .env_remove("ASTER_PROFILE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(2));
            break;
        }
        if started.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("credential command blocked opening a FIFO");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!out.exists());
}

#[test]
fn explicit_key_requires_a_configured_profile_root() {
    let f = Fixture::new("unpinned");
    let key = f.key(38);
    let out = f.0.join("node.jwt");
    let subject = hex::encode(JwtEnrollmentIssuer::from_seed(&[39; 32]).public_key());
    let result = f.run(&[
        "enroll",
        "node",
        "--subject",
        &subject,
        "--audience",
        "fleet",
        "--root-key",
        key.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(result.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&result.stderr).contains("configured root public key"));
    assert!(!out.exists());
}

#[test]
fn only_an_issuer_observes_and_only_persistent_credentials() {
    let f = Fixture::new("observe");
    f.pin_root(41);
    let key = f.key(41);
    let secret = f.0.join("observations.secret");
    std::fs::write(&secret, hex::encode([5; 32])).unwrap();
    let subject = hex::encode(JwtEnrollmentIssuer::from_seed(&[42; 32]).public_key());
    let out = f.0.join("node.jwt");
    let namespace = hex::encode([6; 32]);
    let peer = hex::encode(JwtEnrollmentIssuer::from_seed(&[43; 32]).public_key());
    // A root-issued persistent credential is never observed by an issuer.
    let result = f.run(&[
        "enroll",
        "node",
        "--subject",
        &subject,
        "--audience",
        "fleet",
        "--root-key",
        key.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--persistent",
        "--observations-namespace",
        &namespace,
        "--observations-secret-file",
        secret.to_str().unwrap(),
        "--fleet-namespace",
        &namespace,
        "--peer",
        &peer,
    ]);
    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("only an issuer"));
    assert!(!out.exists());
}

#[test]
fn namespace_create_writes_an_owner_only_secret_once() {
    let f = Fixture::new("namespace");
    let secret = f.0.join("build.secret");
    let created = f.run(&[
        "enroll",
        "namespace",
        "create",
        "--out-secret",
        secret.to_str().unwrap(),
    ]);
    assert!(created.status.success(), "{created:?}");
    let report: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let text = std::fs::read_to_string(&secret).unwrap();
    let bytes: [u8; 32] = hex::decode(text.trim()).unwrap().try_into().unwrap();
    assert_eq!(
        report["namespace"],
        hex::encode(aster::NamespaceSecret::from_bytes(bytes).id().to_bytes())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let again = f.run(&[
        "enroll",
        "namespace",
        "create",
        "--out-secret",
        secret.to_str().unwrap(),
    ]);
    assert!(!again.status.success());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), text);
}
