#!/usr/bin/env bash
set -euo pipefail

# ── Ensure cache dirs exist and are writable ─────────────────────────
for d in /cache/cargo/target /cache/sccache /cache/uv; do
    sudo mkdir -p "$d"
    sudo chown -R runner:runner "$d"
done

# ── Configure the runner (idempotent — skips if already configured) ──
if [ ! -f .runner ]; then
    LABELS="self-hosted,${RUNNER_LABELS:-Linux,X64}"
    ./config.sh \
        --unattended \
        --url "${GITHUB_URL}" \
        --token "${RUNNER_TOKEN}" \
        --name "${RUNNER_NAME:-aster-runner}" \
        --labels "${LABELS}" \
        --work _work \
        --replace
fi

# ── Handle graceful shutdown ─────────────────────────────────────────
cleanup() {
    echo "Caught signal, removing runner..."
    ./config.sh remove --token "${RUNNER_TOKEN}" 2>/dev/null || true
}
trap cleanup SIGTERM SIGINT

# ── Run ──────────────────────────────────────────────────────────────
./run.sh &
wait $!
