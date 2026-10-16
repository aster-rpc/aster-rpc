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
            "assurance_profile": "standard", "workflow": "linux.yml", "source_hashes": {"Cargo.lock": "b" * 64}, "run_id": 42, "run_number": 27}
POLICY = {"assurance_profile": "standard", "optional_assurance_jobs": [], "full_assurance_artifacts": {}, "required_jobs": ["linux", "macos"], "required_artifacts": {"native-linux": {"job": "linux", "matrix": {"target": "x86_64-unknown-linux-gnu"}}}}


def run_fixture(**updates):
    return {"id": 42, "index_in_repo": 27, "repository": {"full_name": C.REPOSITORY},
            "commit_sha": SHA, "workflow_id": "linux.yml", "prettyref": "main", "event": "", "trigger_event": "workflow_dispatch",  # live API shape: event is empty for dispatches
            "is_fork_pull_request": False, "need_approval": False, "is_ref_deleted": False,
            "status": "success", "stopped": datetime.now(timezone.utc).isoformat(), **updates}


def archive_consumer_tasks():
    return [{"run_number": 27, "workflow_id": "linux.yml", "head_sha": SHA,
             "event": "workflow_dispatch", "head_branch": "main", "status": "success", "name": name}
            for name in sorted(C.FFI_ARCHIVE_CONSUMER_TASKS)]


class API:
    def __init__(self, run, manifest, archive, *, final_run=None, files=None, tasks=None):
        self.run = run
        self.final_run = final_run or run
        self.bytes = {"candidate.json": json.dumps(manifest).encode(), "candidate.tar.gz": archive}
        self.files = files or [{"name": name, "sha256": hashlib.sha256(data).hexdigest()} for name, data in self.bytes.items()]
        self.reads = []
        self.tasks = tasks if tasks is not None else archive_consumer_tasks()

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


class NativeReuseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.payload = self.root / "input"
        self.payload.mkdir()
        (self.payload / "libaster_transport_ffi.so").write_bytes(b"exact-native")
        self.recipe = {"schema": 1, "kind": "ffi", "package": "aster_transport_ffi", "profile": "release",
                       "default_features": True, "features": [], "target": "x86_64-unknown-linux-gnu", "target_mode": "host",
                       "rustc": "rustc 1.96.0\nhost: x86_64-unknown-linux-gnu", "cargo": "cargo 1.96.0",
                       "lock_sha256": "b" * 64, "environment": {},
                       "host_runtime": {"machine": "x86_64", "system": "linux", "libc": ["glibc", "2.39"]}}
        self.evidence = {**deepcopy(IDENTITY), "artifact": "native-linux-27", "job": "ffi-release-linux",
                         "attempt": 1, "matrix": {}, "build_recipe": deepcopy(self.recipe), "files": C.file_map(self.payload)}

    def verify(self):
        with patch.object(C, "identity", return_value=deepcopy(IDENTITY)), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}), patch.object(C.subprocess, "check_output", return_value=IDENTITY["version"]):
            C.verify_input(self.payload, self.evidence, "native-linux-27", "ffi-release-linux", self.recipe)

    def test_exact_intra_run_payload_is_reusable(self):
        self.verify()

    def test_record_embeds_recipe_and_rejects_lock_mismatch(self):
        output = self.root / "evidence"
        with patch.object(C, "identity", return_value=deepcopy(IDENTITY)), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}), patch.object(C.shutil, "which", return_value=None):
            C.record("native-linux-27", str(self.payload), output, "ffi-release-linux", "{}", self.recipe)
            recorded = C.read_json(output / "native-linux-27.json")
            self.assertEqual(recorded["build_recipe"], self.recipe)
            self.assertEqual(recorded["files"], self.evidence["files"])
            with self.assertRaisesRegex(ValueError, "lock"):
                C.record("native-linux-27", str(self.payload), output, "ffi-release-linux", "{}", {**self.recipe, "lock_sha256": "c" * 64})

    def test_matching_record_does_not_hide_stale_native_ffi_version(self):
        with patch.object(C, "identity", return_value=deepcopy(IDENTITY)), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}), patch.object(C.subprocess, "check_output", return_value="0.3.775"):
            with self.assertRaisesRegex(ValueError, "native version"):
                C.verify_input(self.payload, self.evidence, "native-linux-27", "ffi-release-linux", self.recipe)

    def test_old_commit_version_lock_run_or_producer_rejected_before_use(self):
        for key, wrong in (("sha", "c" * 40), ("version", "0.3.775"), ("run_id", 43), ("run_number", 28),
                           ("source_hashes", {"Cargo.lock": "bad"}), ("workflow", "old.yml"),
                           ("artifact", "other-27"), ("job", "ffi-java"), ("attempt", 2), ("matrix", {"target": "arm64"})):
            before = deepcopy(self.evidence)
            self.evidence[key] = wrong
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.verify()
            self.evidence = before

    def test_different_profile_features_target_flags_compiler_or_runtime_rejected(self):
        for key, wrong in (("profile", "dev"), ("features", ["coordination-fixture"]), ("target", "aarch64-unknown-linux-gnu"),
                           ("target_mode", "explicit"), ("rustc", "rustc different"), ("cargo", "cargo different"),
                           ("lock_sha256", "c" * 64), ("environment", {"RUSTFLAGS": "-C target-cpu=native"}),
                           ("host_runtime", {"machine": "x86_64", "system": "linux", "libc": ["glibc", "2.40"]})):
            before = deepcopy(self.evidence)
            self.evidence["build_recipe"][key] = wrong
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "recipe"):
                self.verify()
            self.evidence = before

    def test_missing_recipe_corrupt_extra_missing_or_symlink_payload_rejected(self):
        del self.evidence["build_recipe"]
        with self.assertRaises(ValueError):
            self.verify()
        self.evidence["build_recipe"] = deepcopy(self.recipe)
        library = self.payload / "libaster_transport_ffi.so"
        for mutation in (lambda: library.write_bytes(b"wrong"), lambda: library.unlink(),
                         lambda: library.symlink_to("/etc/passwd")):
            mutation()
            with self.assertRaises(ValueError):
                self.verify()
        library.unlink()
        library.write_bytes(b"exact-native")
        (self.payload / "unexpected").write_bytes(b"extra")
        with self.assertRaises(ValueError):
            self.verify()

    def test_consumer_retry_and_untyped_recipe_rejected(self):
        with patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "2"}), self.assertRaises(ValueError):
            C.verify_input(self.payload, self.evidence, "native-linux-27", "ffi-release-linux", self.recipe)
        for mutation in ({}, {**self.recipe, "extra": True}, {**self.recipe, "profile": "dev"},
                         {**self.recipe, "features": ["coordination-fixture"]}, {**self.recipe, "lock_sha256": "bad"}):
            with self.subTest(recipe=mutation), self.assertRaises(ValueError):
                C.validate_recipe(mutation)

    def test_recipe_captures_exact_interpreter_maturin_and_compiler_inputs(self):
        python = {"implementation": "cpython", "version": "3.13.8", "cache_tag": "cpython-313", "machine": "x86_64", "libc": ["glibc", "2.39"]}
        def command(args, **kwargs):
            return {"rustc": self.recipe["rustc"], "cargo": self.recipe["cargo"],
                    "python": json.dumps(python), "maturin": "maturin 1.9.6"}[args[0]]
        with patch.object(C.subprocess, "check_output", side_effect=command), patch.object(C, "source_hashes", return_value={"Cargo.lock": "b" * 64}), patch.dict(os.environ, {"RUSTFLAGS": "-C debuginfo=1", "CARGO_PROFILE_RELEASE_LTO": "thin"}, clear=True):
            recipe = C.build_recipe("python-wheel", "aster_rs", "release", "pyo3/extension-module", "host", "python", "maturin")
        self.assertEqual(recipe["python"], python)
        self.assertEqual(recipe["maturin"], "maturin 1.9.6")
        self.assertEqual(recipe["python_abi"], "abi3-py39")
        self.assertEqual(recipe["environment"]["RUSTFLAGS"], "-C debuginfo=1")
        self.assertEqual(recipe["environment"]["CARGO_PROFILE_RELEASE_LTO"], "thin")

    def test_python_recipe_requires_all_abi_runtime_fields(self):
        recipe = {**self.recipe, "kind": "python-wheel", "package": "aster_rs", "features": ["pyo3/extension-module"],
                  "python_abi": "abi3-py39", "python": {"implementation": "cpython", "version": "3.13.8", "cache_tag": "cpython-313", "machine": "x86_64", "libc": ["glibc", "2.39"]}, "maturin": "maturin 1.9.6"}
        C.validate_recipe(recipe)
        for key, wrong in (("python_abi", "cp313"), ("python", {}), ("maturin", "")):
            with self.subTest(key=key), self.assertRaises(ValueError):
                C.validate_recipe({**recipe, key: wrong})

    def test_distinct_disposable_output_paths_do_not_change_recipe_but_linker_and_flags_do(self):
        def command(args, **kwargs):
            return self.recipe["rustc"] if args[0] == "rustc" else self.recipe["cargo"]
        def recipe(environment):
            with patch.object(C.subprocess, "check_output", side_effect=command), patch.object(C, "source_hashes", return_value={"Cargo.lock": "b" * 64}), patch.dict(os.environ, environment, clear=True):
                return C.build_recipe("ffi", "aster_transport_ffi", "release", "", "host", None, None)
        producer = recipe({"CARGO_TARGET_DIR": "/cache/targets/producer"})
        self.assertEqual(producer, recipe({"CARGO_TARGET_DIR": "/cache/targets/consumer"}))
        for environment in ({"CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER": "different-linker"}, {"RUSTFLAGS": "-C target-cpu=native"}):
            with self.subTest(environment=environment):
                self.assertNotEqual(producer, recipe(environment))

    def test_installed_package_and_native_must_match_wheel_bytes_and_exact_version(self):
        import zipfile
        package = self.root / "site-packages/aster"
        package.mkdir(parents=True)
        (package / "__init__.py").write_bytes(b"python-package")
        (package / "_aster.so").write_bytes(b"native-payload")
        wheel = self.root / "aster.whl"
        with zipfile.ZipFile(wheel, "w") as archive:
            for path in package.iterdir():
                archive.write(path, "aster/" + path.name)
        installed = {"package": str(package / "__init__.py"), "distribution_package": str(package / "__init__.py"),
                     "native": str(package / "_aster.so"), "version": IDENTITY["version"],
                     "native_version": IDENTITY["version"], "distribution_version": IDENTITY["version"]}
        with patch.object(C, "identity", return_value=IDENTITY), patch.object(C.subprocess, "check_output", side_effect=lambda *a, **k: json.dumps(installed)):
            C.verify_python_install(wheel, "python")
            for key in ("version", "native_version", "distribution_version"):
                installed[key] = "0.3.775"
                with self.subTest(key=key), self.assertRaises(ValueError):
                    C.verify_python_install(wheel, "python")
                installed[key] = IDENTITY["version"]
            installed["distribution_package"] = str(self.root / "shadowed/__init__.py")
            with self.assertRaisesRegex(ValueError, "shadows"):
                C.verify_python_install(wheel, "python")
            installed["distribution_package"] = installed["package"]
            for name in ("__init__.py", "_aster.so"):
                path = package / name
                before = path.read_bytes()
                path.write_bytes(b"replacement")
                with self.subTest(name=name), self.assertRaisesRegex(ValueError, "payload differs"):
                    C.verify_python_install(wheel, "python")
                path.write_bytes(before)


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
                         "disabled_assurance_jobs": [], "created_at": datetime.now(timezone.utc).isoformat(), "retention_days": 30,
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

    def test_gate_push_run_is_never_a_candidate(self):
        gate = run_fixture(id=41, index_in_repo=26, event="push", trigger_event="push")
        with self.assertRaises(ValueError):
            C.validate_run(gate, SHA)
        with self.assertRaises(ValueError):
            C.select_run([gate], SHA)
        # The green gate run of the same SHA is neither selected nor counted
        # as a duplicate of the dispatch candidate.
        self.assertEqual(C.select_run([gate, run_fixture()], SHA)["id"], 42)
        with self.assertRaises(ValueError):
            C.select_run([gate, run_fixture(), run_fixture(id=43, index_in_repo=28)], SHA)
        # Gate tasks of the same SHA never qualify a candidate run.
        gate_tasks = [{**task, "event": "push"} for task in archive_consumer_tasks()]
        with self.assertRaisesRegex(ValueError, "untrusted event"):
            C.validate_tasks(gate_tasks, run_fixture())

    def test_no_fallback_to_older_green_or_duplicate_run(self):
        for runs in ([], [run_fixture(), run_fixture(id=43, status="failure")], [run_fixture(), run_fixture(id=43)]):
            with self.subTest(runs=runs), self.assertRaises(ValueError):
                C.select_run(runs, SHA)

    def test_failed_task_attempt_cannot_be_replaced_by_green_retry(self):
        task = {"run_number": 27, "workflow_id": "linux.yml", "head_sha": SHA,
                "event": "workflow_dispatch", "head_branch": "main", "status": "success"}
        for status in ("failure", "cancelled", "running", "unknown"):
            with self.subTest(status=status), self.assertRaises(ValueError):
                C.validate_tasks([task, {**task, "status": status}], run_fixture())
        with self.assertRaises(ValueError):
            C.validate_tasks([], run_fixture())

    def test_archive_consumer_matrix_requires_unique_success_for_every_target(self):
        tasks = archive_consumer_tasks()
        C.validate_tasks(tasks, run_fixture())
        # Other skipped assurance tasks retain their established semantics.
        C.validate_tasks(tasks + [{**tasks[0], "name": "nightly-miri", "status": "skipped"}], run_fixture())
        for index in range(len(tasks)):
            for status in ("skipped", "failure", "cancelled", "running"):
                invalid = deepcopy(tasks)
                invalid[index]["status"] = status
                with self.subTest(index=index, status=status), self.assertRaises(ValueError):
                    C.validate_tasks(invalid, run_fixture())
            for invalid in (tasks[:index] + tasks[index + 1:], tasks + [tasks[index]],
                            tasks[:index] + [{**tasks[index], "head_sha": "c" * 40}] + tasks[index + 1:]):
                with self.subTest(index=index, tasks=invalid), self.assertRaisesRegex(ValueError, "archive consumer"):
                    C.validate_tasks(invalid, run_fixture())

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
                           ("repository", "attacker/repo"), ("source_hashes", {"Cargo.lock": "bad"}), ("assurance_profile", "full")):
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

    def test_profile_disabled_gates_are_explicit_and_cannot_mask_failure(self):
        policy = {**deepcopy(POLICY), "optional_assurance_jobs": ["extreme"]}
        manifest = deepcopy(self.manifest)
        manifest["gates"]["extreme"] = "skipped"
        manifest["disabled_assurance_jobs"] = ["extreme"]
        C.verify_payload(self.payload, manifest, IDENTITY, policy)
        for status in ("failure", "cancelled", "success", "running"):
            changed = deepcopy(manifest)
            changed["gates"]["extreme"] = status
            with self.subTest(status=status), self.assertRaises(ValueError):
                C.verify_payload(self.payload, changed, IDENTITY, policy)
        changed = deepcopy(manifest)
        changed["disabled_assurance_jobs"] = []
        with self.assertRaises(ValueError):
            C.verify_payload(self.payload, changed, IDENTITY, policy)
        full = {**policy, "assurance_profile": "full"}
        full_identity = {**IDENTITY, "assurance_profile": "full"}
        manifest["assurance_profile"] = "full"
        manifest["artifacts"]["native-linux-27"]["assurance_profile"] = "full"
        manifest["disabled_assurance_jobs"] = []
        for status in ("skipped", "failure", "cancelled"):
            manifest["gates"]["extreme"] = status
            with self.subTest(full_status=status), self.assertRaises(ValueError):
                C.verify_payload(self.payload, manifest, full_identity, full)
        manifest["gates"]["extreme"] = "success"
        C.verify_payload(self.payload, manifest, full_identity, full)

    def test_profile_workflow_conditions_and_mobile_publication(self):
        policy = C.read_json(C.POLICY_PATH)
        workflow = (C.ROOT / ".forgejo/workflows/linux.yml").read_text()
        for job in policy["optional_assurance_jobs"]:
            block = re.search(r"^  " + job + r":\n.*?(?=^  [\w-]+:|\Z)", workflow, re.M | re.S).group()
            self.assertIn("needs.lint.outputs.assurance_profile == 'full'", block)
            self.assertIn("lint", re.search(r"needs: (.*)", block).group(1))
        self.assertIn("if: always() && forgejo.event_name", workflow[workflow.index("  candidate:"):])
        self.assertEqual(len(policy["required_jobs"]), 21)
        self.assertEqual(len(C.artifact_policy(policy)), 21)
        self.assertEqual(len(C.artifact_policy({**policy, "assurance_profile": "full"})), 23)
        promotion = (C.ROOT / ".forgejo/workflows/release.yml").read_text()
        self.assertIn("Retain promoted mobile artifacts\n        if: env.ASTER_ASSURANCE_PROFILE == 'full'", promotion)
        with self.assertRaises(ValueError):
            C.expected_gates({**policy, "assurance_profile": "unknown"})

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
        # Intra-run wheel handoff is verified by consumers, but is not a second
        # release product. Nested recipe sidecars are not artifact attestations.
        internal = handoff / "tested-python-linux-27"
        internal.mkdir()
        (internal / "aster.whl").write_bytes(b"tested-wheel")
        internal_evidence = handoff / "ci-evidence-test-27"
        internal_evidence.mkdir()
        (internal_evidence / "tested-python-linux-27.json").write_text(json.dumps(
            {**IDENTITY, "artifact": "tested-python-linux-27", "job": "test", "attempt": 1,
             "files": {"aster.whl": C.digest(internal / "aster.whl")}}))
        (internal_evidence / "recipes").mkdir()
        (internal_evidence / "recipes/python-wheel.json").write_text(json.dumps({"schema": 1}))
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
        self.assertEqual(jobs, set(policy["required_jobs"]) | set(policy["optional_assurance_jobs"]) | {"python-windows", "candidate"})
        for required in ("lint", "rust-tests", "test", "coordination-fault-macos", "coordination-fault-windows", "nightly-sanitize", "nightly-miri", "ffi-go", "ffi-dotnet", "ffi-java", "ffi-macos", "typescript", "typescript-macos", "python-extended", "python-macos"):
            self.assertIn(required, policy["required_jobs"] + policy["optional_assurance_jobs"])
        self.assertEqual(workflow.count("name: Refuse retry-to-green candidate builds"), len(jobs))
        self.assertNotIn('tags: ["v*"]', workflow)
        self.assertIn('ASTER_REQUIRE_FAULT_MATRIX: "1"', workflow)
        self.assertIn("forgejo.event_name != 'pull_request' || steps.iroh.outputs.changed == 'true'", workflow)
        triggers = workflow[workflow.index("\non:"):workflow.index("\nconcurrency:")]
        self.assertNotIn("paths:", triggers)
        self.assertNotIn("paths-ignore:", triggers)
        self.assertIn('cp "$CARGO_TARGET_DIR/$triple/release/libaster_transport_ffi.so"', workflow)
        self.assertIn('if ! python3 scripts/ci/release-candidate.py prune; then', workflow)

    def test_push_runs_the_gate_and_only_a_main_dispatch_is_a_candidate(self):
        workflow = (C.ROOT / ".forgejo/workflows/linux.yml").read_text()
        policy = C.read_json(C.POLICY_PATH)
        body = workflow[workflow.index("\njobs:"):]
        matches = list(re.finditer(r"^  ([\w-]+):$", body, re.M))
        blocks = {m[1]: body[m.end():matches[i + 1].start() if i + 1 < len(matches) else len(body)]
                  for i, m in enumerate(matches)}
        job_if = {job: (re.search(r"^    if: (.+)$", block, re.M) or [None, None])[1] for job, block in blocks.items()}
        gate = {"lint", "rust-tests", "test", "typescript", "ffi-go"}
        # Gate jobs run on every event, so a push to main qualifies correctness.
        for job in gate:
            self.assertIsNone(job_if[job], job)
            self.assertIn(job, policy["required_jobs"])
        # Every producer and assurance job is skipped on a push to main.
        for job in set(blocks) - gate - {"candidate"}:
            self.assertTrue("forgejo.event_name != 'push'" in job_if[job]
                            or "forgejo.event_name == 'workflow_dispatch'" in job_if[job], job)
        self.assertEqual(job_if["candidate"], "always() && forgejo.event_name == 'workflow_dispatch' && forgejo.ref == 'refs/heads/main'")
        candidate_needs = set(re.findall(r"[\w-]+", re.search(r"^    needs: (.+)$", blocks["candidate"], re.M)[1]))
        self.assertEqual(candidate_needs, set(policy["required_jobs"]) | set(policy["optional_assurance_jobs"]))
        # First-attempt rule binds the candidate dispatch, in every job.
        guard = 'if [[ "$FORGEJO_EVENT_NAME" == workflow_dispatch && "$FORGEJO_REF" == refs/heads/main ]]; then'
        self.assertEqual(workflow.count(guard), len(blocks))
        # The old push-based first-attempt guard is gone ...
        self.assertNotIn('"$FORGEJO_EVENT_NAME" == push && "$FORGEJO_REF"', workflow)
        # ... and a push builds unstamped only in the three gate jobs that build
        # the tree; release-candidate.py refuses an unstamped build off a push.
        unstamped = {job for job, block in blocks.items() if 'ASTER_UNSTAMPED_VERSION=' in block}
        self.assertEqual(unstamped, {"rust-tests", "test", "typescript"})
        # The native version must match too, or verify-python-install fails.
        for job in unstamped:
            self.assertIn('echo "ASTER_BUILD_VERSION=$unstamped"', blocks[job])
        # The dispatch names its commit and lint refuses any other checkout.
        self.assertIn("  workflow_dispatch:\n    inputs:\n      sha:", workflow)
        self.assertIn('CANDIDATE_SHA: ${{ forgejo.event.inputs.sha }}', blocks["lint"])
        self.assertIn('"$CANDIDATE_SHA" == "$FORGEJO_SHA"', blocks["lint"])
        self.assertIn("group: forgejo-aster-${{ forgejo.event_name }}-${{ forgejo.sha }}", workflow)
        # Gate builds are debug and never recorded; recorded inputs are release.
        test = blocks["test"]
        self.assertIn("TESTED_PROFILE: ${{ forgejo.event_name == 'push' && 'debug' || 'release' }}", test)
        for step in ("Record tested normal Linux NAPI inputs", "Record tested Linux wheel inputs"):
            self.assertIn(f"- name: {step}\n        if: forgejo.event_name != 'push'\n", test)
        self.assertEqual(test.count('[[ "$TESTED_PROFILE" == release ]] || {'), 2)
        for name in ("tested-napi-linux-", "tested-python-linux-", "ci-evidence-test-"):
            upload = test.index(f"name: {name}" + "${{ forgejo.run_number }}")
            self.assertIn("if: forgejo.event_name != 'push'", test[test.rindex("- uses:", 0, upload):upload])
        self.assertNotIn("--release --locked", test)

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
args=sys.argv[1:]; state=pathlib.Path(os.environ["FAKE_NPM_STATE"]); tags=state.with_suffix(".tags")
if args[0]=="pack":
    output=pathlib.Path(args[args.index("--pack-destination")+1]); data=b"exact-package"
    (output/"fixture.tgz").write_bytes(data)
    print(json.dumps([{"filename":"fixture.tgz","shasum":hashlib.sha1(data).hexdigest()}]))
elif args[0]=="view":
    if len(args)>2 and args[2]=="dist-tags": print(tags.read_text() if tags.exists() else "{}")
    elif state.exists(): print(json.dumps(state.read_text()))
    else: print(json.dumps({"error":{"code":"E404"}})); print("launcher: package manager exited 1"); sys.exit(1)
elif args[0]=="publish":
    data=pathlib.Path(args[1]).read_bytes(); state.write_text(hashlib.sha1(data).hexdigest())
    current=json.loads(tags.read_text()) if tags.exists() else {}; current[args[args.index("--tag")+1]]="0.3.776"; tags.write_text(json.dumps(current))
elif args[0]=="dist-tag":
    current=json.loads(tags.read_text()) if tags.exists() else {}
    if args[1]=="add": current[args[3]]=args[2].rsplit("@",1)[1]
    elif args[1]=="rm": current.pop(args[3],None)
    else: sys.exit(2)
    tags.write_text(json.dumps(current))
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
    def test_standard_artifacts_record_seal_fetch_and_materialize(self):
        self.round_trip("standard")

    def test_full_artifacts_record_seal_fetch_and_materialize(self):
        self.round_trip("full")

    def round_trip(self, profile):
        repository = C.ROOT
        policy_path = C.POLICY_PATH
        policy = {**C.read_json(policy_path), "assurance_profile": profile}
        selected_rules = C.artifact_policy(policy)
        original_read = C.read_json
        def read_policy(path):
            return deepcopy(policy) if path == policy_path else original_read(path)
        workflow = (repository / ".forgejo/workflows/linux.yml").read_text()
        records = re.findall(r'--artifact "([^"]+)" --path "([^"]+)" --job "([^"]+)"', workflow)
        uploads = re.findall(r'name: ([^\n]+)\n          path: ([^\n]+)', workflow)
        identity = {**deepcopy(IDENTITY), "assurance_profile": profile, "source_hashes": C.source_hashes()}
        suffix = "-27"
        platforms = ("linux-x64-gnu", "linux-arm64-gnu", "darwin-arm64", "darwin-x64", "win32-x64-msvc", "transport", "aster")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            handoff = root / "handoff"
            handoff.mkdir()
            for name, rule in selected_rules.items():
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
                elif name == "relay-image":
                    write("staged/relay/aster-relay-0.3.776.oci.tar", b"exact assembled OCI layout")
                    write("staged/relay/aster-relay-0.3.776.json", '{"index_digest": "sha256:fixture"}')
                    write("staged/relay/sbom-linux-amd64.cdx.json", "{}")
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
            needs = {job: {"result": result} for job, result in C.expected_gates(policy).items()}
            sealed = root / "sealed"
            with patch.object(C, "read_json", side_effect=read_policy), patch.object(C, "identity", return_value=identity), patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}):
                C.seal(handoff, sealed, needs)
            manifest = C.read_json(sealed / "candidate.json")
            self.assertEqual(len(manifest["artifacts"]), len(selected_rules))
            self.assertEqual(manifest["artifacts"]["npm-linux-arm64-gnu-27"]["matrix"], policy["required_artifacts"]["npm-linux-arm64-gnu"]["matrix"])
            self.assertEqual(manifest["artifacts"]["npm-win32-x64-msvc-27"]["matrix"], policy["required_artifacts"]["npm-win32-x64-msvc"]["matrix"])
            downloaded = root / "downloaded"
            tasks = [{"run_number": 27, "workflow_id": "linux.yml", "head_sha": SHA, "event": "workflow_dispatch", "head_branch": "main",
                      "status": result, "name": job} for job, result in C.expected_gates(policy).items()]
            tasks.extend(archive_consumer_tasks())
            tasks.append({**tasks[0], "status": "success", "name": "npm-native-cross (win32-x64-msvc)"})
            api = API(run_fixture(), manifest, (sealed / "candidate.tar.gz").read_bytes(), tasks=tasks)
            release = root / "release"
            release.mkdir()
            (release / "mobile/stale").mkdir(parents=True)
            (release / "mobile/stale/old.a").write_bytes(b"old mobile product")
            with patch.object(C, "read_json", side_effect=read_policy), patch.object(C, "identity", return_value={**identity, "run_id": 99, "run_number": 28}), patch.object(C, "ROOT", release), patch.dict(os.environ, {"FORGEJO_REF": "refs/tags/v0.3.776"}):
                C.fetch(downloaded, api)
                C.materialize(downloaded)
            self.assertEqual(len(list((release / "dist/python").iterdir())), 5)
            self.assertEqual(len(list((release / "dist/ffi").iterdir())), 5)
            self.assertEqual(len(list((release / "jars").iterdir())), 2)
            self.assertEqual((release / "dist/nuget/Aster.0.3.776.nupkg").read_bytes(), b"exact tested NuGet archive")
            self.assertEqual((release / "dist/maven/aster-runtime-0.3.776.jar").read_bytes(), b"exact tested JVM bytecode")
            self.assertEqual(C.file_map(release / "dist/relay"), manifest["artifacts"]["relay-image" + suffix]["files"])
            self.assertEqual((release / "publish-order").read_text().splitlines(), ["artifacts/npm-" + platform for platform in platforms])
            self.assertEqual(C.file_map(release / "artifacts"), manifest["artifacts"]["npm-packages-27"]["files"])
            self.assertFalse((release / "mobile/stale").exists())
            for mobile in ("ios-xcframework", "android-libs"):
                if profile == "full":
                    self.assertEqual(C.file_map(release / "mobile" / mobile), manifest["artifacts"][mobile + suffix]["files"])
                else:
                    self.assertFalse((release / "mobile" / mobile).exists())
            self.assertEqual(C.read_json(release / "release-provenance/candidate.json"), manifest)
            # A downloader retaining the selected xcframework root instead of
            # flattening it must be rejected, even when every byte is intact.
            if profile != "full":
                return
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
