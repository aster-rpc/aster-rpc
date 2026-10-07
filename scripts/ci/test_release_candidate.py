#!/usr/bin/env python3
"""Offline release guards: no credentials, builds, retries or registry writes."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import hashlib
import glob
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError

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
    def __init__(self, run, manifest, archive, *, final_run=None, files=None, tasks=None):
        self.run = run
        self.final_run = final_run or run
        self.bytes = {"candidate.json": json.dumps(manifest).encode(), "candidate.tar.gz": archive}
        self.files = files or [{"name": name, "sha256": hashlib.sha256(data).hexdigest()} for name, data in self.bytes.items()]
        self.reads = []
        self.tasks = tasks if tasks is not None else [{"run_number": 27, "workflow_id": "linux.yml", "head_sha": SHA,
                                                      "event": "push", "head_branch": "main", "status": "success"}]

    def json(self, path):
        self.reads.append(path)
        if "/runs?" in path:
            return {"workflow_runs": [self.run]}
        if "/tasks?" in path:
            return {"workflow_runs": self.tasks}
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
        triggers = workflow[workflow.index("\non:"):workflow.index("\nconcurrency:")]
        self.assertNotIn("paths:", triggers)
        self.assertNotIn("paths-ignore:", triggers)
        self.assertIn('cp "$CARGO_TARGET_DIR/$triple/release/libaster_transport_ffi.so"', workflow)
        self.assertIn('if ! python3 scripts/ci/release-candidate.py prune; then', workflow)

    def test_job_token_is_required_for_actions_and_not_sent_to_public_packages(self):
        with patch.dict(os.environ, {"ASTER_ACTIONS_TOKEN": "offline-job-token"}), patch.object(C, "urlopen", return_value=io.BytesIO(b"{}")) as http:
            api = C.Forgejo()
            api.request(f"/api/v1/repos/{C.REPOSITORY}/actions/runs?limit=1")
            self.assertEqual(http.call_args.args[0].get_header("Authorization"), "token offline-job-token")
            for path in ("/api/v1/packages/Aster/generic/aster-ci-candidate/key/files", "/api/packages/Aster/generic/aster-ci-candidate/key/candidate.json"):
                api.request(path)
                self.assertIsNone(http.call_args.args[0].get_header("Authorization"))
            api.token = "offline-package-token"
            api.request("/api/v1/packages/Aster/generic/aster-ci-candidate/key", method="DELETE")
            self.assertEqual(http.call_args.args[0].get_header("Authorization"), "token offline-package-token")
            api.token = ""
            http.reset_mock()
            with self.assertRaisesRegex(ValueError, "running job"):
                api.request(f"/api/v1/repos/{C.REPOSITORY}/actions/tasks")
            http.assert_not_called()

    def test_api_probe_and_auth_failure_are_explicit(self):
        api = API(run_fixture(), self.manifest, self.archive)
        C.check_api(api)
        self.assertEqual(len(api.reads), 2)
        for code in (401, 403, 404):
            with patch.dict(os.environ, {"ASTER_ACTIONS_TOKEN": "offline-job-token"}), patch.object(C, "urlopen", side_effect=HTTPError("offline", code, "denied", {}, None)), self.assertRaisesRegex(ValueError, f"HTTP {code}.*promotion remains blocked"):
                C.Forgejo().json(f"/api/v1/repos/{C.REPOSITORY}/actions/runs")

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


class IOSPackagingTests(unittest.TestCase):
    def run_packaging(self, directory, **extra):
        binaries = directory / "bin"
        binaries.mkdir()
        tool = '''#!/usr/bin/env python3
import json,os,pathlib,sys
name=pathlib.Path(sys.argv[0]).name; args=sys.argv[1:]
with open(os.environ["IOS_CALLS"],"a") as log: log.write(json.dumps([name,*args])+"\\n")
root=pathlib.Path(os.environ["CARGO_TARGET_DIR"])
if name=="cargo":
    assert args[0]=="rustc" and "--lib" in args and args[args.index("--crate-type")+1]=="staticlib"
    target=args[args.index("--target")+1]
    if target!=os.environ.get("IOS_MISSING_TARGET"):
        archive=root/target/"release/libaster_transport_ffi.a"
        archive.parent.mkdir(parents=True,exist_ok=True); archive.write_bytes(b"!<arch>\\nfixture archive")
elif name=="lipo":
    assert all(pathlib.Path(p).is_file() for p in args[1:args.index("-output")])
    pathlib.Path(args[args.index("-output")+1]).write_bytes(b"fat simulator archive")
elif name=="xcodebuild":
    assert args[0]=="-create-xcframework"
    assert all(pathlib.Path(args[i+1]).is_file() for i,a in enumerate(args) if a=="-library")
    output=pathlib.Path(args[args.index("-output")+1]); output.mkdir()
    if not os.environ.get("IOS_MISSING_METADATA"): (output/"Info.plist").write_bytes(b"xcframework metadata")
else: sys.exit(2)
'''
        for name in ("cargo", "lipo", "xcodebuild"):
            executable = binaries / name
            executable.write_text(tool)
            executable.chmod(0o755)
        environment = {**os.environ, "PATH": str(binaries) + os.pathsep + os.environ["PATH"],
                       "CARGO_TARGET_DIR": str(directory / "target"), "IOS_CALLS": str(directory / "calls"), **extra}
        result = subprocess.run(["bash", str(C.ROOT / "scripts/ci/build-ios-xcframework.sh"), str(directory / "AsterTransport.xcframework")],
                                cwd=directory, env=environment, capture_output=True, text=True)
        log = directory / "calls"
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return result, calls

    def test_real_workflow_requests_static_archives_for_all_ios_targets(self):
        workflow = (C.ROOT / ".forgejo/workflows/linux.yml").read_text()
        ios = workflow[workflow.index("  mobile-ios:"):workflow.index("  mobile-android:")]
        self.assertIn("run: scripts/ci/build-ios-xcframework.sh", ios)
        self.assertNotIn("cargo build", ios)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            result, calls = self.run_packaging(directory)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual([call[call.index("--target") + 1] for call in calls if call[0] == "cargo"],
                             ["aarch64-apple-ios", "aarch64-apple-ios-sim", "x86_64-apple-ios"])
            self.assertEqual([call[0] for call in calls], ["cargo", "cargo", "cargo", "lipo", "xcodebuild"])
            self.assertTrue((directory / "AsterTransport.xcframework/Info.plist").is_file())

    def test_missing_static_archive_fails_before_lipo(self):
        with tempfile.TemporaryDirectory() as temporary:
            result, calls = self.run_packaging(Path(temporary), IOS_MISSING_TARGET="aarch64-apple-ios-sim")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("missing iOS static archive", result.stdout)
            self.assertEqual([call[0] for call in calls], ["cargo", "cargo"])

    def test_missing_xcframework_metadata_fails_before_recording(self):
        with tempfile.TemporaryDirectory() as temporary:
            result, _ = self.run_packaging(Path(temporary), IOS_MISSING_METADATA="1")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("XCFramework metadata was not produced", result.stdout)


class FullPolicyHandoffTests(unittest.TestCase):
    """Exercise real workflow selections through every handoff stage.

    The upload simulation is deliberately independent of record(): directory
    selections flatten their selected root and globs retain paths below the
    search root, matching upload-artifact's documented behavior.
    """
    def test_all_artifacts_and_matrices_record_seal_fetch_and_materialize(self):
        repository = C.ROOT
        policy_path = C.POLICY_PATH
        policy = C.read_json(policy_path)
        workflow = (repository / ".forgejo/workflows/linux.yml").read_text()
        records = re.findall(r'--artifact "([^"]+)" --path "([^"]+)" --job "([^"]+)"', workflow)
        uploads = re.findall(r'name: ([^\n]+)\n          path: ([^\n]+)', workflow)
        identity = {**deepcopy(IDENTITY), "source_hashes": C.source_hashes()}
        suffix = "-27"
        platforms = ("linux-x64-gnu", "linux-arm64-gnu", "darwin-arm64", "darwin-x64", "win32-x64-msvc", "transport", "aster")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            handoff = root / "handoff"
            handoff.mkdir()
            for name, rule in policy["required_artifacts"].items():
                def expand(value):
                    return value.replace("${{ forgejo.run_number }}", "27").replace("${{ matrix.platform }}", rule["matrix"].get("platform", ""))
                selected = [(expand(a), expand(p), j) for a, p, j in records if expand(a) == name + suffix]
                self.assertEqual(len(selected), 1, name)
                artifact, selection, job = selected[0]
                self.assertEqual(job, rule["job"])
                self.assertIn((artifact, selection), [(expand(a), expand(p)) for a, p in uploads])
                workspace = root / ("job-" + name)
                workspace.mkdir()
                def write(path, data):
                    destination = workspace / path
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(data if isinstance(data, bytes) else data.encode())
                def package(directory, package_name):
                    write(directory + "/package.json", json.dumps({"name": "@aster-rpc/" + package_name, "version": identity["version"]}))
                    write(directory + "/dist/index.js", b"exact tested JavaScript")
                    if package_name not in ("transport", "aster"):
                        write(directory + "/aster.node", ("native-" + package_name).encode())
                if name.startswith("aster-python-"):
                    write("dist/" + name + "-0.3.776.whl", name)
                    if name == "aster-python-linux":
                        write("dist/aster_rpc-0.3.776.tar.gz", b"source distribution")
                elif name.startswith("ffi-native-"):
                    write(str(Path(selection).parent / ("aster_transport_ffi.dll" if "win" in name else "libaster_transport_ffi.dylib" if "osx" in name else "libaster_transport_ffi.so")), name)
                elif name.startswith("java-classifier-"):
                    write("staged/jar/aster-runtime-0.3.776-" + name.removeprefix("java-classifier-") + ".jar", name)
                elif name == "maven-packages":
                    write("staged/maven/aster-runtime-0.3.776.pom", b"exact tested POM")
                    write("staged/maven/aster-runtime-0.3.776.jar", b"exact tested JVM bytecode")
                    write("staged/maven/packages.json", "[]")
                elif name == "ffi-tarballs":
                    for target in ("linux-x64", "linux-arm64", "win-x64", "osx-arm64"):
                        write("dist/ffi/aster-ffi-0.3.776-" + target + ".tar.gz", target)
                    write("dist/ffi/SHA256SUMS", b"exact package checksums")
                elif name == "dotnet-nupkg":
                    write("dist/nuget/Aster.0.3.776.nupkg", b"exact tested NuGet archive")
                elif name == "npm-packages":
                    for platform in platforms:
                        package("artifacts/npm-" + platform, platform)
                elif name.startswith("npm-"):
                    package(selection.rstrip("/"), name.removeprefix("npm-"))
                elif name == "ios-xcframework":
                    write("AsterTransport.xcframework/Info.plist", b"xcframework metadata")
                    write("AsterTransport.xcframework/ios-arm64/libaster_transport_ffi.a", b"device library")
                    write("AsterTransport.xcframework/ios-arm64_x86_64-simulator/libaster_transport_ffi.a", b"simulator library")
                elif name == "android-libs":
                    for abi in ("arm64-v8a", "armeabi-v7a", "x86", "x86_64"):
                        write("android-libs/" + abi + "/libaster_transport_ffi.so", abi)
                else:
                    self.fail("fixture missing real policy artifact " + name)
                evidence_dir = handoff / ("ci-evidence-" + job + "-" + name)
                with patch.object(C, "ROOT", workspace), patch.object(C, "identity", return_value=identity), patch.object(C.shutil, "which", side_effect=lambda tool: tool if tool in ("rustc", "python3") else None), patch.object(C.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "offline pinned toolchain")), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}):
                    C.record(artifact, selection, evidence_dir, job, json.dumps(rule["matrix"]))
                # Independent upload/download layout: directories omit the
                # selected root; glob files use their fixed search parent.
                matches = [Path(p) for p in glob.glob(str(workspace / selection))]
                base = matches[0] if len(matches) == 1 and matches[0].is_dir() else (workspace / selection).parent
                uploaded = handoff / artifact
                uploaded.mkdir()
                for match in matches:
                    files = match.rglob("*") if match.is_dir() else [match]
                    for file in files:
                        if file.is_file():
                            destination = uploaded / file.relative_to(base)
                            destination.parent.mkdir(parents=True, exist_ok=True)
                            shutil.copyfile(file, destination)
            needs = {job: {"result": "success"} for job in policy["required_jobs"]}
            sealed = root / "sealed"
            with patch.object(C, "identity", return_value=identity), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}):
                C.seal(handoff, sealed, needs)
            manifest = C.read_json(sealed / "candidate.json")
            self.assertEqual(len(manifest["artifacts"]), len(policy["required_artifacts"]))
            self.assertEqual(manifest["artifacts"]["npm-linux-arm64-gnu-27"]["matrix"], policy["required_artifacts"]["npm-linux-arm64-gnu"]["matrix"])
            self.assertEqual(manifest["artifacts"]["npm-win32-x64-msvc-27"]["matrix"], policy["required_artifacts"]["npm-win32-x64-msvc"]["matrix"])
            downloaded = root / "downloaded"
            tasks = [{"run_number": 27, "workflow_id": "linux.yml", "head_sha": SHA, "event": "push", "head_branch": "main",
                      "status": "success", "name": job} for job in policy["required_jobs"]]
            tasks.append({**tasks[-1], "name": "npm-native-cross (win32-x64-msvc)"})
            api = API(run_fixture(), manifest, (sealed / "candidate.tar.gz").read_bytes(), tasks=tasks)
            release = root / "release"
            release.mkdir()
            with patch.object(C, "identity", return_value={**identity, "run_id": 99, "run_number": 28}), patch.object(C, "ROOT", release), patch.dict(os.environ, {"FORGEJO_REF": "refs/tags/v0.3.776"}):
                C.fetch(downloaded, api)
                C.materialize(downloaded)
            self.assertEqual(len(list((release / "dist/python").iterdir())), 5)
            self.assertEqual(len(list((release / "dist/ffi").iterdir())), 5)
            self.assertEqual(len(list((release / "jars").iterdir())), 2)
            self.assertEqual((release / "dist/nuget/Aster.0.3.776.nupkg").read_bytes(), b"exact tested NuGet archive")
            self.assertEqual((release / "dist/maven/aster-runtime-0.3.776.jar").read_bytes(), b"exact tested JVM bytecode")
            self.assertEqual((release / "publish-order").read_text().splitlines(), ["artifacts/npm-" + platform for platform in platforms])
            self.assertEqual(C.file_map(release / "artifacts"), manifest["artifacts"]["npm-packages-27"]["files"])
            for mobile in ("ios-xcframework", "android-libs"):
                self.assertEqual(C.file_map(release / "mobile" / mobile), manifest["artifacts"][mobile + suffix]["files"])
            self.assertEqual(C.read_json(release / "release-provenance/candidate.json"), manifest)
            # A downloader retaining the selected xcframework root instead of
            # flattening it must be rejected, even when every byte is intact.
            ios = downloaded / "payload/ios-xcframework-27"
            nested = ios / "AsterTransport.xcframework"
            nested.mkdir()
            for path in list(ios.iterdir()):
                if path != nested:
                    path.rename(nested / path.name)
            with self.assertRaisesRegex(ValueError, "checksum/inventory mismatch"):
                C.verify_payload(downloaded / "payload", manifest, identity, policy)


if __name__ == "__main__":
    unittest.main()
