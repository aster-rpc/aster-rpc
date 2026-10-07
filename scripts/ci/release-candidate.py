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
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
from urllib.parse import urlencode
from urllib.error import HTTPError
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "Aster/aster-rpc-internal"
WORKFLOW = "linux.yml"
PACKAGE = "aster-ci-candidate"
RETENTION_DAYS = 30
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


def identity() -> dict:
    sha = git("rev-parse", "HEAD")
    if os.environ.get("FORGEJO_SHA", sha) != sha:
        fail("checkout SHA differs from the workflow SHA")
    version = subprocess.check_output([str(ROOT / "scripts/release/version.sh")], text=True).strip()
    return {"schema": 1, "repository": REPOSITORY, "sha": sha, "version": version,
            "workflow": WORKFLOW, "source_hashes": source_hashes(),
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


def record(artifact: str, path_pattern: str, output: Path, job: str, matrix: str) -> None:
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
    output.mkdir(parents=True, exist_ok=True)
    (output / f"{artifact}.json").write_text(json.dumps(evidence, indent=2) + "\n")


def read_json(path: Path):
    return json.loads(path.read_text())


def verify_payload(root: Path, manifest: dict, expected: dict, policy: dict) -> None:
    for key in ("schema", "repository", "sha", "version", "workflow", "source_hashes", "run_id", "run_number"):
        if manifest.get(key) != expected[key]:
            fail(f"candidate identity mismatch: {key}")
    if manifest.get("gates") != {job: "success" for job in policy["required_jobs"]}:
        fail("candidate lacks successful evidence for every required job")
    suffix = f'-{expected["run_number"]}'
    artifacts = manifest.get("artifacts", {})
    required = {name + suffix for name in policy["required_artifacts"]}
    if set(artifacts) != required:
        fail("candidate artifact inventory differs from release policy")
    for name, evidence in artifacts.items():
        safe_name(name)
        if evidence.get("artifact") != name:
            fail(f"artifact {name} provenance names another artifact")
        for key in ("schema", "repository", "sha", "version", "workflow", "source_hashes", "run_id", "run_number"):
            if evidence.get(key) != expected[key]:
                fail(f"artifact {name} has mismatched {key}")
        rule = policy["required_artifacts"][name.removesuffix(suffix)]
        if evidence.get("job") != rule["job"] or evidence.get("matrix") != rule["matrix"]:
            fail(f"artifact {name} has wrong job or build matrix")
        if evidence.get("attempt") != 1:
            fail(f"artifact {name} lacks first-attempt evidence")
        if not evidence.get("tools"):
            fail(f"artifact {name} has no runtime/toolchain evidence")
        actual = file_map(root / name)
        if actual != evidence.get("files"):
            fail(f"artifact {name} checksum/inventory mismatch")
        for path in actual:
            parsed = PurePosixPath(path)
            if parsed.is_absolute() or ".." in parsed.parts:
                fail(f"unsafe artifact path: {path}")
    # No unrecorded payload directories, including from a partial prior run.
    if {p.name for p in root.iterdir()} != required:
        fail("candidate contains unexpected artifact paths")


def seal(handoff: Path, output: Path, needs: dict) -> None:
    if os.environ.get("FORGEJO_RUN_ATTEMPT") != "1":
        fail("candidate sealing requires first-attempt evidence")
    policy = read_json(POLICY_PATH)
    required = policy["required_jobs"]
    if set(needs) != set(required) or any(needs[j].get("result") != "success" for j in required):
        fail("not every candidate dependency completed successfully")
    expected = identity()
    artifacts = {}
    for directory in handoff.glob("ci-evidence-*"):
        for path in directory.glob("*.json"):
            evidence = read_json(path)
            name = evidence["artifact"]
            if name in artifacts:
                fail(f"duplicate provenance for {name}")
            artifacts[name] = evidence
    names = {n + f'-{expected["run_number"]}' for n in policy["required_artifacts"]}
    # Documentation artifacts remain in the run but aren't release inputs.
    artifacts = {n: e for n, e in artifacts.items() if n in names}
    manifest = {**expected, "gates": {job: "success" for job in required}, "artifacts": artifacts,
                "created_at": datetime.now(timezone.utc).isoformat(), "retention_days": RETENTION_DAYS}
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
            or run.get("commit_sha") != sha or run.get("event") != "push"
            or run.get("trigger_event") != "push" or run.get("prettyref") not in ("main", "refs/heads/main")
            or run.get("is_fork_pull_request") is not False or run.get("need_approval") is not False
            or run.get("is_ref_deleted") is not False):
        fail("run is not a trusted main push for this exact commit and workflow")
    if run.get("status") != "success":
        fail(f'candidate run is {run.get("status")}, not successful')
    if not run.get("stopped") or run["stopped"].startswith("0001-"):
        fail("candidate run has not finished")


def select_run(runs: list[dict], sha: str) -> dict:
    # Do not hunt backwards for a green attempt after a failed/cancelled run.
    # A duplicate run for the same SHA is itself an error requiring review.
    candidates = [r for r in runs if r.get("workflow_id") == WORKFLOW
                  and r.get("commit_sha") == sha and r.get("event") == "push"
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
        if task.get("event") != "push" or task.get("head_branch") not in ("main", "refs/heads/main"):
            fail("candidate task has untrusted event/ref evidence")
        if task.get("status") not in ("success", "skipped"):
            fail(f'candidate includes a {task.get("status")} task attempt; no retry-to-green promotion')


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
        query = urlencode({"head_sha": expected["sha"], "workflow_id": WORKFLOW, "event": "push", "ref": "refs/heads/main", "limit": 50, "page": page})
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
    seal_args = sub.add_parser("seal")
    seal_args.add_argument("--handoff", type=Path, default=Path("handoff"))
    seal_args.add_argument("--output", type=Path, default=Path("candidate"))
    fetch_args = sub.add_parser("fetch")
    fetch_args.add_argument("--output", type=Path, default=Path("candidate"))
    layout = sub.add_parser("materialize")
    layout.add_argument("--candidate", type=Path, default=Path("candidate"))
    sub.add_parser("prune")
    sub.add_parser("check-api")
    args = parser.parse_args()
    if args.command == "record":
        record(args.artifact, args.path, args.output, args.job, args.matrix)
    elif args.command == "seal":
        seal(args.handoff, args.output, json.loads(os.environ["CANDIDATE_NEEDS"]))
    elif args.command == "fetch":
        manifest = fetch(args.output)
        print(f'validated candidate {manifest["sha"]} from run {manifest["run_number"]}')
    elif args.command == "materialize":
        materialize(args.candidate)
    elif args.command == "check-api":
        check_api()
    else:
        prune()


if __name__ == "__main__":
    main()
