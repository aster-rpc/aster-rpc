//! End-to-end conventions: exit codes, `--json`, colour, prompts, docs.
//! Every run uses its own `ASTER_HOME` and the file key store under it, with
//! a test credential-store service as a second guard: nothing touches the
//! real `~/.aster` or Keychain entries.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("aster-cli-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .env("ASTER_HOME", &self.0)
            .env_remove("NO_COLOR")
            .env_remove("ASTER_PROFILE")
            .env("ASTER_KEYSTORE", "file")
            .env("ASTER_KEYRING_SERVICE", "aster-cli-test")
            .env_remove("CLICOLOR_FORCE")
            .stdin(Stdio::null())
            .output()
            .expect("run aster")
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(&[args, &["--json"]].concat());
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON value")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn profile_lifecycle_in_json() {
    let home = Home::new("lifecycle");
    assert_eq!(
        home.json(&["profile", "list"])["profiles"],
        serde_json::json!([])
    );
    assert_eq!(home.json(&["profile", "create", "dev"])["active"], true);
    assert_eq!(home.json(&["profile", "create", "prod"])["active"], false);
    home.json(&["profile", "use", "prod"]);
    let show = home.json(&["profile", "show"]);
    assert_eq!(
        (show["name"].as_str(), show["active"].as_bool()),
        (Some("prod"), Some(true))
    );
    home.json(&["profile", "delete", "dev", "--yes"]);
    let names: Vec<_> = home.json(&["profile", "list"])["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["prod"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.0.join("config.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn human_output_is_plain_when_piped_and_coloured_on_request() {
    let home = Home::new("colour");
    home.run(&["profile", "create", "dev"]);
    let piped = home.run(&["profile", "list"]);
    assert!(piped.status.success());
    assert!(text(&piped.stdout).contains("dev") && !piped.stdout.contains(&0x1b));
    let forced = home.run(&["profile", "list", "--color", "always"]);
    assert!(forced.stdout.contains(&0x1b));
}

#[test]
fn not_found_has_exit_code_hint_and_json_form() {
    let home = Home::new("notfound");
    let human = home.run(&["profile", "show", "nope"]);
    assert_eq!(human.status.code(), Some(3));
    let err = text(&human.stderr);
    assert!(
        err.starts_with("error: no profile `nope`") && err.contains("hint:"),
        "{err}"
    );

    let json = home.run(&["profile", "show", "nope", "--json"]);
    assert_eq!(json.status.code(), Some(3));
    assert!(json.stdout.is_empty());
    let v: Value = serde_json::from_slice(&json.stderr).unwrap();
    assert_eq!(v["error"]["kind"], "not_found");
}

#[test]
fn confirmation_refuses_without_a_terminal() {
    let home = Home::new("prompt");
    home.run(&["profile", "create", "dev"]);
    let out = home.run(&["profile", "delete", "dev"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("pass --yes"));
    assert_eq!(
        home.json(&["profile", "list"])["profiles"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn usage_errors_exit_2_in_both_forms() {
    let home = Home::new("usage");
    let human = home.run(&["profile", "frobnicate"]);
    assert_eq!(human.status.code(), Some(2));
    assert!(text(&human.stderr).starts_with("error:"));

    let json = home.run(&["profile", "frobnicate", "--json"]);
    assert_eq!(json.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&json.stderr).unwrap();
    assert_eq!(v["error"]["kind"], "usage");

    let invalid = home.run(&["profile", "create", "Bad Name"]);
    assert_eq!(invalid.status.code(), Some(2));
}

#[test]
fn completions_and_reference_docs_cover_the_tree() {
    let home = Home::new("docs");
    let bash = home.run(&["completions", "bash"]);
    assert!(text(&bash.stdout).contains("profile"));

    let md = aster_cli_kit::docs::markdown(&aster_cli::command());
    for cmd in [
        "aster profile list",
        "aster profile delete",
        "aster mcp",
        "aster completions",
    ] {
        assert!(md.contains(&format!("`{cmd}`")), "missing {cmd}");
    }
    assert_eq!(
        md.matches("`--json`").count(),
        1,
        "global options listed once"
    );
    assert!(md.contains("Environment: `ASTER_HOME`"));
}

#[test]
fn migrates_a_python_cli_config_and_keeps_unknown_fields() {
    let home = Home::new("python");
    std::fs::create_dir_all(&home.0).unwrap();
    std::fs::write(
        home.0.join("config.toml"),
        r#"active_profile = "prod"

[aster_service]
enabled = true
node_id = "abc"
relay = ""
offline_banner = true

[profiles.prod]
root_pubkey = "00ff"
handle = "ops"
handle_status = "verified"
handle_claimed_at = ""
email = "ops@example.com"
signer = "local"
published_services = "svc-a,svc-b"
display_name = "Ops"

[profiles.dev]
root_pubkey = "11ee"

[shell]
first_time = false
"#,
    )
    .unwrap();

    let prod = home.json(&["profile", "show"]);
    assert_eq!(
        prod["handle"],
        serde_json::json!({"name": "ops", "status": "verified"})
    );
    assert_eq!(
        prod["published_services"],
        serde_json::json!(["svc-a", "svc-b"])
    );

    home.json(&["profile", "use", "dev"]);
    let written = std::fs::read_to_string(home.0.join("config.toml")).unwrap();
    for kept in [
        "version = 1",
        "published_services = [\"svc-a\", \"svc-b\"]",
        "display_name = \"Ops\"",
        "[shell]",
        "node_id = \"abc\"",
        "active_profile = \"dev\"",
    ] {
        assert!(written.contains(kept), "lost {kept}:\n{written}");
    }
}

#[test]
fn profile_flag_and_env_select_without_changing_the_active_profile() {
    let home = Home::new("select");
    home.run(&["profile", "create", "dev"]);
    home.run(&["profile", "create", "prod"]);
    assert_eq!(
        home.json(&["profile", "show", "--profile", "prod"])["name"],
        "prod"
    );
    let env = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["profile", "show", "--json"])
        .env("ASTER_HOME", &home.0)
        .env("ASTER_PROFILE", "prod")
        .env("ASTER_KEYSTORE", "file")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&env.stdout).unwrap();
    assert_eq!(
        (v["name"].as_str(), v["active"].as_bool()),
        (Some("prod"), Some(false))
    );
}

#[test]
fn root_key_lifecycle_never_replaces_a_trust_anchor() {
    let home = Home::new("rootkey");
    let generated = home.json(&["key", "root", "generate", "--profile", "ops"]);
    let pubkey = generated["root_pubkey"].as_str().unwrap().to_string();
    assert_eq!(pubkey.len(), 64);
    assert_eq!(
        home.json(&["profile", "show"])["name"],
        "ops",
        "first profile becomes active"
    );
    assert_eq!(home.json(&["key", "root", "show"])["stored"], true);

    // A second generate, or importing a different key, is refused.
    assert_eq!(
        home.run(&["key", "root", "generate"]).status.code(),
        Some(2)
    );
    let other = home.0.join("other.key");
    std::fs::write(&other, format!("{}\n", "11".repeat(32))).unwrap();
    assert_eq!(
        home.run(&["key", "root", "import", other.to_str().unwrap()])
            .status
            .code(),
        Some(2)
    );

    let backup = home.0.join("backup.key");
    let exported = home.json(&["key", "root", "export", "--out", backup.to_str().unwrap()]);
    assert_eq!(exported["root_pubkey"], pubkey.as_str());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let again = home.run(&["key", "root", "export", "--out", backup.to_str().unwrap()]);
    assert!(!again.status.success(), "never overwrites");

    // Without a terminal deletion needs --yes; the public key stays recorded.
    assert_eq!(home.run(&["key", "root", "delete"]).status.code(), Some(2));
    let after = home.json(&["key", "root", "delete", "--yes"]);
    assert_eq!(
        (after["stored"].as_bool(), after["root_pubkey"].as_str()),
        (Some(false), Some(pubkey.as_str()))
    );

    // Only the matching key comes back.
    assert_eq!(
        home.run(&["key", "root", "generate"]).status.code(),
        Some(2)
    );
    let imported = home.json(&["key", "root", "import", backup.to_str().unwrap()]);
    assert_eq!(imported["root_pubkey"], pubkey.as_str());
    assert_eq!(home.json(&["key", "root", "show"])["stored"], true);
}

#[test]
fn imports_python_json_export_from_stdin() {
    use std::io::Write;
    let home = Home::new("pyimport");
    let mut child = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args(["key", "root", "import", "-", "--profile", "dev", "--json"])
        .env("ASTER_HOME", &home.0)
        .env("ASTER_KEYSTORE", "file")
        .env("ASTER_KEYRING_SERVICE", "aster-cli-test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // RFC 8032 test vector 1: this seed's public key is d75a…511a.
    let seed = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    write!(
        child.stdin.take().unwrap(),
        r#"{{"private_key": "{seed}", "public_key": "x"}}"#
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["root_pubkey"],
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    );
}

#[test]
fn deleting_a_profile_removes_its_root_key() {
    let home = Home::new("profiledel");
    home.json(&["key", "root", "generate", "--profile", "dev"]);
    let key = home.0.join("keys/dev.root.key");
    assert!(key.exists());
    home.json(&["profile", "delete", "dev", "--yes"]);
    assert!(!key.exists());
}

#[test]
fn node_key_generation() {
    let home = Home::new("nodekey");
    std::fs::create_dir_all(&home.0).unwrap();
    let file = home.0.join("node.key");
    let v = home.json(&["key", "node", "generate", "--out", file.to_str().unwrap()]);
    assert_eq!(v["node_id"].as_str().unwrap().len(), 64);
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim().len(), 64);
    assert!(
        !home
            .run(&["key", "node", "generate", "--out", file.to_str().unwrap()])
            .status
            .success()
    );
}
