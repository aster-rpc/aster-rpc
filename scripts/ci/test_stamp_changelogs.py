"""Guards for scripts/ci/stamp_changelogs.py (backlog L1)."""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import stamp_changelogs as sc  # noqa: E402

DOC = "# Changelog\n\nintro\n\n## Unreleased\n\n### Added: x\n\nbody\n\n## 0.1.0 - 2026-01-01\n\nold\n"


class StampTests(unittest.TestCase):
    def test_stamps_and_reopens_unreleased(self):
        out = sc.stamp(DOC, "0.3.1258", "2026-10-19")
        self.assertIn("## Unreleased\n\n## 0.3.1258 - 2026-10-19\n\n### Added: x", out)
        self.assertIn("## 0.1.0 - 2026-01-01", out)

    def test_idempotent(self):
        once = sc.stamp(DOC, "0.3.1258", "2026-10-19")
        self.assertIsNone(sc.stamp(once, "0.3.1259", "2026-10-20"))

    def test_no_heading(self):
        self.assertIsNone(sc.stamp("# C\n\n## 0.1.0 - x\n\nold\n", "1.2.3", "2026-10-19"))

    def test_unreleased_last_section(self):
        out = sc.stamp("# C\n\n## Unreleased\n\ntext\n", "1.2.3", "2026-10-19")
        self.assertEqual(out, "# C\n\n## Unreleased\n\n## 1.2.3 - 2026-10-19\n\ntext\n")

    def test_cli_over_git_tree(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            subprocess.run(["git", "init", "-q", d], check=True)
            (root / "a").mkdir()
            (root / "a" / "CHANGELOG.md").write_text(DOC)
            (root / "b").mkdir()
            (root / "b" / "CHANGELOG.md").write_text("# C\n\n## 0.1.0 - x\n")
            subprocess.run(["git", "-C", d, "add", "."], check=True)
            script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "stamp_changelogs.py")
            run = lambda: subprocess.run(
                [sys.executable, script, "0.3.1258", "--date", "2026-10-19", "--root", d],
                check=True, capture_output=True, text=True,
            ).stdout
            self.assertEqual(run().split(), ["a/CHANGELOG.md"])
            self.assertEqual(run(), "")
            self.assertIn("## 0.3.1258 - 2026-10-19", (root / "a" / "CHANGELOG.md").read_text())


if __name__ == "__main__":
    unittest.main()
