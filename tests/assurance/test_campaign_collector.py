"""Collector-only diagnostics; no native SDK, campaign, or host changes.

These controls demonstrate that synchronous evidence I/O can withhold healthy
HTTP responses. They do not attribute an earlier campaign failure to that cause.
"""
import concurrent.futures
import importlib.util
import json
import pathlib
import tempfile
import threading
import unittest
import urllib.request


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


class RecordingEvidence(campaign.Evidence):
    def __init__(self, root):
        super().__init__(root)
        self.responses = 0
        self.finished = threading.Condition()

    def write(self, kind, **fields):
        timing = super().write(kind, **fields)
        if kind == "collector_response":
            with self.finished:
                self.responses += 1
                self.finished.notify_all()
        return timing


class CollectorEvidenceDiagnostics(unittest.TestCase):
    def exercise(self, blocked):
        with tempfile.TemporaryDirectory() as directory:
            evidence = RecordingEvidence(pathlib.Path(directory) / "evidence")
            original = evidence.log
            gate = GatedFlush(original)
            if blocked:
                evidence.log = gate
            collector = campaign.Collector(evidence)
            pool = concurrent.futures.ThreadPoolExecutor(max_workers=6)
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
                    self.assertEqual(len(completed), 0, "healthy responses wait behind evidence I/O")
                    self.assertEqual(len(pending), 6)
                    gate.release.set()
                else:
                    completed, pending = concurrent.futures.wait(requests, timeout=0.5)
                    self.assertEqual(len(completed), 6, "healthy control responds within the 500 ms budget")
                    self.assertEqual(len(pending), 0)
                self.assertEqual([request.result(timeout=3) for request in requests], [200] * 6)
                with evidence.finished:
                    self.assertTrue(evidence.finished.wait_for(lambda: evidence.responses == 6, timeout=3),
                                    "all handler evidence writes finish before closing their file")
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
                    "evidence_entered_monotonic", "evidence_lock_acquired_monotonic",
                    "evidence_flush_started_monotonic", "evidence_flush_completed_monotonic",
                    "response_completed_monotonic", "monotonic")]
                self.assertEqual(stages, sorted(stages))
                self.assertIsNone(record["response_error"])
            if blocked:
                self.assertGreaterEqual(max(record["evidence_flush_completed_monotonic"] -
                                            record["evidence_entered_monotonic"]
                                            for record in responses), 0.5)

    def test_healthy_collector_responds_without_evidence_stall(self):
        self.exercise(blocked=False)

    def test_stalled_evidence_flush_withholds_all_six_healthy_responses(self):
        self.exercise(blocked=True)


if __name__ == "__main__":
    unittest.main()
