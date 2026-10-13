//! `aster trust`: pre-commit a next root, keep it apart from the root,
//! rotate with it, and apply the statement on another profile. Every run uses
//! its own `ASTER_HOME` and the file key store under it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("aster-cli-trust-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .arg("--json")
            .env("ASTER_HOME", &self.0)
            .env_remove("ASTER_PROFILE")
            .env("ASTER_KEYSTORE", "file")
            .env("ASTER_KEYRING_SERVICE", "aster-cli-test")
            .stdin(Stdio::null())
            .output()
            .expect("run aster")
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON value")
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn commit_rotate_and_apply_a_root_rotation() {
    let operator = Home::new("operator");
    let init = operator.json(&["trust", "init"]);
    assert_eq!(init["generated_root"], true);
    let original = init["root_pubkey"].as_str().unwrap().to_owned();
    let commitment = init["next_root_commitment"].as_str().unwrap().to_owned();
    assert_eq!(init["next_root_key_stored"], true);
    // A commitment is never replaced.
    operator.fails(&["trust", "init"]);

    // The next root leaves this machine.
    let offline = operator.path("next-root.key");
    operator.json(&["trust", "next", "export", "--out", s(&offline)]);
    operator.json(&["trust", "next", "delete", "--yes"]);
    assert_eq!(
        operator.json(&["trust", "show"])["next_root_key_stored"],
        false
    );

    // A node profile pins the same root and commitment.
    let node = Home::new("node");
    aster_config::update(&node.0, |cfg| -> anyhow::Result<()> {
        let profile = cfg.profiles.entry("default".into()).or_default();
        profile.root_pubkey = Some(original.clone());
        profile.next_root_commitment = Some(commitment.clone());
        Ok(())
    })
    .unwrap();

    let fleet = "ab".repeat(32);
    let statement = operator.path("rotation.jwt");
    let issue = [
        "trust",
        "rotate",
        "--issue",
        "--fleet-namespace",
        &fleet,
        "--reason",
        "compromise",
        "--out",
        s(&statement),
    ];
    // Rotating needs the committed key, and consent to replace the root key.
    operator.fails(&issue);
    let wrong = operator.path("wrong.key");
    std::fs::write(&wrong, "11".repeat(32)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrong, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    operator.fails(&["trust", "next", "import", s(&wrong)]);
    operator.json(&["trust", "next", "import", s(&offline)]);
    operator.fails(&issue);
    assert!(!statement.exists(), "nothing is written before consent");

    let rotated = operator.json(&[&issue[..], &["--delete-previous"]].concat());
    let new_root = rotated["root_pubkey"].as_str().unwrap().to_owned();
    assert_ne!(new_root, original);
    assert_eq!(rotated["previous_root"], original);
    assert_eq!(rotated["root_seq"], 1);
    assert_eq!(rotated["fleet_namespace"], fleet);
    let show = operator.json(&["trust", "show"]);
    assert_eq!(show["root_pubkey"], new_root);
    assert_ne!(show["next_root_commitment"], commitment);
    assert_eq!(show["next_root_key_stored"], true);
    assert_eq!(show["root_key_stored"], true);

    // The node applies the statement once; a replay is not the next
    // rotation, and a tampered statement never verifies.
    let mut tampered = std::fs::read_to_string(&statement).unwrap();
    tampered.insert(tampered.len() - 3, 'A');
    let tampered_file = node.path("tampered.jwt");
    std::fs::write(&tampered_file, tampered).unwrap();
    node.fails(&["trust", "rotate", "--apply", s(&tampered_file)]);
    let applied = node.json(&["trust", "rotate", "--apply", s(&statement)]);
    assert_eq!(applied["root_pubkey"], new_root);
    assert_eq!(applied["root_seq"], 1);
    let profile = &aster_config::load(&node.0).unwrap().profiles["default"];
    assert_eq!(profile.root_pubkey.as_deref(), Some(new_root.as_str()));
    assert_eq!(
        profile.exec_policy_namespace.as_deref(),
        Some(fleet.as_str())
    );
    assert_eq!(profile.root_seq, 1);
    node.fails(&["trust", "rotate", "--apply", s(&statement)]);
}
