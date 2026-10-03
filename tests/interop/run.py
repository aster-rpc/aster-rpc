#!/usr/bin/env python3
"""Cross-binding RPC interop smoke -- Rust x Python x TypeScript, open gate, unary + streaming.

Builds what is needed, checks that all three languages derive the same
``contract_id`` for the shared ``InteropEcho`` contract, then runs every
server x client cell (same-language cells included) and prints a matrix.

Usage (from anywhere)::

    python3 tests/interop/run.py                 # build Rust example + TS dist/codegen, run matrix
    python3 tests/interop/run.py --native        # also rebuild the Python + TS native modules
    python3 tests/interop/run.py --no-build      # just run
    python3 tests/interop/run.py --servers rust --clients python,ts

Cargo goes through ``$ASTER_CARGO_LAUNCHER`` when set (e.g. the Kache
launcher). Needs: cargo, uv (with the Python extension built), bun, node.
Everything is loopback-only with relays disabled, so it works offline.
Exit status is non-zero if any contract id disagrees or any cell fails.
"""

from __future__ import annotations

import argparse
import os
import queue
import shlex
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
TS_DIR = HERE / "ts"
TS_PKG = REPO / "bindings/typescript/packages/aster"
LANGS = ("rust", "python", "ts")

ENV = {
    **os.environ,
    "ASTER_RELAY_MODE": "disabled",
    "ASTER_BIND_ADDR": "127.0.0.1:0",
    "ASTER_LOG_LEVEL": os.environ.get("ASTER_LOG_LEVEL", "warn"),
    "PYTHONUNBUFFERED": "1",
}


def rust_bin() -> Path:
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
    return target / "debug/examples/interop_echo"


def command(lang: str, *args: str) -> tuple[list[str], Path]:
    if lang == "rust":
        return [str(rust_bin()), *args], REPO
    if lang == "python":
        return ["uv", "run", "--no-sync", "python", str(HERE / "python/interop_echo.py"), *args], REPO
    if lang == "ts":
        return ["bun", "run", "interop_echo.ts", *args], TS_DIR
    raise ValueError(lang)


def run_step(desc: str, cmd: list[str], cwd: Path) -> None:
    print(f"[build] {desc}: {shlex.join(cmd)}", flush=True)
    res = subprocess.run(cmd, cwd=cwd, env=ENV)
    if res.returncode != 0:
        sys.exit(f"[build] {desc} failed (exit {res.returncode})")


def build(native: bool) -> None:
    launcher = shlex.split(os.environ.get("ASTER_CARGO_LAUNCHER", ""))
    if native:
        run_step("python native", [*launcher, "./scripts/build.sh"], REPO)
        run_step("ts native", [*launcher, "bun", "run", "build"], REPO / "bindings/typescript/native")
    run_step(
        "rust interop_echo",
        [*launcher, "cargo", "build", "-p", "aster", "--features", "rpc", "--example", "interop_echo"],
        REPO,
    )
    run_step("ts package (tsc)", ["bun", "run", "build"], TS_PKG)
    run_step(
        "ts aster-gen",
        ["node", str(TS_PKG / "dist/cli/gen.js"), "-p", "tsconfig.json", "-o", "aster-rpc.generated.ts"],
        TS_DIR,
    )


def contract_ids(langs: list[str]) -> dict[str, str]:
    out: dict[str, str] = {}
    for lang in langs:
        cmd, cwd = command(lang, "contract")
        res = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True, text=True, timeout=120)
        cid = next(
            (ln.split()[1] for ln in res.stdout.splitlines() if ln.startswith("CONTRACT_ID ")),
            f"<error: exit {res.returncode}: {res.stderr.strip()[-200:]}>",
        )
        out[lang] = cid
    return out


class Server:
    def __init__(self, lang: str) -> None:
        self.lang = lang
        cmd, cwd = command(lang, "server")
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


def run_client(lang: str, ticket: str) -> tuple[bool, list[str]]:
    cmd, cwd = command(lang, "client", ticket)
    try:
        res = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return False, ["client timed out after 120s"]
    cases = [ln for ln in res.stdout.splitlines() if ln.startswith("CASE ")]
    ok = res.returncode == 0 and bool(cases) and all(" OK " in f"{c} " for c in cases)
    if not cases:
        cases = [f"no CASE output (exit {res.returncode}): {res.stderr.strip()[-400:]}"]
    return ok, cases


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--no-build", action="store_true", help="skip all build steps")
    ap.add_argument("--native", action="store_true", help="also rebuild the Python and TS native modules")
    ap.add_argument("--servers", default=",".join(LANGS))
    ap.add_argument("--clients", default=",".join(LANGS))
    args = ap.parse_args()
    servers = [s for s in args.servers.split(",") if s]
    clients = [c for c in args.clients.split(",") if c]

    if not args.no_build:
        build(args.native)

    failed = False
    ids = contract_ids(sorted(set(servers) | set(clients), key=LANGS.index))
    print("\ncontract_id (InteropEcho v1):")
    for lang, cid in ids.items():
        print(f"  {lang:<7} {cid}")
    if len(set(ids.values())) != 1:
        print("  MISMATCH: contract ids differ across languages")
        failed = True
    else:
        print("  all agree")

    results: dict[tuple[str, str], tuple[bool, list[str]]] = {}
    for s in servers:
        srv = Server(s)
        try:
            ticket = srv.address()
            if ticket is None:
                detail = f"{s} server did not print ADDRESS; stderr tail: {' | '.join(srv.stderr[-5:])}"
                for c in clients:
                    results[(s, c)] = (False, [detail])
                continue
            for c in clients:
                print(f"[run] server={s:<6} client={c:<6} ...", end=" ", flush=True)
                results[(s, c)] = run_client(c, ticket)
                print("PASS" if results[(s, c)][0] else "FAIL", flush=True)
        finally:
            srv.stop()

    w = 8
    print("\nserver \\ client " + "".join(f"{c:<{w}}" for c in clients))
    for s in servers:
        row = "".join(f"{('PASS' if results[(s, c)][0] else 'FAIL'):<{w}}" for c in clients)
        print(f"{s:<16}{row}")

    fails = [(k, v[1]) for k, v in results.items() if not v[0]]
    if fails:
        failed = True
        print("\nfailures:")
        for (s, c), lines in fails:
            print(f"  server={s} client={c}")
            for ln in lines:
                print(f"    {ln}")
    total = len(results)
    print(f"\n{total - len(fails)}/{total} cells passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
