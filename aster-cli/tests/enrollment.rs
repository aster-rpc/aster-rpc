//! CLI issuance uses isolated file keystores; never the developer's Keychain.
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use aster::enrollment::{JwtEnrollmentIssuer, JwtEnrollmentVerifier};

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
        std::fs::write(
            self.0.join("config.toml"),
            format!(
                "version = 1\n[profiles.default]\nroot_pubkey = \"{}\"\n",
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
fn issues_bound_jwt_from_explicit_key_and_never_overwrites() {
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
        "--no-expiry",
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
    assert!(report["expires_at"].is_null());
    let jwt = std::fs::read_to_string(&out).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
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
fn selected_profile_keystore_and_explicit_lifetime_are_required() {
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
    assert_eq!(f.run(&base).status.code(), Some(2));
    assert!(!out.exists());
    let result = f.run(&[base.as_slice(), &["--no-expiry"]].concat());
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
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
        "--no-expiry",
    ]);
    assert_eq!(result.status.code(), Some(4));
    assert!(!rejected_out.exists());
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
    let result = f.run(
        &[
            args.as_slice(),
            &["--no-expiry", "--attributes", attrs.to_str().unwrap()],
        ]
        .concat(),
    );
    assert_eq!(result.status.code(), Some(2));
    assert!(!out.exists());
    std::fs::write(
        &attrs,
        r#"{"role":{"type":"string","value":"reader"},"role":{"type":"string","value":"admin"}}"#,
    )
    .unwrap();
    let result = f.run(
        &[
            args.as_slice(),
            &["--no-expiry", "--attributes", attrs.to_str().unwrap()],
        ]
        .concat(),
    );
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
            "--no-expiry",
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
        "--no-expiry",
    ]);
    assert_eq!(result.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&result.stderr).contains("configured root public key"));
    assert!(!out.exists());
}
