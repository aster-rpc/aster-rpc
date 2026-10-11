#!/usr/bin/env bash
# check-otel-deps.sh -- the observability feature-graph gate (design
# docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md §2.2, phase O1).
#
# Properties a one-line Cargo.toml edit can break without failing any build:
#
#   1. The OpenTelemetry SDK stays out of `aster`'s default and `rpc`-only
#      graphs. `aster-otel` owns the SDK and its exporters (design D1); `aster`
#      reaches it only through its own opt-in `otel` feature (O2), so a
#      consumer that never asked for telemetry links none of it.
#
#   2. `aster-otel` does not depend on `aster` (D1): the dependency points the
#      other way, and a cycle-in-waiting would drag the whole runtime into
#      every binding's telemetry crate.
#
#   3. `aster-otel`'s TLS is the rustls + ring stack iroh already links. Its
#      OTLP client is built with reqwest `rustls-no-provider` and an explicit
#      ring provider precisely so that the telemetry crate itself adds neither
#      aws-lc-rs (a C/asm build) nor OpenSSL (native-tls). The HTTP transport
#      has a separate provider graph (see rule 7).
#
#   4. The SDK resolves at the exactly pinned versions (§2.2).
#
#   5. `aster`'s `otel` feature reaches OpenTelemetry only through `aster-otel`
#      (O2): no direct `opentelemetry*` / `tracing-opentelemetry` edge, so the
#      exact pins live in one crate and `aster` uses the API it re-exports.
#      The binding profile without `otel` stays SDK-free like rule 1.
#
#   6. `health` implies `otel` (O3, design §6 / Q15): `/metrics` is the OTel
#      Prometheus pull reader, so `health` alone must bring aster-otel with its
#      `prometheus` feature. Bindings that do not serve health or telemetry
#      (today's profile, below) stay SDK-free.
#
#   7. The C ABI (`aster_transport_ffi`, ABI 4.6) links the pipeline through
#      aster-otel with OTLP logs and the Prometheus reader, at the pinned
#      versions, without OpenSSL/native-tls. Its HTTP transport also links the
#      pinned Salvo/ACME stack, whose defaults include AWS-LC; that does not
#      change OTLP's explicitly preconfigured ring client.
#
# Usage: ./scripts/check-otel-deps.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
RESET='\033[0m'

pass() { echo -e "${GREEN}OK${RESET}  $1"; }
fail() { echo -e "${RED}FAIL${RESET} $1"; exit 1; }

# `-e normal` keeps dev-dependencies out (tests may use whatever they like);
# `--locked` makes a stale Cargo.lock fail the gate instead of being re-resolved.
tree() { cargo tree --locked -e normal "$@"; }

# ── 1. No OTel SDK in aster's default and rpc-only graphs ───────────────────

for label in "default" "rpc" "rpc,binding-support" \
    "rpc,binding-support,expose,coordination-client"; do
    if [[ "$label" == "default" ]]; then
        out=$(tree -p aster)
    else
        out=$(tree -p aster --no-default-features --features "$label")
    fi
    if grep -q '\bopentelemetry_sdk\b' <<<"$out"; then
        echo "$out" | grep -n 'opentelemetry_sdk' || true
        fail "opentelemetry_sdk is in aster's '$label' graph; it belongs behind aster-otel"
    fi
    pass "no opentelemetry_sdk in aster's '$label' graph"
done

# ── 5. aster's `otel` feature goes through aster-otel only ──────────────────

direct=$(tree -p aster --features rpc,binding-support,otel,health --depth 1)
if ! grep -q 'aster-otel v' <<<"$direct"; then
    fail "aster's 'otel' feature does not depend on aster-otel"
fi
if grep -Eq '(opentelemetry[a-z_-]*|tracing-opentelemetry) v' <<<"$direct"; then
    echo "$direct" | grep -En '(opentelemetry[a-z_-]*|tracing-opentelemetry) v' || true
    fail "aster depends on an OpenTelemetry crate directly; use aster-otel's re-exports"
fi
pass "aster's 'otel' feature reaches OpenTelemetry only through aster-otel"

# ── 6. health implies otel, with the Prometheus pull reader ─────────────────

out=$(tree -p aster --no-default-features --features health)
for needle in 'aster-otel v' 'opentelemetry_sdk v' 'opentelemetry-prometheus v'; do
    if ! grep -q "$needle" <<<"$out"; then
        fail "aster's 'health' feature does not bring '${needle% v}' (health must imply otel + prometheus)"
    fi
done
pass "aster's 'health' feature implies otel with the Prometheus pull reader"

# ── 2 + 3 + 4. aster-otel's own graph ───────────────────────────────────────

for label in "default" "all-features"; do
    if [[ "$label" == "default" ]]; then
        out=$(tree -p aster-otel)
    else
        out=$(tree -p aster-otel --all-features)
    fi
    # "aster v" does not match "aster-otel v": the crate's own root line.
    if grep -q '[[:space:]]aster v' <<<"$out"; then
        fail "aster-otel depends on aster in its '$label' graph (design D1 forbids it)"
    fi
    pass "aster-otel does not depend on aster ('$label')"
    for crate_name in aws-lc-rs aws-lc-sys openssl openssl-sys native-tls; do
        if grep -q "\\b${crate_name} v" <<<"$out"; then
            echo "$out" | grep -n "${crate_name} v" || true
            fail "'$crate_name' is in aster-otel's '$label' graph; OTLP TLS must stay on rustls + ring"
        fi
    done
    pass "no aws-lc / OpenSSL in aster-otel's '$label' graph"
    for pinned in 'opentelemetry_sdk v0.33.0' 'opentelemetry v0.33.0' 'tracing-opentelemetry v0.34.0'; do
        if ! grep -q "$pinned" <<<"$out"; then
            fail "aster-otel's '$label' graph does not resolve the pinned $pinned"
        fi
    done
    pass "pinned OpenTelemetry versions resolve in aster-otel's '$label' graph"
done

# ── 7. the C ABI profile ────────────────────────────────────────────────────

out=$(tree -p aster_transport_ffi)
for needle in 'aster-otel v' 'opentelemetry_sdk v0.33.0' 'opentelemetry-prometheus v0.33.0' \
    'opentelemetry-appender-tracing v0.33.0'; do
    if ! grep -q "$needle" <<<"$out"; then
        fail "the C ABI graph lacks '$needle' (ABI 4.6 observability)"
    fi
done
# Salvo/ACME already uses AWS-LC in the HTTP transport graph. ABI 4.16 now
# exposes that transport to every binding. Keep OTLP's isolated default and
# all-features graphs strictly ring-only above, and keep native TLS out of the
# whole C ABI. aster-otel/src/http.rs uses builder_with_provider(ring) and
# tls_backend_preconfigured, so feature unification cannot select AWS-LC for it.
for crate_name in openssl openssl-sys native-tls; do
    if grep -q "\\b${crate_name} v" <<<"$out"; then
        echo "$out" | grep -n "${crate_name} v" || true
        fail "'$crate_name' is in the C ABI graph; TLS must stay on rustls"
    fi
done
if grep -Eq '\baws-lc-(rs|sys) v' <<<"$out"; then
    for needle in 'aster-transport-salvo v' 'salvo-acme v' 'certon v'; do
        if ! grep -q "$needle" <<<"$out"; then
            fail "AWS-LC in the C ABI graph without the pinned Salvo/ACME transport"
        fi
    done
    pass "the C ABI includes Salvo/ACME AWS-LC; OTLP independently selects ring"
fi
pass "the C ABI links the pinned pipeline without OpenSSL / native-tls"

echo -e "\n${GREEN}Observability dependency gate passed.${RESET}"
