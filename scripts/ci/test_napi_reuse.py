#!/usr/bin/env python3
"""Normal Linux NAPI reuse qualification; fixtures never compile Rust."""
from copy import deepcopy
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("napi_candidate", Path(__file__).with_name("release-candidate.py"))
C = importlib.util.module_from_spec(spec)
spec.loader.exec_module(C)
ROOT = Path(__file__).resolve().parents[2]
IDENTITY = {"schema": 1, "repository": C.REPOSITORY, "sha": "a" * 40, "version": "0.3.900", "workflow": "linux.yml",
            "source_hashes": {"Cargo.lock": "b" * 64}, "run_id": 42, "run_number": 27, "assurance_profile": "standard"}
INPUTS = ("bindings/typescript/bun.lock", "bindings/typescript/package.json", "bindings/typescript/native/package.json",
          "bindings/typescript/scripts/check-native-generated.mjs", "scripts/ci/napi-loader.mjs")


class NapiReuse(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.payload = self.root / "input"
        self.payload.mkdir()
        for name in ("aster-transport.linux-x64-gnu.node", "index.js", "index.d.ts"):
            (self.payload / name).write_bytes(b"qualified-normal")
        self.recipe = {"schema": 1, "kind": "napi", "package": "aster-transport-napi", "profile": "release",
                       "default_features": True, "features": [], "target": "x86_64-unknown-linux-gnu", "target_mode": "explicit",
                       "rustc": "rustc 1.96\nhost: x86_64-unknown-linux-gnu", "cargo": "cargo 1.96",
                       "lock_sha256": "b" * 64, "environment": {},
                       "host_runtime": {"machine": "x86_64", "system": "linux", "libc": ["glibc", "2.39"]},
                       "generator": {"node": "v22", "bun": "1.3", "cli": "3.6.0", "cli_sha256": "c" * 64,
                                     "inputs": {name: "d" * 64 for name in INPUTS}}}
        self.evidence = {**deepcopy(IDENTITY), "artifact": "tested-napi-linux-27", "job": "test", "matrix": {},
                         "attempt": 1, "build_recipe": deepcopy(self.recipe), "files": C.file_map(self.payload)}

    def verify(self):
        with patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}), patch.object(C, "identity", return_value=IDENTITY), patch.object(C.subprocess, "check_output", return_value=IDENTITY["version"]):
            C.verify_input(self.payload, self.evidence, "tested-napi-linux-27", "test", self.recipe)

    def test_exact_source_recipe_and_payload_reused(self):
        self.verify()

    def test_source_toolchain_target_flags_and_generator_mismatch_rejected(self):
        for key, wrong in (("sha", "e" * 40), ("version", "0.3.899"), ("run_id", 43), ("run_number", 28), ("attempt", 2)):
            before = deepcopy(self.evidence)
            self.evidence[key] = wrong
            with self.subTest(identity=key), self.assertRaises(ValueError):
                self.verify()
            self.evidence = before
        for key, wrong in (("features", ["test-hooks"]), ("profile", "dev"), ("target", "aarch64-unknown-linux-gnu"),
                           ("target_mode", "host"), ("rustc", "different"), ("cargo", "different"),
                           ("lock_sha256", "e" * 64), ("environment", {"RUSTFLAGS": "-Ctarget-cpu=native"})):
            before = deepcopy(self.evidence)
            self.evidence["build_recipe"][key] = wrong
            with self.subTest(recipe=key), self.assertRaises(ValueError):
                self.verify()
            self.evidence = before
        for key in ("node", "bun", "cli", "cli_sha256", "inputs"):
            before = deepcopy(self.evidence)
            self.evidence["build_recipe"]["generator"][key] = {name: "e" * 64 for name in INPUTS} if key == "inputs" else "different"
            with self.subTest(generator=key), self.assertRaises(ValueError):
                self.verify()
            self.evidence = before

    def test_corrupt_missing_extra_and_symlink_outputs_rejected(self):
        for name in ("aster-transport.linux-x64-gnu.node", "index.js", "index.d.ts"):
            path = self.payload / name
            path.write_bytes(b"corrupt")
            with self.subTest(output=name), self.assertRaises(ValueError):
                self.verify()
            path.write_bytes(b"qualified-normal")
        (self.payload / "index.js").unlink()
        with self.assertRaises(ValueError):
            self.verify()
        (self.payload / "index.js").symlink_to(self.payload / "index.d.ts")
        with self.assertRaises(ValueError):
            self.verify()
        (self.payload / "index.js").unlink()
        (self.payload / "index.js").write_bytes(b"qualified-normal")
        (self.payload / "test-hooks.node").write_bytes(b"unpublished")
        # Even a matching digest inventory cannot expand the normal product.
        self.evidence["files"] = C.file_map(self.payload)
        with self.assertRaisesRegex(ValueError, "normal addon"):
            self.verify()

    def test_wrong_loaded_version_rejected(self):
        with patch.dict(os.environ, {"FORGEJO_RUN_ATTEMPT": "1"}), patch.object(C, "identity", return_value=IDENTITY), patch.object(C.subprocess, "check_output", return_value="0.3.899"):
            with self.assertRaisesRegex(ValueError, "NAPI native version"):
                C.verify_input(self.payload, self.evidence, "tested-napi-linux-27", "test", self.recipe)

    def test_recipe_requires_normal_explicit_host_and_complete_generator_inputs(self):
        for key, wrong in (("target_mode", "host"), ("features", ["test-hooks"]), ("features", ["coordination-fixture"]), ("default_features", False)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                C.validate_recipe({**self.recipe, key: wrong})
        bad = deepcopy(self.recipe)
        del bad["generator"]["inputs"][INPUTS[0]]
        with self.assertRaises(ValueError):
            C.validate_recipe(bad)

    def test_actual_pinned_generator_reproduces_baseline_and_stamps_only_version(self):
        if not shutil.which("node") or not (ROOT / "bindings/typescript/node_modules/@napi-rs/cli/dist/index.js").exists():
            self.skipTest("pinned JS dependencies unavailable; producer/packaging enforce this live")
        original = ROOT / "bindings/typescript/native/index.js"
        baseline = json.loads((ROOT / "bindings/typescript/native/package.json").read_text())["version"]
        output = self.root / "index.js"
        subprocess.run(["node", str(ROOT / "scripts/ci/napi-loader.mjs"), str(original), baseline, str(output)], check=True, timeout=30)
        self.assertEqual(output.read_bytes(), original.read_bytes())
        subprocess.run(["node", str(ROOT / "scripts/ci/napi-loader.mjs"), str(original), "0.3.900", str(output)], check=True, timeout=30)
        self.assertEqual(output.read_text().replace("0.3.900", baseline), original.read_text())
        bad = self.root / "bad.js"
        bad.write_text(original.read_text() + "\n// changed generator template\n")
        result = subprocess.run(["node", str(ROOT / "scripts/ci/napi-loader.mjs"), str(bad), "0.3.900", str(output)], capture_output=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"did not reproduce", result.stderr)

    def test_workflow_consumers_verify_before_loading_or_staging_and_hooks_stay_separate(self):
        workflow = (ROOT / ".forgejo/workflows/linux.yml").read_text()
        ts = workflow.split("\n  typescript:\n", 1)[1].split("\n  python-extended:", 1)[0]
        npm = workflow.split("\n  npm-native-linux-x64:\n", 1)[1].split("\n  npm-native-cross:", 1)[0]
        self.assertIn("needs: [rust-tests, test]", ts)
        self.assertLess(ts.index("verify-input --artifact"), ts.index("cp napi-input/*"))
        self.assertLess(ts.index("cp napi-input/*"), ts.index("run: bun run build:test-hooks"))
        self.assertNotIn("run: bun run build\n", ts)
        self.assertNotIn("napi build --platform --release --target", npm)
        self.assertLess(npm.index("verify-input --artifact"), npm.index("node scripts/ci/napi-loader.mjs"))
        self.assertLess(npm.index("node scripts/ci/napi-loader.mjs"), npm.index("npx napi create-npm-dirs"))


if __name__ == "__main__":
    unittest.main()
