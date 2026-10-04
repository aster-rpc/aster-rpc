#!/usr/bin/env python3
"""T4.0: TS server dispatch delivery, TSFN vs ring (dynamic-dispatch-design.md section 6).

Serves ``InteropEcho`` from ``ts_dispatch_server.mjs`` (native ``DispatchServer``
prototype + JS handlers) with each delivery variant on Node and Bun, drives it
with the Rust load driver (``driver.rs``, pooled unary at 1/10/50/100 in
flight, server-stream 100/10k, client-stream 10k), and samples the server's
idle CPU. The current TS server (``tests/interop/ts/interop_echo.ts``, Bun) is
measured in the same interleaved runs as the in-run baseline.

Variants: ``tsfn`` (T-a), ``tsfn-batched`` (T-b), ``ring-await`` (R1),
``ring-poll`` (R2).

Idle CPU: server CPU time (``ps -o time=``, 10 ms resolution) over ``--idle``
seconds, sampled twice per run -- right after start (no connection) and right
after the driver disconnects. Load CPU: CPU time over the driver's wall time.

Usage::

    python3 benchmarks/dispatch/t4_bench.py --runs 3 --out docs/_internal/bindings/t4-dispatch-benchmark/results.json
    python3 benchmarks/dispatch/t4_bench.py --runs 1 --duration 1 --idle 3 --runtimes node --variants tsfn-batched

Needs: release ``dispatch_driver`` (``cargo build --release -p aster --features
rpc,binding-support --example dispatch_driver``), the release NAPI addon
(``napi build --platform --release`` in ``bindings/typescript/native``) and the
built TS package (``bun run build`` in ``bindings/typescript/packages/aster``).
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

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as base  # noqa: E402  (shared helpers: Server pump, medians, machine_info)

HERE = base.HERE
REPO = base.REPO
VARIANTS = ("tsfn", "tsfn-batched", "ring-await", "ring-poll")
LABEL = {"tsfn": "T-a tsfn", "tsfn-batched": "T-b tsfn-batched", "ring-await": "R1 ring-await",
         "ring-poll": "R2 ring-poll", "legacy": "current TS server"}


def runtime_bin(runtime: str) -> str:
    """The runtime's real executable, so the CPU sampler sees the server
    itself and not a launcher shim (e.g. a package-manager guard wrapping
    ``bun`` in a shell script that spawns it). Override with ``$NODE_BIN`` /
    ``$BUN_BIN``."""
    env = os.environ.get(f"{runtime.upper()}_BIN")
    if env:
        return env
    if runtime == "bun":
        real = Path.home() / ".bun/bin/bun"
        if real.exists():
            return str(real)
    return runtime


def server_cmd(runtime: str, variant: str) -> tuple[list[str], Path]:
    if variant == "legacy":
        return [runtime_bin("bun"), "interop_echo.ts", "server"], base.INTEROP / "ts"
    return [runtime_bin(runtime), str(HERE / "ts_dispatch_server.mjs"), variant], REPO


class Server(base.Server):
    def __init__(self, runtime: str, variant: str) -> None:  # noqa: D107
        import queue
        import threading

        cmd, cwd = server_cmd(runtime, variant)
        self.proc = subprocess.Popen(cmd, cwd=cwd, env=base.ENV, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
        self.lines = queue.Queue()
        self.stderr = []
        threading.Thread(target=self._pump, args=(self.proc.stdout, self.lines.put), daemon=True).start()
        threading.Thread(target=self._pump, args=(self.proc.stderr, self.stderr.append), daemon=True).start()

    def cpu_s(self) -> float:
        """Total CPU time (user+sys) of the server process, seconds."""
        out = subprocess.run(["ps", "-o", "time=", "-p", str(self.proc.pid)], capture_output=True,
                             text=True).stdout.strip()
        secs = 0.0
        for part in out.split(":"):
            secs = secs * 60 + float(part)
        return secs


def idle_pct(srv: Server, secs: float) -> float:
    c0, t0 = srv.cpu_s(), time.monotonic()
    time.sleep(secs)
    return 100.0 * (srv.cpu_s() - c0) / (time.monotonic() - t0)


def run_one(runtime: str, variant: str, opts: list[str], idle: float,
            driver_timeout: float = 300.0) -> tuple[list[dict], dict, str]:
    srv = Server(runtime, variant)
    cpu: dict = {}
    try:
        ticket = srv.address()
        if ticket is None:
            return [], cpu, f"server did not start: {' | '.join(srv.stderr[-5:])}"
        time.sleep(2.0)  # let startup work (JIT, key gen, address discovery) settle
        cpu["idle_start_pct"] = idle_pct(srv, idle)
        driver = str(base.TARGET / "release/examples/dispatch_driver")
        c0, t0 = srv.cpu_s(), time.monotonic()
        try:
            res = subprocess.run([driver, ticket, *opts], cwd=REPO, env=base.ENV, capture_output=True,
                                 text=True, timeout=driver_timeout)
        except subprocess.TimeoutExpired as exc:
            # A stalled server (calls never answered) wedges the driver on
            # the pool; record it as a failed run instead of aborting.
            out = exc.stdout.decode() if isinstance(exc.stdout, bytes) else (exc.stdout or "")
            res = subprocess.CompletedProcess(exc.cmd, -9, out, f"STALL: driver timed out after {driver_timeout}s")
        cpu["load_pct"] = 100.0 * (srv.cpu_s() - c0) / (time.monotonic() - t0)
        time.sleep(1.0)  # connection teardown
        cpu["idle_after_pct"] = idle_pct(srv, idle)
        results = [json.loads(ln[len("RESULT "):]) for ln in res.stdout.splitlines() if ln.startswith("RESULT ")]
        err = "" if res.returncode == 0 else f"driver exit {res.returncode}: {res.stderr.strip()[-600:]}"
        warn = [ln for ln in res.stderr.splitlines() if ln.startswith("worker error")]
        if warn:
            err = (err + " " if err else "") + f"{len(warn)} worker errors, e.g. {warn[0]}"
        return results, cpu, err
    finally:
        srv.stop()


def unary(med: dict, conc: int) -> dict | None:
    return med.get(f"unary/{conc}")


def decide(configs: list[dict], concs: list[int]) -> dict:
    """Design section 6 rule, verbatim:

    use TSFN (the better of T-a and T-b) unless it is more than 5% behind the
    best ring variant on unary rps, or more than 10% behind on p99, at any
    concurrency on either runtime. Disqualify any variant with idle CPU above 1%.
    """
    by = {(c["runtime"], c["variant"]): c for c in configs}
    runtimes = sorted({c["runtime"] for c in configs if c["variant"] != "legacy"})
    idle = {k: max(c["cpu_median"].get("idle_start_pct", 0), c["cpu_median"].get("idle_after_pct", 0))
            for k, c in by.items()}
    dq = {v for (rt, v), x in idle.items() if v != "legacy" and x > 1.0}
    present = {v for (_, v) in by}
    rings = [v for v in ("ring-await", "ring-poll") if v in present and v not in dq]
    tsfns = [v for v in ("tsfn", "tsfn-batched") if v in present and v not in dq]
    checks: dict[str, list[dict]] = {}
    for t in tsfns:
        rows = []
        for rt in runtimes:
            for n in concs:
                if (rt, t) not in by:
                    continue
                tu = unary(by[(rt, t)]["median"], n)
                ring_rows = [(r, unary(by[(rt, r)]["median"], n)) for r in rings if (rt, r) in by]
                ring_rows = [(r, u) for r, u in ring_rows if u]
                if not tu or not ring_rows:
                    continue
                best_rps_v, best_rps = max(ring_rows, key=lambda x: x[1]["rps"])
                best_p99_v, best_p99 = min(ring_rows, key=lambda x: x[1]["p99_ms"])
                rps_gap = 100.0 * (tu["rps"] / best_rps["rps"] - 1.0)
                p99_gap = 100.0 * (tu["p99_ms"] / best_p99["p99_ms"] - 1.0)
                rows.append({"runtime": rt, "conc": n, "rps": tu["rps"], "best_ring_rps": best_rps["rps"],
                             "best_ring_rps_variant": best_rps_v, "rps_gap_pct": rps_gap,
                             "p99_ms": tu["p99_ms"], "best_ring_p99_ms": best_p99["p99_ms"],
                             "best_ring_p99_variant": best_p99_v, "p99_gap_pct": p99_gap,
                             "fail": rps_gap < -5.0 or p99_gap > 10.0})
        checks[t] = rows
    # "The better of T-a and T-b": fewest rule failures, then higher mean rps.
    def score(t: str) -> tuple:
        rows = checks.get(t, [])
        return (sum(r["fail"] for r in rows), -statistics.mean(r["rps"] for r in rows) if rows else 0)
    best_tsfn = min(tsfns, key=score) if tsfns else None
    tsfn_ok = best_tsfn is not None and not any(r["fail"] for r in checks[best_tsfn])
    if tsfn_ok:
        choice = best_tsfn
    else:
        # Fall back to the best ring variant by mean unary rps across runtimes and concurrencies.
        def ring_mean(r: str) -> float:
            v = [unary(by[(rt, r)]["median"], n)["rps"] for rt in runtimes for n in concs
                 if (rt, r) in by and unary(by[(rt, r)]["median"], n)]
            return statistics.mean(v) if v else 0.0
        choice = max(rings, key=ring_mean) if rings else None
    return {"idle_max_pct": {f"{rt}/{v}": x for (rt, v), x in idle.items()}, "disqualified": sorted(dq),
            "checks": checks, "best_tsfn": best_tsfn, "tsfn_passes": tsfn_ok, "choice": choice}


def paired(configs: list[dict], concs: list[int], runs: int) -> dict:
    """Per-run gaps: each TSFN variant against the best ring variant *of the
    same run* (configs of one run are back to back, so they share the
    machine's load far more than medians across runs do). Returns, per
    (variant, runtime, in-flight), the list of per-run rps gaps in %."""
    by = {(c["runtime"], c["variant"]): c for c in configs}
    out: dict[str, list[float]] = {}
    for (rt, t) in by:
        if t not in ("tsfn", "tsfn-batched"):
            continue
        for n in concs:
            gaps = []
            for run in range(runs):
                def rps(v: str) -> float | None:
                    c = by.get((rt, v))
                    if not c or run >= len(c["runs"]):
                        return None
                    u = [r for r in c["runs"][run] if r["kind"] == "unary" and r["conc"] == n]
                    return u[0]["rps"] if u else None
                mine = rps(t)
                ring = [x for x in (rps("ring-await"), rps("ring-poll")) if x]
                if mine and ring:
                    gaps.append(round(100.0 * (mine / max(ring) - 1.0), 1))
            out[f"{t}/{rt}/{n}"] = gaps
    return out


def tables(res: dict) -> str:
    out = []
    configs = res["configs"]
    concs = [int(x) for x in res["params"]["concurrency"].split(",")]
    out.append("### Unary, Rust driver (pooled): rps / p50 / p99 ms (median of runs)\n")
    out.append("| runtime | variant | in-flight | rps | p50 | p99 | rps min..max | errors |")
    out.append("|---|---|---:|---:|---:|---:|---|---:|")
    for c in configs:
        for n in concs:
            m = unary(c["median"], n)
            if not m:
                continue
            out.append(f"| {c['runtime']} | {LABEL[c['variant']]} | {n} | {m['rps']:,.0f} | {m['p50_ms']:.3f} | "
                       f"{m['p99_ms']:.3f} | {m['rps_min']:,.0f}..{m['rps_max']:,.0f} | {m['errors']:g} |")
    out.append("\n### Streaming + CPU (median of runs)\n")
    out.append("| runtime | variant | server-stream 100 (fr/s) | server-stream 10k (fr/s) | client-stream 10k (msg/s) "
               "| idle CPU % (start / after) | CPU % under load |")
    out.append("|---|---|---:|---:|---:|---|---:|")
    for c in configs:
        med, cm = c["median"], c["cpu_median"]
        f = lambda k, f2: med.get(k, {}).get(f2)  # noqa: E731
        s100, s10k, c10k = f("server_stream/100", "frames_per_s"), f("server_stream/10000", "frames_per_s"), \
            f("client_stream/10000", "msgs_per_s")
        out.append(f"| {c['runtime']} | {LABEL[c['variant']]} | {s100 or 0:,.0f} | {s10k or 0:,.0f} | {c10k or 0:,.0f} "
                   f"| {cm.get('idle_start_pct', 0):.2f} / {cm.get('idle_after_pct', 0):.2f} "
                   f"| {cm.get('load_pct', 0):.0f} |")
    d = res["decision"]
    out.append("\n### Decision rule checks (TSFN vs best ring, per runtime x in-flight)\n")
    out.append("| tsfn variant | runtime | in-flight | rps | best ring rps | gap | p99 | best ring p99 | gap | fails |")
    out.append("|---|---|---:|---:|---:|---:|---:|---:|---:|---|")
    for t, rows in d["checks"].items():
        for r in rows:
            out.append(f"| {LABEL[t]} | {r['runtime']} | {r['conc']} | {r['rps']:,.0f} | {r['best_ring_rps']:,.0f} "
                       f"({r['best_ring_rps_variant']}) | {r['rps_gap_pct']:+.1f}% | {r['p99_ms']:.3f} | "
                       f"{r['best_ring_p99_ms']:.3f} ({r['best_ring_p99_variant']}) | {r['p99_gap_pct']:+.1f}% | "
                       f"{'FAIL' if r['fail'] else ''} |")
    if res.get("paired"):
        out.append("\n### Paired per-run rps gap, TSFN vs best ring of the same run (%)\n")
        out.append("| tsfn variant / runtime / in-flight | per-run gaps | median |")
        out.append("|---|---|---:|")
        for k, g in res["paired"].items():
            if g:
                out.append(f"| {k} | {', '.join(f'{x:+.1f}' for x in g)} | {statistics.median(g):+.1f} |")
    out.append(f"\ndisqualified (idle > 1%): {d['disqualified'] or 'none'}; best TSFN: {d['best_tsfn']}; "
               f"TSFN passes: {d['tsfn_passes']}; **choice: {d['choice']}**")
    errs = [(c["runtime"], c["variant"], e) for c in configs for e in c["errors"] if e]
    if errs:
        out.append("\n### Run errors / warnings\n")
        for rt, v, e in errs:
            out.append(f"- {rt} / {v}: {e[:300]}")
    return "\n".join(out)


def analyze(paths: list[str], max_load: float, concs: list[int]) -> dict:
    """Re-apply the decision rule to saved results, keeping only the unary
    measurements taken while the 1-minute load average was <= ``max_load``
    (pooled across files). For a machine that other work keeps busy: the
    configs of one run do not share load, so medians over all runs mix
    load with variant."""
    pooled: dict[tuple[str, str], list[list[dict]]] = {}
    for path in paths:
        d = json.loads(Path(path).read_text())
        loads = d["params"].get("loadavg_1m_per_config", [])
        plan = [(c["runtime"], c["variant"]) for c in d["configs"]]
        for c in d["configs"]:
            for run, res in enumerate(c["runs"]):
                keep = []
                for r in res:
                    if r["kind"] != "unary":
                        continue
                    ld = r.get("loadavg_1m")
                    if ld is None:  # older files: loads are in plan order per run
                        i = run * len(plan) + plan.index((c["runtime"], c["variant"]))
                        ld = loads[i] if i < len(loads) else float("inf")
                    if ld <= max_load:
                        keep.append(r)
                if keep:
                    pooled.setdefault((c["runtime"], c["variant"]), []).append(keep)
    configs = [{"runtime": rt, "variant": v, "median": base.medians(runs), "cpu_median": {},
                "samples": {n: sum(1 for run in runs for r in run if r["conc"] == n) for n in concs}}
               for (rt, v), runs in pooled.items()]
    return {"max_load": max_load, "files": paths, "configs": configs, "decision": decide(configs, concs)}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--runtimes", default="node,bun")
    ap.add_argument("--variants", default=",".join(VARIANTS))
    ap.add_argument("--no-legacy", action="store_true", help="skip the current TS server (Bun) baseline")
    ap.add_argument("--duration", type=float, default=5.0)
    ap.add_argument("--warmup", type=float, default=1.0)
    ap.add_argument("--idle", type=float, default=10.0, help="seconds per idle CPU sample")
    ap.add_argument("--concurrency", default="1,10,50,100")
    ap.add_argument("--out", default=str(HERE / "results-t4.json"))
    ap.add_argument("--rotate", action="store_true",
                    help="rotate the config order by one each run, so no variant always runs first (under whatever "
                         "load the machine had when the run started)")
    ap.add_argument("--unary-only", action="store_true", help="skip the streaming measurements")
    ap.add_argument("--analyze", nargs="+", metavar="RESULTS_JSON",
                    help="no measuring: re-apply the decision rule to saved results (with --max-load)")
    ap.add_argument("--max-load", type=float, default=float("inf"),
                    help="--analyze: keep only measurements taken at or below this 1-min load average")
    args = ap.parse_args()

    if args.analyze:
        concs = [int(x) for x in args.concurrency.split(",")]
        a = analyze(args.analyze, args.max_load, concs)
        for c in sorted(a["configs"], key=lambda c: (c["runtime"], c["variant"])):
            for n in concs:
                m = unary(c["median"], n)
                if m:
                    print(f"{c['runtime']:<4} {c['variant']:<13} c{n:<3} rps={m['rps']:>8,.0f} "
                          f"p99={m['p99_ms']:.3f} samples={c['samples'][n]}")
        d = a["decision"]
        for t, rows in d["checks"].items():
            for r in rows:
                print(f"{t:<13} {r['runtime']:<4} c{r['conc']:<3} rps {r['rps_gap_pct']:+6.1f}% vs "
                      f"{r['best_ring_rps_variant']:<10} p99 {r['p99_gap_pct']:+6.1f}% vs "
                      f"{r['best_ring_p99_variant']:<10} {'FAIL' if r['fail'] else 'ok'}")
        print(f"best TSFN: {d['best_tsfn']}; TSFN passes: {d['tsfn_passes']}; choice: {d['choice']}")
        if args.out != str(HERE / "results-t4.json"):
            Path(args.out).write_text(json.dumps(a, indent=2))
        return 0

    plan = [(rt, v) for rt in args.runtimes.split(",") if rt for v in args.variants.split(",") if v]
    if not args.no_legacy:
        plan.append(("bun", "legacy"))
    opts = ["--duration", str(args.duration), "--warmup", str(args.warmup), "--concurrency", args.concurrency]
    if args.unary_only:
        opts.append("--skip-streams")
    info = base.machine_info()
    info["git_dirty"] = bool(subprocess.run(["git", "-C", str(REPO), "status", "--porcelain"],
                                            capture_output=True, text=True).stdout.strip())
    print(json.dumps(info, indent=2), flush=True)
    raw = {p: [] for p in plan}
    cpus = {p: [] for p in plan}
    errors = {p: [] for p in plan}
    loads = []
    t_start = time.time()
    for run in range(args.runs):
        order = plan[run % len(plan):] + plan[:run % len(plan)] if args.rotate else plan
        for rt, v in order:
            t0 = time.time()
            print(f"[run {run + 1}/{args.runs}] {rt:<4} {v:<13}", end=" ", flush=True)
            load0 = os.getloadavg()[0]
            results, cpu, err = run_one(rt, v, opts, args.idle)
            loads.append(load0)
            for r in results:
                r["run"], r["loadavg_1m"] = run, load0
            raw[(rt, v)].append(results)
            cpus[(rt, v)].append(cpu)
            errors[(rt, v)].append(err)
            summary = " ".join(f"c{r['conc']}={r['rps']:.0f}" for r in results if r["kind"] == "unary")
            print(f"{time.time() - t0:5.1f}s load={load0:.1f} {summary} idle={cpu.get('idle_start_pct', -1):.2f}/"
                  f"{cpu.get('idle_after_pct', -1):.2f}% {'ERR: ' + err[:160] if err else ''}", flush=True)

    concs = [int(x) for x in args.concurrency.split(",")]
    configs = []
    for rt, v in plan:
        cm = {}
        for k in ("idle_start_pct", "idle_after_pct", "load_pct"):
            vals = [c[k] for c in cpus[(rt, v)] if k in c]
            if vals:
                cm[k] = statistics.median(vals)
        configs.append({"runtime": rt, "variant": v, "label": LABEL[v], "median": base.medians(raw[(rt, v)]),
                        "cpu_median": cm, "runs": raw[(rt, v)], "cpu_runs": cpus[(rt, v)],
                        "errors": errors[(rt, v)]})
    res = {
        "machine": info,
        "params": {"runs": args.runs, "duration_s": args.duration, "warmup_s": args.warmup, "idle_s": args.idle,
                   "concurrency": args.concurrency, "client": "rust driver (pooled)",
                   "loadavg_1m_per_config": loads},
        "wall_s": round(time.time() - t_start, 1),
        "loadavg_end": os.getloadavg(),
        "configs": configs,
    }
    res["decision"] = decide(configs, concs)
    res["paired"] = paired(configs, concs, args.runs)
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(res, indent=2))
    print("\n" + tables(res))
    print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
