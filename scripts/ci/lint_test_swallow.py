#!/usr/bin/env python3
"""lint_test_swallow.py -- refuse swallowed failures in test code.

A test that catches an error and does nothing passes when the code under test
fails (FFI-1: .NET soak failures were swallowed; the Go soak dropped errors).
This lint finds, in tracked test code of every language:

  C#, Java, Kotlin          an empty `catch {}` or a catch of Exception /
                            Throwable with an empty body (only comments
                            counts as empty); a catch of a specific type is
                            the expected-exception idiom and is not flagged,
                            nor is a C# catch with a `when` filter
  TS/JS                     any empty `catch {}`, and `.catch(() => {})` /
                            `.catch(() => null)` on a promise
  Python                    `except:` / `except Exception:` /
                            `except BaseException:` (alone or in a tuple)
                            whose body is only `pass` or `...`
  Go                        `_ = err`, and `if err != nil {}` with an empty body
  Rust                      `let _ = ...` or a statement ending `.ok();` whose
                            operation is a call named connect/call/fetch/
                            request/dial (deliberately narrow: channel sends
                            and server-side accepts in fixtures are plumbing)

Generated code (`*.generated.*`) is skipped.

A hit is allowed when the statement carries `swallow-ok: <reason>` in a
comment on its first line, inside it, or on the line just above:

    } catch (e) { /* swallow-ok: the peer may already be gone */ }
    except Exception:  # swallow-ok: best-effort cleanup in teardown

Usage: scripts/ci/lint_test_swallow.py [--root DIR] [FILE...]
With no files, every tracked test file under the root is checked. Exit 1 on
any unannotated hit.
"""

from __future__ import annotations

import argparse
import ast
import os
import re
import subprocess
import sys

MARKER = re.compile(r"swallow-ok:\s*[A-Za-z0-9]")
EXCLUDED_PARTS = {"node_modules", "target", "bin", "obj", "vendor", ".venv", "dist", "build"}
# This lint's own fixtures, which contain swallows on purpose.
EXCLUDED_FILES = {"scripts/ci/test_lint_test_swallow.py"}

C_LIKE = (".cs", ".java", ".kt", ".ts", ".tsx", ".js", ".mjs", ".cjs")
TEST_DIRS = {"tests", "test", "__tests__", "testing"}


def is_test_file(path: str) -> bool:
    parts = path.split("/")
    if EXCLUDED_PARTS & set(parts[:-1]) or path in EXCLUDED_FILES:
        return False
    name = parts[-1]
    dirs = set(parts[:-1])
    # Generated client code checked in as a fixture is product output, not test logic.
    if ".generated." in name:
        return False
    if name.endswith(".py"):
        return bool(dirs & TEST_DIRS) or name.startswith("test_") or name.endswith("_test.py") \
            or name == "conftest.py"
    if name.endswith(".rs"):
        return "tests" in dirs
    if name.endswith(".go"):
        return name.endswith("_test.go")
    if name.endswith(".cs"):
        return bool(dirs & TEST_DIRS) or any(d.endswith((".Tests", "Tests")) for d in dirs) \
            or name.endswith(("Tests.cs", "Test.cs"))
    if name.endswith((".java", ".kt")):
        return "/src/test/" in "/" + path or bool(dirs & TEST_DIRS)
    if name.endswith((".ts", ".tsx", ".js", ".mjs", ".cjs")):
        return bool(re.search(r"\.(test|spec)\.[cm]?[jt]sx?$", name)) or bool(dirs & TEST_DIRS)
    return False


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def annotated(lines: list[str], first: int, last: int) -> bool:
    """True when `swallow-ok:` is on the line above the hit or within it (1-based)."""
    lo = max(first - 1, 1)
    return any(MARKER.search(lines[i - 1]) for i in range(lo, last + 1))


# A body of nothing but whitespace and comments.
EMPTY_BODY = r"\{(?:\s|//[^\n]*\n|/\*.*?\*/)*\}"
# `catch`, its optional parameter, an optional C# `when` filter, an empty body.
CATCH_EMPTY = re.compile(r"\bcatch\b\s*(?P<param>\([^()]*\))?\s*(?P<when>when\s*\((?:[^()]|\([^()]*\))*\)\s*)?" + EMPTY_BODY, re.S)
# Types whose catch takes every failure. A specific type (OperationCanceledException,
# ExecutionException) is the expected-exception idiom, not a swallow.
BROAD_TYPES = {"Exception", "System.Exception", "Throwable", "java.lang.Exception", "java.lang.Throwable"}
PROMISE_CATCH = re.compile(r"\.catch\(\s*(?:\(\s*[\w\s,:]*\)|\w+)\s*=>\s*(?:" + EMPTY_BODY + r"|undefined|null|void 0)\s*\)", re.S)
GO_BLANK_ERR = re.compile(r"^\s*_\s*=\s*err\s*$", re.M)
GO_EMPTY_IF = re.compile(r"\bif\s+(?:[^{};]*;\s*)?\w+\s*!=\s*nil\s*" + EMPTY_BODY, re.S)
# Client-side operations a test exercises. Channel `send`, server-side `accept`
# and other fixture plumbing stay out: there a failure means the other side
# is already gone, and flagging them is noise.
RUST_CALLS = r"(?:connect|call|fetch|request|dial)\w*"
# The receiver chain before the call has no parentheses, so the named call is
# the statement's operation, not something inside its arguments.
RUST_LET = re.compile(r"^[ \t]*let\s+_\s*=\s*[\w.:&*\s]*?\b" + RUST_CALLS + r"\s*(?:::<[^>]*>)?\(", re.M)
RUST_OK = re.compile(r"^[ \t]*[\w.:&*\s]*?\b" + RUST_CALLS + r"\s*(?:::<[^>]*>)?\((?:[^;()]|\([^;()]*\))*\)(?:\.await)?\.ok\(\)\s*;", re.M)


def catch_is_broad(m: re.Match, ts: bool) -> bool:
    if m.group("when"):
        return False  # a filtered catch takes only what the filter admits
    param = m.group("param")
    if param is None or ts:
        return True  # `catch {}` / any JS/TS catch takes everything
    inner = param.strip("()").strip()
    if ":" in inner:  # Kotlin `e: Exception`
        types = inner.split(":", 1)[1]
    else:
        types = inner.rsplit(None, 1)[0] if len(inner.split()) > 1 else inner
    return any(t.strip() in BROAD_TYPES for t in types.split("|"))


def strip_strings(text: str, quotes: bool = True) -> str:
    """Blank string literals so their contents can't fake a match; keep offsets.
    `quotes=False` leaves '...' alone (Rust lifetimes and chars)."""
    def blank(m: re.Match) -> str:
        return re.sub(r"[^\n]", " ", m.group(0))
    single = r"|'(?:\\.|[^'\\\n])*'|`(?:\\.|[^`\\])*`" if quotes else ""
    return re.sub(r'"(?:\\.|[^"\\\n])*"' + single, blank, text)


def regex_hits(path: str, text: str, patterns: list[tuple]) -> list[tuple[int, str]]:
    lines = text.splitlines()
    scan = strip_strings(text, quotes=not path.endswith(".rs"))
    hits = []
    for pat, what, *keep in patterns:
        for m in pat.finditer(scan):
            if keep and not keep[0](m):
                continue
            first = line_of(scan, m.start() + len(m.group(0)) - len(m.group(0).lstrip()))
            last = line_of(scan, m.end() - 1)
            if not annotated(lines, first, last):
                hits.append((first, what))
    return hits


BROAD = {"Exception", "BaseException"}


def python_hits(text: str) -> list[tuple[int, str]]:
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return []
    lines = text.splitlines()
    hits = []

    def names(node: ast.expr | None) -> set[str]:
        if node is None:
            return {"<bare>"}
        if isinstance(node, ast.Tuple):
            return set().union(*(names(e) for e in node.elts))
        if isinstance(node, ast.Name):
            return {node.id}
        if isinstance(node, ast.Attribute):
            return {node.attr}
        return set()

    for node in ast.walk(tree):
        if not isinstance(node, ast.ExceptHandler):
            continue
        caught = names(node.type)
        if not (caught & (BROAD | {"<bare>"})):
            continue
        if not all(isinstance(s, ast.Pass) or (isinstance(s, ast.Expr) and isinstance(s.value, ast.Constant)
                                                and s.value.value is Ellipsis) for s in node.body):
            continue
        last = node.body[-1].end_lineno or node.lineno
        if not annotated(lines, node.lineno, last):
            what = "bare except: pass" if "<bare>" in caught else "except Exception: pass"
            hits.append((node.lineno, what))
    return hits


def check_file(path: str, text: str) -> list[tuple[int, str]]:
    if path.endswith(".py"):
        return python_hits(text)
    if path.endswith(C_LIKE):
        ts = path.endswith((".ts", ".tsx", ".js", ".mjs", ".cjs"))
        pats = [(CATCH_EMPTY, "empty catch", lambda m: catch_is_broad(m, ts))]
        if ts:
            pats.append((PROMISE_CATCH, "empty promise .catch"))
        return regex_hits(path, text, pats)
    if path.endswith(".go"):
        return regex_hits(path, text, [(GO_BLANK_ERR, "_ = err"), (GO_EMPTY_IF, "empty if err != nil {}")])
    if path.endswith(".rs"):
        return regex_hits(path, text, [(RUST_LET, "let _ = on a fallible call"),
                                       (RUST_OK, ".ok(); on a fallible call")])
    return []


def tracked_files(root: str) -> list[str]:
    out = subprocess.run(["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True)
    return [p for p in out.stdout.decode().split("\0") if p]


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--root", default=os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
    ap.add_argument("files", nargs="*")
    args = ap.parse_args(argv)
    files = args.files or [p for p in tracked_files(args.root) if is_test_file(p)]
    total = 0
    for rel in sorted(files):
        full = rel if os.path.isabs(rel) else os.path.join(args.root, rel)
        try:
            with open(full, encoding="utf-8") as f:
                text = f.read()
        except (OSError, UnicodeDecodeError):
            continue
        for line, what in check_file(rel, text):
            print(f"{rel}:{line}: {what} in test code; handle it, or say why with `swallow-ok: <reason>`")
            total += 1
    if total:
        print(f"lint_test_swallow: {total} swallowed failure(s) in test code", file=sys.stderr)
        return 1
    print(f"lint_test_swallow: {len(files)} test files clean")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
