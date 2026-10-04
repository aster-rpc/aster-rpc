#!/usr/bin/env python3
"""Dispatch benchmark orchestrator (dynamic-dispatch-design.md section 6).

Starts each ``InteropEcho`` server (``tests/interop/`` Rust/Python/TS, plus
``tests/interop/java/InteropEcho.java``), drives it with its own-language
client and with the Rust load driver (``driver.rs``: the raw pooled path and
``RpcConnection::unary``, pooled since R3d), repeats every configuration
``--runs`` times (fresh server per run, runs interleaved across
configurations), and reports the median of each
metric. Results go to ``--out`` (JSON, raw runs + medians) and stdout
(markdown tables).

Usage (from anywhere)::

    python3 benchmarks/dispatch/run.py --build            # build everything, then measure
    python3 benchmarks/dispatch/run.py                    # measure only
    python3 benchmarks/dispatch/run.py --servers python --clients own,rust --runs 1 --duration 2

``--build`` runs: Python native (``scripts/build.sh``), TS native (``bun run
build``), the release FFI library, the release ``interop_echo`` +
``dispatch_driver`` examples, the TS package + ``aster-gen`` for the interop
contract, and the Java modules (``mvn -P fast -o package``). Cargo goes through
``$ASTER_CARGO_LAUNCHER`` when set (e.g. the Kache launcher).

Loopback only with relays disabled, except the Java leg: the Java/FFI node
cannot be configured for loopback today (``aster_node_memory_with_alpns``
takes no endpoint config), so its ticket carries no direct address. Clients
of the Java server dial by endpoint id through the default relay + address
lookup (needs network) and then run on the direct loopback path.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import queue
import shlex
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
INTEROP = REPO / "tests/interop"
JAVA_DIR = REPO / "bindings/java"
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
SERVERS = ("rust", "python", "ts", "java")
CLIENTS = ("own", "rust", "rust-rpc")

ENV = {
    **os.environ,
    "ASTER_RELAY_MODE": "disabled",
    "ASTER_BIND_ADDR": "127.0.0.1:0",
    "ASTER_LOG_LEVEL": os.environ.get("ASTER_LOG_LEVEL", "warn"),
    "PYTHONUNBUFFERED": "1",
    "IROH_LIB_PATH": str(TARGET / "release" / ("libaster_transport_ffi" + (".dylib" if sys.platform == "darwin" else ".so"))),
}


def sh(desc: str, cmd: list[str], cwd: Path = REPO) -> None:
    print(f"[build] {desc}: {shlex.join(cmd)}", flush=True)
    if subprocess.run(cmd, cwd=cwd, env=ENV).returncode != 0:
        sys.exit(f"[build] {desc} failed")


def build() -> None:
    launcher = shlex.split(os.environ.get("ASTER_CARGO_LAUNCHER", ""))
    sh("python native (release)", [*launcher, "./scripts/build.sh"])
    sh("ts native (release)", [*launcher, "bun", "run", "build"], REPO / "bindings/typescript/native")
    sh("ffi (release)", [*launcher, "cargo", "build", "--release", "-p", "aster_transport_ffi"])
    sh("rust examples (release)", [
        *launcher, "cargo", "build", "--release", "-p", "aster", "--features", "rpc,binding-support",
        "--example", "interop_echo", "--example", "dispatch_driver",
    ])
    pkg = REPO / "bindings/typescript/packages/aster"
    sh("ts package (tsc)", ["bun", "run", "build"], pkg)
    sh("ts aster-gen", ["node", str(pkg / "dist/cli/gen.js"), "-p", "tsconfig.json", "-o", "aster-rpc.generated.ts"],
       INTEROP / "ts")
    sh("java modules", ["mvn", "-P", "fast", "-o", "-q", "-pl", "aster-examples-mission-control", "-am", "package",
                        "-DskipTests"], JAVA_DIR)


_java_cp: str | None = None


def java_classpath() -> str:
    """Runtime classpath: the freshly built module classes + their Maven deps."""
    global _java_cp
    if _java_cp is None:
        out = HERE / ".java-classpath.txt"
        subprocess.run(
            ["mvn", "-o", "-q", "-pl", "aster-examples-mission-control", "dependency:build-classpath",
             f"-Dmdep.outputFile={out}"],
            cwd=JAVA_DIR, env=ENV, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        deps = [p for p in out.read_text().strip().split(":") if p and "/site/aster/" not in p]
        local = [str(JAVA_DIR / m / "target/classes") for m in ("aster-runtime", "aster-annotations")]
        _java_cp = ":".join(local + deps)
    return _java_cp


def java_cmd(*args: str) -> list[str]:
    return ["java", "--enable-native-access=ALL-UNNAMED", "-cp", java_classpath(),
            str(INTEROP / "java/InteropEcho.java"), *args]


def server_cmd(lang: str) -> tuple[list[str], Path]:
    if lang == "rust":
        return [str(TARGET / "release/examples/interop_echo"), "server"], REPO
    if lang == "python":
        return ["uv", "run", "--no-sync", "python", str(INTEROP / "python/interop_echo.py"), "server"], REPO
    if lang == "ts":
        return ["bun", "run", "interop_echo.ts", "server"], INTEROP / "ts"
    if lang == "java":
        return java_cmd("server"), REPO
    raise ValueError(lang)


def client_cmd(server: str, client: str, ticket: str, opts: list[str]) -> tuple[list[str], Path]:
    driver = str(TARGET / "release/examples/dispatch_driver")
    online = ["--online"] if server == "java" else []
    if client == "rust" or (client == "own" and server == "rust"):
        return [driver, ticket, *opts, *online], REPO
    if client == "rust-rpc":
        return [driver, ticket, "--mode", "rpc", "--skip-streams", *opts, *online], REPO
    if server == "python":
        return ["uv", "run", "--no-sync", "python", str(HERE / "bench_client.py"), ticket, *opts], REPO
    if server == "ts":
        return ["bun", "run", str(HERE / "bench_client.ts"), ticket, *opts], REPO
    if server == "java":
        return java_cmd("bench", ticket, *opts), REPO
    raise ValueError((server, client))


class Server:
    def __init__(self, lang: str) -> None:
        cmd, cwd = server_cmd(lang)
        self.proc = subprocess.Popen(cmd, cwd=cwd, env=ENV, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
        self.lines: queue.Queue[str] = queue.Queue()
        self.stderr: list[str] = []
        threading.Thread(target=self._pump, args=(self.proc.stdout, self.lines.put), daemon=True).start()
        threading.Thread(target=self._pump, args=(self.proc.stderr, self.stderr.append), daemon=True).start()

    @staticmethod
    def _pump(stream, sink) -> None:
        for line in stream:
            sink(line.rstrip("\n"))

    def address(self, timeout: float = 120.0) -> str | None:
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
            self.proc.stdin.close()
            self.proc.wait(timeout=15)
        except Exception:  # noqa: BLE001
            self.proc.kill()
            self.proc.wait()


def run_one(server: str, client: str, opts: list[str]) -> tuple[list[dict], str]:
    srv = Server(server)
    try:
        ticket = srv.address()
        if ticket is None:
            return [], f"server did not start: {' | '.join(srv.stderr[-5:])}"
        cmd, cwd = client_cmd(server, client, ticket, opts)
        res = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True, text=True, timeout=1800)
        results = [json.loads(ln[len("RESULT "):]) for ln in res.stdout.splitlines() if ln.startswith("RESULT ")]
        err = "" if res.returncode == 0 else f"client exit {res.returncode}: {res.stderr.strip()[-600:]}"
        warn = [ln for ln in res.stderr.splitlines() if ln.startswith("worker error")]
        if warn:
            err = (err + " " if err else "") + "; ".join(warn[:3])
        return results, err
    finally:
        srv.stop()


def key_of(r: dict) -> str:
    if r["kind"] == "unary":
        return f"unary/{r['conc']}"
    if r["kind"] == "server_stream":
        return f"server_stream/{r['frames']}"
    return f"client_stream/{r['messages']}"


METRICS = {"unary": ("rps", "p50_ms", "p90_ms", "p99_ms", "errors"),
           "server_stream": ("frames_per_s",), "client_stream": ("msgs_per_s",)}


def medians(runs: list[list[dict]]) -> dict[str, dict]:
    by_key: dict[str, list[dict]] = {}
    for run in runs:
        for r in run:
            by_key.setdefault(key_of(r), []).append(r)
    out = {}
    for k, rs in by_key.items():
        m = {f: statistics.median(r[f] for r in rs) for f in METRICS[rs[0]["kind"]]}
        m["n_runs"] = len(rs)
        if rs[0]["kind"] == "unary":
            m["rps_min"] = min(r["rps"] for r in rs)
            m["rps_max"] = max(r["rps"] for r in rs)
        out[k] = m
    return out


def cmd_out(cmd: list[str]) -> str:
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=60, env=ENV)
        return (r.stdout or r.stderr).strip().splitlines()[0]
    except Exception as exc:  # noqa: BLE001
        return f"<{exc}>"


def machine_info() -> dict:
    info = {
        "os": f"{platform.system()} {platform.release()} ({platform.platform()})",
        "machine": platform.machine(),
        "git": cmd_out(["git", "-C", str(REPO), "rev-parse", "HEAD"]),
        "git_dirty": bool(subprocess.run(["git", "-C", str(REPO), "status", "--porcelain", "--untracked-files=no"],
                                         capture_output=True, text=True).stdout.strip()),
        "python": cmd_out(["uv", "run", "--no-sync", "python", "--version"]),
        "bun": cmd_out(["bun", "--version"]),
        "node": cmd_out(["node", "--version"]),
        "java": cmd_out(["java", "-version"]),
        "rustc": cmd_out(["rustc", "--version"]),
        "build": "release (cargo --release, maturin build release, napi --release)",
        "loadavg_start": os.getloadavg(),
    }
    if sys.platform == "darwin":
        info["cpu"] = cmd_out(["sysctl", "-n", "machdep.cpu.brand_string"])
        info["cores"] = (f"{cmd_out(['sysctl', '-n', 'hw.ncpu'])} logical "
                         f"({cmd_out(['sysctl', '-n', 'hw.perflevel0.physicalcpu'])}P + "
                         f"{cmd_out(['sysctl', '-n', 'hw.perflevel1.physicalcpu'])}E)")
        info["mem_gb"] = int(cmd_out(["sysctl", "-n", "hw.memsize"])) // 2**30
        info["os_version"] = cmd_out(["sw_vers", "-productVersion"])
    else:
        info["cpu"] = platform.processor()
        info["cores"] = os.cpu_count()
    return info


def fmt(x: float, d: int = 0) -> str:
    return f"{x:,.{d}f}"


def tables(res: dict) -> str:
    out = []
    configs = res["configs"]
    out.append("### Unary (rps / p50 / p90 / p99 ms, median of runs)\n")
    out.append("| server | client | in-flight | rps | p50 | p90 | p99 | rps min..max | errors |")
    out.append("|---|---|---:|---:|---:|---:|---:|---|---:|")
    for c in configs:
        med = c["median"]
        for k in sorted((k for k in med if k.startswith("unary/")), key=lambda k: int(k.split("/")[1])):
            m = med[k]
            out.append(f"| {c['server']} | {c['client_label']} | {k.split('/')[1]} | {fmt(m['rps'])} | "
                       f"{m['p50_ms']:.3f} | {m['p90_ms']:.3f} | {m['p99_ms']:.3f} | "
                       f"{fmt(m['rps_min'])}..{fmt(m['rps_max'])} | {m['errors']:g} |")
    out.append("\n### Streaming (median of runs)\n")
    out.append("| server | client | server-stream 100 (frames/s) | server-stream 10k (frames/s) | client-stream 10k (msgs/s) |")
    out.append("|---|---|---:|---:|---:|")
    for c in configs:
        med = c["median"]
        if not any(k.startswith(("server_stream", "client_stream")) for k in med):
            continue
        s100 = med.get("server_stream/100", {}).get("frames_per_s")
        s10k = med.get("server_stream/10000", {}).get("frames_per_s")
        c10k = med.get("client_stream/10000", {}).get("msgs_per_s")
        out.append(f"| {c['server']} | {c['client_label']} | {fmt(s100) if s100 else '-'} | "
                   f"{fmt(s10k) if s10k else '-'} | {fmt(c10k) if c10k else '-'} |")
    errs = [(c["server"], c["client_label"], e) for c in configs for e in c["errors"] if e]
    if errs:
        out.append("\n### Run errors / warnings\n")
        for s, cl, e in errs:
            out.append(f"- {s} / {cl}: {e}")
    return "\n".join(out)


def client_label(server: str, client: str) -> str:
    if client == "own":
        return {"rust": "rust (pooled)", "python": "python", "ts": "ts (bun)", "java": "java"}[server]
    return {"rust": "rust driver (pooled)", "rust-rpc": "rust driver (RpcConnection)"}[client]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--build", action="store_true")
    ap.add_argument("--servers", default=",".join(SERVERS))
    ap.add_argument("--clients", default=",".join(CLIENTS), help="own,rust,rust-rpc")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--duration", type=float, default=5.0, help="seconds per unary point")
    ap.add_argument("--warmup", type=float, default=1.0)
    ap.add_argument("--concurrency", default="1,10,50,100")
    ap.add_argument("--out", default=str(HERE / "results.json"))
    args = ap.parse_args()

    if args.build:
        build()
    servers = [s for s in args.servers.split(",") if s]
    clients = [c for c in args.clients.split(",") if c]
    plan = [(s, c) for s in servers for c in clients if not (s == "rust" and c == "rust")]
    opts = ["--duration", str(args.duration), "--warmup", str(args.warmup), "--concurrency", args.concurrency]

    info = machine_info()
    print(json.dumps(info, indent=2), flush=True)
    raw: dict[tuple[str, str], list[list[dict]]] = {p: [] for p in plan}
    errors: dict[tuple[str, str], list[str]] = {p: [] for p in plan}
    t_start = time.time()
    for run in range(args.runs):
        for s, c in plan:
            t0 = time.time()
            print(f"[run {run + 1}/{args.runs}] server={s:<6} client={client_label(s, c):<40}", end=" ", flush=True)
            results, err = run_one(s, c, opts)
            raw[(s, c)].append(results)
            errors[(s, c)].append(err)
            u = [r for r in results if r["kind"] == "unary"]
            summary = " ".join(f"c{r['conc']}={r['rps']:.0f}" for r in u)
            print(f"{time.time() - t0:5.1f}s {summary} {'ERR: ' + err[:200] if err else ''}", flush=True)

    res = {
        "machine": info,
        "params": {"runs": args.runs, "duration_s": args.duration, "warmup_s": args.warmup,
                   "concurrency": args.concurrency, "stream_frames": [100, 10000], "client_stream": [10000],
                   "unary_request": "EchoRequest{message='bench', count=1, data=64 bytes}",
                   "stream_request": "EchoRequest{message='s', count=N, data=64 bytes}"},
        "wall_s": round(time.time() - t_start, 1),
        "loadavg_end": os.getloadavg(),
        "configs": [
            {"server": s, "client": c, "client_label": client_label(s, c), "median": medians(raw[(s, c)]),
             "runs": raw[(s, c)], "errors": errors[(s, c)]}
            for s, c in plan
        ],
    }
    Path(args.out).write_text(json.dumps(res, indent=2))
    print("\n" + tables(res))
    print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
