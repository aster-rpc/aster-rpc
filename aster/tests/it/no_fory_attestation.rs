//! INV-A20: the Fory ownership-attestation API is gone from core, the
//! facade, the C ABI and every binding. Delegation chains replace it
//! (docs/_internal/ownership-attestations.md). Source-level guard: it fails
//! if any removed symbol or the `attestation` Cargo feature reappears in
//! code. Comment lines (the C ABI changelog) may still name them.

use std::fs;
use std::path::{Path, PathBuf};

const REMOVED: &[&str] = &[
    // core and facade
    "attest_root_node",
    "AttestationChain",
    "AttestationEdge",
    "AttestationBody",
    "attestation::verify_chain",
    "aster::attestation",
    "crate::attestation",
    // C ABI
    "aster_attest_",
    "aster_attestation_",
    "aster_verified_chain_",
    // bindings
    "attestation_public_key",
    "attestationPublicKey",
    "attestRootNode",
    "AttestRootNode",
    "VerifyAttestationChain",
    "AttestationChainToText",
    "AttestationChainFromText",
    "site.aster.attestation",
    "Aster.Crypto.Attestation",
];

const SOURCE_EXTENSIONS: &[&str] = &["rs", "h", "c", "py", "pyi", "ts", "java", "kt", "go", "cs"];
const SKIPPED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "bin",
    "obj",
    "__pycache__",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .into()
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_ref()) && !name.starts_with('.') {
                sources(&path, out);
            }
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
        {
            out.push(path);
        }
    }
}

fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("//") || line.starts_with('#') || line.starts_with('*')
}

#[test]
fn no_fory_attestation_symbol_remains_in_code() {
    let root = repo();
    let mut files = Vec::new();
    for dir in [
        "core/src",
        "aster/src",
        "ffi/src",
        "ffi/aster_ffi.h",
        "bindings",
    ] {
        let path = root.join(dir);
        if path.is_file() {
            files.push(path);
        } else {
            sources(&path, &mut files);
        }
    }
    assert!(
        files.len() > 100,
        "the scan found too few sources: {}",
        files.len()
    );
    let mut found = Vec::new();
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            if is_comment(line) {
                continue;
            }
            for symbol in REMOVED {
                if line.contains(symbol) {
                    found.push(format!(
                        "{}:{}: {symbol}",
                        file.strip_prefix(&root).unwrap().display(),
                        number + 1
                    ));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "removed attestation API found:\n{}",
        found.join("\n")
    );
}

#[test]
fn no_attestation_cargo_feature_remains() {
    for manifest in ["core/Cargo.toml", "aster/Cargo.toml", "ffi/Cargo.toml"] {
        let text = fs::read_to_string(repo().join(manifest)).unwrap();
        assert!(
            !text
                .lines()
                .any(|line| line.trim_start().starts_with("attestation =")
                    || line.contains("\"attestation\"")),
            "{manifest} still declares or enables an `attestation` feature"
        );
    }
}
