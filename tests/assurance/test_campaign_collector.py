"""Collector-only diagnostics; no native SDK, campaign, or host changes.

Healthy acknowledgements precede evidence I/O; shutdown drains evidence and
rejects write failures. These tests do not attribute an earlier campaign failure.
"""
import concurrent.futures
import importlib.util
import json
import pathlib
import tempfile
import threading
import unittest
import urllib.request
import http.client


SOURCE = pathlib.Path(__file__).resolve().parents[2] / "scripts/assurance/campaign.py"
SPEC = importlib.util.spec_from_file_location("assurance_campaign", SOURCE)
campaign = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(campaign)


class GatedFlush:
    def __init__(self, stream):
        self.stream = stream
        self.entered = threading.Event()
        self.release = threading.Event()

    def write(self, data):
        return self.stream.write(data)

    def flush(self):
        self.entered.set()
        if not self.release.wait(3):
            raise TimeoutError("diagnostic evidence flush was not released")
        self.stream.flush()


class CollectorEvidenceDiagnostics(unittest.TestCase):
    def exercise(self, blocked):
        with tempfile.TemporaryDirectory() as directory:
            evidence = campaign.Evidence(pathlib.Path(directory) / "evidence")
            original = evidence.log
            gate = GatedFlush(original)
            if blocked:
                evidence.log = gate
            collector = campaign.Collector(evidence)
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=7)
            try:
                def post(language):
                    request = urllib.request.Request(
                        collector.endpoint + "/v1/logs",
                        data=f"campaign:{language}:178;".encode(),
                        method="POST",
                    )
                    with urllib.request.urlopen(request, timeout=3) as response:
                        return response.status

                requests = [pool.submit(post, language) for language in campaign.LANGUAGES]
                if blocked:
                    self.assertTrue(gate.entered.wait(1), "a collector handler reached evidence flush")
                completed, pending = concurrent.futures.wait(requests, timeout=0.5)
                self.assertEqual(len(completed), 6, "healthy responses do not wait for evidence I/O")
                self.assertEqual(len(pending), 0)
                self.assertEqual([request.result(timeout=3) for request in requests], [200] * 6)
                if blocked:
                    closing = pool.submit(collector.close)
                    with self.assertRaises(concurrent.futures.TimeoutError):
                        closing.result(timeout=0.6)
                    gate.release.set()
                    closing.result(timeout=3)
            finally:
                gate.release.set()
                pool.shutdown(wait=True)
                collector.close()
                original.close()
            records = [json.loads(line) for line in (evidence.root / "events.jsonl").read_text().splitlines()]
            responses = [record for record in records if record["kind"] == "collector_response"]
            self.assertEqual(len(responses), 6)
            for record in responses:
                stages = [record[key] for key in (
                    "handler_started_monotonic", "body_read_completed_monotonic",
                    "response_completed_monotonic",
                    "evidence_entered_monotonic", "evidence_lock_acquired_monotonic",
                    "evidence_flush_started_monotonic", "evidence_flush_completed_monotonic",
                    "monotonic")]
                self.assertEqual(stages, sorted(stages))
                self.assertIsNone(record["response_error"])
            if blocked:
                self.assertGreaterEqual(max(record["evidence_flush_completed_monotonic"] -
                                            record["evidence_entered_monotonic"]
                                            for record in responses), 0.5)

    def test_healthy_collector_responds_without_evidence_stall(self):
        self.exercise(blocked=False)

    def test_stalled_evidence_flush_does_not_gate_responses_and_shutdown_drains(self):
        self.exercise(blocked=True)

    def test_handler_evidence_failure_rejects_shutdown(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = campaign.Evidence(pathlib.Path(directory) / "evidence")
            original = evidence.log

            class FailedFlush:
                def write(self, data):
                    return original.write(data)

                def flush(self):
                    raise OSError("injected evidence write failure")

            evidence.log = FailedFlush()
            collector = campaign.Collector(evidence)
            try:
                request = urllib.request.Request(collector.endpoint + "/v1/logs",
                                                 data=b"campaign:go:179;", method="POST")
                with urllib.request.urlopen(request, timeout=3) as response:
                    self.assertEqual(response.status, 200)
                with self.assertRaisesRegex(RuntimeError, "evidence is incomplete") as failure:
                    collector.close()
                self.assertIsInstance(failure.exception.__cause__, OSError)
            finally:
                try:
                    collector.close()
                except RuntimeError:
                    pass
                original.close()

    def test_fault_restore_and_shutdown_drain_stalled_handlers(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = campaign.Evidence(pathlib.Path(directory) / "evidence")
            collector = campaign.Collector(evidence)
            class ObservedRelease(threading.Event):
                def __init__(self):
                    super().__init__()
                    self.waiting = threading.Event()

                def wait(self, timeout=None):
                    self.waiting.set()
                    return super().wait(timeout)

            collector.release = ObservedRelease()
            collector.release.set()
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
            try:
                collector.fault("stall")
                request = urllib.request.Request(collector.endpoint + "/v1/logs",
                                                 data=b"campaign:go:180;", method="POST")
                pending = pool.submit(urllib.request.urlopen, request, timeout=3)
                self.assertTrue(collector.release.waiting.wait(1))
                collector.restore()
                collector.fault("refusal")
                collector.restore()
                collector.close()
                self.assertEqual(collector.pending_handlers, 0)
                with self.assertRaises(http.client.RemoteDisconnected):
                    pending.result(timeout=3)
            finally:
                collector.close()
                pool.shutdown(wait=True)
                evidence.log.close()


if __name__ == "__main__":
    unittest.main()
