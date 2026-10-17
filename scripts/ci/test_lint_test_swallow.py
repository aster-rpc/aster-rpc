"""Guards for scripts/ci/lint_test_swallow.py: what it flags, what it lets
through, and that the repository's test code is clean."""

import importlib.util
import os
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SCRIPT = os.path.join(HERE, "lint_test_swallow.py")

spec = importlib.util.spec_from_file_location("lint_test_swallow", SCRIPT)
lint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lint)


def hits(path, text):
    return [what for _, what in lint.check_file(path, text)]


class Flags(unittest.TestCase):
    def test_csharp(self):
        self.assertTrue(hits("A.Tests/X.cs", "try { F(); } catch { }"))
        self.assertTrue(hits("A.Tests/X.cs", "try { F(); } catch (Exception) { /* closing */ }"))
        self.assertTrue(hits("A.Tests/X.cs", "try { F(); }\ncatch (System.Exception e)\n{\n    // nothing\n}"))

    def test_java_and_kotlin(self):
        self.assertTrue(hits("a/src/test/java/X.java", "try { f(); } catch (Exception ignored) {\n}"))
        self.assertTrue(hits("a/src/test/java/X.java", "try { f(); } catch (Throwable t) {\n  // already\n}"))
        self.assertTrue(hits("a/src/test/kotlin/X.kt", "try { f() } catch (e: Exception) {}"))

    def test_typescript(self):
        self.assertTrue(hits("tests/x.test.ts", "try { f(); } catch { /* ignore */ }"))
        self.assertTrue(hits("tests/x.test.ts", "try { f(); } catch (e) {}"))
        self.assertTrue(hits("tests/x.test.ts", "await p.catch(() => {});"))
        self.assertTrue(hits("tests/x.test.ts", "const r = call().catch(() => null);"))

    def test_python(self):
        self.assertTrue(hits("tests/test_x.py", "try:\n    f()\nexcept:\n    pass\n"))
        self.assertTrue(hits("tests/test_x.py", "try:\n    f()\nexcept Exception:\n    pass\n"))
        self.assertTrue(hits("tests/test_x.py", "try:\n    f()\nexcept (KeyError, BaseException):\n    ...\n"))

    def test_go(self):
        self.assertTrue(hits("x_test.go", "func T() {\n\terr := f()\n\t_ = err\n}\n"))
        self.assertTrue(hits("x_test.go", "if err := f(); err != nil {\n}\n"))
        self.assertTrue(hits("x_test.go", "if err != nil { /* later */ }\n"))

    def test_rust(self):
        self.assertTrue(hits("core/tests/t.rs", "    let _ = client.connect(addr).await;\n"))
        self.assertTrue(hits("core/tests/t.rs", "    node.fetch_blob(hash).await.ok();\n"))
        self.assertTrue(hits("core/tests/t.rs", "    let _ = stub.call::<Req>(req);\n"))


class LetsThrough(unittest.TestCase):
    def test_annotated(self):
        self.assertFalse(hits("tests/x.test.ts", "try { f(); } catch { /* swallow-ok: teardown */ }"))
        self.assertFalse(hits("tests/test_x.py", "try:\n    f()\nexcept Exception:  # swallow-ok: teardown\n    pass\n"))
        self.assertFalse(hits("tests/test_x.py", "try:\n    f()\nexcept Exception:\n    pass  # swallow-ok: teardown\n"))
        self.assertFalse(hits("x_test.go", "// swallow-ok: the error is the subject of the next assertion\n_ = err\n"))
        self.assertFalse(hits("core/tests/t.rs", "    let _ = c.connect(a).await; // swallow-ok: retried below\n"))

    def test_a_marker_needs_a_reason(self):
        self.assertTrue(hits("tests/x.test.ts", "try { f(); } catch { /* swallow-ok: */ }"))

    def test_handled_or_specific(self):
        self.assertFalse(hits("A.Tests/X.cs", "try { F(); } catch (OperationCanceledException) { }"))
        self.assertFalse(hits("A.Tests/X.cs", "try { F(); } catch (Exception e) when (e is IOException) { }"))
        self.assertFalse(hits("a/src/test/java/X.java", "try { f(); } catch (ExecutionException expected) {\n}"))
        self.assertFalse(hits("tests/x.test.ts", "try { f(); } catch (e) { errors.push(e); }"))
        self.assertFalse(hits("tests/test_x.py", "try:\n    f()\nexcept asyncio.CancelledError:\n    pass\n"))
        self.assertFalse(hits("tests/test_x.py", "try:\n    f()\nexcept Exception as e:\n    errors.append(e)\n"))
        self.assertFalse(hits("x_test.go", "if err != nil {\n\tt.Fatal(err)\n}\n"))
        self.assertFalse(hits("core/tests/t.rs", "    let _ = tx.send(event);\n"))
        self.assertFalse(hits("core/tests/t.rs", "    let _ = format!(\"{}\", connect(a));\n"))

    def test_strings_do_not_count(self):
        self.assertFalse(hits("tests/x.test.ts", "const s = 'catch {}';"))

    def test_only_test_files(self):
        self.assertTrue(lint.is_test_file("bindings/go/node_test.go"))
        self.assertTrue(lint.is_test_file("bindings/dotnet/src/Aster.Tests/X.cs"))
        self.assertTrue(lint.is_test_file("bindings/java/aster-runtime/src/test/java/X.java"))
        self.assertTrue(lint.is_test_file("tests/typescript/unit/x.test.ts"))
        self.assertTrue(lint.is_test_file("aster/tests/it/x.rs"))
        self.assertFalse(lint.is_test_file("bindings/go/node.go"))
        self.assertFalse(lint.is_test_file("core/src/lib.rs"))
        self.assertFalse(lint.is_test_file("bindings/typescript/node_modules/x/tests/a.test.ts"))
        self.assertFalse(lint.is_test_file("tests/fixtures/aster-rpc.generated.ts"))


class Repository(unittest.TestCase):
    def test_test_code_has_no_unexplained_swallows(self):
        if not os.path.isdir(os.path.join(ROOT, ".git")) and not os.path.isfile(os.path.join(ROOT, ".git")):
            self.skipTest("not a git checkout")
        out = subprocess.run([sys.executable, SCRIPT], capture_output=True, text=True, cwd=ROOT)
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)


if __name__ == "__main__":
    unittest.main()
