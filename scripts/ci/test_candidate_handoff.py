#!/usr/bin/env python3
"""Offline fixtures for REL-01 transport and exact-run handoff failure modes."""
import base64
from copy import deepcopy
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("handoff", Path(__file__).with_name("candidate-handoff.py"))
H = importlib.util.module_from_spec(spec)
spec.loader.exec_module(H)
C = H.C
IDENTITY = {"schema": 1, "repository": C.REPOSITORY, "sha": "a" * 40, "version": "0.3.27",
            "workflow": "linux.yml", "source_hashes": {"Cargo.lock": "b" * 64},
            "run_id": 31, "run_number": 27, "assurance_profile": "standard"}
POLICY = {"assurance_profile": "standard", "required_jobs": ["producer"], "optional_assurance_jobs": [],
          "required_artifacts": {"product": {"job": "producer", "matrix": {}}}}


def zipped(files):
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w") as archive:
        for name, data in files.items():
            archive.writestr(name, data)
    return output.getvalue()


class HandoffTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def transport(self, replies):
        def run(args, **kwargs):
            status, mime, content, code = replies.pop(0)
            Path(args[args.index("--output") + 1]).write_bytes(content)
            self.assertIn("--connect-timeout", args)
            self.assertIn("--max-time", args)
            self.assertNotIn("secret", " ".join(args))
            return subprocess.CompletedProcess(args, code, f"{status}\n{mime}\n{len(content)}", "")
        return patch.object(H.subprocess, "run", side_effect=run)

    def test_html_retry_is_bounded_and_diagnosable(self):
        replies = [(200, "text/html", b"<html>secret</html>", 0)] * 3
        with self.transport(replies), patch.object(H.time, "sleep"):
            with self.assertRaisesRegex(ValueError, "three attempts"):
                H.Transport(self.root / "log").get("https://forge.emrul.dev/rpc?secret", self.root / "response", token="secret", data={})
        logs = (self.root / "log").read_text()
        self.assertEqual(len(logs.splitlines()), 3)
        self.assertIn('"content_type": "text/html"', logs)
        self.assertNotIn("secret", logs)

    def test_html_then_json_transport_recovery(self):
        with self.transport([(200, "text/html", b"<html>", 0), (200, "application/json", b'{"artifacts": []}', 0)]), patch.object(H.time, "sleep"):
            result = H.Transport(self.root / "log").get("https://forge.emrul.dev/rpc", self.root / "response", data={})
        self.assertEqual(result, {"artifacts": []})

    def test_missing_is_not_retried(self):
        with self.transport([(404, "text/html", b"missing", 0)]), patch.object(H.time, "sleep"):
            with self.assertRaisesRegex(ValueError, "HTTP 404"):
                H.Transport(self.root / "log").get("https://forge.emrul.dev/blob", self.root / "response")

    def test_truncated_transfer_cannot_complete(self):
        with self.transport([(200, "application/zip", b"PKbroken", 18)] * 3), patch.object(H.time, "sleep"):
            with self.assertRaisesRegex(ValueError, "three attempts"):
                H.Transport(self.root / "log").get("https://forge.emrul.dev/blob", self.root / "response")

    def test_overall_budget_prevents_another_request(self):
        transport = H.Transport(self.root / "log")
        transport.deadline = 0
        with patch.object(H.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "480-second"):
                transport.get("https://forge.emrul.dev/blob", self.root / "response")
            run.assert_not_called()

    def listing(self):
        return {"artifacts": [{"name": n, "workflowRunBackendId": "31", "workflowJobRunBackendId": "consumer-id", "databaseId": str(i)}
                              for i, n in enumerate(("product-27", "ci-evidence-producer-27", "unrelated-27"), 1)]}

    def test_selection_only_exact_products_and_attestations(self):
        selected = H.selection(self.listing(), POLICY, IDENTITY, "31")
        self.assertEqual({e["name"] for e in selected}, {"product-27", "ci-evidence-producer-27"})
        for mutation in ("missing", "duplicate", "wrong-run"):
            listing = self.listing()
            if mutation == "missing":
                listing["artifacts"].pop(0)
            elif mutation == "duplicate":
                listing["artifacts"].append(listing["artifacts"][0])
            else:
                listing["artifacts"][0]["workflowRunBackendId"] = "another-run"
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                H.selection(listing, POLICY, IDENTITY, "31")

    def test_conflicting_wire_identity_fields_rejected(self):
        listing = self.listing()
        listing["artifacts"][0]["workflow_run_backend_id"] = "other"
        with self.assertRaisesRegex(ValueError, "conflicting protobuf"):
            H.selection(listing, POLICY, IDENTITY, "31")

    def test_backend_scope_identity(self):
        payload = base64.urlsafe_b64encode(json.dumps({"scp": "Other Actions.Results:run:job"}).encode()).decode().rstrip("=")
        self.assertEqual(H.backend_ids("x." + payload + ".x"), {"workflow_run_backend_id": "run", "workflow_job_run_backend_id": "job"})
        with self.assertRaises(ValueError):
            H.backend_ids("invalid")

    def test_unsafe_and_corrupt_zip(self):
        archive = self.root / "archive.zip"
        archive.write_bytes(zipped({"../escape": b"bad"}))
        with self.assertRaises(ValueError):
            H.extract(archive, self.root / "payload")
        archive.write_bytes(b"truncated")
        with self.assertRaises(zipfile.BadZipFile):
            H.extract(archive, self.root / "payload")

    def test_retrieval_validates_hashes_before_exposing_handoff(self):
        data = b"qualified"
        import hashlib
        evidence = {**deepcopy(IDENTITY), "artifact": "product-27", "job": "producer", "matrix": {}, "attempt": 1,
                    "tools": {"rustc": "1.90"}, "files": {"native": hashlib.sha256(data).hexdigest()}}
        payload = base64.urlsafe_b64encode(json.dumps({"scp": "Actions.Results:31:job"}).encode()).decode().rstrip("=")
        env = {"FORGEJO_RUN_ATTEMPT": "1", "CANDIDATE_NEEDS": '{"producer":{"result":"success"}}',
               "ACTIONS_RUNTIME_TOKEN": "x." + payload + ".x", "ACTIONS_RESULTS_URL": "https://forge.emrul.dev/"}
        for conflict in (False, True):
            destination = self.root / ("conflicting" if conflict else "valid")
            def get(transport, url, target, **kwargs):
                if url.endswith("ListArtifacts"):
                    self.assertEqual(kwargs["data"], {"workflow_run_backend_id": "31", "workflow_job_run_backend_id": "job"})
                    return self.listing()
                if url.endswith("GetSignedArtifactURL"):
                    self.assertEqual(set(kwargs["data"]), {"workflow_run_backend_id", "workflow_job_run_backend_id", "name"})
                    self.assertEqual(kwargs["data"]["workflow_run_backend_id"], "31")
                    self.assertEqual(kwargs["data"]["workflow_job_run_backend_id"], "consumer-id")
                    return {"signedUrl": "https://forge.emrul.dev/" + kwargs["data"]["name"]}
                content = {"product-27": {"native": b"changed" if conflict else data},
                           "ci-evidence-producer-27": {"product-27.json": json.dumps(evidence)}}[url.rsplit("/", 1)[-1]]
                target.write_bytes(zipped(content))
            with patch.dict(os.environ, env), patch.object(C, "identity", return_value=IDENTITY), patch.object(C, "read_json", wraps=C.read_json) as read, patch.object(H.Transport, "get", get), patch.object(C.shutil, "copytree", side_effect=AssertionError("retrieval copied payload")), patch.object(C.tarfile, "open", side_effect=AssertionError("retrieval created archive")), patch.object(C, "seal", side_effect=AssertionError("retrieval invoked sealing")):
                read.side_effect = lambda path: POLICY if path == C.POLICY_PATH else json.loads(path.read_text())
                if conflict:
                    with self.assertRaisesRegex(ValueError, "checksum/inventory"):
                        H.retrieve(destination)
                    self.assertFalse(destination.exists())
                else:
                    H.retrieve(destination)
                    self.assertEqual((destination / "product-27/native").read_bytes(), data)

    def test_runtime_backend_from_another_run_is_rejected_before_transport(self):
        payload = base64.urlsafe_b64encode(json.dumps({"scp": "Actions.Results:32:job"}).encode()).decode().rstrip("=")
        env = {"FORGEJO_RUN_ATTEMPT": "1", "CANDIDATE_NEEDS": '{"producer":{"result":"success"}}',
               "ACTIONS_RUNTIME_TOKEN": "x." + payload + ".x", "ACTIONS_RESULTS_URL": "https://forge.emrul.dev/"}
        with patch.dict(os.environ, env), patch.object(C, "identity", return_value=IDENTITY), patch.object(C, "read_json", return_value=POLICY), patch.object(H.Transport, "get") as get:
            with self.assertRaisesRegex(ValueError, "runtime backend ID"):
                H.retrieve(self.root / "handoff")
            get.assert_not_called()

    def test_direct_validation_preserves_strict_payload_and_handoff_inventories(self):
        handoff = self.root / "handoff"
        product = handoff / "product-27"
        product.mkdir(parents=True)
        (product / "native").write_bytes(b"qualified")
        evidence = {**deepcopy(IDENTITY), "artifact": "product-27", "job": "producer", "matrix": {}, "attempt": 1,
                    "tools": {"rustc": "1.90"}, "files": C.file_map(product)}
        directory = handoff / "ci-evidence-producer-27"
        directory.mkdir()
        (directory / "product-27.json").write_text(json.dumps(evidence))
        needs = {"producer": {"result": "success"}}
        manifest = C.handoff_manifest(handoff, needs, IDENTITY, POLICY, strict_inventory=True)
        self.assertEqual(manifest["artifacts"], {"product-27": evidence})
        # A payload root cannot silently admit the handoff's attestation dirs.
        with self.assertRaisesRegex(ValueError, "unexpected artifact paths"):
            C.verify_payload(handoff, manifest, IDENTITY, POLICY)
        (handoff / "ci-evidence-unselected-27").mkdir()
        with self.assertRaisesRegex(ValueError, "unexpected artifact paths"):
            C.handoff_manifest(handoff, needs, IDENTITY, POLICY, strict_inventory=True)


if __name__ == "__main__":
    unittest.main()
