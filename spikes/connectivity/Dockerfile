# Optimised multi-stage build with cargo-chef. The expensive iroh/noq-fork
# dependency compile is cached as its OWN layer, busted only when dependencies
# change — so editing the prober source re-runs just the fast final build
# (~1-2 min) instead of the full ~15 min cold compile.
#
# Build CONTEXT must be the REPO ROOT (the example depends on the workspace).
# Deploy from the repo root:
#   fly deploy --config spikes/connectivity/fly.toml \
#     --dockerfile spikes/connectivity/Dockerfile \
#     --ignorefile spikes/connectivity/Dockerfile.dockerignore .

# ── base: rust + native build deps + cargo-chef ──────────────────────────────
FROM rust:1-bookworm AS chef
# aws-lc-sys (amd64) wants cmake + a C/C++ toolchain + nasm; ring wants perl.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake perl nasm pkg-config libssl-dev git && \
    rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /src

# ── planner: distil the dependency graph into recipe.json ────────────────────
FROM chef AS planner
COPY . .
# aster-expose path-deps the Salvo fork (../../salvo), which is OUTSIDE this repo
# and not in the build context. Trim the workspace to what the prober needs so
# neither the recipe nor the build tries to resolve that missing path.
RUN sed -i 's#^members = .*#members = ["core", "aster", "aster-macros"]#' Cargo.toml
RUN cargo chef prepare --recipe-path recipe.json

# ── builder: cook deps (cached), then build the prober ───────────────────────
FROM chef AS builder
# 1) Compile & CACHE every dependency from the recipe. recipe.json is identical
#    across source-only edits, so this layer (the iroh/noq forks) is reused.
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
# 2) Bring the real source and build the prober against the cached deps.
COPY . .
RUN sed -i 's#^members = .*#members = ["core", "aster", "aster-macros"]#' Cargo.toml
RUN cargo build --release -p aster --example probe_edge
RUN cp target/release/examples/probe_edge /usr/local/bin/probe_edge

# ── runtime ──────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
# ca-certificates: iroh relays are HTTPS/WebSocket; TLS needs the trust store.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/probe_edge /usr/local/bin/probe_edge
ENV PROBE_INTERVAL_MS=1000 PROBE_PINGS=0
# PROBE_TICKET is injected as a Fly secret at deploy time.
ENTRYPOINT ["/usr/local/bin/probe_edge"]
