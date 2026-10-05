"""Evidence must retain failures and refuse misleading reuse."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("record.py")


class RecorderTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.output = Path(self.temp.name)

    def invoke(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), "--output", str(self.output), *args],
                              capture_output=True, text=True, timeout=15)

    def result(self):
        return json.loads(next(self.output.glob("*/result.json")).read_text())

    def test_failure_is_retained_and_not_retried(self):
        marker = self.output / "attempts"
        code = f"from pathlib import Path; p=Path({str(marker)!r}); p.write_text(p.read_text()+'x' if p.exists() else 'x'); raise SystemExit(7)"
        run = self.invoke("run", "--scenario", "RPC-02", "--", sys.executable, "-c", code)
        self.assertEqual(run.returncode, 1)
        self.assertEqual(marker.read_text(), "x")
        self.assertEqual(self.result()["exit_code"], 7)
        self.assertEqual(self.result()["qualification"], "NOT_EVALUATED")

    def test_timeout_is_failure_with_persisted_report(self):
        run = self.invoke("run", "--scenario", "RPC-02", "--timeout", "0.1", "--",
                          sys.executable, "-c", "import time; time.sleep(10)")
        self.assertEqual(run.returncode, 1)
        self.assertEqual(self.result()["status"], "TIMEOUT")

    def test_changed_artifact_invalidates_success(self):
        artifact = self.output / "native"
        artifact.write_bytes(b"old")
        run = self.invoke("run", "--scenario", "RPC-02", "--artifact", str(artifact), "--",
                          sys.executable, "-c", f"from pathlib import Path; Path({str(artifact)!r}).write_bytes(b'new')")
        self.assertEqual(run.returncode, 1)
        self.assertEqual(self.result()["status"], "COMMAND_OK")
        self.assertTrue(self.result()["inputs_changed"])

    def test_unknown_scenario_cannot_execute_command(self):
        run = self.invoke("run", "--scenario", "RPC-999", "--", sys.executable, "-c", "raise SystemExit(0)")
        self.assertNotEqual(run.returncode, 0)
        self.assertEqual(list(self.output.glob("*/result.json")), [])

    def test_seed_does_not_overwrite_evidence(self):
        self.assertEqual(self.invoke("seed").returncode, 0)
        ledger = self.output / "catalogue-ledger.json"
        original = ledger.read_bytes()
        self.assertNotEqual(self.invoke("seed").returncode, 0)
        self.assertEqual(ledger.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
