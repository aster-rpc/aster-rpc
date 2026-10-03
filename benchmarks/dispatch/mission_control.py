#!/usr/bin/env python3
"""End-to-end Mission Control benchmarks (dynamic-dispatch-design.md section 6).

Runs the existing Mission Control benchmark clients ``--runs`` times each and
reports the median of each reported number:

- ``python``: ``examples/python/mission_control/benchmark.py`` (run as a module
  so its typed-client section can import ``examples.*``) against the Python
  Mission Control server.
- ``ts-vs-python``: ``examples/typescript/missionControl/benchmark.ts`` against
  the Python server. Only its dynamic-proxy section runs: the typed section
  fails (see perf-baseline-r3.md), and a TS server cannot be used at all
  because ``client.proxy()`` against a TS server needs dynamic Fory (plan D5).
- ``java``: ``MissionControlBenchmark`` (JUnit, tag ``benchmark``), Java server
  and client in one JVM.
- ``--jmh``: also run ``ServerStreamThroughputBenchmark`` (JMH, once; JMH
  reports its own mean and error).

Assumes the builds from ``run.py --build``. Writes ``--out`` (JSON).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
JAVA_DIR = REPO / "bindings/java"
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
LIB = TARGET / "release" / ("libaster_transport_ffi" + (".dylib" if sys.platform == "darwin" else ".so"))
ENV = {
    **os.environ,
    "ASTER_RELAY_MODE": "disabled",
    "ASTER_BIND_ADDR": "127.0.0.1:0",
    "ASTER_LOG_LEVEL": "warn",
    "PYTHONUNBUFFERED": "1",
    "IROH_LIB_PATH": str(LIB),
}

# label -> regex with one or more numeric groups (first group is the headline).
PATTERNS = {
    "unary_getStatus_rps": r"Unary \(getStatus\)\s+([\d,]+) req/s\s+p50=\s*([\d.]+)ms\s+p90=\s*([\d.]+)ms\s+p99=\s*([\d.]+)ms",
    "unary_submitLog_rps": r"Unary \(submitLog\)\s+([\d,]+) req/s",
    "client_stream_100_mps": r"Client stream \(\s*100\)\s+([\d,]+) msg/s",
    "client_stream_1000_mps": r"Client stream \(\s*1,?000\)\s+([\d,]+) msg/s",
    "client_stream_10000_mps": r"Client stream \(\s*10,?000\)\s+([\d,]+) msg/s",
    "concurrent_10_rps": r"Concurrent \(\s*10\)\s+([\d,]+) req/s",
    "concurrent_50_rps": r"Concurrent \(\s*50\)\s+([\d,]+) req/s",
    "concurrent_100_rps": r"Concurrent \(100\)\s+([\d,]+) req/s",
}


def parse(text: str) -> dict[str, float]:
    """First occurrence of each metric (the dynamic-proxy / first section)."""
    out: dict[str, float] = {}
    for name, pat in PATTERNS.items():
        m = re.search(pat, text)
        if m:
            out[name] = float(m.group(1).replace(",", ""))
            if name == "unary_getStatus_rps":
                out["unary_getStatus_p50_ms"] = float(m.group(2))
                out["unary_getStatus_p90_ms"] = float(m.group(3))
                out["unary_getStatus_p99_ms"] = float(m.group(4))
    return out


def start_python_server() -> tuple[subprocess.Popen, str]:
    proc = subprocess.Popen(
        ["uv", "run", "--no-sync", "python", "-m", "examples.python.mission_control.server"],
        cwd=REPO, env=ENV, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
    )
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        line = proc.stdout.readline()
        m = re.search(r"aster1[A-Za-z0-9]+", line or "")
        if m:
            return proc, m.group(0)
        if proc.poll() is not None:
            break
    proc.kill()
    raise RuntimeError("python mission control server did not print an address")


def run_client(kind: str) -> tuple[str, int]:
    if kind == "java":
        cmd = ["mvn", "-P", "fast", "-o", "-pl", "aster-examples-mission-control", "-am", "test",
               "-Dtest=MissionControlBenchmark", "-Dgroups=benchmark", "-Dsurefire.failIfNoSpecifiedTests=false"]
        r = subprocess.run(cmd, cwd=JAVA_DIR, env=ENV, capture_output=True, text=True, timeout=900)
        return r.stdout + r.stderr, r.returncode
    srv, addr = start_python_server()
    try:
        if kind == "python":
            cmd, cwd = ["uv", "run", "--no-sync", "python", "-m", "examples.python.mission_control.benchmark", addr], REPO
        else:
            cmd, cwd = ["bun", "run", "../../examples/typescript/missionControl/benchmark.ts", addr], \
                REPO / "bindings/typescript"
        r = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True, text=True, timeout=900)
        return r.stdout + r.stderr, r.returncode
    finally:
        srv.terminate()
        try:
            srv.wait(timeout=10)
        except subprocess.TimeoutExpired:
            srv.kill()


def run_jmh() -> str:
    """Compile ServerStreamThroughputBenchmark with the JMH processor and run it once."""
    cp_file = HERE / ".java-test-classpath.txt"
    subprocess.run(["mvn", "-o", "-q", "-pl", "aster-runtime", "dependency:build-classpath",
                    "-Dmdep.includeScope=test", f"-Dmdep.outputFile={cp_file}"],
                   cwd=JAVA_DIR, env=ENV, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deps = cp_file.read_text().strip()
    m2 = Path.home() / ".m2/repository/org/openjdk/jmh/jmh-generator-annprocess/1.37/jmh-generator-annprocess-1.37.jar"
    rt = JAVA_DIR / "aster-runtime/target"
    out = Path(os.environ.get("TMPDIR", "/tmp")) / "aster-jmh-classes"
    gen = out.with_name("aster-jmh-generated")
    out.mkdir(exist_ok=True)
    gen.mkdir(exist_ok=True)
    cp = f"{rt / 'classes'}:{rt / 'test-classes'}:{deps}:{m2}"
    src = JAVA_DIR / "aster-runtime/src/test/java/site/aster/benchmarks/ServerStreamThroughputBenchmark.java"
    subprocess.run(["javac", "-proc:full", "-cp", cp, "-d", str(out), "-s", str(gen), str(src)],
                   check=True, env=ENV)
    r = subprocess.run(["java", "--enable-native-access=ALL-UNNAMED", "-cp", f"{out}:{cp}", "org.openjdk.jmh.Main",
                        "ServerStreamThroughputBenchmark"], env=ENV, capture_output=True, text=True, timeout=1800)
    return r.stdout + r.stderr


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--clients", default="python,ts-vs-python,java")
    ap.add_argument("--jmh", action="store_true")
    ap.add_argument("--out", default=str(HERE / "results-mission-control.json"))
    args = ap.parse_args()

    kinds = [k for k in args.clients.split(",") if k]
    raw: dict[str, list[dict]] = {k: [] for k in kinds}
    logs: dict[str, list[str]] = {k: [] for k in kinds}
    for run in range(args.runs):
        for k in kinds:
            text, rc = run_client(k)
            parsed = parse(text)
            raw[k].append(parsed)
            tail = "\n".join(text.strip().splitlines()[-6:]) if rc else ""
            logs[k].append(f"exit {rc}" + (f": {tail}" if tail else ""))
            print(f"[run {run + 1}/{args.runs}] {k:<14} exit={rc} {parsed}", flush=True)
    med = {}
    for k, runs in raw.items():
        keys = sorted({m for r in runs for m in r})
        med[k] = {m: statistics.median(r[m] for r in runs if m in r) for m in keys}
    res = {"runs": args.runs, "median": med, "raw": raw, "exit": logs}
    if args.jmh:
        res["jmh_server_stream"] = run_jmh()
        print(res["jmh_server_stream"][-2500:])
    Path(args.out).write_text(json.dumps(res, indent=2))
    print(json.dumps(med, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
