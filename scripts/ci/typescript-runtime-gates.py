#!/usr/bin/env python3
"""Run each existing Node/Bun gate once and retain both outcomes on failure."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]


def invoke_bounded(argv, *, cwd, env, check, timeout):
    # npx starts child processes. Kill the whole group on timeout so a hung
    # test cannot keep touching shared storage while later gates run.
    with subprocess.Popen(argv, cwd=cwd, env=env, start_new_session=True) as child:
        try:
            return subprocess.CompletedProcess(argv, child.wait(timeout=timeout))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
            raise


def gates(root, temporary):
    package = root / "bindings/typescript/packages/aster"
    report = temporary / "o7-vitest.json"
    return [
        ("node", "vitest", package, ["npx", "vitest", "run", "--reporter=default", "--reporter=json", f"--outputFile={report}"]),
        ("node", "observability-counts", package, ["node", "../../scripts/check-vitest-count.mjs", str(report), "observability-logs.test.ts=7", "observability-export.test.ts=13", "o7-server-health.test.ts=3"]),
        ("node", "smoke", root, ["node", "tests/typescript/smoke/o7-observability-smoke.mjs"]),
        ("bun", "smoke", root, ["bun", "tests/typescript/smoke/o7-observability-smoke.mjs"]),
        ("node", "config-env", root, ["node", "tests/typescript/smoke/config-env-smoke.mjs"]),
        ("bun", "config-env", root, ["bun", "tests/typescript/smoke/config-env-smoke.mjs"]),
        ("bun", "package-load", package, ["bun", "--eval", "const m = await import('./dist/index.js'); for (const k of ['AsterServer', 'encodeStreamHeader']) { if (!(k in m)) throw new Error('missing export ' + k) } console.log('ok')"]),
    ]


def run(root, output, temporary, invoke=invoke_bounded):
    # A fresh file prevents a failed O7 invocation consuming stale counts.
    (temporary / "o7-vitest.json").unlink(missing_ok=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    report_copy = output.parent / "vitest.json"
    report_copy.unlink(missing_ok=True)
    outcomes = {"schema": 1, "sha": os.environ.get("FORGEJO_SHA"), "run_id": os.environ.get("FORGEJO_RUN_ID"), "attempt": os.environ.get("FORGEJO_RUN_ATTEMPT"), "runtimes": {"node": [], "bun": []}}
    env = {**os.environ, "ASTER_LOG_STDERR": "1", "ASTER_LOG": "warn,aster::lease::diagnostic=debug,aster::coordination::diagnostic=debug"}
    failed = False
    for runtime, name, cwd, argv in gates(root, temporary):
        print(f"::group::{runtime}: {name}", flush=True)
        started = time.monotonic()
        timeout = 600 if name == "vitest" else 120
        try:
            code = invoke(argv, cwd=cwd, env=env, check=False, timeout=timeout).returncode
        except subprocess.TimeoutExpired:
            print(f"::error::{runtime} {name}: exceeded {timeout}s deadline", flush=True)
            code = 124
        except OSError as error:
            print(f"::error::{runtime} {name}: {error}", flush=True)
            code = 127
        outcomes["runtimes"][runtime].append({"gate": name, "exit_code": code, "timeout_seconds": timeout, "seconds": round(time.monotonic() - started, 3)})
        failed |= code != 0
        output.write_text(json.dumps(outcomes, indent=2) + "\n")
        print("::endgroup::", flush=True)
    report = temporary / "o7-vitest.json"
    if report.is_file():
        shutil.copyfile(report, report_copy)
    output.write_text(json.dumps(outcomes, indent=2) + "\n")
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(run(ROOT, ROOT / "runtime-evidence/typescript.json", Path(os.environ["RUNNER_TEMP"])))
