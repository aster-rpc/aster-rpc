#!/usr/bin/env python3
"""Cross-binding interop matrix -- Rust x Python x TypeScript x Java x .NET x Go, open gate.

Builds what is needed, checks that all languages derive the same contract ids
for the shared ``InteropEcho`` and ``InteropSession`` contracts, then runs two
matrices and reports **per-case** results (a failing case is named, not just
its cell):

* RPC matrix: every server x client cell (same-language cells included). Each
  client runs ``RPC_CASES``: unary (small, zstd, long_string), server_stream,
  client_stream, bidi (interleaved ``chat``), deadline and cancel (``hold``
  blocks; the client gets DEADLINE_EXCEEDED / CANCELLED and the server must
  observe the cancellation) and session (state persists per session, isolated
  between sessions).
* Expose matrix: every language serves an HTTP handler through expose; the
  Rust and Python streaming-relay consumers call each one (``EXPOSE_CASES``).

Usage (from anywhere)::

    python3 tests/interop/run.py                 # build everything, run both matrices
    python3 tests/interop/run.py --native        # also rebuild the Python + TS native modules
    python3 tests/interop/run.py --no-build      # just run
    python3 tests/interop/run.py --servers rust --clients python,ts
    python3 tests/interop/run.py --no-expose     # RPC matrix only
    python3 tests/interop/run.py --require-all   # a missing toolchain is an error, not a skipped leg

Cargo goes through ``$ASTER_CARGO_LAUNCHER`` when set (e.g. the Kache
launcher). Needs: cargo, uv (with the Python extension built), bun, node,
mvn + java 22+ (Java runs ``java/InteropEcho.java`` against
``libaster_transport_ffi`` via ``IROH_LIB_PATH``; Maven runs offline unless
``--mvn-online``), the .NET 9 SDK (``dotnet/`` is built against the same debug
``libaster_transport_ffi``), Go 1.25+ (``go/`` is built with cgo against the
same library and ``ffi/aster_ffi.h``). Everything is loopback-only with relays
disabled, so it works offline. Exit status is non-zero if any contract id
disagrees, any case fails unexpectedly, or an expected failure starts passing.

Program protocol (every language): ``<prog> contract | server | client <ticket>
| expose-server | expose-client <ticket>``; stdout lines ``CONTRACT_ID <hex>``,
``SESSION_CONTRACT_ID <hex>``, ``ADDRESS <aster1...>`` (servers) and
``CASE <name> OK|FAIL <detail>`` (clients). Servers run until stdin closes.
"""

from __future__ import annotations

import argparse
import os
import queue
import shlex
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
TS_DIR = HERE / "ts"
TS_PKG = REPO / "bindings/typescript/packages/aster"
JAVA_DIR = REPO / "bindings/java"
DOTNET_DIR = HERE / "dotnet"
GO_DIR = HERE / "go"
LANGS = ("rust", "python", "ts", "java", "dotnet", "go")
# Languages that can consume an exposed service (the streaming relay): the
# expose matrix is every language serving x these consuming.
EXPOSE_CLIENTS = ("rust", "python")
# Java, .NET and Go join the default matrix only where their toolchains are
# installed; name them in --servers/--clients (or pass --require-all) to
# require them.
OPTIONAL_TOOLS = {"java": ("java", "mvn"), "dotnet": ("dotnet",), "go": ("go",)}
DEFAULT_LANGS = tuple(
    lang for lang in LANGS if all(shutil.which(t) for t in OPTIONAL_TOOLS.get(lang, ()))
)

RPC_CASES = (
    "small", "zstd", "long_string",
    "server_stream", "client_stream",
    "bidi", "deadline", "cancel", "session",
)
EXPOSE_CASES = ("expose_hello", "expose_echo_stream")

# Known binding bugs: (matrix, server, client, case) -> reason; "*" matches any
# language. A listed case that FAILs is reported XFAIL and does not fail the
# run; one that PASSes is XPASS and does (remove the entry). Keep empty unless
# a binding bug blocks a case, and say which bug.
EXPECTED_FAIL: dict[tuple[str, str, str, str], str] = {}

ENV = {
    **os.environ,
    "ASTER_RELAY_MODE": "disabled",
    "ASTER_BIND_ADDR": "127.0.0.1:0",
    "ASTER_LOG_LEVEL": os.environ.get("ASTER_LOG_LEVEL", "warn"),
    "PYTHONUNBUFFERED": "1",
}

CLIENT_TIMEOUT = 180  # seconds for one client run (all cases)


def target_dir() -> Path:
    return Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))


def rust_bin() -> Path:
    return target_dir() / "debug/examples/interop_echo"


def ffi_lib() -> Path:
    name = "libaster_transport_ffi.dylib" if sys.platform == "darwin" else "libaster_transport_ffi.so"
    return target_dir() / "debug" / name


# The Java leg loads the freshly built debug FFI library explicitly (the Java
# loader would otherwise prefer a possibly stale target/release copy).
ENV["IROH_LIB_PATH"] = str(ffi_lib())

_java_cp: str | None = None
_mvn_offline = True


def java_classpath() -> str:
    """aster-runtime classes + their Maven dependencies."""
    global _java_cp
    if _java_cp is None:
        out = HERE / ".java-classpath.txt"
        # -am + compile: sibling modules (aster-annotations) resolve from the
        # reactor, not the local Maven repo, which a fresh CI cache lacks.
        # The last module built is aster-runtime, so its classpath wins.
        proc = subprocess.run(
            ["mvn", "-B", *(["-o"] if _mvn_offline else []), "-q", "-pl", "aster-runtime", "-am",
             "compile", "dependency:build-classpath", f"-Dmdep.outputFile={out}"],
            cwd=JAVA_DIR, env=ENV, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        if proc.returncode != 0:
            sys.exit(f"java classpath: mvn failed (exit {proc.returncode}):\n{proc.stdout}")
        deps = [p for p in out.read_text().strip().split(os.pathsep) if p and "/site/aster/" not in p]
        local = [str(JAVA_DIR / m / "target/classes") for m in ("aster-runtime", "aster-annotations")]
        _java_cp = os.pathsep.join(local + deps)
    return _java_cp


def command(lang: str, *args: str) -> tuple[list[str], Path]:
    if lang == "rust":
        return [str(rust_bin()), *args], REPO
    if lang == "python":
        return ["uv", "run", "--no-sync", "python", str(HERE / "python/interop_echo.py"), *args], REPO
    if lang == "ts":
        return ["bun", "run", "interop_echo.ts", *args], TS_DIR
    if lang == "java":
        return ["java", "--enable-native-access=ALL-UNNAMED", "-cp", java_classpath(),
                str(HERE / "java/InteropEcho.java"), *args], REPO
    if lang == "dotnet":
        return ["dotnet", str(DOTNET_DIR / "bin/Release/net9.0/InteropEcho.dll"), *args], REPO
    if lang == "go":
        return [str(GO_DIR / "interop_echo"), *args], REPO
    raise ValueError(lang)


def run_step(desc: str, cmd: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    print(f"[build] {desc}: {shlex.join(cmd)}", flush=True)
    res = subprocess.run(cmd, cwd=cwd, env=env or ENV)
    if res.returncode != 0:
        sys.exit(f"[build] {desc} failed (exit {res.returncode})")


def build(native: bool, langs: set[str]) -> None:
    launcher = shlex.split(os.environ.get("ASTER_CARGO_LAUNCHER", ""))
    if native:
        run_step("python native", [*launcher, "./scripts/build.sh"], REPO)
        run_step("ts native", [*launcher, "bun", "run", "build"], REPO / "bindings/typescript/native")
    run_step(
        "rust interop_echo",
        [*launcher, "cargo", "build", "-p", "aster", "--features", "rpc,expose", "--example", "interop_echo"],
        REPO,
    )
    if langs & {"java", "dotnet", "go"}:
        run_step("ffi cdylib", [*launcher, "cargo", "build", "-p", "aster_transport_ffi", "--lib"], REPO)
    if "java" in langs:
        run_step("java aster-runtime",
                 ["mvn", "-B", "-q", *(["-o"] if _mvn_offline else []), "-pl", "aster-runtime", "-am", "compile"],
                 JAVA_DIR)
    if "dotnet" in langs:
        # Pin the native library to the debug build above (the .NET project would
        # otherwise prefer a possibly stale target/release copy).
        run_step("dotnet InteropEcho",
                 ["dotnet", "build", "-c", "Release", "--nologo", "-v", "q",
                  f"-p:NativeLibraryPath={ffi_lib().parent}", str(DOTNET_DIR)], REPO)
    if "go" in langs:
        # cgo against the debug FFI library above; the rpath lets the binary
        # find it at run time without DYLD/LD_LIBRARY_PATH.
        lib_dir = ffi_lib().parent
        go_env = {
            **ENV,
            "CGO_ENABLED": "1",
            "CGO_CFLAGS": f"-I{REPO / 'ffi'}",
            "CGO_LDFLAGS": f"-L{lib_dir} -laster_transport_ffi -Wl,-rpath,{lib_dir}",
        }
        run_step("go interop_echo", ["go", "build", "-o", str(GO_DIR / "interop_echo"), "."], GO_DIR, go_env)
    run_step("ts package (tsc)", ["bun", "run", "build"], TS_PKG)
    run_step(
        "ts aster-gen",
        ["node", str(TS_PKG / "dist/cli/gen.js"), "-p", "tsconfig.json", "-o", "aster-rpc.generated.ts"],
        TS_DIR,
    )


def contract_ids(langs: list[str]) -> dict[str, tuple[str, str]]:
    """lang -> (InteropEcho id, InteropSession id)."""
    out: dict[str, tuple[str, str]] = {}
    for lang in langs:
        cmd, cwd = command(lang, "contract")
        try:
            res = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True, text=True, timeout=120)
        except subprocess.TimeoutExpired:
            out[lang] = ("<error: contract timed out>",) * 2
            continue
        found = {}
        for ln in res.stdout.splitlines():
            parts = ln.split()
            if len(parts) == 2 and parts[0] in ("CONTRACT_ID", "SESSION_CONTRACT_ID"):
                found[parts[0]] = parts[1]
        err = f"<error: exit {res.returncode}: {res.stderr.strip()[-200:]}>"
        out[lang] = (found.get("CONTRACT_ID", err), found.get("SESSION_CONTRACT_ID", err))
    return out


class Server:
    def __init__(self, lang: str, mode: str = "server") -> None:
        self.lang = lang
        cmd, cwd = command(lang, mode)
        self.proc = subprocess.Popen(
            cmd, cwd=cwd, env=ENV, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.lines: queue.Queue[str] = queue.Queue()
        self.stderr: list[str] = []
        threading.Thread(target=self._pump, args=(self.proc.stdout, self.lines.put), daemon=True).start()
        threading.Thread(target=self._pump, args=(self.proc.stderr, self.stderr.append), daemon=True).start()

    @staticmethod
    def _pump(stream, sink) -> None:
        for line in stream:
            sink(line.rstrip("\n"))

    def address(self, timeout: float = 90.0) -> str | None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                line = self.lines.get(timeout=0.5)
            except queue.Empty:
                if self.proc.poll() is not None:
                    return None
                continue
            if line.startswith("ADDRESS "):
                return line.split()[1]
        return None

    def stop(self) -> None:
        try:
            self.proc.stdin.close()  # servers exit on stdin EOF
            self.proc.wait(timeout=10)
        except Exception:  # noqa: BLE001
            self.proc.kill()
            self.proc.wait()


# One cell's per-case results: case -> (ok, detail), in the order reported.
CaseResults = dict[str, tuple[bool, str]]


def run_client(lang: str, mode: str, ticket: str, expected: tuple[str, ...]) -> CaseResults:
    cmd, cwd = command(lang, mode, ticket)
    results: CaseResults = {}
    try:
        res = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True, text=True, timeout=CLIENT_TIMEOUT)
    except subprocess.TimeoutExpired as exc:
        out = exc.stdout.decode(errors="replace") if isinstance(exc.stdout, bytes) else (exc.stdout or "")
        _parse_cases(out, results)
        for case in expected:
            results.setdefault(case, (False, f"client timed out after {CLIENT_TIMEOUT}s"))
        return results
    _parse_cases(res.stdout, results)
    if "connect" in results and not results["connect"][0]:
        # Connect failure: every case is a failure of the same cause.
        detail = results["connect"][1]
        return {case: (False, f"connect failed: {detail}") for case in expected}
    tail = res.stderr.strip()[-400:]
    for case in expected:
        results.setdefault(case, (False, f"no CASE line (client exit {res.returncode}): {tail}"))
    return {case: results[case] for case in expected}


def _parse_cases(stdout: str, results: CaseResults) -> None:
    for ln in stdout.splitlines():
        if not ln.startswith("CASE "):
            continue
        parts = ln.split(" ", 3)
        if len(parts) < 3:
            continue
        results[parts[1]] = (parts[2] == "OK", parts[3] if len(parts) > 3 else "")


def classify(matrix: str, server: str, client: str, case: str, ok: bool) -> str:
    """PASS / FAIL / XFAIL (expected failure) / XPASS (expected failure that passed)."""
    reason = None
    for s in (server, "*"):
        for c in (client, "*"):
            reason = reason or EXPECTED_FAIL.get((matrix, s, c, case))
    if reason is None:
        return "PASS" if ok else "FAIL"
    return "XPASS" if ok else "XFAIL"


def run_matrix(
    matrix: str, servers: list[str], clients: list[str], server_mode: str, client_mode: str,
    expected: tuple[str, ...],
) -> dict[tuple[str, str], dict[str, str]]:
    """(server, client) -> case -> status; prints per-cell progress."""
    out: dict[tuple[str, str], dict[str, str]] = {}
    details: dict[tuple[str, str, str], str] = {}
    for s in servers:
        srv = Server(s, server_mode)
        try:
            ticket = srv.address()
            if ticket is None:
                reason = f"{s} {server_mode} did not print ADDRESS; stderr tail: {' | '.join(srv.stderr[-5:])}"
                for c in clients:
                    out[(s, c)] = {case: "FAIL" for case in expected}
                    for case in expected:
                        details[(s, c, case)] = reason
                    print(f"[{matrix}] server={s:<6} client={c:<6} FAIL (server did not start)", flush=True)
                continue
            for c in clients:
                print(f"[{matrix}] server={s:<6} client={c:<6} ...", end=" ", flush=True)
                results = run_client(c, client_mode, ticket, expected)
                status = {}
                for case, (ok, detail) in results.items():
                    status[case] = classify(matrix, s, c, case, ok)
                    details[(s, c, case)] = detail
                out[(s, c)] = status
                bad = [k for k, v in status.items() if v in ("FAIL", "XPASS")]
                print(f"{sum(v in ('PASS', 'XFAIL') for v in status.values())}/{len(status)}"
                      + (f"  failing: {', '.join(bad)}" if bad else ""), flush=True)
        finally:
            srv.stop()
    _DETAILS.update({(matrix, *k): v for k, v in details.items()})
    return out


_DETAILS: dict[tuple[str, str, str, str], str] = {}


def report(matrix: str, title: str, servers: list[str], clients: list[str], cases: tuple[str, ...],
           results: dict[tuple[str, str], dict[str, str]]) -> bool:
    """Print the cell matrix and the per-case table; return True if all green."""
    print(f"\n{title}: cells (cases passed / total)")
    w = 9
    print("server \\ client ".ljust(16) + "".join(f"{c:<{w}}" for c in clients))
    for s in servers:
        row = ""
        for c in clients:
            st = results.get((s, c), {})
            good = sum(v in ("PASS", "XFAIL") for v in st.values())
            row += f"{good}/{len(cases):<{w - 2}}"
        print(f"{s:<16}{row}")

    print(f"\n{title}: per case (cells passed / total; failing cells are server>client)")
    name_w = max(len(c) for c in cases)
    for case in cases:
        sts = {k: st.get(case, "FAIL") for k, st in results.items()}
        good = sum(v in ("PASS", "XFAIL") for v in sts.values())
        bad = [f"{s}>{c}" + ("(xfail)" if v == "XFAIL" else "") for (s, c), v in sts.items() if v != "PASS"]
        print(f"  {case:<{name_w}}  {good}/{len(sts)}" + (f"  not passing: {', '.join(bad)}" if bad else ""))

    ok = True
    problems = [(k, case, v) for k, st in results.items() for case, v in st.items() if v in ("FAIL", "XPASS")]
    xfails = [(k, case) for k, st in results.items() for case, v in st.items() if v == "XFAIL"]
    if xfails:
        print(f"\n{title}: expected failures")
        for (s, c), case in xfails:
            print(f"  server={s} client={c} case={case}: {_reason(matrix, s, c, case)}")
    if problems:
        ok = False
        print(f"\n{title}: failures")
        for (s, c), case, v in problems:
            detail = _DETAILS.get((matrix, s, c, case), "")
            note = " (expected failure now passes: remove it from EXPECTED_FAIL)" if v == "XPASS" else ""
            print(f"  server={s} client={c} case={case}{note}: {detail}")
    total_cells = len(results)
    good_cells = sum(all(v in ("PASS", "XFAIL") for v in st.values()) for st in results.values())
    total_cases = sum(len(st) for st in results.values())
    good_cases = sum(v in ("PASS", "XFAIL") for st in results.values() for v in st.values())
    print(f"\n{title}: {good_cells}/{total_cells} cells, {good_cases}/{total_cases} cases passed")
    return ok


def _reason(matrix: str, s: str, c: str, case: str) -> str:
    for ss in (s, "*"):
        for cc in (c, "*"):
            if (matrix, ss, cc, case) in EXPECTED_FAIL:
                return EXPECTED_FAIL[(matrix, ss, cc, case)]
    return ""


def main() -> int:
    global _mvn_offline
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--no-build", action="store_true", help="skip all build steps")
    ap.add_argument("--native", action="store_true", help="also rebuild the Python and TS native modules")
    ap.add_argument("--servers", default=",".join(DEFAULT_LANGS))
    ap.add_argument("--clients", default=",".join(DEFAULT_LANGS))
    ap.add_argument("--no-expose", action="store_true", help="skip the expose/relay matrix")
    ap.add_argument("--no-rpc", action="store_true", help="skip the RPC matrix")
    ap.add_argument("--require-all", action="store_true",
                    help="all six languages are required: a missing toolchain is an error")
    ap.add_argument("--mvn-online", action="store_true", help="let Maven resolve dependencies (default: offline)")
    args = ap.parse_args()
    _mvn_offline = not args.mvn_online

    if args.require_all:
        missing = [f"{lang} ({'/'.join(t)})" for lang, t in OPTIONAL_TOOLS.items() if lang not in DEFAULT_LANGS]
        if missing:
            sys.exit(f"--require-all: toolchain missing for: {', '.join(missing)}")
    servers = [s for s in args.servers.split(",") if s]
    clients = [c for c in args.clients.split(",") if c]
    for lang, tools in OPTIONAL_TOOLS.items():
        if lang not in DEFAULT_LANGS:
            print(f"[note] {'/'.join(tools)} not on PATH: {lang} leg skipped by default", flush=True)

    if not args.no_build:
        build(args.native, set(servers) | set(clients))

    failed = False
    ids = contract_ids(sorted(set(servers) | set(clients), key=LANGS.index))
    for idx, label in enumerate(("InteropEcho v1", "InteropSession v1 (session-scoped)")):
        print(f"\ncontract_id ({label}):")
        for lang, cids in ids.items():
            print(f"  {lang:<7} {cids[idx]}")
        if len({cids[idx] for cids in ids.values()}) != 1:
            print("  MISMATCH: contract ids differ across languages")
            failed = True
        else:
            print("  all agree")

    if not args.no_rpc:
        rpc = run_matrix("rpc", servers, clients, "server", "client", RPC_CASES)
        failed |= not report("rpc", "RPC matrix", servers, clients, RPC_CASES, rpc)
    if not args.no_expose:
        ex_clients = [c for c in clients if c in EXPOSE_CLIENTS]
        expose = run_matrix("expose", servers, ex_clients, "expose-server", "expose-client", EXPOSE_CASES)
        failed |= not report("expose", "Expose matrix", servers, ex_clients, EXPOSE_CASES, expose)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
