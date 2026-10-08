#!/usr/bin/env python3
"""Sequential exact-run ArtifactService handoff; transport retries only.

Uses the same runtime JWT/Twirp protocol as forgejo/download-artifact v4,
but lists once, selects only policy products/attestations and fails closed.
An eight-minute overall budget includes three attempts per transport request.
Diagnostics never contain runtime tokens, signed URLs or response bodies.

Runtime/protocol sources checked against the deployed runner/server series:
https://code.forgejo.org/forgejo/runner/src/tag/v13.0.0/internal/app/run/runner.go
https://code.forgejo.org/forgejo/runner/src/tag/v13.0.0/internal/app/run/runner_test.go
https://codeberg.org/forgejo/forgejo/src/tag/v15.0.1/routers/api/actions/artifactsv4.go
https://codeberg.org/forgejo/forgejo/src/tag/v15.0.1/services/actions/auth.go
https://codeberg.org/forgejo/forgejo/src/tag/v15.0.1/services/actions/context.go
Runner runEnvs supplies the runtime token/results origin to shell steps; its
environment test explicitly exercises those variables in a run: step.
Server CreateAuthorizationToken scopes Actions.Results to task.Job.RunID,
the same database ID assigned to the workflow run_id context (not run_number).
"""
from __future__ import annotations

import base64
import importlib.util
import json
import os
import re
from pathlib import Path, PurePosixPath
import stat
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit
import zipfile

spec = importlib.util.spec_from_file_location("candidate", Path(__file__).with_name("release-candidate.py"))
C = importlib.util.module_from_spec(spec)
spec.loader.exec_module(C)
SERVICE = "/twirp/github.actions.results.api.v1.ArtifactService/"


def wire_fields(value: dict) -> dict:
    """Protobuf accepts snake_case requests but defaults to camelCase JSON."""
    result = dict(value)
    for camel, snake in (("workflowRunBackendId", "workflow_run_backend_id"),
                         ("workflowJobRunBackendId", "workflow_job_run_backend_id"),
                         ("databaseId", "database_id"), ("signedUrl", "signed_url")):
        if camel in result:
            if snake in result and result[snake] != result[camel]:
                C.fail("conflicting protobuf JSON field identities")
            result[snake] = result.pop(camel)
    return result


def origin(url: str) -> str:
    parsed = urlsplit(url)
    if parsed.scheme != "https" or parsed.hostname != "forge.emrul.dev" or parsed.port not in (None, 443) or parsed.username or parsed.password:
        C.fail("artifact endpoint is not canonical HTTPS Forgejo")
    return "https://forge.emrul.dev"


class Transport:
    def __init__(self, diagnostics: Path):
        self.deadline = time.monotonic() + 480
        self.diagnostics = diagnostics

    def get(self, url: str, target: Path, *, token: str = "", data: dict | None = None, artifact: str = ""):
        origin(url)
        if artifact:
            C.safe_name(artifact)
        for attempt in range(1, 4):
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                C.fail("candidate retrieval exceeded 480-second transport budget")
            limit = min(90, remaining)
            # curl config via stdin keeps signed URLs and credentials out of argv.
            config = ["url = " + json.dumps(url)]
            if token:
                config.append("header = " + json.dumps("Authorization: Bearer " + token))
            if data is not None:
                config += ['header = "Content-Type: application/json"', "data = " + json.dumps(json.dumps(data))]
            command = ["curl", "--config", "-", "--silent", "--connect-timeout", "10",
                       "--max-time", str(limit), "--max-filesize", "2147483648",
                       "--output", str(target), "--write-out", "%{http_code}\n%{content_type}\n%{size_download}"]
            try:
                result = subprocess.run(command, input="\n".join(config) + "\n", text=True, capture_output=True, timeout=limit + 1)
            except subprocess.TimeoutExpired:
                result = subprocess.CompletedProcess(command, 28, "", "")
            fields = result.stdout.splitlines()
            status = fields[0] if fields else "000"
            mime = fields[1].split(";", 1)[0].lower() if len(fields) > 1 else ""
            size = target.stat().st_size if target.exists() else 0
            event = {"request": SERVICE + urlsplit(url).path.rsplit("/", 1)[-1] if data is not None else "artifact-blob",
                     "attempt": attempt, "http_status": status, "content_type": mime,
                     "artifact": artifact, "bytes": size, "curl_exit": result.returncode}
            with self.diagnostics.open("a") as stream:
                stream.write(json.dumps(event) + "\n")
            print(json.dumps(event), flush=True)
            valid = result.returncode == 0 and status == "200" and size > 0
            if data is not None:
                valid = valid and mime == "application/json"
                if valid:
                    try:
                        response = json.loads(target.read_bytes())
                        if not isinstance(response, dict):
                            valid = False
                    except (ValueError, UnicodeError):
                        valid = False
                    if valid:
                        return response
            elif valid and mime not in ("text/html", "application/json") and zipfile.is_zipfile(target):
                return None
            if status in ("401", "403", "404") or (status.startswith("4") and status not in ("408", "429")):
                C.fail(f"artifact transport denied: HTTP {status}")
            if attempt < 3:
                time.sleep(min(attempt, max(0, self.deadline - time.monotonic())))
        C.fail("artifact transport exhausted three attempts; inspect candidate-retrieval.jsonl")


def backend_ids(token: str) -> dict:
    try:
        payload = token.split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        scopes = [s.split(":") for s in claims["scp"].split() if s.startswith("Actions.Results:")]
        if len(scopes) != 1 or len(scopes[0]) != 3 or not all(scopes[0][1:]):
            raise ValueError()
        return dict(zip(("workflow_run_backend_id", "workflow_job_run_backend_id"), scopes[0][1:]))
    except (ValueError, KeyError, IndexError, TypeError):
        C.fail("runtime token lacks unique Actions.Results backend identity")


def selection(listing: dict, policy: dict, expected: dict, run_backend: str) -> list[dict]:
    suffix = f'-{expected["run_number"]}'
    rules = C.artifact_policy(policy)
    required = {name + suffix for name in rules}
    required |= {"ci-evidence-" + rule["job"] + ("-" + rule["matrix"]["platform"] if rule["matrix"] else "") + suffix for rule in rules.values()}
    entries = listing.get("artifacts")
    if not isinstance(entries, list):
        C.fail("artifact listing is missing inventory")
    entries = [wire_fields(entry) for entry in entries]
    selected = []
    for name in sorted(required):
        matches = [entry for entry in entries if entry.get("name") == name]
        if len(matches) != 1:
            C.fail(f"expected one exact-run artifact {name}, found {len(matches)}")
        entry = matches[0]
        if entry.get("workflow_run_backend_id") != run_backend or not entry.get("workflow_job_run_backend_id") or not entry.get("database_id"):
            C.fail(f"artifact {name} has wrong backend identity")
        selected.append(entry)
    return selected


def extract(archive: Path, target: Path) -> None:
    with zipfile.ZipFile(archive) as bundle:
        names = set()
        total = 0
        for member in bundle.infolist():
            path = PurePosixPath(member.filename)
            total += member.file_size
            if (path.is_absolute() or ".." in path.parts or "\\" in member.filename or not path.parts
                    or stat.S_ISLNK(member.external_attr >> 16) or member.filename in names or total > 2147483648):
                C.fail("unsafe or oversized artifact ZIP")
            names.add(member.filename)
        if not names or bundle.testzip() is not None:
            C.fail("empty or corrupt artifact ZIP")
        target.mkdir()
        bundle.extractall(target)


def retrieve(handoff: Path = Path("handoff")) -> None:
    if os.environ.get("FORGEJO_RUN_ATTEMPT") != "1":
        C.fail("candidate retrieval requires first-attempt evidence")
    policy = C.read_json(C.POLICY_PATH)
    gates = C.expected_gates(policy)
    needs = json.loads(os.environ["CANDIDATE_NEEDS"])
    if set(needs) != set(gates) or any(needs[j].get("result") != result for j, result in gates.items()):
        C.fail("candidate dependencies did not qualify; no artifact retrieval")
    expected = C.identity()
    token = os.environ["ACTIONS_RUNTIME_TOKEN"]
    ids = backend_ids(token)
    if ids["workflow_run_backend_id"] != str(expected["run_id"]):
        C.fail("runtime backend ID differs from the exact checkout run")
    base = origin(os.environ["ACTIONS_RESULTS_URL"])
    if handoff.exists():
        C.fail("candidate handoff already exists")
    transport = Transport(Path("candidate-retrieval.jsonl"))
    with tempfile.TemporaryDirectory(prefix="candidate-handoff-", dir=handoff.parent) as temporary:
        stage = Path(temporary)
        root = stage / "handoff"
        root.mkdir()
        response = stage / "response"
        listing = transport.get(base + SERVICE + "ListArtifacts", response, token=token, data=ids)
        for entry in selection(listing, policy, expected, ids["workflow_run_backend_id"]):
            name = entry["name"]
            C.safe_name(name)
            request = {key: entry[key] for key in ids}
            request["name"] = name
            signed = wire_fields(transport.get(base + SERVICE + "GetSignedArtifactURL", response, token=token, data=request, artifact=name))
            archive = stage / "artifact.zip"
            transport.get(signed["signed_url"], archive, artifact=name)
            expected_digest = entry.get("digest")
            if expected_digest is not None:
                if not isinstance(expected_digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", expected_digest):
                    C.fail(f"artifact {name} has invalid transport digest")
                if expected_digest != "sha256:" + C.digest(archive):
                    C.fail(f"artifact {name} transport checksum mismatch")
            extract(archive, root / name)
            archive.unlink()
        # Seal has the authoritative identity/inventory/hash checks. Validate
        # into disposable staging first, so a failed retrieval leaves no handoff.
        C.seal(root, stage / "validation", needs)
        root.rename(handoff)


if __name__ == "__main__":
    try:
        retrieve()
    except Exception as error:
        # External response objects and signed URLs must never reach a traceback.
        print(f"candidate retrieval failed ({type(error).__name__}); inspect candidate-retrieval.jsonl", file=sys.stderr)
        sys.exit(1)
