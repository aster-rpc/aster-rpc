"""Persistent local receiver fixture for the Aster durable audit v1 contract.

Not a production service. It binds loopback, stores exact envelope bytes in SQLite
with synchronous=FULL, and acknowledges only after commit. Run with --database
PATH --port PORT (0 chooses an ephemeral port); prints its endpoint once ready.
"""
from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import re
import sqlite3
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAX_ENVELOPE_BYTES = 1024 * 1024
MAX_BATCH_BYTES = 1024 * 1024
MAX_REQUEST_BYTES = 1_420_000
EVENT_ID = re.compile(r"aster-audit-v1:([0-9a-f]{32}):([1-9][0-9]*):([0-9a-f]{64})\Z")


def batch_id(ids: list[str]) -> str:
    digest = hashlib.sha256(b"aster-audit-batch-v1\0")
    for event_id in ids:
        raw = event_id.encode("utf-8")
        digest.update(len(raw).to_bytes(4, "big"))
        digest.update(raw)
    return digest.hexdigest()


class IntegrityConflict(ValueError):
    """The full ID does not match the immutable bytes."""


def verify_event(record: dict) -> tuple[str, bytes]:
    if not isinstance(record, dict) or set(record) != {"event_id", "envelope_base64"}:
        raise ValueError("invalid record fields")
    event_id, encoded = record["event_id"], record["envelope_base64"]
    if not isinstance(event_id, str) or not isinstance(encoded, str):
        raise ValueError("invalid record types")
    match = EVENT_ID.fullmatch(event_id)
    if match is None or len(event_id) > 133 or int(match[2]) > (1 << 64) - 1:
        raise ValueError("invalid event ID")
    if len(encoded) > 4 * ((MAX_ENVELOPE_BYTES + 2) // 3):
        raise ValueError("envelope too large")
    try:
        raw = base64.b64decode(encoded, validate=True)
    except (ValueError, binascii.Error) as exc:
        raise ValueError("invalid base64") from exc
    if len(raw) > MAX_ENVELOPE_BYTES or base64.b64encode(raw).decode("ascii") != encoded:
        raise ValueError("invalid envelope encoding")
    if hashlib.sha256(raw).hexdigest() != match[3]:
        raise IntegrityConflict("envelope digest mismatch")
    envelope = json.loads(raw)
    if not isinstance(envelope, dict) or type(envelope.get("version")) is not int or envelope.get("version") != 1:
        raise ValueError("unsupported envelope")
    if set(envelope) != {"version", "outbox_id", "sequence", "timestamp_ms", "kind", "payload"}:
        raise ValueError("invalid envelope fields")
    if type(envelope["sequence"]) is not int or type(envelope["timestamp_ms"]) is not int:
        raise ValueError("invalid envelope number")
    if not 0 <= envelope["timestamp_ms"] <= (1 << 64) - 1 or not isinstance(envelope["payload"], dict):
        raise ValueError("invalid timestamp or payload")
    if not isinstance(envelope["kind"], str) or not 1 <= len(envelope["kind"].encode()) <= 128:
        raise ValueError("invalid event kind")
    if envelope.get("outbox_id") != match[1] or envelope.get("sequence") != int(match[2]):
        raise IntegrityConflict("envelope identity mismatch")
    return event_id, raw


class Receiver:
    """Fixture supports restarting on the same database and dropped ACK injection."""

    def __init__(self, database: Path, port: int = 0):
        self.database = str(database)
        self.lock = threading.Lock()
        self.drop_next_ack = False
        self.response_status: int | None = None
        self.attempts: list[list[str]] = []
        with self.connect() as db:
            db.execute("CREATE TABLE IF NOT EXISTS events (id TEXT PRIMARY KEY, envelope BLOB NOT NULL)")
        receiver = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass  # Never log request payloads or credentials.

            def respond(self, status, payload):
                body = json.dumps(payload, separators=(",", ":")).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                if self.path != "/audit":
                    self.respond(404, {"error": "path"})
                    return
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if not 0 < length <= MAX_REQUEST_BYTES:
                        self.respond(413, {"error": "size"})
                        return
                    self.connection.settimeout(5)
                    body = self.rfile.read(length)
                    if len(body) != length:
                        raise ValueError("incomplete body")
                    batch = json.loads(body)
                    if not isinstance(batch, dict) or set(batch) != {"version", "batch_id", "events"}:
                        raise ValueError("invalid batch fields")
                    if type(batch["version"]) is not int or batch["version"] != 1 or not isinstance(batch["batch_id"], str):
                        raise ValueError("invalid batch identity")
                    records = batch["events"]
                    if not isinstance(records, list) or not 1 <= len(records) <= 64:
                        raise ValueError("invalid batch count")
                    verified = [verify_event(record) for record in records]
                    if batch["batch_id"] != batch_id([key for key, _ in verified]):
                        raise ValueError("batch ID mismatch")
                    if sum(len(raw) for _, raw in verified) > MAX_BATCH_BYTES:
                        self.respond(413, {"error": "batch size"})
                        return
                    with receiver.lock:
                        receiver.attempts.append([key for key, _ in verified])
                        if receiver.response_status is not None:
                            self.respond(receiver.response_status, {"error": "injected"})
                            return
                        with receiver.connect() as db:
                            for key, raw in verified:
                                previous = db.execute("SELECT envelope FROM events WHERE id=?", (key,)).fetchone()
                                if previous is not None and previous[0] != raw:
                                    raise IntegrityConflict("stored full ID conflict")
                                db.execute("INSERT OR IGNORE INTO events VALUES (?,?)", (key, raw))
                        # Context manager committed before either lost reply or ACK.
                        if receiver.drop_next_ack:
                            receiver.drop_next_ack = False
                            self.close_connection = True
                            return
                    self.respond(200, {"version": 1, "batch_id": batch["batch_id"], "durable": True})
                except IntegrityConflict:
                    self.respond(409, {"version": 1, "error": "integrity_conflict"})
                except (ValueError, TypeError, KeyError, UnicodeError):
                    self.respond(422, {"error": "invalid record"})
                except sqlite3.Error:
                    self.respond(503, {"error": "storage unavailable"})

        self.server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, name="audit-reference-receiver", daemon=True)

    @property
    def endpoint(self):
        return f"http://127.0.0.1:{self.server.server_port}/audit"

    def connect(self):
        # Returning a connection context alone doesn't close it; callers use the
        # short-lived wrapper below to bound file descriptors across retries.
        return _Database(self.database)

    def events(self):
        with self.connect() as db:
            return dict(db.execute("SELECT id,envelope FROM events"))

    def start(self):
        self.thread.start()
        return self

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class _Database:
    def __init__(self, path):
        self.db = sqlite3.connect(path, timeout=5)
        self.db.execute("PRAGMA synchronous=FULL")

    def __enter__(self):
        return self.db.__enter__()

    def __exit__(self, *args):
        try:
            return self.db.__exit__(*args)
        finally:
            self.db.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--response-status", type=int)
    args = parser.parse_args()
    fixture = Receiver(args.database, args.port)
    fixture.response_status = args.response_status
    fixture.start()
    print(fixture.endpoint, flush=True)
    try:
        fixture.thread.join()
    except KeyboardInterrupt:
        fixture.close()
