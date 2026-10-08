#!/usr/bin/env python3
"""Exercise generic publication against an isolated HTTP registry fixture."""
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[2]


class GenericPublication(unittest.TestCase):
    def exercise(self, initial=None, listing_error=None):
        stored = {} if initial is None else {"fixture.bin": initial}
        writes = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_GET(self):
                if listing_error:
                    self.send_response(listing_error)
                    self.end_headers()
                    return
                self.send_response(200 if stored else 404)
                self.end_headers()
                self.wfile.write(json.dumps([{"name": key, "sha256": hashlib.sha256(value).hexdigest()} for key, value in stored.items()]).encode())
            def do_PUT(self):
                writes.append(self.path)
                stored["fixture.bin"] = self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(201)
                self.end_headers()
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                artifact = Path(directory) / "fixture.bin"
                artifact.write_bytes(b"qualified bytes")
                env = {**os.environ, "ASTER_PACKAGE_TOKEN": "fixture", "FORGEJO_SERVER_URL": f"http://127.0.0.1:{server.server_port}"}
                result = subprocess.run(["bash", str(ROOT / "scripts/release/publish-generic-package.sh"), "fixture", "0.0.1", str(artifact)], env=env, capture_output=True, timeout=10)
                return result.returncode, stored, writes
        finally:
            server.shutdown()
            server.server_close()
            worker.join()

    def test_missing_version_uploads_and_reads_back(self):
        code, stored, writes = self.exercise()
        self.assertEqual(code, 0)
        self.assertEqual(stored["fixture.bin"], b"qualified bytes")
        self.assertEqual(len(writes), 1)

    def test_identical_skips_and_conflicting_bytes_fail_without_writes(self):
        for payload, expected in ((b"qualified bytes", 0), (b"conflict", 3)):
            code, stored, writes = self.exercise(payload)
            self.assertEqual(code, expected)
            self.assertEqual(writes, [])
            self.assertEqual(stored["fixture.bin"], payload)

    def test_listing_errors_never_authorize_upload(self):
        for status in (401, 403, 500):
            code, _, writes = self.exercise(listing_error=status)
            self.assertNotEqual(code, 0)
            self.assertEqual(writes, [])


if __name__ == "__main__":
    unittest.main()
