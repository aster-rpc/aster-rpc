#!/usr/bin/env python3
"""fork_compat.py -- the iroh-stack forks against the stack the manifest pins.

When `iroh-fork-manifest.toml` changes (against the merge base with
origin/main), every fork whose pin moved, and every fork whose own
`[patch.crates-io]` names one that moved, runs its own test suite at its
pinned commit (resilience-test-plan.md §4; TM-2, TM-7). Before that, every
fork's patch block and Cargo.lock at its pinned commit must resolve the other
forks to the manifest's commits: a fork that validated itself against an older
stack, or took a forked crate from crates.io, is stale (TM-7).

  scripts/ci/fork_compat.sh               gate: nothing unless the manifest changed
  scripts/ci/fork_compat.sh --check       staleness of every fork, no tests
  scripts/ci/fork_compat.sh --all         staleness, then every fork's suite
  scripts/ci/fork_compat.sh --fork NAME   staleness, then that fork's suite

Fork checkouts are found under $ASTER_FORKS_DIR (default: the directory that
holds the main aster-rpc-internal checkout, ~/dev/aster). Suites run in a
detached `git worktree` at the pinned commit with a disposable
CARGO_TARGET_DIR, through the Kache launcher when it exists. Local only: CI
has no fork checkouts.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
MANIFEST = "iroh-fork-manifest.toml"
KACHE = "/Users/emrul/dev/ai/rust-dev/scripts/with-kache"

# The iroh-stack forks this gate covers, in dependency order, and each one's
# own suite (iroh/docs/iroh-patches.md records these commands; iroh's lib
# tests run single-threaded, its threaded run is known to crash locally).
SUITES = {
    "noq": [["cargo", "test", "--locked", "-p", "noq-proto", "-p", "noq", "-p", "noq-udp"]],
    "iroh": [["cargo", "test", "--locked", "-p", "iroh", "--all-features", "--lib", "--", "--test-threads=1"]],
    "iroh-util": [["cargo", "test", "--locked", "--all-features"]],
    "iroh-blobs": [["cargo", "test", "--locked", "--all-features"]],
    "iroh-gossip": [["cargo", "test", "--locked", "--all-features"]],
    "iroh-docs": [["cargo", "test", "--locked", "--all-features"]],
}

PATCH_LINE = re.compile(r'^\s*([A-Za-z0-9_-]+)\s*=\s*\{([^}]*)\}', re.M)


def git(*args: str, cwd: str = ROOT, check: bool = True) -> str:
    out = subprocess.run(["git", "-C", cwd, *args], capture_output=True, text=True)
    if check and out.returncode != 0:
        raise RuntimeError(f"git -C {cwd} {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout if out.returncode == 0 else ""


def load_toml(text: str) -> dict:
    if tomllib is None:
        raise SystemExit("fork_compat: needs Python 3.11+ (tomllib)")
    return tomllib.loads(text)


def forks_dir() -> str:
    if os.environ.get("ASTER_FORKS_DIR"):
        return os.environ["ASTER_FORKS_DIR"]
    common = git("rev-parse", "--path-format=absolute", "--git-common-dir").strip()
    return os.path.dirname(os.path.dirname(common))  # <main checkout>/.git -> its parent


def crate_owner(manifest: dict) -> dict[str, str]:
    owner = {}
    for name, fork in manifest.get("forks", {}).items():
        for crate in fork.get("crates", []) + fork.get("registry_crates", []):
            owner.setdefault(crate, name)
    return owner


def patch_block(cargo_toml: str) -> dict[str, dict]:
    """[patch.crates-io] entries as {crate: {"git": url, "rev": rev, ...}}."""
    m = re.search(r"^\[patch\.crates-io\]\s*$(.*?)(?=^\[|\Z)", cargo_toml, re.M | re.S)
    if not m:
        return {}
    block = load_toml("[p]\n" + m.group(1))["p"]
    return {k: v for k, v in block.items() if isinstance(v, dict)}


def lock_packages(cargo_lock: str) -> list[dict]:
    return load_toml(cargo_lock).get("package", [])


def stale_findings(name: str, fork: dict, manifest: dict, cargo_toml: str, cargo_lock: str,
                   equivalent=None) -> list[str]:
    """Why `name` at its pinned commit does not build against the manifest's stack.

    `equivalent(other, rev, want)` says whether fork `other` at `rev` is the
    same source as the manifest's `want` (see `lock_only_repin`). A sibling
    pinned at an older commit that differs only by a lock-only re-pin builds
    the same code, so it is not stale (the ledger's convention: such re-pins
    keep the crate version and the publisher skips them as identical).
    """
    same = equivalent or (lambda other, rev, want: rev == want)
    owner = crate_owner(manifest)
    forks = manifest["forks"]
    own = set(fork.get("crates", []) + fork.get("registry_crates", []))
    found = []
    for crate, spec in sorted(patch_block(cargo_toml).items()):
        other = owner.get(crate)
        if other is None or other == name:
            continue
        want = forks[other]["commit"]
        rev = spec.get("rev") or spec.get("tag") or spec.get("branch") or spec.get("path")
        if not (spec.get("rev") and same(other, spec["rev"], want)):
            found.append(f"patch {crate} -> {rev}, manifest {other} is {want}")
    for pkg in lock_packages(cargo_lock):
        crate, source = pkg["name"], pkg.get("source", "")
        other = owner.get(crate)
        if other is None or other == name or crate in own or not source:
            continue
        want = forks[other]["commit"]
        if source.startswith("git+"):
            sha = source.rsplit("#", 1)[-1]
            if not same(other, sha, want):
                found.append(f"Cargo.lock {crate} {pkg['version']} at {sha[:12]}, manifest {other} is {want[:12]}")
        else:
            found.append(f"Cargo.lock {crate} {pkg['version']} from {source.split('+')[0]}, "
                         f"not the {other} fork at {want[:12]}")
    return found


PATCH_SECTION = re.compile(r"^\[patch\.crates-io\]\s*$.*?(?=^\[|\Z)", re.M | re.S)


def lock_only_repin(path: str, a: str, b: str) -> bool:
    """Whether commits `a` and `b` of the fork at `path` hold the same source:
    every file identical except Cargo.lock and Cargo.toml's [patch.crates-io]
    section (a lock-only re-pin, e.g. iroh-gossip 0.101.1 -> 0.101.1-p1)."""
    if a == b:
        return True
    if not (has_commit(path, a) and has_commit(path, b)):
        return False
    diff = subprocess.run(["git", "-C", path, "diff", "--quiet", a, b, "--", ".",
                           ":(exclude)Cargo.lock", ":(exclude)Cargo.toml"],
                          capture_output=True).returncode
    if diff != 0:
        return False
    def toml(c: str) -> list[str]:
        # The rest of Cargo.toml, ignoring blank lines (adding a patch block
        # where there was none also adds a separating blank line).
        rest = PATCH_SECTION.sub("", git("show", f"{c}:Cargo.toml", cwd=path))
        return [line.rstrip() for line in rest.splitlines() if line.strip()]
    return toml(a) == toml(b)


def depends_on(cargo_toml: str, manifest: dict) -> set[str]:
    owner = crate_owner(manifest)
    return {owner[c] for c in patch_block(cargo_toml) if c in owner}


def changed_forks(base: dict, head: dict) -> set[str]:
    keys = ("commit", "tag", "crate_version")
    names = set(base.get("forks", {})) | set(head.get("forks", {}))
    return {n for n in names
            if tuple(base.get("forks", {}).get(n, {}).get(k) for k in keys)
            != tuple(head.get("forks", {}).get(n, {}).get(k) for k in keys)}


def has_commit(path: str, commit: str) -> bool:
    return subprocess.run(["git", "-C", path, "cat-file", "-e", f"{commit}^{{commit}}"],
                          capture_output=True).returncode == 0


def ensure_commit(path: str, fork: dict) -> None:
    commit = fork["commit"]
    if not has_commit(path, commit):
        print(f"  fetching {fork['tag']} into {path}")
        git("fetch", "--quiet", "origin", "tag", fork["tag"], cwd=path)
        if not has_commit(path, commit):
            raise RuntimeError(f"{path}: manifest commit {commit} not found after fetching {fork['tag']}")
    tag_commit = git("rev-parse", "--verify", "--quiet", f"{fork['tag']}^{{commit}}", cwd=path, check=False).strip()
    if tag_commit and tag_commit != commit:
        raise RuntimeError(f"{path}: tag {fork['tag']} is {tag_commit}, manifest commit is {commit}")


def run_suite(name: str, path: str, commit: str, keep: bool) -> bool:
    tmp = tempfile.mkdtemp(prefix=f"fork-compat-{name}-")
    tree = os.path.join(tmp, "src")
    shared = os.environ.get("FORK_COMPAT_TARGET_DIR")
    target = os.path.join(shared, name) if shared else os.path.join(tmp, "target")
    git("worktree", "add", "--quiet", "--detach", tree, commit, cwd=path)
    env = dict(os.environ, CARGO_TARGET_DIR=target)
    launcher = [KACHE] if os.access(KACHE, os.X_OK) else []
    ok = True
    try:
        for cmd in SUITES[name]:
            print(f"  $ {' '.join(cmd)}   (in {name} @ {commit[:12]})", flush=True)
            if subprocess.run(launcher + cmd, cwd=tree, env=env).returncode != 0:
                ok = False
                break
    finally:
        git("worktree", "remove", "--force", tree, cwd=path, check=False)
        if not keep:
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"  kept {tmp}")
    return ok


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="iroh-stack fork compatibility gate")
    ap.add_argument("--base", default="origin/main", help="compare the manifest with its merge base with this ref")
    ap.add_argument("--check", action="store_true", help="staleness of every fork only, no tests")
    ap.add_argument("--all", action="store_true", help="run every fork's suite")
    ap.add_argument("--fork", action="append", default=[], help="run this fork's suite (repeatable)")
    ap.add_argument("--keep", action="store_true", help="keep the worktrees' build targets")
    args = ap.parse_args(argv)

    with open(os.path.join(ROOT, MANIFEST), encoding="utf-8") as f:
        head = load_toml(f.read())
    merge_base = git("merge-base", "HEAD", args.base, check=False).strip()
    if not merge_base and not (args.check or args.all or args.fork):
        print(f"fork_compat: no merge base with {args.base}; fetch it, or pass --base/--all/--fork")
        return 2
    base_text = git("show", f"{merge_base}:{MANIFEST}", check=False) if merge_base else ""
    base = load_toml(base_text) if base_text else {"forks": {}}
    moved = changed_forks(base, head) & set(SUITES)

    if not (args.check or args.all or args.fork or moved):
        print(f"fork_compat: {MANIFEST} unchanged against {args.base}; nothing to do")
        return 0

    root = forks_dir()
    forks = {n: f for n, f in head["forks"].items() if n in SUITES}
    print(f"fork_compat: forks under {root}; pins moved: {', '.join(sorted(moved)) or 'none'}")

    missing, stale, deps = [], {}, {}
    for name, fork in forks.items():
        path = os.path.join(root, name)
        if not os.path.isdir(os.path.join(path, ".git")) and not os.path.isfile(os.path.join(path, ".git")):
            missing.append(name)
            continue
        ensure_commit(path, fork)
        toml_text = git("show", f"{fork['commit']}:Cargo.toml", cwd=path)
        lock_text = git("show", f"{fork['commit']}:Cargo.lock", cwd=path, check=False)
        deps[name] = depends_on(toml_text, head)
        findings = stale_findings(
            name, fork, head, toml_text, lock_text,
            equivalent=lambda other, rev, want: lock_only_repin(os.path.join(root, other), rev, want),
        )
        if findings:
            stale[name] = findings
        print(f"  {name} {fork['tag']} @ {fork['commit'][:12]}: "
              f"{'STALE' if findings else 'patch block and lock match the manifest'}")
        for line in findings:
            print(f"    - {line}")

    if missing:
        print(f"fork_compat: no checkout for {', '.join(missing)} under {root} (set ASTER_FORKS_DIR)")
        return 2

    if args.check:
        to_run = []
    elif args.all:
        to_run = list(forks)
    elif args.fork:
        unknown = set(args.fork) - set(forks)
        if unknown:
            print(f"fork_compat: unknown fork(s) {', '.join(sorted(unknown))}; known: {', '.join(forks)}")
            return 2
        to_run = [n for n in forks if n in args.fork]
    else:
        # A moved fork, and every fork whose own patch block names one.
        to_run = [n for n in forks if n in moved or deps[n] & moved]

    failed = []
    for name in to_run:
        if name in stale:
            print(f"fork_compat: not testing {name}: its pinned commit is stale against the manifest")
            continue
        print(f"fork_compat: {name} suite", flush=True)
        if not run_suite(name, os.path.join(root, name), forks[name]["commit"], args.keep):
            failed.append(name)

    if moved:
        ledger = os.path.join(root, "iroh", "docs", "iroh-patches.md")
        text = open(ledger, encoding="utf-8").read() if os.path.exists(ledger) else ""
        unrecorded = [n for n in sorted(moved) if forks[n]["tag"] not in text]
        if unrecorded:
            print(f"fork_compat: the patch ledger ({ledger}) does not mention "
                  + ", ".join(f"{n} {forks[n]['tag']}" for n in unrecorded))
            failed += [f"{n} (ledger)" for n in unrecorded]

    if stale:
        print("fork_compat: STALE: " + ", ".join(sorted(stale))
              + ". Re-pin that fork's [patch.crates-io] and lock to the manifest's commits, run its suite,"
              " tag it, record it in the ledger and update the manifest (iroh/docs/iroh-patches.md).")
    if failed:
        print("fork_compat: FAILED: " + ", ".join(failed))
    if stale or failed:
        return 1
    ran = ", ".join(to_run) if to_run else "no suites"
    print(f"fork_compat: passed ({ran}); record the run in the patch ledger")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
