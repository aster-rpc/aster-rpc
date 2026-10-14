#!/usr/bin/env python3
"""Record, seal and retrieve an exact-commit release candidate. No test retries.

Forgejo 15 has a run API but no cross-run artifact REST API. Job handoff uses
workflow artifacts; the successful candidate is retained as an immutable
generic package, keyed by full SHA and run ID. No release credential is used
by this module. Network reads fail closed, including 404 and permission errors.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timedelta, timezone
import glob
import hashlib
import json
import os
import platform
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from urllib.parse import urlencode
from urllib.error import HTTPError
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "Aster/aster-rpc-internal"
WORKFLOW = "linux.yml"
# A push to main runs only the gate jobs of linux.yml and never seals. The
# release candidate is the workflow_dispatch run on main for the exact SHA
# (scripts/release.sh). Gate push runs of the same SHA are never candidates.
CANDIDATE_EVENT = "workflow_dispatch"
PACKAGE = "aster-ci-candidate"
RETENTION_DAYS = 30
FFI_ARCHIVE_CONSUMER_TASKS = frozenset(
    "ffi-archive-consumer-" + rid
    for rid in ("linux-x64", "linux-arm64", "win-x64", "osx-arm64")
)
POLICY_PATH = ROOT / "scripts/ci/release-candidate-policy.json"
BOUND_FILES = (".forgejo/workflows/linux.yml", "Cargo.lock", "iroh-fork-manifest.toml", "VERSION", "scripts/ci/release-candidate-policy.json")


def fail(message: str) -> None:
    raise ValueError(message)


def digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True).strip()


def source_hashes() -> dict[str, str]:
    # Build stamping changes Cargo.lock in place; bind the original committed
    # inputs, not that stamped copy. Version and exact workflow are also bound.
    return {name: hashlib.sha256(subprocess.check_output(["git", "-C", str(ROOT), "show", f"HEAD:{name}"])).hexdigest() for name in BOUND_FILES}


def assurance_profile(policy: dict) -> str:
    profile = policy.get("assurance_profile")
    if profile not in ("standard", "full"):
        fail("release policy assurance_profile must be standard or full")
    return profile


def expected_gates(policy: dict) -> dict[str, str]:
    profile = assurance_profile(policy)
    required = policy["required_jobs"]
    optional = policy.get("optional_assurance_jobs", [])
    if set(required) & set(optional) or len(set(required + optional)) != len(required + optional):
        fail("release policy has overlapping or duplicate jobs")
    return {**{job: "success" for job in required},
            **{job: "success" if profile == "full" else "skipped" for job in optional}}


def artifact_policy(policy: dict) -> dict:
    return {**policy["required_artifacts"],
            **(policy.get("full_assurance_artifacts", {}) if assurance_profile(policy) == "full" else {})}


def identity() -> dict:
    sha = git("rev-parse", "HEAD")
    if os.environ.get("FORGEJO_SHA", sha) != sha:
        fail("checkout SHA differs from the workflow SHA")
    version = subprocess.check_output([str(ROOT / "scripts/release/version.sh")], text=True).strip()
    return {"schema": 1, "repository": REPOSITORY, "sha": sha, "version": version,
            "workflow": WORKFLOW, "source_hashes": source_hashes(),
            "assurance_profile": assurance_profile(read_json(POLICY_PATH)),
            "run_id": int(os.environ["FORGEJO_RUN_ID"]),
            "run_number": int(os.environ["FORGEJO_RUN_NUMBER"])}


def safe_name(name: str) -> None:
    if not re.fullmatch(r"[a-zA-Z0-9_.-]+", name) or name in (".", ".."):
        fail(f"invalid artifact name: {name}")


def file_map(root: Path) -> dict[str, str]:
    found = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            fail(f"symlinks are not candidate artifacts: {path}")
        if path.is_file():
            found[path.relative_to(root).as_posix()] = digest(path)
    if not found:
        fail(f"empty artifact: {root}")
    return found


def record(artifact: str, path_pattern: str, output: Path, job: str, matrix: str, recipe: dict | None = None) -> None:
    if os.environ.get("FORGEJO_RUN_ATTEMPT") != "1":
        fail("candidate provenance must come from the first job attempt")
    safe_name(artifact)
    pattern = ROOT / path_pattern
    paths = [Path(p) for p in glob.glob(str(pattern))]
    if not paths:
        fail(f"artifact selection is empty: {path_pattern}")
    # upload-artifact preserves paths relative to the first wildcard's parent,
    # or to the selected directory itself. All production selections are one
    # directory or one file glob; reject more general patterns.
    if len(paths) == 1 and paths[0].is_dir():
        base = paths[0]
    else:
        base = pattern.parent
        if glob.has_magic(str(base)):
            fail("artifact glob must have a fixed parent directory")
    files = {}
    for path in paths:
        selected = file_map(path) if path.is_dir() else {path.name: digest(path)}
        if path.is_dir() and path != base:
            selected = {f"{path.relative_to(base).as_posix()}/{p}": h for p, h in selected.items()}
        if set(files) & set(selected):
            fail("duplicate artifact file")
        files.update(selected)
    tools = {}
    for command in (("rustc", "--version"), ("cargo", "--version"), ("python3", "--version"), ("node", "--version"), ("bun", "--version"), ("go", "version"), ("dotnet", "--version"), ("java", "-version")):
        if shutil.which(command[0]):
            result = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=True)
            tools[command[0]] = result.stdout.strip()
    evidence = {**identity(), "artifact": artifact, "job": job, "matrix": json.loads(matrix) or {}, "attempt": 1,
                "tools": tools, "files": files,
                "build_environment": {k: os.environ.get(k, "") for k in ("RUSTUP_TOOLCHAIN", "RUSTFLAGS", "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_RELEASE_DEBUG")}}
    if recipe is not None:
        validate_recipe(recipe)
        if recipe["lock_sha256"] != evidence["source_hashes"]["Cargo.lock"]:
            fail("recorded native recipe differs from committed lock inputs")
        evidence["build_recipe"] = recipe
    output.mkdir(parents=True, exist_ok=True)
    (output / f"{artifact}.json").write_text(json.dumps(evidence, indent=2) + "\n")


def read_json(path: Path):
    return json.loads(path.read_text())


def validate_recipe(recipe: dict) -> None:
    base_keys = {"schema", "kind", "package", "profile", "default_features", "features", "target", "target_mode",
                 "rustc", "cargo", "lock_sha256", "environment", "host_runtime"}
    python_keys = {"python", "python_abi", "maturin"} if recipe.get("kind") == "python-wheel" else set()
    napi_keys = {"generator"} if recipe.get("kind") == "napi" else set()
    if set(recipe) != base_keys | python_keys | napi_keys or recipe.get("schema") != 1:
        fail("native recipe schema/inventory mismatch")
    if (recipe["kind"], recipe["package"]) not in (("python-wheel", "aster_rs"), ("ffi", "aster_transport_ffi"), ("napi", "aster-transport-napi")):
        fail("unsupported reusable native product")
    mode = "explicit" if napi_keys else "host"
    if recipe["profile"] != "release" or recipe["default_features"] is not True or recipe["target_mode"] != mode:
        fail("reuse currently supports only default-feature release host products")
    expected_features = ["pyo3/extension-module"] if python_keys else []
    if recipe["features"] != expected_features or not re.fullmatch(r"[a-z0-9_]+-[a-z0-9_-]+", recipe["target"]):
        fail("native recipe feature/target mismatch")
    if not all(isinstance(recipe[key], str) and recipe[key] for key in ("rustc", "cargo", "lock_sha256")):
        fail("native recipe lacks compiler/lock evidence")
    if not re.fullmatch(r"[0-9a-f]{64}", recipe["lock_sha256"]) or "host: " + recipe["target"] not in recipe["rustc"].splitlines():
        fail("native recipe compiler host/lock mismatch")
    if not isinstance(recipe["environment"], dict):
        fail("native recipe lacks build environment")
    if not isinstance(recipe["host_runtime"], dict) or set(recipe["host_runtime"]) != {"machine", "system", "libc"}:
        fail("native recipe lacks host runtime")
    if python_keys and (recipe["python_abi"] != "abi3-py39" or not isinstance(recipe["python"], dict)
                        or set(recipe["python"]) != {"implementation", "version", "cache_tag", "machine", "libc"}
                        or recipe["python"]["implementation"] != "cpython" or not recipe["maturin"]):
        fail("native recipe lacks Python ABI/interpreter evidence")
    if napi_keys:
        generator = recipe["generator"]
        if recipe["target"] != "x86_64-unknown-linux-gnu" or not isinstance(generator, dict) or set(generator) != {"node", "bun", "cli", "cli_sha256", "inputs"}:
            fail("NAPI reuse requires explicit Linux host and pinned generator evidence")
        if not all(isinstance(generator[key], str) and generator[key] for key in ("node", "bun", "cli")):
            fail("NAPI recipe lacks generator versions")
        if not re.fullmatch(r"[0-9a-f]{64}", generator["cli_sha256"]):
            fail("NAPI recipe lacks generator code digest")
        inputs = generator["inputs"]
        required = {"bindings/typescript/bun.lock", "bindings/typescript/package.json", "bindings/typescript/native/package.json", "bindings/typescript/scripts/check-native-generated.mjs", "scripts/ci/napi-loader.mjs"}
        if not isinstance(inputs, dict) or set(inputs) != required or not all(re.fullmatch(r"[0-9a-f]{64}", value) for value in inputs.values()):
            fail("NAPI recipe generator input inventory mismatch")


def build_recipe(kind: str, package: str, profile: str, features: str,
                 target: str, interpreter: str | None, maturin: str | None) -> dict:
    """Describe one native build command, not the workspace's unified test graph."""
    if (kind, package) not in (("python-wheel", "aster_rs"), ("ffi", "aster_transport_ffi"), ("napi", "aster-transport-napi")):
        fail("unsupported reusable native product")
    rustc = subprocess.check_output(["rustc", "-vV"], text=True).strip()
    host = next(line.removeprefix("host: ") for line in rustc.splitlines() if line.startswith("host: "))
    resolved_target = host if target == "host" else target
    if target == "host" and os.environ.get("CARGO_BUILD_TARGET", host) != host:
        fail("host recipe cannot describe an implicit cross-target build")
    environment = {key: os.environ.get(key, "") for key in
                   ("RUSTUP_TOOLCHAIN", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET",
                    "CC", "CXX", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "AR",
                    "PYO3_USE_ABI3_FORWARD_COMPATIBILITY", "PYO3_NO_PYTHON")}
    environment.update({key: value for key, value in sorted(os.environ.items())
                        if key.startswith("CARGO_PROFILE_" + profile.upper() + "_")
                        or (key.startswith("CARGO_TARGET_") and key != "CARGO_TARGET_DIR")})
    # Cross-Python config and external linker/config files need additional input
    # hashing; none belongs to the host products currently shared by this DAG.
    if any(os.environ.get(key) for key in ("PYO3_CONFIG_FILE", "PYO3_CROSS", "PYO3_CROSS_LIB_DIR",
                                          "PYO3_CROSS_PYTHON_VERSION", "PYO3_CROSS_PYTHON_IMPLEMENTATION")):
        fail("external/cross Python configuration is not a reusable host recipe")
    recipe = {"schema": 1, "kind": kind, "package": package, "profile": profile,
              "default_features": True, "features": sorted(set(filter(None, features.split(",")))),
              "target": resolved_target, "target_mode": "host" if target == "host" else "explicit",
              "rustc": rustc, "cargo": subprocess.check_output(["cargo", "--version"], text=True).strip(),
              "lock_sha256": source_hashes()["Cargo.lock"], "environment": environment,
              "host_runtime": {"machine": platform.machine(), "system": sys.platform, "libc": list(platform.libc_ver())}}
    if kind == "python-wheel":
        if not interpreter or not maturin or resolved_target != host:
            fail("wheel recipe requires host interpreter and Maturin")
        code = "import json,platform,sys; print(json.dumps({'implementation':sys.implementation.name,'version':sys.version,'cache_tag':sys.implementation.cache_tag,'machine':platform.machine(),'libc':platform.libc_ver()}))"
        recipe["python"] = json.loads(subprocess.check_output([interpreter, "-c", code], text=True))
        recipe["python_abi"] = "abi3-py39"
        recipe["maturin"] = subprocess.check_output([maturin, "--version"], text=True).strip()
    elif interpreter or maturin:
        fail("FFI recipe has no Python build inputs")
    if kind == "napi":
        cli_root = ROOT / "bindings/typescript/node_modules/@napi-rs/cli"
        inputs = ("bindings/typescript/bun.lock", "bindings/typescript/package.json", "bindings/typescript/native/package.json", "bindings/typescript/scripts/check-native-generated.mjs", "scripts/ci/napi-loader.mjs")
        recipe["generator"] = {"node": subprocess.check_output(["node", "--version"], text=True).strip(),
                               "bun": subprocess.check_output(["bun", "--version"], text=True).strip(),
                               "cli": read_json(cli_root / "package.json")["version"],
                               "cli_sha256": digest(cli_root / "dist/index.js"),
                               "inputs": {name: hashlib.sha256(subprocess.check_output(["git", "-C", str(ROOT), "show", f"HEAD:{name}"])).hexdigest() for name in inputs}}
    validate_recipe(recipe)
    return recipe


def verify_input(root: Path, evidence: dict, artifact: str, job: str, recipe: dict) -> None:
    """Reject a mismatched intra-run handoff before a consumer installs/loads it."""
    if os.environ.get("FORGEJO_RUN_ATTEMPT") != "1":
        fail("native reuse requires a first-attempt consumer")
    safe_name(artifact)
    expected = identity()
    validate_recipe(recipe)
    for key, value in expected.items():
        if evidence.get(key) != value:
            fail(f"reused input has mismatched {key}")
    if evidence.get("artifact") != artifact or evidence.get("job") != job:
        fail("reused input has wrong artifact or producer")
    if evidence.get("attempt") != 1 or evidence.get("matrix") != {}:
        fail("reused host input has wrong attempt or matrix")
    if evidence.get("build_recipe") != recipe or recipe.get("lock_sha256") != expected["source_hashes"]["Cargo.lock"]:
        fail("reused input has mismatched native build recipe")
    if root.is_symlink() or not root.is_dir() or file_map(root) != evidence.get("files"):
        fail("reused input checksum/inventory mismatch")
    if recipe["kind"] == "ffi":
        libraries = [path for path in root.iterdir() if path.name in ("libaster_transport_ffi.so", "libaster_transport_ffi.dylib")]
        if len(libraries) != 1:
            fail("reused FFI input must contain exactly one host native library")
        code = "import ctypes,sys; lib=ctypes.CDLL(sys.argv[1]); lib.aster_version.restype=ctypes.c_char_p; print(lib.aster_version().decode())"
        native_version = subprocess.check_output([sys.executable, "-c", code, str(libraries[0].resolve())], text=True).strip()
        if native_version != expected["version"]:
            fail("reused FFI native version differs from exact candidate")
    elif recipe["kind"] == "napi":
        required = {"aster-transport.linux-x64-gnu.node", "index.js", "index.d.ts"}
        if set(evidence["files"]) != required:
            fail("reused NAPI payload must contain normal addon and generated bindings only")
        code = "const n=require(process.argv[1]); if('emitTestLogs' in n || 'testEmitted' in n) throw Error('test hooks in normal addon'); console.log(n.version())"
        native_version = subprocess.check_output(["node", "-e", code, str((root / "aster-transport.linux-x64-gnu.node").resolve())], text=True, timeout=30).strip()
        if native_version != expected["version"]:
            fail("reused NAPI native version differs from exact candidate")


def verify_python_install(wheel: Path, interpreter: str) -> None:
    """Prove tests import the wheel's Python and native bytes, at this exact version."""
    code = "import aster,aster._aster,importlib.metadata,json; d=importlib.metadata.distribution('aster-rpc'); print(json.dumps({'package':aster.__file__,'distribution_package':str(d.locate_file('aster/__init__.py')),'native':aster._aster.__file__,'version':aster.VERSION,'native_version':aster._aster.VERSION,'distribution_version':d.version}))"
    installed = json.loads(subprocess.check_output([interpreter, "-c", code], text=True))
    # A push gate builds the committed, unstamped tree (linux.yml sets
    # ASTER_UNSTAMPED_VERSION there and records nothing); candidates are stamped.
    unstamped = os.environ.get("ASTER_UNSTAMPED_VERSION")
    if unstamped and os.environ.get("FORGEJO_EVENT_NAME") != "push":
        fail("an unstamped build is only allowed in a push gate")
    expected = unstamped or identity()["version"]
    if any(installed[key] != expected for key in ("version", "native_version", "distribution_version")):
        fail("installed Python/native version differs from exact candidate")
    package = Path(installed["package"]).parent
    if Path(installed["package"]) != Path(installed["distribution_package"]):
        fail("Python source shadows the installed wheel")
    native = Path(installed["native"])
    with zipfile.ZipFile(wheel) as archive:
        files = [name for name in archive.namelist() if name.startswith("aster/") and not name.endswith("/")]
        if not files or "aster/__init__.py" not in files or "aster/" + native.name not in files:
            fail("wheel lacks the imported package/native payload")
        for name in files:
            relative = PurePosixPath(name).relative_to("aster")
            if ".." in relative.parts:
                fail("unsafe wheel package path")
            path = package.joinpath(*relative.parts)
            if path.is_symlink() or not path.is_file() or path.read_bytes() != archive.read(name):
                fail(f"installed Python payload differs from wheel: {name}")
        if native.parent != package:
            fail("imported native module is outside installed package")


def _verify_artifacts(root: Path, manifest: dict, expected: dict, policy: dict) -> None:
    """Authoritative identity/inventory/hash checks, independent of container layout."""
    for key in ("schema", "repository", "sha", "version", "workflow", "source_hashes", "run_id", "run_number", "assurance_profile"):
        if manifest.get(key) != expected[key]:
            fail(f"candidate identity mismatch: {key}")
    if manifest.get("gates") != expected_gates(policy):
        fail("candidate lacks successful evidence for every required job")
    disabled = policy.get("optional_assurance_jobs", []) if assurance_profile(policy) == "standard" else []
    if manifest.get("disabled_assurance_jobs") != disabled:
        fail("candidate disabled assurance jobs differ from release policy")
    suffix = f'-{expected["run_number"]}'
    artifacts = manifest.get("artifacts", {})
    rules = artifact_policy(policy)
    required = {name + suffix for name in rules}
    if set(artifacts) != required:
        fail("candidate artifact inventory differs from release policy")
    for name, evidence in artifacts.items():
        safe_name(name)
        if evidence.get("artifact") != name:
            fail(f"artifact {name} provenance names another artifact")
        for key in ("schema", "repository", "sha", "version", "workflow", "source_hashes", "run_id", "run_number", "assurance_profile"):
            if evidence.get(key) != expected[key]:
                fail(f"artifact {name} has mismatched {key}")
        rule = rules[name.removesuffix(suffix)]
        if evidence.get("job") != rule["job"] or evidence.get("matrix") != rule["matrix"]:
            fail(f"artifact {name} has wrong job or build matrix")
        if evidence.get("attempt") != 1:
            fail(f"artifact {name} lacks first-attempt evidence")
        if not evidence.get("tools"):
            fail(f"artifact {name} has no runtime/toolchain evidence")
        directory = root / name
        if directory.is_symlink() or not directory.is_dir():
            fail(f"artifact {name} has invalid payload directory")
        actual = file_map(directory)
        if actual != evidence.get("files"):
            fail(f"artifact {name} checksum/inventory mismatch")
        for path in actual:
            parsed = PurePosixPath(path)
            if parsed.is_absolute() or ".." in parsed.parts:
                fail(f"unsafe artifact path: {path}")


def verify_payload(root: Path, manifest: dict, expected: dict, policy: dict) -> None:
    _verify_artifacts(root, manifest, expected, policy)
    required = {name + f'-{expected["run_number"]}' for name in artifact_policy(policy)}
    # No unrecorded payload directories, including from a partial prior run.
    if {p.name for p in root.iterdir()} != required:
        fail("candidate contains unexpected artifact paths")


def handoff_names(policy: dict, expected: dict) -> set[str]:
    """Only release products and their producer attestation directories."""
    suffix = f'-{expected["run_number"]}'
    rules = artifact_policy(policy)
    return ({name + suffix for name in rules}
            | {"ci-evidence-" + rule["job"] + ("-" + rule["matrix"]["platform"] if "platform" in rule["matrix"] else "") + suffix
               for rule in rules.values()})


def _qualified_gates(needs: dict, policy: dict) -> dict[str, str]:
    gates = expected_gates(policy)
    if set(needs) != set(gates) or any(needs[j].get("result") != result for j, result in gates.items()):
        fail("not every candidate dependency completed successfully")
    return gates


def handoff_manifest(handoff: Path, needs: dict, expected: dict, policy: dict, *, strict_inventory: bool = False) -> dict:
    """Validate original handoff bytes without copying payloads or creating an archive.

    Retrieval requires the exact selected product/attestation directory set.
    Seal also supports the older bulk handoff layout containing internal reuse
    and documentation artifacts; those never become release payloads.
    """
    gates = _qualified_gates(needs, policy)
    if handoff.is_symlink() or not handoff.is_dir():
        fail("invalid candidate handoff directory")
    if strict_inventory and {path.name for path in handoff.iterdir()} != handoff_names(policy, expected):
        fail("candidate handoff contains unexpected artifact paths")
    artifacts = {}
    for directory in handoff.glob("ci-evidence-*"):
        if directory.is_symlink() or not directory.is_dir():
            fail("invalid candidate attestation directory")
        for path in directory.glob("*.json"):
            if path.is_symlink() or not path.is_file():
                fail("invalid candidate attestation file")
            evidence = read_json(path)
            name = evidence["artifact"]
            if name in artifacts:
                fail(f"duplicate provenance for {name}")
            artifacts[name] = evidence
    names = {n + f'-{expected["run_number"]}' for n in artifact_policy(policy)}
    # Documentation artifacts remain in the run but aren't release inputs.
    artifacts = {n: e for n, e in artifacts.items() if n in names}
    manifest = {**expected, "gates": {job: needs[job]["result"] for job in gates}, "artifacts": artifacts,
                "disabled_assurance_jobs": policy.get("optional_assurance_jobs", []) if assurance_profile(policy) == "standard" else [],
                "created_at": datetime.now(timezone.utc).isoformat(), "retention_days": RETENTION_DAYS}
    _verify_artifacts(handoff, manifest, expected, policy)
    return manifest


def seal(handoff: Path, output: Path, needs: dict) -> None:
    if os.environ.get("FORGEJO_RUN_ATTEMPT") != "1":
        fail("candidate sealing requires first-attempt evidence")
    policy = read_json(POLICY_PATH)
    # Preserve the fail-before-identity gate for unqualified dependencies.
    _qualified_gates(needs, policy)
    expected = identity()
    manifest = handoff_manifest(handoff, needs, expected, policy)
    names = set(manifest["artifacts"])
    payload = output / "payload"
    payload.mkdir(parents=True)
    for name in names:
        shutil.copytree(handoff / name, payload / name)
    verify_payload(payload, manifest, expected, policy)
    archive = output / "candidate.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        for name in sorted(names):
            bundle.add(payload / name, arcname=name, recursive=True)
    manifest["bundle_sha256"] = digest(archive)
    (output / "candidate.json").write_text(json.dumps(manifest, indent=2) + "\n")
    shutil.rmtree(payload)
    print(f'{expected["sha"]}-{expected["run_id"]}')


class Forgejo:
    def __init__(self):
        self.base = os.environ.get("FORGEJO_SERVER_URL", "https://forge.emrul.dev").rstrip("/")
        if self.base != "https://forge.emrul.dev":
            fail("release candidates must come from the canonical Forgejo instance")
        self.token = os.environ.get("ASTER_ACTIONS_TOKEN", "")

    def request(self, path: str, *, method: str = "GET"):
        headers = {"User-Agent": "aster-release-candidate/1"}
        actions = path.startswith(f"/api/v1/repos/{REPOSITORY}/actions/")
        if actions and not self.token:
            fail("Actions API requires ASTER_ACTIONS_TOKEN from forgejo.token in a running job of this repository")
        # Public candidate package reads are anonymous. The repository-bound
        # job token is not a package token; pruning alone uses a package token.
        if self.token and (actions or method != "GET"):
            headers["Authorization"] = "token " + self.token
        try:
            return urlopen(Request(self.base + path, headers=headers, method=method), timeout=60)
        except HTTPError as error:
            if actions and error.code in (401, 403, 404):
                error.close()
                fail(f"Actions API unavailable (HTTP {error.code}); use forgejo.token from a running job in {REPOSITORY} with Actions enabled; promotion remains blocked")
            raise

    def json(self, path: str):
        with self.request(path) as response:
            return json.load(response)


def check_api(api: Forgejo | None = None) -> None:
    """Probe the actual task token before an expensive main qualification."""
    api = api or Forgejo()
    for endpoint in ("runs", "tasks"):
        response = api.json(f"/api/v1/repos/{REPOSITORY}/actions/{endpoint}?limit=1&page=1")
        if not isinstance(response.get("workflow_runs"), list):
            fail(f"unexpected Forgejo Actions {endpoint} API response")
    print("repository job token can read candidate run and task evidence")


def validate_run(run: dict, sha: str) -> None:
    repository = run.get("repository") or {}
    if (repository.get("full_name") != REPOSITORY or run.get("workflow_id") != WORKFLOW
            or run.get("commit_sha") != sha
            # Forgejo's runs API leaves "event" empty for a dispatch and reports
            # it in trigger_event (checked live, 2026-10-14).
            or run.get("event") not in ("", CANDIDATE_EVENT)
            or run.get("trigger_event") != CANDIDATE_EVENT or run.get("prettyref") not in ("main", "refs/heads/main")
            or run.get("is_fork_pull_request") is not False or run.get("need_approval") is not False
            or run.get("is_ref_deleted") is not False):
        fail("run is not a trusted main candidate dispatch for this exact commit and workflow")
    if run.get("status") != "success":
        fail(f'candidate run is {run.get("status")}, not successful')
    # Unfinished runs report a zero time: 1970-01-01 from the API, 0001- in Go.
    if not run.get("stopped") or run["stopped"].startswith(("0001-", "1970-01-01")):
        fail("candidate run has not finished")


def select_run(runs: list[dict], sha: str) -> dict:
    # Do not hunt backwards for a green attempt after a failed/cancelled run.
    # A duplicate run for the same SHA is itself an error requiring review.
    candidates = [r for r in runs if r.get("workflow_id") == WORKFLOW
                  and r.get("commit_sha") == sha and r.get("trigger_event") == CANDIDATE_EVENT
                  and r.get("prettyref") in ("main", "refs/heads/main")]
    if len(candidates) != 1:
        fail(f"expected exactly one main candidate run, found {len(candidates)}")
    validate_run(candidates[0], sha)
    return candidates[0]


def validate_tasks(tasks: list[dict], run: dict) -> None:
    # The run status and current task statuses must agree. The workflow's
    # FORGEJO_RUN_ATTEMPT gate and per-artifact attempt records prevent reruns
    # from replacing failed qualification with green evidence.
    selected = [task for task in tasks if task.get("run_number") == run["index_in_repo"]
                and task.get("workflow_id") == WORKFLOW and task.get("head_sha") == run["commit_sha"]]
    if not selected:
        fail("candidate has no task-attempt evidence")
    for task in selected:
        if task.get("event") != CANDIDATE_EVENT or task.get("head_branch") not in ("main", "refs/heads/main"):
            fail("candidate task has untrusted event/ref evidence")
        if task.get("status") not in ("success", "skipped"):
            fail(f'candidate includes a {task.get("status")} task attempt; no retry-to-green promotion')
    # Aggregate matrix success alone does not establish complete per-target evidence.
    # The workflow gives each archive consumer a stable explicit task name.
    for name in sorted(FFI_ARCHIVE_CONSUMER_TASKS):
        matches = [task for task in selected if task.get("name") == name]
        if len(matches) != 1 or matches[0].get("status") != "success":
            fail(f"candidate requires one successful archive consumer task: {name}")


def safe_extract(archive: Path, output: Path) -> None:
    with tarfile.open(archive, "r:gz") as bundle:
        seen = set()
        for member in bundle.getmembers():
            path = PurePosixPath(member.name)
            if (path.is_absolute() or ".." in path.parts or member.name in seen
                    or not (member.isfile() or member.isdir())):
                fail(f"unsafe candidate archive member: {member.name}")
            seen.add(member.name)
        bundle.extractall(output, filter="data")


def fetch(output: Path, api: Forgejo | None = None) -> dict:
    api = api or Forgejo()
    expected = identity()
    if os.environ.get("FORGEJO_REF") != f'refs/tags/v{expected["version"]}':
        fail("promotion tag differs from the exact derived commit version")
    runs = []
    page = 1
    while True:
        query = urlencode({"head_sha": expected["sha"], "workflow_id": WORKFLOW, "event": CANDIDATE_EVENT, "ref": "refs/heads/main", "limit": 50, "page": page})
        result = api.json(f"/api/v1/repos/{REPOSITORY}/actions/runs?{query}")
        batch = result["workflow_runs"]
        runs.extend(batch)
        if len(batch) < 50:
            break
        page += 1
    run = select_run(runs, expected["sha"])
    tasks = []
    page = 1
    while True:
        result = api.json(f"/api/v1/repos/{REPOSITORY}/actions/tasks?limit=50&page={page}")
        batch = result["workflow_runs"]
        tasks.extend(batch)
        if len(batch) < 50:
            break
        page += 1
    validate_tasks(tasks, run)
    expected.update(run_id=run["id"], run_number=run["index_in_repo"])
    key = f'{expected["sha"]}-{expected["run_id"]}'
    base = f"/api/packages/Aster/generic/{PACKAGE}/{key}"
    listing = api.json(f"/api/v1/packages/Aster/generic/{PACKAGE}/{key}/files")
    checksums = {item["name"]: item["sha256"] for item in listing}
    if set(checksums) != {"candidate.json", "candidate.tar.gz"} or len(listing) != 2:
        fail("candidate package is missing or has an unexpected file inventory")
    output.mkdir(parents=True, exist_ok=False)
    for name in ("candidate.json", "candidate.tar.gz"):
        with api.request(f"{base}/{name}") as response, (output / name).open("wb") as target:
            shutil.copyfileobj(response, target)
        if digest(output / name) != checksums[name]:
            fail(f"candidate package {name} does not match Forgejo checksum")
    manifest = read_json(output / "candidate.json")
    if digest(output / "candidate.tar.gz") != manifest.get("bundle_sha256"):
        fail("candidate bundle differs from recorded provenance")
    created = datetime.fromisoformat(manifest["created_at"])
    now = datetime.now(timezone.utc)
    if created.tzinfo is None or created > now + timedelta(minutes=5) or now - created > timedelta(days=RETENTION_DAYS):
        fail("candidate provenance is expired or dated in the future")
    payload = output / "payload"
    payload.mkdir()
    safe_extract(output / "candidate.tar.gz", payload)
    verify_payload(payload, manifest, expected, read_json(POLICY_PATH))
    # Re-read after download to detect cancellation/deletion/changed provenance.
    current = api.json(f'/api/v1/repos/{REPOSITORY}/actions/runs/{run["id"]}')
    validate_run(current, expected["sha"])
    if current.get("id") != run["id"] or current.get("index_in_repo") != run["index_in_repo"]:
        fail("candidate run identity changed while retrieving artifacts")
    return manifest


def prune(api: Forgejo | None = None) -> None:
    api = api or Forgejo()
    # Listing is anonymous; deletion uses the existing package token only in
    # the HTTP Authorization header. Never delete a release namespace.
    api.token = os.environ["ASTER_PACKAGE_TOKEN"]
    packages = []
    page = 1
    while True:
        batch = api.json(f"/api/v1/packages/Aster?type=generic&q={PACKAGE}&limit=50&page={page}")
        packages.extend(batch)
        if len(batch) < 50:
            break
        page += 1
    cutoff = datetime.now(timezone.utc) - timedelta(days=RETENTION_DAYS)
    for package in packages:
        key = package.get("version", "")
        if package.get("name") != PACKAGE or package.get("type") != "generic" or not re.fullmatch(r"[0-9a-f]{40}-[0-9]+", key):
            continue
        created = datetime.fromisoformat(package["created_at"].replace("Z", "+00:00"))
        if created < cutoff:
            with api.request(f"/api/v1/packages/Aster/generic/{PACKAGE}/{key}", method="DELETE"):
                pass
            print(f"pruned expired candidate {key}")


def materialize(candidate: Path) -> None:
    manifest = read_json(candidate / "candidate.json")
    expected = identity()
    expected.update(run_id=manifest["run_id"], run_number=manifest["run_number"])
    verify_payload(candidate / "payload", manifest, expected, read_json(POLICY_PATH))
    number = manifest["run_number"]
    if (ROOT / "mobile").exists():
        shutil.rmtree(ROOT / "mobile")
    for prefix, destination in (("aster-python-", "dist/python"), ("ffi-tarballs-", "dist/ffi"),
                                ("dotnet-nupkg-", "dist/nuget"), ("maven-packages-", "dist/maven"),
                                ("java-classifier-", "jars")):
        target = ROOT / destination
        target.mkdir(parents=True, exist_ok=True)
        for name in manifest["artifacts"]:
            if name.startswith(prefix):
                for source in (candidate / "payload" / name).iterdir():
                    if (target / source.name).exists():
                        fail(f"duplicate promoted file: {source.name}")
                    shutil.copy2(source, target / source.name)
    shutil.copytree(candidate / "payload" / f"npm-packages-{number}", ROOT / "artifacts")
    order = [f"artifacts/npm-{p}" for p in ("linux-x64-gnu", "linux-arm64-gnu", "darwin-arm64", "darwin-x64", "win32-x64-msvc", "transport", "aster")]
    (ROOT / "publish-order").write_text("\n".join(order) + "\n")
    for path in order:
        package = read_json(ROOT / path / "package.json")
        if package.get("version") != manifest["version"]:
            fail(f"promoted npm package has wrong version: {path}")
    for name in ("ios-xcframework", "android-libs"):
        if f"{name}-{number}" in manifest["artifacts"]:
            shutil.copytree(candidate / "payload" / f"{name}-{number}", ROOT / "mobile" / name)
    # Registry writes across several services cannot be atomic. This immutable
    # provenance binds retries to identical bytes; a completion receipt is
    # created only after every required publisher has succeeded.
    (ROOT / "release-provenance").mkdir()
    shutil.copyfile(candidate / "candidate.json", ROOT / "release-provenance/candidate.json")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    rec = sub.add_parser("record")
    rec.add_argument("--artifact", required=True)
    rec.add_argument("--path", required=True)
    rec.add_argument("--output", type=Path, default=Path("ci-evidence"))
    rec.add_argument("--job", required=True)
    rec.add_argument("--matrix", default="{}")
    rec.add_argument("--recipe", type=Path)
    recipe_args = sub.add_parser("build-recipe")
    recipe_args.add_argument("--kind", choices=("python-wheel", "ffi", "napi"), required=True)
    recipe_args.add_argument("--package", required=True)
    recipe_args.add_argument("--profile", choices=("release",), default="release")
    recipe_args.add_argument("--features", default="")
    recipe_args.add_argument("--target", default="host")
    recipe_args.add_argument("--interpreter")
    recipe_args.add_argument("--maturin")
    recipe_args.add_argument("--output", type=Path, required=True)
    reuse = sub.add_parser("verify-input")
    reuse.add_argument("--artifact", required=True)
    reuse.add_argument("--path", type=Path, required=True)
    reuse.add_argument("--evidence", type=Path, required=True)
    reuse.add_argument("--job", required=True)
    reuse.add_argument("--recipe", type=Path, required=True)
    installed = sub.add_parser("verify-python-install")
    installed.add_argument("--wheel", type=Path, required=True)
    installed.add_argument("--interpreter", required=True)
    seal_args = sub.add_parser("seal")
    seal_args.add_argument("--handoff", type=Path, default=Path("handoff"))
    seal_args.add_argument("--output", type=Path, default=Path("candidate"))
    fetch_args = sub.add_parser("fetch")
    fetch_args.add_argument("--output", type=Path, default=Path("candidate"))
    layout = sub.add_parser("materialize")
    layout.add_argument("--candidate", type=Path, default=Path("candidate"))
    sub.add_parser("prune")
    sub.add_parser("check-api")
    sub.add_parser("profile")
    args = parser.parse_args()
    if args.command == "record":
        record(args.artifact, args.path, args.output, args.job, args.matrix, read_json(args.recipe) if args.recipe else None)
    elif args.command == "build-recipe":
        recipe = build_recipe(args.kind, args.package, args.profile, args.features, args.target, args.interpreter, args.maturin)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(recipe, indent=2) + "\n")
    elif args.command == "verify-input":
        verify_input(args.path, read_json(args.evidence), args.artifact, args.job, read_json(args.recipe))
    elif args.command == "verify-python-install":
        verify_python_install(args.wheel, args.interpreter)
    elif args.command == "seal":
        seal(args.handoff, args.output, json.loads(os.environ["CANDIDATE_NEEDS"]))
    elif args.command == "fetch":
        manifest = fetch(args.output)
        print(f'validated candidate {manifest["sha"]} from run {manifest["run_number"]}')
    elif args.command == "materialize":
        materialize(args.candidate)
    elif args.command == "check-api":
        check_api()
    elif args.command == "profile":
        print(assurance_profile(read_json(POLICY_PATH)))
    else:
        prune()


if __name__ == "__main__":
    main()
