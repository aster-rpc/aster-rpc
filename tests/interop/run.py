#!/usr/bin/env python3
"""Cross-binding RPC interop smoke -- Rust x Python x TypeScript x Java x .NET x Go, open gate, unary + streaming.

Builds what is needed, checks that all languages derive the same
``contract_id`` for the shared ``InteropEcho`` contract, then runs every
server x client cell (same-language cells included) and prints a matrix.

Usage (from anywhere)::

    python3 tests/interop/run.py                 # build Rust example + FFI lib + Java + .NET + Go + TS dist/codegen, run matrix
    python3 tests/interop/run.py --native        # also rebuild the Python + TS native modules
    python3 tests/interop/run.py --no-build      # just run
    python3 tests/interop/run.py --servers rust --clients python,ts

Cargo goes through ``$ASTER_CARGO_LAUNCHER`` when set (e.g. the Kache
launcher). Needs: cargo, uv (with the Python extension built), bun, node,
mvn + java 22+ (Java runs ``java/InteropEcho.java`` against
``libaster_transport_ffi`` via ``IROH_LIB_PATH``), the .NET 9 SDK (``dotnet/``
is built against the same debug ``libaster_transport_ffi``), Go 1.25+ (``go/``
is built with cgo against the same library and ``ffi/aster_ffi.h``).
Everything is loopback-only with relays disabled, so it works offline.
Exit status is non-zero if any contract id disagrees or any cell fails.
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
# Java, .NET and Go join the default matrix only where their toolchains are
# installed (the Forgejo runner has none of them); name them in
# --servers/--clients to require them.
OPTIONAL_TOOLS = {"java": ("java", "mvn"), "dotnet": ("dotnet",), "go": ("go",)}
DEFAULT_LANGS = tuple(
    lang for lang in LANGS if all(shutil.which(t) for t in OPTIONAL_TOOLS.get(lang, ()))
)

ENV = {
    **os.environ,
    "ASTER_RELAY_MODE": "disabled",
    "ASTER_BIND_ADDR": "127.0.0.1:0",
    "ASTER_LOG_LEVEL": os.environ.get("ASTER_LOG_LEVEL", "warn"),
    "PYTHONUNBUFFERED": "1",
}


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


def java_classpath() -> str:
    """aster-runtime classes + their Maven dependencies."""
    global _java_cp
    if _java_cp is None:
        out = HERE / ".java-classpath.txt"
        subprocess.run(
            ["mvn", "-o", "-q", "-pl", "aster-runtime", "dependency:build-classpath",
             f"-Dmdep.outputFile={out}"],
            cwd=JAVA_DIR, env=ENV, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        deps = [p for p in out.read_text().strip().split(":") if p and "/site/aster/" not in p]
        local = [str(JAVA_DIR / m / "target/classes") for m in ("aster-runtime", "aster-annotations")]
        _java_cp = ":".join(local + deps)
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
        [*launcher, "cargo", "build", "-p", "aster", "--features", "rpc", "--example", "interop_echo"],
        REPO,
    )
    if langs & {"java", "dotnet", "go"}:
        run_step("ffi cdylib", [*launcher, "cargo", "build", "-p", "aster_transport_ffi", "--lib"], REPO)
    if "java" in langs:
        run_step("java aster-runtime", ["mvn", "-B", "-q", "-o", "-pl", "aster-runtime", "-am", "compile"],
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
    ap.add_argument("--servers", default=",".join(DEFAULT_LANGS))
    ap.add_argument("--clients", default=",".join(DEFAULT_LANGS))
    args = ap.parse_args()
    servers = [s for s in args.servers.split(",") if s]
    clients = [c for c in args.clients.split(",") if c]
    for lang, tools in OPTIONAL_TOOLS.items():
        if lang not in DEFAULT_LANGS:
            print(f"[note] {'/'.join(tools)} not on PATH: {lang} leg skipped by default", flush=True)

    if not args.no_build:
        build(args.native, set(servers) | set(clients))

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
