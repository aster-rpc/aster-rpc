#!/usr/bin/env python3
"""Small exploratory evidence recorder. Command success is not scenario qualification."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
CATALOGUE = ROOT / "docs/_internal/release_test_scenarios.md"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)


def catalogue():
    ids = re.findall(
        r"^\| ((?:RPC|LIFE|LIMIT|WIRE|DATA|COORD|HTTP|OBS|SEC|NET|SHIP)-\d+) \|",
        CATALOGUE.read_text(), re.M,
    )
    if not ids or len(ids) != len(set(ids)):
        raise ValueError("Catalogue IDs must be nonempty and unique")
    return ids


def source_identity():
    # Include tracked working changes, but never copy file contents into evidence.
    # Untracked source needs explicit integration before candidate qualification.
    return {
        "commit": git("rev-parse", "HEAD").decode().strip(),
        "tracked_diff_sha256": hashlib.sha256(git("diff", "HEAD", "--binary")).hexdigest(),
        "untracked_paths": git("ls-files", "--others", "--exclude-standard").decode().splitlines(),
    }


def write_json(path, data):
    temp = path.with_suffix(".tmp")
    temp.write_text(json.dumps(data, indent=2) + "\n")
    temp.replace(path)


def snapshot(paths):
    return [{"path": str(p.resolve()), "sha256": digest(p)} for p in paths]


def record(args):
    ids = catalogue()
    if not args.scenario or set(args.scenario) - set(ids):
        raise ValueError("Supply known catalogue scenario IDs")
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command or args.timeout <= 0:
        raise ValueError("A command and positive timeout are required")
    before_artifacts = snapshot(args.artifact)
    run_dir = args.output / (time.strftime("%Y%m%dT%H%M%S") + "-" + uuid.uuid4().hex[:8])
    run_dir.mkdir(parents=True, exist_ok=False)
    report = {
        "schema": 1, "kind": "EXPLORATORY", "scenario_ids": args.scenario,
        "command": command, "cwd": str(args.cwd.resolve()),
        "host": platform.node(), "platform": platform.platform(),
        "source_before": source_identity(), "artifacts_before": before_artifacts,
        "catalogue_sha256": digest(CATALOGUE), "started_unix": time.time(),
        "status": "RUNNING", "qualification": "NOT_EVALUATED",
        "note": "Artifact hashes do not prove loaded-library identity or freshness. No automatic retry.",
    }
    report_path = run_dir / "result.json"
    write_json(report_path, report)
    print(run_dir, flush=True)
    start = time.monotonic()
    process = None
    try:
        with (run_dir / "output.log").open("wb") as log:
            process = subprocess.Popen(command, cwd=args.cwd, stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                report["exit_code"] = process.wait(timeout=args.timeout)
                report["status"] = "COMMAND_OK" if process.returncode == 0 else "COMMAND_FAILED"
            except subprocess.TimeoutExpired:
                report["status"] = "TIMEOUT"
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                report["exit_code"] = process.returncode
    except (OSError, KeyboardInterrupt) as error:
        if process is not None and process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        report["status"] = "INTERRUPTED" if isinstance(error, KeyboardInterrupt) else "LAUNCH_ERROR"
        report["error"] = str(error)
    finally:
        report["elapsed_seconds"] = time.monotonic() - start
        report["finished_unix"] = time.time()
        try:
            report["source_after"] = source_identity()
            report["artifacts_after"] = snapshot(args.artifact)
            report["inputs_changed"] = (
                report["source_before"] != report["source_after"]
                or report["artifacts_before"] != report["artifacts_after"]
            )
        except OSError as error:
            report["verification_error"] = str(error)
            report["inputs_changed"] = True
        write_json(report_path, report)
    print(report["status"], flush=True)
    return 0 if report["status"] == "COMMAND_OK" and not report["inputs_changed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / ".aster/assurance")
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("seed", help="Generate unmapped catalogue ledger; does not invent applicability")
    run = sub.add_parser("run", help="Record one bounded exploratory command without retries")
    run.add_argument("--scenario", action="append", default=[])
    run.add_argument("--artifact", action="append", type=Path, default=[])
    run.add_argument("--cwd", type=Path, default=ROOT)
    run.add_argument("--timeout", type=float, default=300)
    run.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.action == "seed":
        args.output.mkdir(parents=True, exist_ok=True)
        # Never overwrite accumulated evidence with a fresh seed.
        path = args.output / "catalogue-ledger.json"
        with path.open("x") as out:
            json.dump({"source": source_identity(), "catalogue_sha256": digest(CATALOGUE),
                       "scope_frozen": False, "scenarios": [
                           {"id": i, "mapping": "UNMAPPED", "result": "NOT_RUN",
                            "variants": [], "owner": "UNASSIGNED"} for i in catalogue()
                       ]}, out, indent=2)
            out.write("\n")
        print(path)
        return 0
    return record(args)


if __name__ == "__main__":
    raise SystemExit(main())
