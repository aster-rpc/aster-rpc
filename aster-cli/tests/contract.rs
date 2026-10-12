//! `aster contract gen` against a stub interpreter that speaks the manifest
//! handoff, so the CLI side is tested without a Python installation.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let d =
            std::env::temp_dir().join(format!("aster-cli-contract-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    /// A fake interpreter: records its arguments, prints `stdout`, exits `code`.
    fn stub(&self, stdout: &str, stderr: &str, code: i32) -> PathBuf {
        let path = self.0.join("python");
        let script = format!(
            "#!/bin/sh\necho \"$@\" > '{}'\nprintf '%s' '{}'\nprintf '%s\\n' '{}' >&2\nexit {code}\n",
            self.0.join("args").display(),
            stdout.replace('\'', r"'\''"),
            stderr.replace('\'', r"'\''"),
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aster"))
            .args(args)
            .current_dir(&self.0)
            .env("ASTER_HOME", self.0.join("home"))
            .env_remove("ASTER_PYTHON")
            .output()
            .unwrap()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn manifest(service: &str) -> String {
    format!(
        r#"{{"v": 1, "service": "{service}", "version": 2, "contract_id": "ab12", "methods": [{{"name": "go", "pattern": "unary", "request_wire_tag": "x/Req", "response_wire_tag": "x/Resp"}}], "scoped": "shared", "description": "é", "vcs_revision": null}}"#
    )
}

#[test]
fn gen_writes_the_bindings_manifests_in_order() {
    let dir = Dir::new("ok");
    let python = dir.stub(
        &format!(
            r#"{{"aster_manifest_handoff": 1, "manifests": [{}, {}]}}"#,
            manifest("A"),
            manifest("B")
        ),
        "",
        0,
    );
    let out = dir.run(&[
        "contract",
        "gen",
        "--service",
        "pkg.mod:A",
        "--service",
        "pkg.mod:B",
        "--semver",
        "1.2.3",
        "--python",
        python.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let args = std::fs::read_to_string(dir.0.join("args")).unwrap();
    assert_eq!(
        args.trim(),
        "-m aster.contract gen --service pkg.mod:A --service pkg.mod:B --semver 1.2.3"
    );

    let written = std::fs::read_to_string(dir.0.join(".aster/manifest.json")).unwrap();
    assert!(
        written.starts_with("[\n  {\n    \"v\": 1,\n    \"service\": \"A\""),
        "{written}"
    );
    assert!(
        written.contains("\"description\": \"é\""),
        "non-ASCII kept readable"
    );
    assert!(
        written.contains("\"vcs_revision\": null"),
        "not a git checkout"
    );

    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["services"][1]["service"], "B");
    assert_eq!(report["services"][0]["methods"][0]["request"], "x/Req");

    let shown = dir.run(&["contract", "show"]);
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        text.contains("A v2") && text.contains("1 method ·") && text.contains("go      unary"),
        "{text}"
    );
}

#[test]
fn a_single_manifest_is_written_as_an_object() {
    let dir = Dir::new("single");
    let python = dir.stub(
        &format!(
            r#"{{"aster_manifest_handoff": 1, "manifests": [{}]}}"#,
            manifest("A")
        ),
        "",
        0,
    );
    let out = dir.run(&[
        "contract",
        "gen",
        "--service",
        "m:A",
        "--python",
        python.to_str().unwrap(),
        "--out",
        "x/m.json",
    ]);
    assert!(out.status.success());
    assert!(
        std::fs::read_to_string(dir.0.join("x/m.json"))
            .unwrap()
            .starts_with("{\n")
    );
}

#[test]
fn binding_errors_and_unknown_handoff_versions_are_reported() {
    let dir = Dir::new("err");
    let python = dir.stub(
        "",
        "Traceback...\nValueError: class A is not decorated with @service",
        1,
    );
    let out = dir.run(&[
        "contract",
        "gen",
        "--service",
        "m:A",
        "--python",
        python.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not decorated with @service"));

    let python = dir.stub(r#"{"aster_manifest_handoff": 2, "manifests": []}"#, "", 0);
    let out = dir.run(&[
        "contract",
        "gen",
        "--service",
        "m:A",
        "--python",
        python.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unsupported aster_manifest_handoff version")
    );
    assert!(!Path::new(&dir.0.join(".aster/manifest.json")).exists());

    let missing = dir.run(&["contract", "show", "nope.json"]);
    assert_eq!(missing.status.code(), Some(3));
}

#[test]
fn client_hands_off_to_the_language_binding() {
    let dir = Dir::new("client");
    std::fs::write(dir.0.join("svc.aster.json"), manifest("Mission")).unwrap();
    let envelope = r#"{"aster_codegen_handoff": 1, "files": ["gen/svc/a.py", "gen/svc/b.py"], "usage": "\nUsage: here"}"#;
    let python = dir.stub(envelope, "", 0);
    let out = dir.run(&[
        "contract",
        "client",
        "svc.aster.json",
        "--lang",
        "python",
        "--out",
        "gen",
        "--python",
        python.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let args = std::fs::read_to_string(dir.0.join("args")).unwrap();
    assert_eq!(
        args.trim(),
        "-m aster.codegen client --manifest svc.aster.json --out gen --namespace svc"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Generated 2 Python files for package `svc`") && text.contains("Usage: here"),
        "{text}"
    );

    // TypeScript: ASTER_TS_GEN names the command; the default manifest name
    // takes the package name from the first service.
    std::fs::create_dir_all(dir.0.join(".aster")).unwrap();
    std::fs::write(
        dir.0.join(".aster/manifest.json"),
        manifest("Mission Control!"),
    )
    .unwrap();
    let ts = dir.stub(envelope, "", 0);
    let out = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args([
            "contract",
            "client",
            "--lang",
            "typescript",
            "--out",
            "gen",
            "--json",
        ])
        .current_dir(&dir.0)
        .env("ASTER_TS_GEN", format!("{} --flag", ts.display()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let args = std::fs::read_to_string(dir.0.join("args")).unwrap();
    assert_eq!(
        args.trim(),
        "--flag client --manifest .aster/manifest.json --out gen --namespace Mission_Control"
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["files"].as_array().unwrap().len(), 2);
}

#[test]
fn typescript_needs_a_local_aster_gen() {
    let dir = Dir::new("nots");
    std::fs::write(dir.0.join("m.json"), manifest("A")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args([
            "contract",
            "client",
            "m.json",
            "--lang",
            "typescript",
            "--out",
            "gen",
        ])
        .current_dir(&dir.0)
        .env_remove("ASTER_TS_GEN")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no `aster-gen` in node_modules/.bin"));

    let bin = dir.0.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stub = dir.stub(
        r#"{"aster_codegen_handoff": 1, "files": [], "usage": ""}"#,
        "",
        0,
    );
    std::fs::rename(stub, bin.join("aster-gen")).unwrap();
    std::fs::create_dir_all(dir.0.join("sub")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aster"))
        .args([
            "contract",
            "client",
            "../m.json",
            "--lang",
            "typescript",
            "--out",
            "gen",
        ])
        .current_dir(dir.0.join("sub"))
        .env_remove("ASTER_TS_GEN")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "found in a parent directory: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
