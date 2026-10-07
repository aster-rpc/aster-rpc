#!/usr/bin/env python3
"""Offline release guards: no credentials, builds, retries or registry writes."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("candidate", Path(__file__).with_name("release-candidate.py"))
C = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(C)
SHA = "a" * 40
IDENTITY = {"schema": 1, "repository": C.REPOSITORY, "sha": SHA, "version": "0.3.776",
            "workflow": "linux.yml", "source_hashes": {"Cargo.lock": "b" * 64}, "run_id": 42, "run_number": 27}
POLICY = {"required_jobs": ["linux", "macos"], "required_artifacts": {"native-linux": {"job": "linux", "matrix": {"target": "x86_64-unknown-linux-gnu"}}}}


def run_fixture(**updates):
    return {"id": 42, "index_in_repo": 27, "repository": {"full_name": C.REPOSITORY},
            "commit_sha": SHA, "workflow_id": "linux.yml", "prettyref": "main", "event": "push", "trigger_event": "push",
            "is_fork_pull_request": False, "need_approval": False, "is_ref_deleted": False,
            "status": "success", "stopped": datetime.now(timezone.utc).isoformat(), **updates}


class API:
    def __init__(self, run, manifest, archive, *, final_run=None, files=None):
        self.run = run
        self.final_run = final_run or run
        self.bytes = {"candidate.json": json.dumps(manifest).encode(), "candidate.tar.gz": archive}
        self.files = files or [{"name": name, "sha256": hashlib.sha256(data).hexdigest()} for name, data in self.bytes.items()]
        self.reads = []

    def json(self, path):
        self.reads.append(path)
        if "/runs?" in path:
            return {"workflow_runs": [self.run]}
        if "/tasks?" in path:
            return {"workflow_runs": [{"run_number": 27, "workflow_id": "linux.yml", "head_sha": SHA,
                                       "event": "push", "head_branch": "main", "status": "success"}]}
        if "/runs/" in path:
            return self.final_run
        return self.files

    def request(self, path):
        self.reads.append(path)
        return io.BytesIO(self.bytes[Path(path).name])


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.payload = self.root / "payload"
        artifact = self.payload / "native-linux-27"
        artifact.mkdir(parents=True)
        (artifact / "lib.so").write_bytes(b"tested-native-exact-version")
        self.manifest = {**deepcopy(IDENTITY), "gates": {"linux": "success", "macos": "success"},
                         "created_at": datetime.now(timezone.utc).isoformat(), "retention_days": 30,
                         "artifacts": {"native-linux-27": {**deepcopy(IDENTITY), "artifact": "native-linux-27", "job": "linux",
                                       "matrix": {"target": "x86_64-unknown-linux-gnu"}, "attempt": 1, "tools": {"rustc": "rustc 1.96.0"},
                                       "files": {"lib.so": C.digest(artifact / "lib.so")}}}}
        archive = self.root / "archive.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(artifact, arcname=artifact.name)
        self.archive = archive.read_bytes()
        self.manifest["bundle_sha256"] = hashlib.sha256(self.archive).hexdigest()

    def fetch(self, api):
        with patch.object(C, "identity", return_value=deepcopy(IDENTITY)), patch.object(C, "read_json", side_effect=lambda p: deepcopy(POLICY) if p == C.POLICY_PATH else json.loads(p.read_text())), patch.dict(os.environ, {"FORGEJO_REF": "refs/tags/v0.3.776"}):
            return C.fetch(self.root / "download", api)

    def test_valid_exact_candidate_round_trip(self):
        manifest = self.fetch(API(run_fixture(), self.manifest, self.archive))
        self.assertEqual(manifest["sha"], SHA)
        self.assertEqual((self.root / "download/payload/native-linux-27/lib.so").read_bytes(), b"tested-native-exact-version")

    def test_failed_cancelled_pending_or_skipped_runs_cannot_promote(self):
        for status in ("failure", "cancelled", "running", "waiting", "skipped", "blocked"):
            with self.subTest(status=status), self.assertRaises(ValueError):
                C.select_run([run_fixture(status=status)], SHA)

    def test_untrusted_or_mismatched_runs_cannot_promote(self):
        for update in ({"event": "pull_request"}, {"trigger_event": "pull_request"}, {"prettyref": "feature"},
                       {"repository": {"full_name": "attacker/aster-rpc-internal"}}, {"commit_sha": "c" * 40},
                       {"workflow_id": "untrusted.yml"}, {"is_fork_pull_request": True}, {"need_approval": True},
                       {"is_ref_deleted": True}, {"stopped": "0001-01-01T00:00:00Z"}):
            with self.subTest(update=update), self.assertRaises(ValueError):
                C.validate_run(run_fixture(**update), SHA)

    def test_no_fallback_to_older_green_or_duplicate_run(self):
        for runs in ([], [run_fixture(), run_fixture(id=43, status="failure")], [run_fixture(), run_fixture(id=43)]):
            with self.subTest(runs=runs), self.assertRaises(ValueError):
                C.select_run(runs, SHA)

    def test_failed_task_attempt_cannot_be_replaced_by_green_retry(self):
        task = {"run_number": 27, "workflow_id": "linux.yml", "head_sha": SHA,
                "event": "push", "head_branch": "main", "status": "success"}
        for status in ("failure", "cancelled", "running", "unknown"):
            with self.subTest(status=status), self.assertRaises(ValueError):
                C.validate_tasks([task, {**task, "status": status}], run_fixture())
        with self.assertRaises(ValueError):
            C.validate_tasks([], run_fixture())

    def test_failed_run_rejected_before_artifact_download(self):
        api = API(run_fixture(status="failure"), self.manifest, self.archive)
        with self.assertRaises(ValueError):
            self.fetch(api)
        self.assertEqual(len(api.reads), 1)

    def test_missing_expired_or_partial_artifact_listing(self):
        for files in ([{"name": "candidate.json", "sha256": "b" * 64}], [{"name": "other", "sha256": "c" * 64}]):
            with self.subTest(files=files), self.assertRaises(ValueError):
                self.fetch(API(run_fixture(), self.manifest, self.archive, files=files))

    def test_corrupt_registry_download_rejected(self):
        api = API(run_fixture(), self.manifest, self.archive)
        api.bytes["candidate.tar.gz"] = b"modified archive"
        with self.assertRaisesRegex(ValueError, "Forgejo checksum"):
            self.fetch(api)

    def test_expired_candidate_fails_closed(self):
        self.manifest["created_at"] = (datetime.now(timezone.utc) - timedelta(days=31)).isoformat()
        with self.assertRaisesRegex(ValueError, "expired"):
            self.fetch(API(run_fixture(), self.manifest, self.archive))

    def test_future_candidate_rejected(self):
        self.manifest["created_at"] = (datetime.now(timezone.utc) + timedelta(hours=2)).isoformat()
        with self.assertRaisesRegex(ValueError, "future"):
            self.fetch(API(run_fixture(), self.manifest, self.archive))

    def test_changed_run_after_download_rejected(self):
        with self.assertRaises(ValueError):
            self.fetch(API(run_fixture(), self.manifest, self.archive, final_run=run_fixture(status="cancelled")))

    def test_sha_version_run_and_build_inputs_must_all_match(self):
        for key, wrong in (("sha", "c" * 40), ("version", "0.3.775"), ("run_id", 43), ("run_number", 28),
                           ("repository", "attacker/repo"), ("source_hashes", {"Cargo.lock": "bad"})):
            for location in (self.manifest, self.manifest["artifacts"]["native-linux-27"]):
                changed = deepcopy(self.manifest)
                target = changed if location is self.manifest else changed["artifacts"]["native-linux-27"]
                target[key] = wrong
                with self.subTest(key=key, location=location is self.manifest), self.assertRaises(ValueError):
                    C.verify_payload(self.payload, changed, IDENTITY, POLICY)

    def test_platform_and_runtime_recipe_must_match(self):
        for key, wrong in (("job", "windows"), ("matrix", {"target": "aarch64-unknown-linux-gnu"}), ("attempt", 2), ("tools", {})):
            changed = deepcopy(self.manifest)
            changed["artifacts"]["native-linux-27"][key] = wrong
            with self.subTest(key=key), self.assertRaises(ValueError):
                C.verify_payload(self.payload, changed, IDENTITY, POLICY)

    def test_failed_missing_or_skipped_gate_cannot_seal(self):
        for needs in ({"linux": {"result": "success"}}, {"linux": {"result": "success"}, "macos": {"result": "failure"}},
                      {"linux": {"result": "success"}, "macos": {"result": "skipped"}}):
            with patch.object(C, "read_json", return_value=POLICY), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}), self.assertRaises(ValueError):
                C.seal(self.root / "handoff", self.root / "sealed", needs)

    def test_extra_or_missing_artifacts_and_files_cannot_promote(self):
        changed = deepcopy(self.manifest)
        changed["artifacts"] = {}
        with self.assertRaises(ValueError):
            C.verify_payload(self.payload, changed, IDENTITY, POLICY)
        (self.payload / "extra").mkdir()
        with self.assertRaises(ValueError):
            C.verify_payload(self.payload, self.manifest, IDENTITY, POLICY)
        (self.payload / "extra").rmdir()
        (self.payload / "native-linux-27/extra.so").write_bytes(b"unrecorded")
        with self.assertRaises(ValueError):
            C.verify_payload(self.payload, self.manifest, IDENTITY, POLICY)

    def test_modified_binary_and_symlink_rejected(self):
        library = self.payload / "native-linux-27/lib.so"
        library.write_bytes(b"replacement")
        with self.assertRaises(ValueError):
            C.verify_payload(self.payload, self.manifest, IDENTITY, POLICY)
        library.unlink()
        library.symlink_to("/etc/passwd")
        with self.assertRaises(ValueError):
            C.file_map(self.payload)

    def test_archive_traversal_symlinks_duplicates_rejected(self):
        for name, kind in (("../escape", tarfile.REGTYPE), ("/absolute", tarfile.REGTYPE), ("safe-link", tarfile.SYMTYPE)):
            archive = self.root / "evil.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.linkname = "/etc/passwd"
                bundle.addfile(info, io.BytesIO())
            with self.subTest(name=name), self.assertRaises(ValueError):
                C.safe_extract(archive, self.root / "evil-out")
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.addfile(tarfile.TarInfo("duplicate"), io.BytesIO())
            bundle.addfile(tarfile.TarInfo("duplicate"), io.BytesIO())
        with self.assertRaises(ValueError):
            C.safe_extract(archive, self.root / "evil-out")

    def test_wrong_release_tag_rejected_before_network(self):
        api = API(run_fixture(), self.manifest, self.archive)
        with patch.object(C, "identity", return_value=IDENTITY), patch.dict(os.environ, {"FORGEJO_REF": "refs/tags/v0.3.775"}), self.assertRaises(ValueError):
            C.fetch(self.root / "download", api)
        self.assertFalse(api.reads)

    def test_successful_candidate_seal_preserves_bytes_and_inventory(self):
        handoff = self.root / "handoff"
        handoff.mkdir()
        import shutil
        shutil.copytree(self.payload / "native-linux-27", handoff / "native-linux-27")
        evidence = handoff / "ci-evidence-linux-27"
        evidence.mkdir()
        (evidence / "native-linux-27.json").write_text(json.dumps(self.manifest["artifacts"]["native-linux-27"]))
        policy = self.root / "policy.json"
        policy.write_text(json.dumps(POLICY))
        with patch.object(C, "POLICY_PATH", policy), patch.object(C, "identity", return_value=IDENTITY), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}):
            C.seal(handoff, self.root / "sealed", {"linux": {"result": "success"}, "macos": {"result": "success"}})
        sealed = json.loads((self.root / "sealed/candidate.json").read_text())
        self.assertEqual(sealed["artifacts"], self.manifest["artifacts"])
        self.assertEqual(sealed["bundle_sha256"], C.digest(self.root / "sealed/candidate.tar.gz"))

    def test_workflow_policy_contains_every_required_gate(self):
        workflow = (C.ROOT / ".forgejo/workflows/linux.yml").read_text()
        policy = C.read_json(C.POLICY_PATH)
        jobs = set(re.findall(r"^  ([\w-]+):$", workflow[workflow.index("\njobs:"):], re.M))
        self.assertEqual(jobs, set(policy["required_jobs"]) | {"python-windows", "candidate"})
        for required in ("lint", "test", "coordination-fault-macos", "coordination-fault-windows", "nightly-sanitize", "nightly-miri", "ffi-go", "ffi-dotnet", "ffi-java", "ffi-macos", "typescript", "typescript-macos", "python-extended", "python-macos"):
            self.assertIn(required, policy["required_jobs"])
        self.assertEqual(workflow.count("name: Refuse retry-to-green candidate builds"), len(jobs))
        self.assertNotIn('tags: ["v*"]', workflow)
        self.assertIn('ASTER_REQUIRE_FAULT_MATRIX: "1"', workflow)
        self.assertIn("forgejo.event_name != 'pull_request' || steps.iroh.outputs.changed == 'true'", workflow)

    def test_promotion_has_no_binary_or_binding_rebuild_commands(self):
        workflow = (C.ROOT / ".forgejo/workflows/release.yml").read_text()
        for command in ("maturin build", "cargo build", "cargo test", "napi build", "mvn deploy ", "dotnet build", "dotnet pack", "npx tsc"):
            self.assertNotIn(command, workflow)
        self.assertLess(workflow.index("release-candidate.py fetch"), workflow.index("secrets.ASTER_PACKAGE_TOKEN"))
        self.assertIn("Publish Rust native dependency wave", workflow)
        self.assertIn("publish-native-stack.py publish", workflow)
        self.assertLess(workflow.index("Record completed promotion"), workflow.index("Publish Forgejo release after"))

    def test_nuget_conflict_fails_before_skip_duplicate(self):
        spec = importlib.util.spec_from_file_location("nuget", Path(__file__).with_name("verify-nuget.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        package = self.root / "Aster.0.3.776.nupkg"
        package.write_bytes(b"exact NuGet package")
        listing = [{"name": package.name, "sha256": "b" * 64}]
        with patch.object(module, "urlopen", return_value=io.BytesIO(json.dumps(listing).encode())), patch.dict(os.environ, {"FORGEJO_SERVER_URL": "https://forge.emrul.dev"}), self.assertRaises(ValueError):
            module.verify(package, "0.3.776", required=False)

    def test_maven_conflict_fails_before_deployment(self):
        spec = importlib.util.spec_from_file_location("maven", Path(__file__).with_name("maven-release.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        directory = self.root / "maven"
        directory.mkdir()
        classifier = self.root / "classifiers"
        classifier.mkdir()
        for name in ("aster-runtime-0.3.776.pom", "aster-runtime-0.3.776.jar"):
            (directory / name).write_bytes(b"exact tested Maven package")
        for target in ("linux-x86_64", "macos-aarch64"):
            (classifier / f"aster-runtime-0.3.776-{target}.jar").write_bytes(b"exact tested classifier")
        (directory / "packages.json").write_text(json.dumps([{"group": "site.aster", "artifact": "aster-runtime", "version": "0.3.776", "packaging": "jar", "pom": "aster-runtime-0.3.776.pom", "file": "aster-runtime-0.3.776.jar"}]))
        with patch.object(module, "remote_digest", return_value="b" * 64), patch.object(module.subprocess, "run") as deploy, patch.dict(os.environ, {"FORGEJO_SERVER_URL": "https://forge.emrul.dev"}), self.assertRaises(ValueError):
            module.deploy(directory, classifier)
        deploy.assert_not_called()

    def test_npm_publishes_the_packed_tarball_and_rejects_conflicts(self):
        if not __import__("shutil").which("node"):
            self.skipTest("Node is required to test the existing npm script")
        binaries = self.root / "bin"
        binaries.mkdir()
        state = self.root / "npm-state"
        fake = binaries / "npm"
        fake.write_text('''#!/usr/bin/env python3
import hashlib,json,os,pathlib,sys
args=sys.argv[1:]; state=pathlib.Path(os.environ["FAKE_NPM_STATE"])
if args[0]=="pack":
    output=pathlib.Path(args[args.index("--pack-destination")+1]); data=b"exact-package"
    (output/"fixture.tgz").write_bytes(data)
    print(json.dumps([{"filename":"fixture.tgz","shasum":hashlib.sha1(data).hexdigest()}]))
elif args[0]=="view":
    if state.exists(): print(json.dumps(state.read_text()))
    else: print(json.dumps({"error":{"code":"E404"}})); print("launcher: package manager exited 1"); sys.exit(1)
elif args[0]=="publish":
    data=pathlib.Path(args[1]).read_bytes(); state.write_text(hashlib.sha1(data).hexdigest())
else: sys.exit(2)
''')
        fake.chmod(0o755)
        package = self.root / "npm-fixture"
        package.mkdir()
        (package / "package.json").write_text(json.dumps({"name": "@aster-rpc/fixture", "version": "0.3.776"}))
        (self.root / "order").write_text("npm-fixture\n")
        environment = {**os.environ, "PATH": str(binaries) + os.pathsep + os.environ["PATH"], "NODE_AUTH_TOKEN": "offline-fixture", "FAKE_NPM_STATE": str(state)}
        command = ["bash", str(C.ROOT / "scripts/ci/npm-publish-all.sh"), "https://registry.example.invalid/", "latest", "order"]
        result = subprocess.run(command, cwd=self.root, env=environment, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.run(command, cwd=self.root, env=environment, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        state.write_text("another-checksum")
        result = subprocess.run(command, cwd=self.root, env=environment, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("another checksum", result.stderr)


if __name__ == "__main__":
    unittest.main()
