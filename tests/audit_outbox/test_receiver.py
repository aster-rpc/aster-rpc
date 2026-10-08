"""Independent contract tests for the persistent receiver fixture."""
import base64
import hashlib
import http.client
import json
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path

from receiver import Receiver, batch_id, verify_event, IntegrityConflict


def record(sequence=1, payload=None):
    identity = "0123456789abcdef" * 2
    raw = json.dumps({"version": 1, "outbox_id": identity, "sequence": sequence,
                      "timestamp_ms": 1000, "kind": "test", "payload": payload or {"a": 1}},
                     separators=(",", ":")).encode()
    key = f"aster-audit-v1:{identity}:{sequence}:{hashlib.sha256(raw).hexdigest()}"
    return {"event_id": key, "envelope_base64": base64.b64encode(raw).decode()}


def post(receiver, records, *, pretty=False):
    body = json.dumps({"version": 1, "batch_id": batch_id([r["event_id"] for r in records]),
                       "events": records}, indent=2 if pretty else None).encode()
    request = urllib.request.Request(receiver.endpoint, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.load(response)


class ReceiverTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.path = Path(self.directory.name) / "receiver.sqlite"
        self.receiver = Receiver(self.path).start()

    def tearDown(self):
        self.receiver.close()
        self.directory.cleanup()

    def test_ack_is_durable_and_repeated_ids_deduplicate_after_restart(self):
        item = record()
        self.assertTrue(post(self.receiver, [item])["durable"])
        self.receiver.close()
        self.receiver = Receiver(self.path).start()
        self.assertTrue(post(self.receiver, [item], pretty=True)["durable"])
        self.assertEqual(len(self.receiver.events()), 1)
        self.assertEqual(self.receiver.events()[item["event_id"]], base64.b64decode(item["envelope_base64"]))

    def test_lost_ack_commits_then_retry_is_idempotent(self):
        self.receiver.drop_next_ack = True
        item = record()
        with self.assertRaises((http.client.RemoteDisconnected, urllib.error.URLError)):
            post(self.receiver, [item])
        self.assertIn(item["event_id"], self.receiver.events())
        self.assertTrue(post(self.receiver, [item])["durable"])
        self.assertEqual(len(self.receiver.events()), 1)

    def test_inner_reencoding_conflicts_and_batch_remains_atomic(self):
        good, bad = record(1), record(2)
        raw = json.loads(base64.b64decode(bad["envelope_base64"]))
        bad["envelope_base64"] = base64.b64encode(json.dumps(raw, indent=2).encode()).decode()
        with self.assertRaises(urllib.error.HTTPError) as caught:
            post(self.receiver, [good, bad])
        self.assertEqual(caught.exception.code, 409)
        self.assertEqual(self.receiver.events(), {})

    def test_existing_full_id_with_other_bytes_is_never_overwritten(self):
        item = record()
        with self.receiver.connect() as db:
            db.execute("INSERT INTO events VALUES (?,?)", (item["event_id"], b"different bytes"))
        with self.assertRaises(urllib.error.HTTPError) as caught:
            post(self.receiver, [item])
        self.assertEqual(caught.exception.code, 409)
        self.assertEqual(self.receiver.events()[item["event_id"]], b"different bytes")

    def test_restore_reuses_sequence_with_new_content_without_collision(self):
        first = record(1, {"value": "before"})
        restored = record(1, {"value": "after"})
        self.assertNotEqual(first["event_id"], restored["event_id"])
        post(self.receiver, [first])
        post(self.receiver, [restored])
        self.assertEqual(len(self.receiver.events()), 2)

    def test_verifier_rejects_malformed_oversize_and_identity_mismatch(self):
        item = record((1 << 64) - 1)
        verify_event(item)
        item["envelope_base64"] = "!"
        with self.assertRaises(ValueError):
            verify_event(item)
        item["envelope_base64"] = "A" * 1_398_105
        with self.assertRaises(ValueError):
            verify_event(item)
        item = record(1)
        item["event_id"] = item["event_id"].replace(":1:", ":2:")
        with self.assertRaises(IntegrityConflict):
            verify_event(item)


if __name__ == "__main__":
    unittest.main()
