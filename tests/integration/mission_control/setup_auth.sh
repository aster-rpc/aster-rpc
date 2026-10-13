#!/bin/bash
# setup_auth.sh — produce the Enrollment files for the auth-mode matrix with
# the Rust `aster` CLI, exactly as the Mission Control README's Chapter 5.
#
# Usage: ./setup_auth.sh <work_dir>
#
# Creates (the CLI state lives in <work_dir>/aster-home, file keystore):
#   <work_dir>/auth.env     — ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_AUDIENCE,
#                             ASTER_ENROLLMENT_FLEET_NAMESPACE (source it)
#   <work_dir>/fleet.secret — the fleet namespace secret (root publication)
#   <work_dir>/server.key, server.cred — the server's node key + credential
#   <work_dir>/edge.key,   edge.cred   — edge-node-7 (ops.status, ops.ingest)
#   <work_dir>/ops.key,    ops.cred    — ops-team (all roles)
#
# A server runs with `auth.env` plus ASTER_SECRET_KEY_FILE=server.key,
# ASTER_ENROLLMENT_CREDENTIAL=server.cred and ASTER_ENROLLMENT_STATE_PATH; a
# client with `auth.env` plus its own key and credential.

set -euo pipefail

WORK_DIR="${1:-}"
if [[ -z "$WORK_DIR" ]]; then
  echo "Usage: $0 <work_dir>" >&2
  exit 1
fi

mkdir -p "$WORK_DIR"
WORK_DIR="$(cd "$WORK_DIR" && pwd)"
cd "$(dirname "$0")/../../.."  # repo root

if [[ -x target/debug/aster ]]; then
  ASTER_BIN=(target/debug/aster)
else
  ASTER_BIN=(cargo run -q -p aster-cli --)
fi
aster() { ASTER_HOME="$WORK_DIR/aster-home" ASTER_KEYSTORE=file "${ASTER_BIN[@]}" "$@"; }

AUDIENCE=mission-control
rm -rf "$WORK_DIR/aster-home"
rm -f "$WORK_DIR"/{auth.env,fleet.secret,server.key,server.cred,edge.key,edge.cred,ops.key,ops.cred,*-roles.json}

aster key root generate >/dev/null
ROOT=$(aster key root show --json | jq -r .root_pubkey)
FLEET=$(aster enroll namespace create --out-secret "$WORK_DIR/fleet.secret" --json | jq -r .namespace)

enroll() {  # enroll <name> <roles or empty>
  local name="$1" roles="$2" id
  id=$(aster key node generate --out "$WORK_DIR/$name.key" --json | jq -r .node_id)
  if [[ -n "$roles" ]]; then
    printf '{"aster.role":{"type":"string","value":"%s"}}\n' "$roles" > "$WORK_DIR/$name-roles.json"
    aster enroll node --subject "$id" --audience "$AUDIENCE" \
      --attributes "$WORK_DIR/$name-roles.json" --out "$WORK_DIR/$name.cred" >/dev/null
  else
    aster enroll node --subject "$id" --audience "$AUDIENCE" --out "$WORK_DIR/$name.cred" >/dev/null
  fi
}
enroll server ""
enroll edge "ops.status,ops.ingest"
enroll ops "ops.status,ops.logs,ops.admin,ops.ingest"

cat > "$WORK_DIR/auth.env" <<EOF
ASTER_ROOT_PUBKEY=$ROOT
ASTER_ENROLLMENT_AUDIENCE=$AUDIENCE
ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET
EOF

for f in auth.env server.key server.cred edge.key edge.cred ops.key ops.cred; do
  if [[ ! -s "$WORK_DIR/$f" ]]; then
    echo "ERROR: $WORK_DIR/$f is missing or empty" >&2
    exit 1
  fi
done

echo "Auth setup complete in $WORK_DIR"
echo "  auth.env  : root $ROOT, fleet $FLEET, audience $AUDIENCE"
echo "  server    : server.key / server.cred"
echo "  edge      : edge.key / edge.cred (ops.status, ops.ingest)"
echo "  ops       : ops.key / ops.cred   (ops.status, ops.logs, ops.admin, ops.ingest)"
