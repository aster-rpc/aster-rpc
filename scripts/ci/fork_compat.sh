#!/usr/bin/env bash
# fork_compat.sh -- run the iroh-stack forks' own suites against the stack
# iroh-fork-manifest.toml pins, when the manifest changes; fail on a fork whose
# own patch block or lock is stale against it. Modes and environment:
# scripts/ci/fork_compat.py. Local gate only (CI has no fork checkouts).
exec python3 "$(dirname "$0")/fork_compat.py" "$@"
