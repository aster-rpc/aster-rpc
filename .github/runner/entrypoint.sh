#!/usr/bin/env bash
set -euo pipefail

# Ephemeral self-hosted runner via just-in-time (JIT) config.
#
# On every container start we mint a *fresh* one-shot JIT config from a PAT,
# run exactly one job, and the runner auto-removes itself. The container then
# exits and `restart: always` brings it back with a clean _work — which is the
# whole point: no stale/corrupt checkout can survive between jobs.
#
# Required env:
#   GITHUB_URL    e.g. https://github.com/emrul/iroh-python
#   GH_PAT        fine-grained PAT with repo "Administration: read & write"
#                 (or a classic PAT with the `repo` scope)
# Optional env:
#   RUNNER_NAME_PREFIX  default "aster"; a random suffix is always appended
#   RUNNER_LABELS       default "Linux,X64" ("self-hosted" is added for you)
#   RUNNER_GROUP_ID     default 1 (the "Default" group)

: "${GITHUB_URL:?GITHUB_URL is required}"
: "${GH_PAT:?GH_PAT is required (PAT used to mint JIT configs)}"

RUNNER_LABELS="${RUNNER_LABELS:-Linux,X64}"
RUNNER_GROUP_ID="${RUNNER_GROUP_ID:-1}"
# JIT runners are one-shot entities, so the name must be unique each boot.
RUNNER_NAME="${RUNNER_NAME_PREFIX:-aster}-$(hostname)-${RANDOM}${RANDOM}"

# ── Ensure cache dirs exist and are writable (mounted host volumes) ──
for d in /cache/cargo/target /cache/sccache /cache/uv; do
    sudo mkdir -p "$d"
    sudo chown -R runner:runner "$d"
done

# ── Guarantee a clean workspace every boot ───────────────────────────
# `restart: always` reuses the same container fs, so _work would otherwise
# persist across restarts. Wiping it here is what eliminates the
# "not a git repository" checkout failures the native runner hit.
rm -rf _work && mkdir -p _work

# ── Mint a fresh JIT config from the PAT ─────────────────────────────
repo_path="${GITHUB_URL#https://github.com/}"
labels_json=$(jq -cn --arg l "self-hosted,${RUNNER_LABELS}" '$l | split(",")')
body=$(jq -cn \
    --arg name "${RUNNER_NAME}" \
    --argjson group "${RUNNER_GROUP_ID}" \
    --argjson labels "${labels_json}" \
    '{name: $name, runner_group_id: $group, labels: $labels, work_folder: "_work"}')

jit_config=$(curl -fsSL -X POST \
    -H "Authorization: Bearer ${GH_PAT}" \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "https://api.github.com/repos/${repo_path}/actions/runners/generate-jitconfig" \
    -d "${body}" | jq -r '.encoded_jit_config // empty')

if [ -z "${jit_config}" ]; then
    echo "ERROR: failed to mint a JIT config for ${repo_path} (check GH_PAT scope/url)" >&2
    sleep 5  # avoid a tight restart loop on persistent failure
    exit 1
fi

echo "Starting ephemeral runner ${RUNNER_NAME} for ${repo_path} [${RUNNER_LABELS}]"

# ── Run exactly one job, then exit (container restarts → fresh JIT) ──
# JIT runners are inherently ephemeral and de-register themselves, so no
# token-based cleanup is needed on shutdown.
exec ./run.sh --jitconfig "${jit_config}"
