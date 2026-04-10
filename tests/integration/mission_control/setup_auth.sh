#!/bin/bash
# setup_auth.sh — generate root key and enrollment credentials for the test suite.
#
# Usage: ./setup_auth.sh <work_dir>
#
# Creates:
#   <work_dir>/root.key       — private root key (JSON, mode 0600)
#   <work_dir>/root.pub       — public root key (JSON)
#   <work_dir>/edge.cred      — TOML identity with edge-node-7 peer (ops.status, ops.ingest)
#   <work_dir>/ops.cred       — TOML identity with ops-team peer (all roles)

set -euo pipefail

WORK_DIR="${1:-}"
if [[ -z "$WORK_DIR" ]]; then
  echo "Usage: $0 <work_dir>" >&2
  exit 1
fi

mkdir -p "$WORK_DIR"
cd "$(dirname "$0")/../../.."  # repo root

ROOT_KEY="$WORK_DIR/root.key"
ROOT_PUB="$WORK_DIR/root.pub"
EDGE_CRED="$WORK_DIR/edge.cred"
OPS_CRED="$WORK_DIR/ops.cred"

# 1. Generate root keypair (writes both .key and .pub)
rm -f "$ROOT_KEY" "$ROOT_PUB"
uv run aster trust keygen --out-key "$ROOT_KEY" >/dev/null

if [[ ! -f "$ROOT_KEY" ]]; then
  echo "ERROR: keygen did not produce $ROOT_KEY" >&2
  exit 1
fi
if [[ ! -f "$ROOT_PUB" ]]; then
  echo "ERROR: keygen did not produce $ROOT_PUB" >&2
  exit 1
fi

# 2. Enroll edge-node-7 (consumer with ops.status, ops.ingest only)
rm -f "$EDGE_CRED"
uv run aster enroll node \
  --role consumer \
  --name "edge-node-7" \
  --capabilities "ops.status,ops.ingest" \
  --root-key "$ROOT_KEY" \
  --out "$EDGE_CRED" >/dev/null

# 3. Enroll ops-team (consumer with all roles including admin)
rm -f "$OPS_CRED"
uv run aster enroll node \
  --role consumer \
  --name "ops-team" \
  --capabilities "ops.status,ops.logs,ops.admin,ops.ingest" \
  --root-key "$ROOT_KEY" \
  --out "$OPS_CRED" >/dev/null

# Verify files exist and have content
for f in "$ROOT_KEY" "$ROOT_PUB" "$EDGE_CRED" "$OPS_CRED"; do
  if [[ ! -s "$f" ]]; then
    echo "ERROR: $f is missing or empty" >&2
    exit 1
  fi
done

echo "Auth setup complete in $WORK_DIR"
echo "  root.key  : $ROOT_KEY"
echo "  root.pub  : $ROOT_PUB"
echo "  edge.cred : $EDGE_CRED (ops.status, ops.ingest)"
echo "  ops.cred  : $OPS_CRED  (ops.status, ops.logs, ops.admin, ops.ingest)"
