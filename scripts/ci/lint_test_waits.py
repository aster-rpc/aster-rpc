#!/usr/bin/env python3
"""lint_test_waits.py -- refuse short literal waits in test code.

A test that waits for something that must happen (a reply, a cancel reaching
the server, a member becoming ready) with a tight literal bound fails when the
runner is loaded rather than when the code is wrong: a .NET `WaitAsync(3 s)`
for a server-side cancel and Rust status reads without a retry failed release
candidates whose Linux host was also compiling. Such waits use the binding's
wait budget instead, which `ASTER_TEST_TIME_SCALE` scales in CI:

  Rust     aster_transport_core::test_support::wait_budget()
  Python   WAIT_BUDGET (seconds) in tests/python/wait_budget.py
  TS       WAIT_BUDGET_MS in tests/typescript/helpers/wait-budget.ts
  Java     site.aster.testing.WaitBudget.SECONDS / .duration()
  .NET     Aster.Tests.WaitBudget.Value
  Go       waitBudget() in bindings/go/wait_budget_test.go

This lint finds, in tracked test code, a wait or timeout call whose bound is a
literal under 10 seconds:

  Rust      timeout(D, ..), .recv_timeout(D), Instant::now() + D, and a D
            argument to a call named *wait*/*within*/*until*/*eventually*
  C#        .WaitAsync(T), .CancelAfter(T), new CancellationTokenSource(T),
            .Wait(T), SpinWait.SpinUntil(.., T), Task.Delay(T) as a
            Task.WhenAny racer
  Go        time.After(D), time.NewTimer(D), context.WithTimeout(.., D),
            context.WithDeadline(.., time.Now().Add(D)), deadline :=
            time.Now().Add(D), Eventually(.., D, ..)
  TS/JS     within/withTimeout/raceTimeout(p, ms, ..), the last argument of a
            call named wait*/waitFor/eventually, AbortSignal.timeout(ms),
            and setTimeout(() => <reject/abort/resolve-a-value>, ms)
  Python    asyncio.wait_for(.., N) / timeout=N, asyncio.timeout(N), and
            timeout=N on wait/join/result/get/communicate, urlopen and
            create_connection, or on any call named *wait*/*until*
  Java/Kt   .get/.await/.poll/.orTimeout/.waitFor/... (N, TimeUnit.X), a
            Duration.ofX(N) argument to a call named *wait*/*await*/next*/
            join, and Kotlin withTimeout(ms)

where the bound is a literal (Duration::from_secs(3), TimeSpan.FromSeconds(3),
3 * time.Second, 3000, 3, ...). A named constant or the budget passes.

A wait that is meant to expire -- "nothing arrives within X", a deadline whose
expiry is the behaviour under test -- carries `short-wait-ok: <reason>` in a
comment on its line or the line above:

    // short-wait-ok: nothing may arrive; the bound is the assertion
    assert!(timeout(Duration::from_millis(200), rx.recv()).await.is_err());

Usage: scripts/ci/lint_test_waits.py [--root DIR] [FILE...]
With no files, every tracked test file under the root is checked (the same
selection as lint_test_swallow.py). Exit 1 on any unannotated hit.
test_waits_deferred.txt caps the hits left in suites the standard-profile
candidate does not run (`<count> <path>`); --all lists them anyway.
"""

from __future__ import annotations

import argparse
import ast
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lint_test_swallow import is_test_file, line_of, strip_strings, tracked_files  # noqa: E402

MARKER = re.compile(r"short-wait-ok:\s*[A-Za-z0-9]")
# Under this many seconds a literal bound is a guess about scheduling.
SHORT = 10.0
# This lint's own fixtures, which contain short waits on purpose.
EXCLUDED_FILES = {"scripts/ci/test_lint_test_waits.py"}

C_LIKE = (".cs", ".java", ".kt", ".ts", ".tsx", ".js", ".mjs", ".cjs", ".rs", ".go")
TS_EXT = (".ts", ".tsx", ".js", ".mjs", ".cjs")

NUM = r"([0-9][0-9_]*(?:\.[0-9_]+)?)"


def num(text: str) -> float:
    return float(text.replace("_", ""))


# ── Literal durations, per language: the whole argument, or None ──────────

RUST_DUR = re.compile(r"(?:[\w:]*::)?Duration::from_(secs|millis|micros|nanos|secs_f32|secs_f64)\(\s*"
                      + NUM + r"(?:_?[uif](?:8|16|32|64|size))?\s*\)$")
RUST_UNIT = {"secs": 1.0, "secs_f32": 1.0, "secs_f64": 1.0, "millis": 1e-3, "micros": 1e-6, "nanos": 1e-9}


def rust_seconds(arg: str) -> float | None:
    m = RUST_DUR.match(arg)
    return num(m.group(2)) * RUST_UNIT[m.group(1)] if m else None


CS_DUR = re.compile(r"(?:System\.)?TimeSpan\.From(Seconds|Milliseconds|Minutes)\(\s*" + NUM + r"[dDfFmM]?\s*\)$")
CS_UNIT = {"Seconds": 1.0, "Milliseconds": 1e-3, "Minutes": 60.0}
BARE_MS = re.compile(NUM + r"$")


def cs_seconds(arg: str) -> float | None:
    m = CS_DUR.match(arg)
    if m:
        return num(m.group(2)) * CS_UNIT[m.group(1)]
    m = BARE_MS.match(arg)  # an int argument is milliseconds (CancelAfter(3000))
    return num(m.group(1)) * 1e-3 if m else None


GO_DUR = re.compile(r"(?:" + NUM + r"\s*\*\s*)?time\.(Second|Millisecond|Microsecond|Nanosecond)$")
GO_UNIT = {"Second": 1.0, "Millisecond": 1e-3, "Microsecond": 1e-6, "Nanosecond": 1e-9}


def go_seconds(arg: str) -> float | None:
    m = GO_DUR.match(arg)
    if not m:
        return None
    return (num(m.group(1)) if m.group(1) else 1.0) * GO_UNIT[m.group(2)]


def ts_seconds(arg: str) -> float | None:
    m = BARE_MS.match(arg)
    return num(m.group(1)) * 1e-3 if m else None


JAVA_DUR = re.compile(r"(?:java\.time\.)?Duration\.of(Seconds|Millis)\(\s*" + NUM + r"L?\s*\)$")
JAVA_UNIT = {"Seconds": 1.0, "Millis": 1e-3}
TIME_UNIT = re.compile(r"(?:(?:java\.util\.concurrent\.)?TimeUnit\.)?(SECONDS|MILLISECONDS|MICROSECONDS)$")
TIME_UNIT_SCALE = {"SECONDS": 1.0, "MILLISECONDS": 1e-3, "MICROSECONDS": 1e-6}


def java_seconds(arg: str) -> float | None:
    m = JAVA_DUR.match(arg)
    return num(m.group(2)) * JAVA_UNIT[m.group(1)] if m else None


# ── Call scanning for the brace languages ─────────────────────────────────

CALL = re.compile(r"(?P<name>(?:new\s+)?[A-Za-z_][\w]*(?:\s*(?:\.|::)\s*[A-Za-z_]\w*)*)\s*(?:::<[^<>()]*>|<[\w\s,<>?]*>)?\s*\(")


def blank_comments(text: str) -> str:
    def blank(m: re.Match) -> str:
        return re.sub(r"[^\n]", " ", m.group(0))
    return re.sub(r"//[^\n]*|/\*.*?\*/", blank, text, flags=re.S)


def split_args(scan: str, open_paren: int) -> list[tuple[int, int]]:
    """Top-level argument spans of the call whose `(` is at open_paren."""
    depth, start, spans = 0, open_paren + 1, []
    pairs = {"(": ")", "[": "]", "{": "}"}
    i = open_paren
    while i < len(scan):
        c = scan[i]
        if c in pairs:
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                spans.append((start, i))
                return [s for s in spans if scan[s[0]:s[1]].strip()]
        elif c == "," and depth == 1:
            spans.append((start, i))
            start = i + 1
        i += 1
    return []


def last_name(name: str) -> str:
    return re.split(r"\.|::", re.sub(r"\s+", "", name))[-1].removeprefix("new").strip()


# Call names whose duration argument is a bound on waiting for something.
WAITISH = re.compile(r"(?i)wait|within|until|eventually|timeout|deadline")
# ... except names that configure a timeout rather than wait on one: a client
# option or a server setting is the behaviour under test, not a test's wait.
CONFIGISH = re.compile(r"(?i)^(with_?|set_?)?(idle_?|keep_?alive_?|request_?|call_?|default_?|max_?|handshake_?|connect_?)?"
                       r"(timeout|deadline)s?(_?(secs|ms|for_testing))?$|^deadlines_for_testing$")


def rust_hits(scan: str) -> list[tuple[int, str]]:
    hits = []
    for m in CALL.finditer(scan):
        name = m.group("name")
        short = last_name(name)
        spans = split_args(scan, m.end() - 1)
        args = [scan[a:b].strip() for a, b in spans]
        if not args:
            continue
        free = "." not in name and not scan[max(0, m.start() - 40):m.start()].rstrip().endswith(".")
        secs = None
        if short in ("timeout", "timeout_at") and free:
            secs = rust_seconds(args[0])
        elif short in ("recv_timeout", "wait_timeout", "wait_timeout_while", "recv_deadline"):
            idx = 1 if short.startswith("wait_timeout") else 0
            secs = rust_seconds(args[idx]) if len(args) > idx else None
        elif WAITISH.search(short) and not CONFIGISH.search(short):
            secs = min((s for s in (rust_seconds(a) for a in args) if s is not None), default=None)
        if secs is not None and secs < SHORT:
            hits.append((m.start(), f"{short}() bounded by a literal {fmt(secs)}"))
    for m in re.finditer(r"Instant::now\(\)\s*\+\s*((?:[\w:]*::)?Duration::from_\w+\([^()]*\))", scan):
        secs = rust_seconds(m.group(1))
        if secs is not None and secs < SHORT:
            hits.append((m.start(), f"deadline Instant::now() + a literal {fmt(secs)}"))
    return hits


def cs_hits(scan: str) -> list[tuple[int, str]]:
    hits = []
    for m in CALL.finditer(scan):
        short = last_name(m.group("name"))
        spans = split_args(scan, m.end() - 1)
        args = [scan[a:b].strip() for a, b in spans]
        if not args:
            continue
        secs = None
        if short in ("WaitAsync", "CancelAfter", "CancellationTokenSource", "Wait"):
            secs = cs_seconds(args[0])
            if short == "Wait" and CS_DUR.match(args[0]) is None:
                secs = None  # Wait(int) is too often an unrelated API
        elif short == "SpinUntil" and len(args) > 1:
            secs = cs_seconds(args[1])
        elif short == "WhenAny":
            for a in args:
                d = re.match(r"Task\.Delay\((.*)\)$", a, re.S)
                if d:
                    s = cs_seconds(d.group(1).strip())
                    if s is not None:
                        secs = s if secs is None else min(secs, s)
        if secs is not None and secs < SHORT:
            hits.append((m.start(), f"{short}() bounded by a literal {fmt(secs)}"))
    return hits


def go_hits(scan: str) -> list[tuple[int, str]]:
    hits = []
    for m in CALL.finditer(scan):
        name = re.sub(r"\s+", "", m.group("name"))
        short = last_name(name)
        spans = split_args(scan, m.end() - 1)
        args = [scan[a:b].strip() for a, b in spans]
        if not args:
            continue
        secs = None
        if name in ("time.After", "time.NewTimer"):
            secs = go_seconds(args[0])
        elif short == "WithTimeout" and len(args) > 1:
            secs = go_seconds(args[1])
        elif short in ("Eventually", "EventuallyWithT", "Eventuallyf") and len(args) > 2:
            secs = go_seconds(args[2])
        elif WAITISH.search(short) and not CONFIGISH.search(short):
            secs = min((s for s in (go_seconds(a) for a in args) if s is not None), default=None)
        if secs is not None and secs < SHORT:
            hits.append((m.start(), f"{short}() bounded by a literal {fmt(secs)}"))
    # A deadline: assigned to a deadline-like name (deadline := time.Now().Add(D))
    # or handed to context.WithDeadline. A time.Now().Add(D) compared or
    # stored as data is not a wait.
    for m in re.finditer(r"(?:\b\w*(?i:deadline|until|end|by)\w*\s*:?=\s*|context\.WithDeadline\([^,()]*,\s*)"
                         r"time\.Now\(\)\s*\.\s*Add\(", scan):
        spans = split_args(scan, m.end() - 1)
        if spans:
            secs = go_seconds(scan[spans[0][0]:spans[0][1]].strip())
            if secs is not None and secs < SHORT:
                hits.append((m.start(), f"deadline time.Now().Add() of a literal {fmt(secs)}"))
    return hits


# setTimeout whose callback ends a race (reject, abort, or resolve with a
# value) rather than resolving a plain sleep.
TS_RACE_CALLBACK = re.compile(r"^(?:\([^()]*\)|\w+)\s*=>")
TS_BOUND_SECOND = {"within", "withTimeout", "raceTimeout", "timeoutAfter"}
TS_WAIT_NAMES = {"waitFor", "waitUntil", "eventually"}


def ts_hits(scan: str) -> list[tuple[int, str]]:
    hits = []
    for m in CALL.finditer(scan):
        name = re.sub(r"\s+", "", m.group("name"))
        short = last_name(name)
        spans = split_args(scan, m.end() - 1)
        args = [scan[a:b].strip() for a, b in spans]
        if not args:
            continue
        secs = None
        if name == "AbortSignal.timeout":
            secs = ts_seconds(args[0])
        elif short == "setTimeout" and name == "setTimeout" and len(args) > 1:
            if TS_RACE_CALLBACK.match(args[0]):
                secs = ts_seconds(args[1])
        elif name.startswith("Atomics."):
            pass  # Atomics.wait stalls a thread on purpose; its arguments are not a bound
        elif short in TS_BOUND_SECOND:
            secs = ts_seconds(args[1]) if len(args) > 1 else None
        elif short in TS_WAIT_NAMES or (WAITISH.search(short) and short.startswith(("wait", "within"))):
            secs = ts_seconds(args[-1])  # waitFor(cond, ms), waitAcknowledgements(limit, ms)
        if secs is not None and secs < SHORT:
            hits.append((m.start(), f"{short}() bounded by a literal {fmt(secs)}"))
    return hits


JAVA_UNIT_CALLS = {"get", "await", "poll", "offer", "orTimeout", "completeOnTimeout", "waitFor", "tryAcquire",
                   "awaitTermination", "recvWithTimeout", "join", "take", "tryLock", "awaitAdvanceInterruptibly"}


def java_hits(scan: str, kotlin: bool) -> list[tuple[int, str]]:
    hits = []
    for m in CALL.finditer(scan):
        short = last_name(m.group("name"))
        spans = split_args(scan, m.end() - 1)
        args = [scan[a:b].strip() for a, b in spans]
        if not args:
            continue
        secs = None
        if short in JAVA_UNIT_CALLS:
            for i in range(len(args) - 1):
                u = TIME_UNIT.match(args[i + 1])
                n = re.match(NUM + r"L?$", args[i])
                if u and n:
                    secs = num(n.group(1)) * TIME_UNIT_SCALE[u.group(1)]
        if secs is None and (WAITISH.search(short) or short.startswith(("await", "next")) or short == "join") \
                and not CONFIGISH.search(short):
            secs = min((s for s in (java_seconds(a) for a in args) if s is not None), default=None)
        if secs is None and kotlin and short in ("withTimeout", "withTimeoutOrNull"):
            secs = ts_seconds(args[0])
        if secs is not None and secs < SHORT:
            hits.append((m.start(), f"{short}() bounded by a literal {fmt(secs)}"))
    return hits


def fmt(secs: float) -> str:
    return f"{secs * 1000:g} ms" if secs < 1 else f"{secs:g} s"


def annotated(lines: list[str], first: int, last: int) -> bool:
    lo = max(first - 1, 1)
    return any(MARKER.search(lines[i - 1]) for i in range(lo, min(last, len(lines)) + 1))


def brace_hits(path: str, text: str) -> list[tuple[int, str]]:
    lines = text.splitlines()
    scan = blank_comments(strip_strings(text, quotes=not path.endswith((".rs", ".go"))))
    if path.endswith(".rs"):
        raw = rust_hits(scan)
    elif path.endswith(".cs"):
        raw = cs_hits(scan)
    elif path.endswith(".go"):
        raw = go_hits(scan)
    elif path.endswith(TS_EXT):
        raw = ts_hits(scan)
    else:
        raw = java_hits(scan, kotlin=path.endswith(".kt"))
    out, seen = [], set()
    for offset, what in raw:
        line = line_of(scan, offset)
        if line in seen:
            continue
        seen.add(line)
        if not annotated(lines, line, line):
            out.append((line, what))
    return out


# ── Python ────────────────────────────────────────────────────────────────

PY_KW_CALLS = {"wait_for", "wait", "join", "result", "get", "communicate", "urlopen", "create_connection",
               "exception", "acquire"}


def py_literal(node: ast.expr | None) -> float | None:
    if isinstance(node, ast.Constant) and isinstance(node.value, (int, float)) and not isinstance(node.value, bool):
        return float(node.value)
    if isinstance(node, ast.IfExp):  # `2 if deadline else 1`: the smaller branch
        branches = [b for b in (py_literal(node.body), py_literal(node.orelse)) if b is not None]
        return min(branches) if branches else None
    return None


def python_hits(text: str) -> list[tuple[int, str]]:
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return []
    lines = text.splitlines()
    hits = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        f = node.func
        name = f.attr if isinstance(f, ast.Attribute) else f.id if isinstance(f, ast.Name) else ""
        owner = f.value.id if isinstance(f, ast.Attribute) and isinstance(f.value, ast.Name) else ""
        kw = {k.arg: k.value for k in node.keywords if k.arg}
        secs = None
        if name == "wait_for" and owner in ("asyncio", ""):
            secs = py_literal(node.args[1]) if len(node.args) > 1 else py_literal(kw.get("timeout"))
        elif name in ("timeout", "timeout_at") and owner == "asyncio":
            secs = py_literal(node.args[0]) if node.args else None
        elif name in PY_KW_CALLS or re.search(r"wait|until", name):
            secs = py_literal(kw.get("timeout"))
            if secs is None and name in ("join", "result") and node.args:
                secs = py_literal(node.args[0])
        if secs is not None and secs < SHORT:
            first, last = node.lineno, node.end_lineno or node.lineno
            if not annotated(lines, first, last):
                hits.append((first, f"{name}() bounded by a literal {fmt(secs)}"))
    return hits


def check_file(path: str, text: str) -> list[tuple[int, str]]:
    if path.endswith(".py"):
        return python_hits(text)
    if path.endswith(C_LIKE):
        return brace_hits(path, text)
    return []


DEFERRED = os.path.join(os.path.dirname(os.path.abspath(__file__)), "test_waits_deferred.txt")


def read_deferred(path: str) -> dict[str, int]:
    """`<count> <path>` per line: suites outside the standard-profile candidate
    whose hits are not converted yet. A file may hold at most its count."""
    deferred: dict[str, int] = {}
    try:
        with open(path, encoding="utf-8") as f:
            for raw in f:
                line = raw.split("#", 1)[0].strip()
                if line:
                    count, rel = line.split(None, 1)
                    deferred[rel] = int(count)
    except FileNotFoundError:
        pass
    return deferred


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--root", default=os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
    ap.add_argument("--all", action="store_true", help="also list the hits test_waits_deferred.txt defers")
    ap.add_argument("files", nargs="*")
    args = ap.parse_args(argv)
    files = args.files or [p for p in tracked_files(args.root) if is_test_file(p) and p not in EXCLUDED_FILES]
    deferred = {} if args.all else read_deferred(DEFERRED)
    total = parked = 0
    for rel in sorted(files):
        full = rel if os.path.isabs(rel) else os.path.join(args.root, rel)
        try:
            with open(full, encoding="utf-8") as f:
                text = f.read()
        except (OSError, UnicodeDecodeError):
            continue
        hits = check_file(rel, text)
        allowed = deferred.get(rel)
        if allowed is not None:
            if len(hits) <= allowed:
                if len(hits) < allowed:
                    print(f"{rel}: {len(hits)} deferred hit(s), test_waits_deferred.txt allows {allowed}; lower it")
                    total += 1
                parked += len(hits)
                continue
            print(f"{rel}: {len(hits)} hits, test_waits_deferred.txt allows {allowed}:")
        for line, what in hits:
            print(f"{rel}:{line}: {what} in test code; use the wait budget, or say why it may expire with "
                  f"`short-wait-ok: <reason>`")
            total += 1
    if total:
        print(f"lint_test_waits: {total} short literal wait(s) in test code", file=sys.stderr)
        return 1
    print(f"lint_test_waits: {len(files)} test files clean ({parked} deferred hits in "
          f"{len(deferred)} files, test_waits_deferred.txt)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
