#!/usr/bin/env python3
"""Publish Aster's pinned Iroh/Noq/Salvo source closure to Forgejo.

Fork code is checked out at the exact revisions in iroh-fork-manifest.toml.
Unmodified bridge crates are downloaded from crates.io and checksum-verified.
Only Cargo dependency-source metadata is rewritten in a temporary directory;
the source repositories and crates.io archives are never edited in place.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from typing import Any
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.9/3.10 in supported maintainer envs.
    import tomli as tomllib  # type: ignore[no-redef]


ROOT = Path(__file__).resolve().parents[2]
MANIFEST_PATH = ROOT / "iroh-fork-manifest.toml"
REGISTRY_INDEX = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
REGISTRY_HTTP = "https://forge.emrul.dev/api/packages/Aster/cargo"
CRATES_IO_API = "https://crates.io/api/v1/crates"
USER_AGENT = "aster-native-stack-publisher/1"


@dataclasses.dataclass(frozen=True)
class Package:
    name: str
    version: str
    kind: str
    source: str
    manifest: str | None = None


# Dependency order is part of the release contract. Vendor packages are copied
# out of the Salvo workspace because upstream deliberately excludes vendor/ as
# workspace members.
PACKAGES = [
    Package("noq-proto", "1.0.1", "fork", "noq", "noq-proto/Cargo.toml"),
    Package("noq-udp", "1.0.1", "fork", "noq", "noq-udp/Cargo.toml"),
    Package("noq", "1.0.1", "fork", "noq", "noq/Cargo.toml"),
    Package("irpc", "0.17.0", "bridge", "irpc"),
    Package("netwatch", "0.19.1", "bridge", "netwatch"),
    Package("portmapper", "0.19.1", "bridge", "portmapper"),
    Package("iroh-base", "1.0.1", "fork", "iroh", "iroh-base/Cargo.toml"),
    Package("iroh-dns", "1.0.1", "fork", "iroh", "iroh-dns/Cargo.toml"),
    Package("iroh-relay", "1.0.1", "fork", "iroh", "iroh-relay/Cargo.toml"),
    Package("iroh", "1.0.1", "fork", "iroh", "iroh/Cargo.toml"),
    Package("iroh-mdns-address-lookup", "0.4.0", "bridge", "iroh-mdns-address-lookup"),
    Package("iroh-tickets", "1.0.0", "bridge", "iroh-tickets"),
    Package("iroh-util", "0.6.0", "bridge", "iroh-util"),
    Package("iroh-gossip", "0.101.0", "fork", "iroh-gossip", "Cargo.toml"),
    Package("iroh-blobs", "0.103.1", "fork", "iroh-blobs", "Cargo.toml"),
    Package("iroh-docs", "0.101.0", "fork", "iroh-docs", "Cargo.toml"),
    Package("h3", "0.0.8", "vendor", "salvo", "vendor/h3/Cargo.toml"),
    Package("h3-datagram", "0.0.2", "bridge", "h3-datagram"),
    Package("h3-quinn", "0.0.10", "vendor", "salvo", "vendor/h3-quinn/Cargo.toml"),
    Package("salvo-http3", "0.7.0", "vendor", "salvo", "vendor/salvo-http3/Cargo.toml"),
    Package("salvo-serde-util", "0.93.0", "fork", "salvo", "crates/serde-util/Cargo.toml"),
    Package("salvo_macros", "0.93.0", "fork", "salvo", "crates/macros/Cargo.toml"),
    Package("salvo_core", "0.93.0", "fork", "salvo", "crates/core/Cargo.toml"),
    Package("salvo-acme", "0.93.0", "fork", "salvo", "crates/acme/Cargo.toml"),
    Package("salvo-proxy", "0.93.0", "fork", "salvo", "crates/proxy/Cargo.toml"),
    Package("salvo-serve-static", "0.93.0", "fork", "salvo", "crates/serve-static/Cargo.toml"),
    Package("salvo", "0.93.0", "fork", "salvo", "crates/salvo/Cargo.toml"),
]

PACKAGE_VERSIONS = {package.name: package.version for package in PACKAGES}
PACKAGE_NAMES = set(PACKAGE_VERSIONS)


def run(
    args: list[str],
    *,
    cwd: Path | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    print("+ " + " ".join(args), flush=True)
    return subprocess.run(
        args,
        cwd=cwd,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    )


def get_json(url: str) -> Any:
    request = Request(url, headers={"User-Agent": USER_AGENT})
    with urlopen(request, timeout=30) as response:
        return json.load(response)


def index_path(name: str) -> str:
    name = name.lower()
    if len(name) == 1:
        return f"1/{name}"
    if len(name) == 2:
        return f"2/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def is_published(name: str, version: str) -> bool:
    request = Request(
        f"{REGISTRY_HTTP}/{index_path(name)}",
        headers={"User-Agent": USER_AGENT},
    )
    try:
        with urlopen(request, timeout=30) as response:
            lines = response.read().decode().splitlines()
    except HTTPError as error:
        if error.code == 404:
            return False
        raise
    return any(json.loads(line).get("vers") == version for line in lines if line)


def wait_until_published(name: str, version: str) -> None:
    for _ in range(30):
        if is_published(name, version):
            return
        time.sleep(2)
    raise RuntimeError(f"timed out waiting for {name} {version} in sparse index")


def load_release_manifest() -> dict[str, Any]:
    with MANIFEST_PATH.open("rb") as manifest_file:
        manifest = tomllib.load(manifest_file)
    expected = {
        name
        for fork in manifest["forks"].values()
        for name in fork.get("registry_crates", [])
    }
    expected.update(manifest["bridges"])
    actual = {package.name for package in PACKAGES}
    if actual != expected:
        raise RuntimeError(
            "publisher/BOM mismatch: "
            f"missing={sorted(expected - actual)}, stale={sorted(actual - expected)}"
        )
    for package in PACKAGES:
        if package.kind == "bridge":
            recorded = manifest["bridges"][package.source]["version"]
        else:
            recorded = manifest["forks"][package.source]["crate_version"]
            if package.source == "salvo" and package.name in {
                "h3",
                "h3-quinn",
                "salvo-http3",
            }:
                recorded = package.version
        if recorded != package.version:
            raise RuntimeError(
                f"version mismatch for {package.name}: publisher={package.version}, "
                f"BOM={recorded}"
            )
    return manifest


DEPENDENCY_SECTION = re.compile(
    r"^(?:.*\.)?(?:workspace\.)?(?:dev-|build-)?dependencies$"
)
DEPENDENCY_TABLE = re.compile(
    r"^(?:.*\.)?(?:dev-|build-)?dependencies\.([A-Za-z0-9_-]+)$"
)
ENTRY = re.compile(
    r"^(?P<indent>\s*)(?P<key>[A-Za-z0-9_-]+)(?P<equals>\s*=\s*)"
    r"(?P<value>.*?)(?P<newline>\r?\n?)$"
)


def add_registry_to_value(value: str, actual_name: str) -> str:
    stripped = value.strip()
    if "registry" in stripped or "workspace" in stripped:
        return value
    if stripped.startswith(('"', "'")):
        return f'{{ version = {stripped}, registry = "aster" }}'
    if not (stripped.startswith("{") and stripped.endswith("}")):
        raise RuntimeError(f"unsupported dependency syntax for {actual_name}: {value}")
    inner = stripped[1:-1].strip()
    if "version" not in inner:
        version = PACKAGE_VERSIONS[actual_name]
        inner += (", " if inner else "") + f'version = "={version}"'
    inner += (", " if inner else "") + 'registry = "aster"'
    return "{ " + inner + " }"


def patch_manifest(path: Path) -> None:
    lines = path.read_text().splitlines(keepends=True)
    output: list[str] = []
    section = ""
    index = 0
    while index < len(lines):
        line = lines[index]
        header = re.match(r"^\s*\[([^]]+)]\s*(?:#.*)?(?:\r?\n)?$", line)
        if header:
            section = header.group(1)
            if section.startswith("patch."):
                index += 1
                while index < len(lines) and not re.match(r"^\s*\[", lines[index]):
                    index += 1
                continue

            table = DEPENDENCY_TABLE.match(section)
            if table:
                end = index + 1
                while end < len(lines) and not re.match(r"^\s*\[", lines[end]):
                    end += 1
                block = lines[index:end]
                actual_name = table.group(1)
                for block_line in block[1:]:
                    package_match = re.match(
                        r'^\s*package\s*=\s*["\']([^"\']+)["\']', block_line
                    )
                    if package_match:
                        actual_name = package_match.group(1)
                        break
                if actual_name in PACKAGE_NAMES:
                    joined = "".join(block)
                    if "workspace = true" not in joined and not re.search(
                        r"(?m)^\s*registry\s*=", joined
                    ):
                        additions = ['registry = "aster"\n']
                        if not re.search(r"(?m)^\s*version\s*=", joined):
                            additions.insert(
                                0, f'version = "={PACKAGE_VERSIONS[actual_name]}"\n'
                            )
                        block[1:1] = additions
                output.extend(block)
                index = end
                continue

            output.append(line)
            index += 1
            continue

        if DEPENDENCY_SECTION.match(section):
            match = ENTRY.match(line)
            if match:
                key = match.group("key")
                value = match.group("value")
                stripped = value.strip()
                if stripped.startswith("{") and stripped.count("{") > stripped.count("}"):
                    end = index + 1
                    balance = stripped.count("{") - stripped.count("}")
                    while end < len(lines) and balance > 0:
                        balance += lines[end].count("{") - lines[end].count("}")
                        end += 1
                    if balance != 0:
                        raise RuntimeError(f"unclosed dependency table in {path}: {key}")
                    joined = line + "".join(lines[index + 1 : end])
                    package_match = re.search(
                        r'\bpackage\s*=\s*["\']([^"\']+)["\']', joined
                    )
                    actual_name = package_match.group(1) if package_match else key
                    if (
                        actual_name in PACKAGE_NAMES
                        and "workspace = true" not in joined
                        and "registry" not in joined
                    ):
                        additions = ' registry = "aster",'
                        if not re.search(r"\bversion\s*=", joined):
                            additions += f' version = "={PACKAGE_VERSIONS[actual_name]}",'
                        brace = line.index("{") + 1
                        line = line[:brace] + additions + line[brace:]
                    output.append(line)
                    output.extend(lines[index + 1 : end])
                    index = end
                    continue
                package_match = re.search(r'\bpackage\s*=\s*["\']([^"\']+)["\']', value)
                actual_name = package_match.group(1) if package_match else key
                if actual_name in PACKAGE_NAMES:
                    value = add_registry_to_value(value, actual_name)
                    line = (
                        match.group("indent")
                        + key
                        + match.group("equals")
                        + value
                        + match.group("newline")
                    )
        output.append(line)
        index += 1
    path.write_text("".join(output))


def patch_tree(root: Path) -> None:
    for manifest_path in root.rglob("Cargo.toml"):
        if any(part in {"target", ".git"} for part in manifest_path.parts):
            continue
        patch_manifest(manifest_path)


def clone_fork(
    manifest: dict[str, Any], source: str, destination: Path
) -> Path:
    fork = manifest["forks"][source]
    run(["git", "clone", "--quiet", "--filter=blob:none", "--no-checkout", fork["repo"], str(destination)])
    run(["git", "checkout", "--quiet", "--detach", fork["commit"]], cwd=destination)
    revision = run(["git", "rev-parse", "HEAD"], cwd=destination, capture=True).stdout.strip()
    if revision != fork["commit"]:
        raise RuntimeError(f"{source} checkout mismatch: {revision}")
    patch_tree(destination)
    return destination


def safe_extract(archive: Path, destination: Path) -> None:
    destination_resolved = destination.resolve()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            target = (destination / member.name).resolve()
            if destination_resolved not in target.parents and target != destination_resolved:
                raise RuntimeError(f"unsafe archive path: {member.name}")
            if member.issym() or member.islnk():
                raise RuntimeError(f"bridge archive contains link: {member.name}")
        tar.extractall(destination)


def download_bridge(package: Package, destination: Path) -> Path:
    metadata = get_json(
        f"{CRATES_IO_API}/{quote(package.name)}/{quote(package.version)}"
    )
    checksum = metadata["version"]["checksum"]
    request = Request(
        f"{CRATES_IO_API}/{quote(package.name)}/{quote(package.version)}/download",
        headers={"User-Agent": USER_AGENT},
    )
    archive = destination / f"{package.name}-{package.version}.crate"
    with urlopen(request, timeout=60) as response, archive.open("wb") as output:
        shutil.copyfileobj(response, output)
    actual_checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    if actual_checksum != checksum:
        raise RuntimeError(
            f"checksum mismatch for {package.name} {package.version}: "
            f"expected {checksum}, got {actual_checksum}"
        )
    extract_root = destination / f"extract-{package.name}"
    extract_root.mkdir()
    safe_extract(archive, extract_root)
    package_root = extract_root / f"{package.name}-{package.version}"
    # Cargo adds these files while creating a .crate archive and reserves their
    # names. They describe the first packaging operation, are not upstream
    # source, and must not be included when the verified archive is repackaged.
    for generated in ("Cargo.toml.orig", ".cargo_vcs_info.json", ".cargo-ok"):
        (package_root / generated).unlink(missing_ok=True)
    patch_tree(package_root)
    return package_root / "Cargo.toml"


def validate_package(package: Package, manifest_path: Path) -> None:
    metadata_result = run(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
            str(manifest_path),
        ],
        cwd=manifest_path.parent,
        capture=True,
    )
    metadata = json.loads(metadata_result.stdout)
    matches = [item for item in metadata["packages"] if item["name"] == package.name]
    if len(matches) != 1 or matches[0]["version"] != package.version:
        raise RuntimeError(f"could not identify {package.name} {package.version}")
    errors = []
    for dependency in matches[0]["dependencies"]:
        if dependency["name"] not in PACKAGE_NAMES:
            continue
        if dependency.get("kind") == "dev":
            continue
        registry = dependency.get("registry") or ""
        if "/api/packages/Aster/cargo/" not in registry:
            errors.append(dependency["name"])
    if errors:
        raise RuntimeError(
            f"{package.name} dependencies lack Aster registry metadata: "
            + ", ".join(sorted(set(errors)))
        )
    # Cargo reserves these names for its own archive metadata. Check them
    # directly: even `cargo package --list` resolves alternate-registry
    # dependencies, so it cannot validate a first dependency wave in advance.
    reserved = [
        name
        for name in ("Cargo.toml.orig", ".cargo_vcs_info.json", ".cargo-ok")
        if (manifest_path.parent / name).exists()
    ]
    if reserved:
        raise RuntimeError(
            f"{package.name} contains Cargo-reserved package files: "
            + ", ".join(reserved)
        )


def publish_package(package: Package, manifest_path: Path) -> None:
    common = [
        "cargo",
        "publish",
        "--registry",
        "aster",
        "--allow-dirty",
        "--manifest-path",
        str(manifest_path),
    ]
    run(common + ["--dry-run"], cwd=manifest_path.parent)
    run(common, cwd=manifest_path.parent)
    wait_until_published(package.name, package.version)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("check", "publish"))
    args = parser.parse_args()
    if args.mode == "publish" and not os.environ.get("CARGO_REGISTRIES_ASTER_TOKEN"):
        raise SystemExit("CARGO_REGISTRIES_ASTER_TOKEN is required for publication")

    manifest = load_release_manifest()
    with tempfile.TemporaryDirectory(prefix="aster-native-publish-") as temporary:
        staging = Path(temporary)
        config_dir = staging / ".cargo"
        config_dir.mkdir()
        (config_dir / "config.toml").write_text(
            "[registries.aster]\n"
            f'index = "{REGISTRY_INDEX}"\n'
            'credential-provider = "cargo:token"\n'
        )
        fork_roots: dict[str, Path] = {}
        bridge_manifests: dict[str, Path] = {}
        vendor_manifests: dict[str, Path] = {}

        for package in PACKAGES:
            if args.mode == "publish" and is_published(package.name, package.version):
                print(f"= {package.name} {package.version} already published; skipping")
                continue
            if package.kind == "bridge":
                manifest_path = bridge_manifests.get(package.name)
                if manifest_path is None:
                    manifest_path = download_bridge(package, staging)
                    bridge_manifests[package.name] = manifest_path
            else:
                fork_root = fork_roots.get(package.source)
                if fork_root is None:
                    fork_root = clone_fork(
                        manifest, package.source, staging / f"fork-{package.source}"
                    )
                    fork_roots[package.source] = fork_root
                assert package.manifest is not None
                source_manifest = fork_root / package.manifest
                if package.kind == "vendor":
                    manifest_path = vendor_manifests.get(package.name)
                    if manifest_path is None:
                        package_root = source_manifest.parent
                        standalone = staging / f"vendor-{package.name}"
                        shutil.copytree(package_root, standalone)
                        patch_tree(standalone)
                        manifest_path = standalone / "Cargo.toml"
                        vendor_manifests[package.name] = manifest_path
                else:
                    manifest_path = source_manifest
            validate_package(package, manifest_path)
            if args.mode == "publish":
                publish_package(package, manifest_path)
            else:
                print(f"✓ {package.name} {package.version}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
