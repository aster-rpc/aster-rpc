"""Guards for scripts/ci/hermetic.sh (resilience-test-plan.md §4).

The guard must block a test that reaches the internet and let a loopback test
through, and the CI test steps must keep running under it.
"""

import os
import re
import subprocess
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HERMETIC = os.path.join(ROOT, "scripts", "ci", "hermetic.sh")
WORKFLOW = os.path.join(ROOT, ".forgejo", "workflows", "linux.yml")
SHELL = "shell: bash scripts/ci/hermetic.sh bash --noprofile --norc -eo pipefail {0}"

PROBE = r"""
import socket, sys
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen()
socket.create_connection(s.getsockname(), timeout=5).close()
try:
    socket.getaddrinfo("iroh.link", 443)
except OSError:
    print("blocked")
    sys.exit(0)
print("resolved iroh.link")
sys.exit(1)
"""


def mode():
    env = {k: v for k, v in os.environ.items() if k != "ASTER_HERMETIC"}
    out = subprocess.run([HERMETIC, "--check"], env=env, capture_output=True, text=True, check=True)
    return out.stdout.strip()


def steps(job):
    """(name, body) for each step of `job` in linux.yml, body = the step's lines."""
    with open(WORKFLOW, encoding="utf-8") as f:
        text = f.read()
    m = re.search(rf"^  {re.escape(job)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", text, re.S | re.M)
    assert m, f"job {job} not found"
    parts = re.split(r"^      - ", m.group(1), flags=re.M)[1:]
    result = []
    for part in parts:
        name = re.match(r"(?:name|uses): (.*)", part)
        result.append((name.group(1) if name else "?", part))
    return result


class HermeticGuard(unittest.TestCase):
    def test_blocks_the_internet_and_allows_loopback(self):
        m = mode()
        # The root-only resolv.conf mode rewrites container files; it is
        # exercised in CI's test steps, not from a unit test.
        if m not in ("sandbox-exec", "netns", "netns-root", "netns-sudo"):
            self.skipTest(f"no namespace/sandbox guard here (mode {m})")
        env = {k: v for k, v in os.environ.items() if k != "ASTER_HERMETIC"}
        out = subprocess.run([HERMETIC, sys.executable, "-I", "-c", PROBE],
                             env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertIn("blocked", out.stdout)

    def test_off_runs_unguarded_and_says_so(self):
        env = dict(os.environ, ASTER_HERMETIC="off")
        out = subprocess.run([HERMETIC, "true"], env=env, capture_output=True, text=True)
        self.assertEqual(out.returncode, 0)
        self.assertIn("unguarded", out.stderr)

    def test_rust_test_steps_run_under_the_guard(self):
        job = steps("rust-tests")
        names = [n for n, _ in job]
        self.assertIn("Fetch crates", names)
        fetch = names.index("Fetch crates")
        wrapped = 0
        for i, (name, body) in enumerate(job):
            if re.search(r"\bcargo test\b", body):
                self.assertIn(SHELL, body, f"rust-tests step {name!r} runs cargo test outside hermetic.sh")
                self.assertGreater(i, fetch, f"{name!r} runs before the crates are fetched")
                wrapped += 1
        self.assertGreater(wrapped, 0)


if __name__ == "__main__":
    unittest.main()
