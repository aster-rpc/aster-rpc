#!/usr/bin/env python3
"""T4 perf gate (dynamic-dispatch-design.md section 6): the TS InteropEcho
server on dynamic dispatch, interleaved with the pre-T4 TS stack, on Node and
Bun, driven by the TS own-language client and the Rust load driver.

    python3 benchmarks/dispatch/t4_gate.py --old-root /path/to/pre-T4/tree \
        --work /tmp/t4-gate --runs 3 --out /tmp/t4-gate.json

``--old-root`` is a copy of the repo (at least ``bindings/typescript`` with its
``node_modules`` and built addon, ``tests/interop/ts`` and
``benchmarks/dispatch/bench_client.ts``) at the pre-T4 commit. Node needs
compiled sources (decorators): ``bun build`` bundles ``interop_echo.ts`` and
``bench_client.ts`` per tree into ``--work`` (externals resolve through a
``node_modules`` symlink to that tree's install). Bun runs the ``.ts`` files.

Each run walks runtime x variant x client with the variant order flipped every
run, records the 1-minute load average before every config, and the gate
(medians over runs) compares new against ``perf-baseline-r3.md`` and against
the interleaved old stack. Loopback only, relays off.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import run as R  # noqa: E402  (shares Server plumbing and ENV)

REPO = HERE.parents[1]
BUN = os.environ.get("BUN_BIN", str(Path.home() / ".bun/bin/bun"))
NODE = os.environ.get("NODE_BIN", "node")
DRIVER = str(R.TARGET / "release/examples/dispatch_driver")

# perf-baseline-r3.md (TS server on Bun): own client and Rust pooled driver.
BASELINE = {
    "own": {
        "unary/1": (5206, 0.177, 0.831), "unary/10": (14944, 0.602, 2.094),
        "unary/50": (15323, 3.049, 5.870), "unary/100": (15475, 6.208, 10.329),
        "server_stream/100": 19312, "server_stream/10000": 21347, "client_stream/10000": 21611,
    },
    "rust": {
        "unary/1": (7168, 0.134, 0.226), "unary/10": (16569, 0.565, 1.850),
        "unary/50": (16593, 2.855, 4.912), "unary/100": (16703, 5.738, 8.216),
        "server_stream/100": 46044, "server_stream/10000": 49359, "client_stream/10000": 28298,
    },
}


def bundle(root: Path, work: Path, name: str) -> Path:
    out = work / f"node-{name}"
    out.mkdir(parents=True, exist_ok=True)
    link = out / "node_modules"
    if link.is_symlink() or link.exists():
        link.unlink()
    link.symlink_to(root / "bindings/typescript/node_modules")
    ext = ["--external", "@aster-rpc/transport", "--external", "@apache-fory/core"]
    for src, cwd in (("interop_echo.ts", root / "tests/interop/ts"), ("bench_client.ts", root / "benchmarks/dispatch")):
        subprocess.run([BUN, "build", src, "--target=node", "--format=esm", *ext,
                        "--outfile", str(out / src.replace(".ts", ".mjs"))], cwd=cwd, check=True,
                       capture_output=True)
    return out


class Srv(R.Server):
    def __init__(self, cmd: list[str], cwd: Path) -> None:  # noqa: D107
        import queue
        import threading
        self.proc = subprocess.Popen(cmd, cwd=cwd, env=R.ENV, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
        self.lines = queue.Queue()
        self.stderr = []
        threading.Thread(target=self._pump, args=(self.proc.stdout, self.lines.put), daemon=True).start()
        threading.Thread(target=self._pump, args=(self.proc.stderr, self.stderr.append), daemon=True).start()


def commands(root: Path, nb: Path, runtime: str) -> tuple[tuple[list[str], Path], tuple[list[str], Path]]:
    if runtime == "bun":
        return ([BUN, "run", "interop_echo.ts", "server"], root / "tests/interop/ts"), \
               ([BUN, "run", "bench_client.ts"], root / "benchmarks/dispatch")
    return ([NODE, str(nb / "interop_echo.mjs"), "server"], nb), ([NODE, str(nb / "bench_client.mjs")], nb)


def run_cfg(root: Path, nb: Path, runtime: str, client: str, opts: list[str]) -> tuple[list[dict], str]:
    (scmd, scwd), (ccmd, ccwd) = commands(root, nb, runtime)
    srv = Srv(scmd, scwd)
    try:
        ticket = srv.address()
        if ticket is None:
            return [], f"server did not start: {' | '.join(srv.stderr[-5:])}"
        if client == "own":
            cmd, cwd = [*ccmd, ticket, *opts], ccwd
        elif client == "rust":
            cmd, cwd = [DRIVER, ticket, *opts], REPO
        else:  # rust-rpc
            cmd, cwd = [DRIVER, ticket, "--mode", "rpc", "--skip-streams", *opts], REPO
        res = subprocess.run(cmd, cwd=cwd, env=R.ENV, capture_output=True, text=True, timeout=1800)
        results = [json.loads(ln[len("RESULT "):]) for ln in res.stdout.splitlines() if ln.startswith("RESULT ")]
        err = "" if res.returncode == 0 else f"client exit {res.returncode}: {res.stderr.strip()[-600:]}"
        return results, err
    finally:
        srv.stop()


def gate_rows(med: dict, base: dict) -> list[tuple[str, str, float, float, bool]]:
    rows = []
    for k, b in base.items():
        m = med.get(k)
        if not m:
            continue
        if k.startswith("unary"):
            rps, p50, p99 = b
            rows.append((k, "rps", m["rps"], (m["rps"] / rps - 1) * 100, m["rps"] >= rps * 0.95))
            rows.append((k, "p50", m["p50_ms"], (m["p50_ms"] / p50 - 1) * 100, m["p50_ms"] <= p50 * 1.05))
            rows.append((k, "p99", m["p99_ms"], (m["p99_ms"] / p99 - 1) * 100, m["p99_ms"] <= p99 * 1.10))
            if k == "unary/100":
                rows.append((k, "rps>=base", m["rps"], (m["rps"] / rps - 1) * 100, m["rps"] >= rps))
        else:
            f = "frames_per_s" if "server" in k else "msgs_per_s"
            rows.append((k, f, m[f], (m[f] / b - 1) * 100, m[f] >= b * 0.95))
    return rows


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--old-root", type=Path, required=True)
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--runtimes", default="bun,node")
    ap.add_argument("--clients", default="own,rust")
    ap.add_argument("--variants", default="new,old")
    ap.add_argument("--duration", default="5")
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()
    roots = {"new": REPO, "old": a.old_root.resolve()}
    variants = a.variants.split(",")
    bundles = {v: bundle(roots[v], a.work, v) for v in variants}
    opts = ["--duration", a.duration, "--warmup", "1", "--concurrency", "1,10,50,100",
            "--stream-frames", "100,10000", "--client-stream", "10000"]
    raw: list[dict] = []
    for run in range(a.runs):
        order = variants if run % 2 == 0 else list(reversed(variants))
        for runtime in a.runtimes.split(","):
            for variant in order:
                for client in a.clients.split(","):
                    load = os.getloadavg()[0]
                    t0 = time.time()
                    results, err = run_cfg(roots[variant], bundles[variant], runtime, client, opts)
                    print(f"[run {run + 1}] {runtime:4} {variant:3} client={client:8} load={load:5.1f} "
                          f"{len(results)} results {time.time() - t0:5.0f}s {err[:200]}", flush=True)
                    raw.append({"run": run, "runtime": runtime, "variant": variant, "client": client,
                                "load": load, "results": results, "error": err})
    by: dict[tuple, list[list[dict]]] = {}
    for r in raw:
        by.setdefault((r["runtime"], r["variant"], r["client"]), []).append(r["results"])
    med = {f"{k[0]}/{k[1]}/{k[2]}": R.medians(v) for k, v in by.items()}
    loads = [r["load"] for r in raw]
    report = {"raw": raw, "medians": med, "load_min": min(loads), "load_max": max(loads)}
    if a.out:
        a.out.write_text(json.dumps(report, indent=1))
    print(f"\nload (1-min avg) {min(loads):.1f}..{max(loads):.1f}")
    ok_all = True
    for key, m in med.items():
        runtime, variant, client = key.split("/")
        print(f"\n## {runtime} {variant} client={client}")
        for k in sorted(m):
            print(f"  {k:22} " + " ".join(f"{f}={v:.3f}" if isinstance(v, float) else f"{f}={v}"
                                         for f, v in m[k].items()))
        if variant == "new" and client in BASELINE:
            for k, metric, val, delta, ok in gate_rows(m, BASELINE[client]):
                ok_all &= ok
                print(f"  GATE {k:20} {metric:12} {val:12.3f} {delta:+7.1f}% {'PASS' if ok else 'FAIL'}")
    print(f"\nGATE vs perf-baseline-r3: {'PASS' if ok_all else 'FAIL'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
