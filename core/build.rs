use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=ASTER_BUILD_VERSION");

    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let repo = manifest_dir.parent().unwrap_or(&manifest_dir);
    let version = std::env::var("ASTER_BUILD_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| derive_version(repo))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());

    println!("cargo:rustc-env=ASTER_BUILD_VERSION={version}");
    println!("cargo:rerun-if-changed={}", repo.join("VERSION").display());
    println!(
        "cargo:rerun-if-changed={}",
        repo.join("BUILD_NUMBER").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo.join(".git/HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo.join(".git/refs/heads").display()
    );
}

fn derive_version(repo: &Path) -> Option<String> {
    let version_file = std::fs::read_to_string(repo.join("VERSION")).ok()?;
    let line = version_file
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))?;
    let mut fields = line.split_whitespace();
    let major_minor = fields.next()?;
    let offset: u64 = fields.next()?.parse().ok()?;

    let count = std::fs::read_to_string(repo.join("BUILD_NUMBER"))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .or_else(|| git_commit_count(repo))?;
    let patch = count.checked_sub(offset)?;
    Some(format!("{major_minor}.{patch}"))
}

fn git_commit_count(repo: &Path) -> Option<u64> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}
