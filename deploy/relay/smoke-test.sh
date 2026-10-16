#!/usr/bin/env bash
# Smoke-test an aster-relay image: start it hardened (non-root, read-only root
# filesystem, no capabilities) with a throwaway self-signed certificate, then
# probe its endpoints and check that it stops promptly.
#
# Usage: smoke-test.sh <image-ref>
# Env:   CONTAINER_CLI (default: docker, falls back to nerdctl)
#
# Requires openssl and curl on the host. Exit codes: 0 pass, 1 a check
# failed, 64 usage error.
set -euo pipefail
IFS=$'\n\t'

log() { printf '%s relay-smoke %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
die() { log "FAIL: $*"; exit 1; }

[[ $# -eq 1 ]] || { log "usage: smoke-test.sh <image-ref>"; exit 64; }
image="$1"
cli="${CONTAINER_CLI:-}"
if [[ -z "$cli" ]]; then
  if command -v docker >/dev/null; then cli=docker; else cli=nerdctl; fi
fi
name="aster-relay-smoke-$$"
work="$(mktemp -d)"
cleanup() {
  "$cli" rm -f "$name" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 \
  -subj "/CN=relay.example.com" -addext "subjectAltName=DNS:relay.example.com,IP:127.0.0.1" \
  -keyout "$work/tls.key" -out "$work/tls.crt" 2>/dev/null
# The container runs as 65532; the bind-mounted files must be readable to it.
chmod 0644 "$work/tls.key" "$work/tls.crt"
# Use the shipped Kubernetes ConfigMap's config.toml, so the test also
# validates the configuration operators deploy.
configmap="$(dirname "${BASH_SOURCE[0]}")/k8s/configmap.yaml"
sed -n '/^  config.toml: |$/,$p' "$configmap" | tail -n +2 | sed 's/^    //' > "$work/config.toml"
grep -q '^cert_mode = "Reloading"$' "$work/config.toml" || die "could not extract config.toml from $configmap"
chmod 0644 "$work/config.toml"

log "starting $image with $cli"
"$cli" run -d --name "$name" \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  -e RUST_LOG=info \
  -v "$work/config.toml:/etc/iroh-relay/config.toml:ro" \
  -v "$work/tls.crt:/etc/iroh-relay/tls/tls.crt:ro" \
  -v "$work/tls.key:/etc/iroh-relay/tls/tls.key:ro" \
  -p 127.0.0.1:18080:8080 -p 127.0.0.1:18443:8443 -p 127.0.0.1:17842:7842/udp -p 127.0.0.1:19090:9090 \
  "$image" >/dev/null

user="$("$cli" inspect -f '{{.Config.User}}' "$name")"
[[ "$user" == "65532:65532" ]] || die "image user is '$user', expected 65532:65532"
log "image user $user"

for _ in $(seq 1 30); do
  curl -fsk --max-time 2 https://127.0.0.1:18443/healthz >/dev/null 2>&1 && break
  sleep 1
done

check() { # description expected-status url
  local status
  status="$(curl -sk --max-time 5 -o /dev/null -w '%{http_code}' "$3")" || true
  [[ "$status" == "$2" ]] || { "$cli" logs "$name" >&2 || true; die "$1: $3 returned $status, expected $2"; }
  log "ok  $1 ($3 -> $status)"
}
check "HTTPS health" 200 https://127.0.0.1:18443/healthz
check "relay probe" 200 https://127.0.0.1:18443/ping
check "captive-portal probe" 204 http://127.0.0.1:18080/generate_204
check "metrics" 200 http://127.0.0.1:19090/metrics
# Verify the served certificate is the mounted one (TLS terminates in the relay).
served="$(openssl s_client -connect 127.0.0.1:18443 -servername relay.example.com </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256)"
expected="$(openssl x509 -in "$work/tls.crt" -noout -fingerprint -sha256)"
[[ "$served" == "$expected" ]] || die "served certificate differs from the mounted one"
log "ok  served certificate matches the mounted Secret"

start="$(date +%s)"
"$cli" stop -t 20 "$name" >/dev/null
elapsed=$(( $(date +%s) - start ))
(( elapsed < 10 )) || die "relay took ${elapsed}s to stop; the stop signal is not honoured"
log "ok  stopped in ${elapsed}s"
log "PASS $image"
