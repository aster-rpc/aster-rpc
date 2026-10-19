"""Guards for scripts/ci/lint_test_waits.py: what it flags, what it lets
through, and that the repository's test code is clean."""

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SCRIPT = os.path.join(HERE, "lint_test_waits.py")

sys.path.insert(0, HERE)
spec = importlib.util.spec_from_file_location("lint_test_waits", SCRIPT)
lint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lint)


def hits(path, text):
    return [what for _, what in lint.check_file(path, text)]


class Flags(unittest.TestCase):
    def test_rust(self):
        self.assertTrue(hits("core/tests/t.rs", "tokio::time::timeout(Duration::from_secs(3), rx.recv()).await"))
        self.assertTrue(hits("core/tests/t.rs", "timeout(\n    Duration::from_millis(500),\n    f,\n).await"))
        self.assertTrue(hits("core/tests/t.rs", "rx.recv_timeout(Duration::from_secs(2)).unwrap();"))
        self.assertTrue(hits("core/tests/t.rs", "let deadline = Instant::now() + Duration::from_secs(5);"))
        self.assertTrue(hits("core/tests/t.rs", "c.wait_for(path, 1, Duration::from_secs(5));"))

    def test_single_attempt_proposal(self):
        # The shape that failed candidate 20: one proposal, a fixed 20 s, no retry.
        before = ("let accepted = coordination\n    .propose_for_testing_within(plan, Duration::from_secs(20))\n"
                  "    .await\n    .expect(\"the plan commits\");")
        self.assertTrue(hits("aster/tests/m.rs", before))
        self.assertTrue(hits("aster/tests/m.rs", "tokio::time::timeout(Duration::from_secs(30), at.propose_for_testing(c)).await"))
        self.assertTrue(hits("aster/tests/m.rs", "timeout(Duration::from_secs(10), raft.client_write(c)).await"))
        # A proposal after a loop, not inside it, is a single attempt.
        self.assertTrue(hits("aster/tests/m.rs", "loop { break; }\nat.propose_for_testing_within(c, Duration::from_secs(20)).await"))

    def test_csharp(self):
        self.assertTrue(hits("A.Tests/X.cs", "await t.WaitAsync(TimeSpan.FromSeconds(3));"))
        self.assertTrue(hits("A.Tests/X.cs", "using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(200));"))
        self.assertTrue(hits("A.Tests/X.cs", "cts.CancelAfter(3000);"))
        self.assertTrue(hits("A.Tests/X.cs", "await Task.WhenAny(t, Task.Delay(TimeSpan.FromSeconds(2)));"))

    def test_go(self):
        self.assertTrue(hits("x_test.go", "select {\ncase <-time.After(3 * time.Second):\n}"))
        self.assertTrue(hits("x_test.go", "ctx, cancel := context.WithTimeout(ctx, 5*time.Second)"))
        self.assertTrue(hits("x_test.go", "deadline := time.Now().Add(time.Second)"))

    def test_typescript(self):
        self.assertTrue(hits("tests/x.test.ts", "await within(p, 3000, 'reply');"))
        self.assertTrue(hits("tests/x.test.ts", "setTimeout(() => reject(new Error('t')), 5000);"))
        self.assertTrue(hits("tests/x.test.ts", "fetch(u, { signal: AbortSignal.timeout(2_000) });"))
        self.assertTrue(hits("tests/x.test.ts", "await waitFor(() => done, 500);"))

    def test_python(self):
        self.assertTrue(hits("tests/test_x.py", "await asyncio.wait_for(ev.wait(), 3)\n"))
        self.assertTrue(hits("tests/test_x.py", "await asyncio.wait_for(ev.wait(), timeout=2.0)\n"))
        self.assertTrue(hits("tests/test_x.py", "async with asyncio.timeout(1):\n    pass\n"))
        self.assertTrue(hits("tests/test_x.py", "t.join(timeout=5)\n"))
        self.assertTrue(hits("tests/test_x.py", "f.result(2 if slow else 1)\n"))

    def test_java_and_kotlin(self):
        self.assertTrue(hits("a/src/test/java/X.java", "f.get(5, TimeUnit.SECONDS);"))
        self.assertTrue(hits("a/src/test/java/X.java", "assertTrue(latch.await(500, MILLISECONDS));"))
        self.assertTrue(hits("a/src/test/java/X.java", "events.nextAsync(Duration.ofSeconds(5));"))
        self.assertTrue(hits("a/src/test/kotlin/X.kt", "withTimeout(3000) { call() }"))


class LetsThrough(unittest.TestCase):
    def test_marked(self):
        self.assertFalse(hits("core/tests/t.rs",
                              "// short-wait-ok: nothing may arrive\nassert!(timeout(Duration::from_millis(200), rx.recv()).await.is_err());"))
        self.assertFalse(hits("tests/test_x.py",
                              "with pytest.raises(TimeoutError):\n    await asyncio.wait_for(x, 0.1)  # short-wait-ok: must expire\n"))
        self.assertFalse(hits("x_test.go", "case <-time.After(200 * time.Millisecond): // short-wait-ok: nothing arrives\n"))

    def test_a_marker_needs_a_reason(self):
        self.assertTrue(hits("A.Tests/X.cs", "await t.WaitAsync(TimeSpan.FromSeconds(3)); // short-wait-ok:"))

    def test_budget_constants_and_long_literals(self):
        self.assertFalse(hits("core/tests/t.rs", "timeout(wait_budget(), rx.recv()).await"))
        self.assertFalse(hits("core/tests/t.rs", "timeout(Duration::from_secs(30), rx.recv()).await"))
        self.assertFalse(hits("A.Tests/X.cs", "await t.WaitAsync(WaitBudget.Value);"))
        self.assertFalse(hits("x_test.go", "case <-time.After(waitBudget()):"))
        self.assertFalse(hits("tests/test_x.py", "await asyncio.wait_for(x, WAIT_BUDGET)\n"))
        self.assertFalse(hits("a/src/test/java/X.java", "f.get(WaitBudget.SECONDS, TimeUnit.SECONDS);"))
        self.assertFalse(hits("tests/x.test.ts", "await within(p, WAIT_BUDGET_MS, 'reply');"))

    def test_proposal_that_retries_or_scales(self):
        looped = ("loop {\n    match at.propose_for_testing_within(c.clone(), Duration::from_secs(20)).await {\n"
                  "        Ok(x) => return x,\n        Err(_) => {}\n    }\n}")
        self.assertFalse(hits("aster/tests/m.rs", looped))
        self.assertFalse(hits("aster/tests/m.rs", "while Instant::now() < d {\n  let r = at.propose_for_testing_within(c, Duration::from_secs(20)).await;\n}"))
        self.assertFalse(hits("aster/tests/m.rs", "for _ in 0..3 {\n  at.propose_for_testing_within(c.clone(), Duration::from_secs(20)).await;\n}"))
        self.assertFalse(hits("aster/tests/m.rs", "retry(|| at.propose_for_testing_within(c.clone(), Duration::from_secs(20))).await"))
        # A budget-scaled or named bound is not a literal; a wait that is not a proposal is not this rule.
        self.assertFalse(hits("aster/tests/m.rs", "at.propose_for_testing_within(c, wait_budget()).await"))
        self.assertFalse(hits("aster/tests/m.rs", "at.propose_for_testing_within(c, PATIENCE).await"))
        self.assertFalse(hits("aster/tests/m.rs", "timeout(Duration::from_secs(20), coordination.wait_ready()).await"))
        self.assertFalse(hits("aster/tests/m.rs", "timeout(wait_budget(), at.propose_for_testing(c)).await"))
        self.assertFalse(hits("aster/tests/m.rs",
                              "// short-wait-ok: the proposal must stall\nat.propose_for_testing_within(c, Duration::from_secs(20)).await"))

    def test_not_waits(self):
        # Sleeps, client options under test, plain sleeps via setTimeout, and
        # deadline values that are data.
        self.assertFalse(hits("core/tests/t.rs", "tokio::time::sleep(Duration::from_millis(50)).await;"))
        self.assertFalse(hits("core/tests/t.rs", "let c = conn.with_deadline(Duration::from_millis(100));"))
        self.assertFalse(hits("core/tests/t.rs", "let b = builder.timeout(Duration::from_secs(1));"))
        self.assertFalse(hits("tests/x.test.ts", "await new Promise((r) => setTimeout(r, 50));"))
        self.assertFalse(hits("tests/x.test.ts", "Atomics.wait(cell, 0, 0, 250);"))
        self.assertFalse(hits("tests/x.test.ts", "await outbox.waitAcknowledgements(c, 10, WAIT_BUDGET_MS);"))
        self.assertFalse(hits("x_test.go", "near := time.Now().Add(time.Second)"))
        self.assertFalse(hits("tests/test_x.py", "client = make(CallOptions(timeout=0.5))\n"))
        self.assertFalse(hits("A.Tests/X.cs", "await Task.Delay(TimeSpan.FromSeconds(1));"))

    def test_strings_and_comments_do_not_count(self):
        self.assertFalse(hits("core/tests/t.rs", 'let s = "timeout(Duration::from_secs(1), f)";'))
        self.assertFalse(hits("core/tests/t.rs", "// timeout(Duration::from_secs(1), f)"))


class Deferred(unittest.TestCase):
    def run_lint(self, deferred_text, source):
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, "tests"))
            with open(os.path.join(tmp, "tests", "test_x.py"), "w") as f:
                f.write(source)
            listing = os.path.join(tmp, "deferred.txt")
            with open(listing, "w") as f:
                f.write(deferred_text)
            saved = lint.DEFERRED
            lint.DEFERRED = listing
            try:
                with open(os.devnull, "w") as null:
                    saved_out, sys.stdout, saved_err, sys.stderr = sys.stdout, null, sys.stderr, null
                    try:
                        return lint.main(["--root", tmp, "tests/test_x.py"])
                    finally:
                        sys.stdout, sys.stderr = saved_out, saved_err
            finally:
                lint.DEFERRED = saved

    def test_a_file_may_hold_its_count(self):
        two = "await asyncio.wait_for(a, 1)\nawait asyncio.wait_for(b, 2)\n"
        self.assertEqual(self.run_lint("2 tests/test_x.py\n", two), 0)
        self.assertEqual(self.run_lint("1 tests/test_x.py\n", two), 1)  # one too many
        self.assertEqual(self.run_lint("3 tests/test_x.py\n", two), 1)  # lower the count
        self.assertEqual(self.run_lint("", two), 1)


class Repository(unittest.TestCase):
    def test_test_code_has_no_unexplained_short_waits(self):
        if not os.path.isdir(os.path.join(ROOT, ".git")) and not os.path.isfile(os.path.join(ROOT, ".git")):
            self.skipTest("not a git checkout")
        out = subprocess.run([sys.executable, SCRIPT], capture_output=True, text=True, cwd=ROOT)
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)


if __name__ == "__main__":
    unittest.main()
