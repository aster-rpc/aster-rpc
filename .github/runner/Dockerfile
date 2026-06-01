# Self-hosted GitHub Actions runner for aster-python CI.
#
# Multi-arch: builds on both x86_64 and aarch64.
# Host-mounted cache volumes keep Cargo/sccache/uv state across restarts.
#
# Build + run via compose (preferred — see docker-compose.yml):
#   docker compose -f .github/runner/docker-compose.yml up -d --build linux-x64
#
# Or standalone (ephemeral, restarts clean after each job):
#   docker run -d --name aster-runner --restart unless-stopped \
#     -e GITHUB_URL=https://github.com/emrul/iroh-python \
#     -e GH_PAT=<pat> \
#     -e RUNNER_LABELS=Linux,X64 \
#     -v aster-cache-cargo:/cache/cargo \
#     -v aster-cache-sccache:/cache/sccache \
#     -v aster-cache-uv:/cache/uv \
#     aster-runner

FROM ubuntu:24.04

ARG RUNNER_VERSION=2.334.0
# Rust pin — keep in sync with the dev machines. Mac is on 1.96.0 (2026-05-25);
# the Linux runner hosts are still on 1.94.1 and should be bumped to match.
ARG RUST_TOOLCHAIN=1.96.0
# sccache pin — matches the Mac's installed sccache (prebuilt, not cargo-built).
ARG SCCACHE_VERSION=0.15.0
ARG PYTHON_VERSION=3.13
ARG TARGETARCH

# Avoid interactive prompts
ENV DEBIAN_FRONTEND=noninteractive

# ── System packages ──────────────────────────────────────────────────
# Note: setup-java@v4 installs the JDK but NOT Maven, so we provide
# `maven` here for the Java binding workflow. Without it, build-java.yml
# fails with `mvn: command not found`.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    cmake \
    curl \
    git \
    jq \
    libssl-dev \
    lsb-release \
    maven \
    pkg-config \
    software-properties-common \
    sudo \
    unzip \
    wget \
    zip \
    # Python build deps
    libbz2-dev \
    libffi-dev \
    liblzma-dev \
    libncurses-dev \
    libreadline-dev \
    libsqlite3-dev \
    zlib1g-dev \
  && rm -rf /var/lib/apt/lists/*

# ── Runner user (GitHub Actions expects non-root) ────────────────────
RUN useradd -m -s /bin/bash runner \
  && echo "runner ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers.d/runner

# ── Python (deadsnakes PPA for the exact version) ────────────────────
RUN add-apt-repository ppa:deadsnakes/ppa -y \
  && apt-get update \
  && apt-get install -y --no-install-recommends \
       python${PYTHON_VERSION} \
       python${PYTHON_VERSION}-dev \
       python${PYTHON_VERSION}-venv \
  && rm -rf /var/lib/apt/lists/* \
  && ln -sf /usr/bin/python${PYTHON_VERSION} /usr/local/bin/python3 \
  && ln -sf /usr/bin/python${PYTHON_VERSION} /usr/local/bin/python

# ── uv (fast Python package manager) ────────────────────────────────
RUN curl -LsSf https://astral.sh/uv/install.sh | sh \
  && mv /root/.local/bin/uv /usr/local/bin/uv \
  && mv /root/.local/bin/uvx /usr/local/bin/uvx

# ── Node.js (LTS 20 + 22) ────────────────────────────────────────────
# Seeded into the GitHub Actions hosted-tool-cache so `actions/setup-node`
# resolves from cache (no per-job download). The newest seeded major is also
# symlinked onto PATH as the default `node`. AGENT_TOOLSDIRECTORY is the env
# the runner reads to locate that cache.
ENV AGENT_TOOLSDIRECTORY=/opt/hostedtoolcache
RUN NODEARCH=$(case "$(uname -m)" in x86_64) echo x64;; aarch64) echo arm64;; *) echo x64;; esac) \
  && for major in 20 22; do \
       ver=$(curl -fsSL https://nodejs.org/dist/index.json \
             | jq -r --arg m "v${major}." '[.[] | select(.version | startswith($m))][0].version' \
             | sed 's/^v//'); \
       dest="${AGENT_TOOLSDIRECTORY}/node/${ver}/${NODEARCH}"; \
       mkdir -p "$dest"; \
       curl -fsSL "https://nodejs.org/dist/v${ver}/node-v${ver}-linux-${NODEARCH}.tar.gz" \
         | tar -xz --strip-components=1 -C "$dest"; \
       touch "${AGENT_TOOLSDIRECTORY}/node/${ver}/${NODEARCH}.complete"; \
       echo "seeded node ${ver} (${NODEARCH})"; \
     done \
  && latest=$(ls -1 "${AGENT_TOOLSDIRECTORY}/node" | sort -V | tail -1) \
  && for b in node npm npx corepack; do \
       ln -sf "${AGENT_TOOLSDIRECTORY}/node/${latest}/${NODEARCH}/bin/${b}" "/usr/local/bin/${b}"; \
     done \
  && chown -R runner:runner "${AGENT_TOOLSDIRECTORY}" \
  && node --version && npm --version

# ── Bun (pinned to the version oven-sh/setup-bun uses in CI) ─────────
ARG BUN_VERSION=1.2.15
RUN BUNARCH=$(case "$(uname -m)" in x86_64) echo x64;; aarch64) echo aarch64;; *) echo x64;; esac) \
  && curl -fsSL "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-${BUNARCH}.zip" -o /tmp/bun.zip \
  && unzip -q /tmp/bun.zip -d /tmp \
  && install -m 0755 "/tmp/bun-linux-${BUNARCH}/bun" /usr/local/bin/bun \
  && ln -sf /usr/local/bin/bun /usr/local/bin/bunx \
  && rm -rf /tmp/bun.zip "/tmp/bun-linux-${BUNARCH}" \
  && bun --version

# ── .NET SDK (channel matches DOTNET_VERSION=9.0.x in CI) ────────────
ARG DOTNET_CHANNEL=9.0
ENV DOTNET_ROOT=/usr/lib/dotnet \
    DOTNET_INSTALL_DIR=/usr/lib/dotnet \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1
RUN curl -fsSL https://dot.net/v1/dotnet-install.sh -o /tmp/dotnet-install.sh \
  && bash /tmp/dotnet-install.sh --channel "${DOTNET_CHANNEL}" --install-dir "${DOTNET_ROOT}" \
  && ln -sf "${DOTNET_ROOT}/dotnet" /usr/local/bin/dotnet \
  && chown -R runner:runner "${DOTNET_ROOT}" \
  && rm /tmp/dotnet-install.sh \
  && dotnet --version

# ── Rust toolchain ──────────────────────────────────────────────────
USER runner
ENV RUSTUP_HOME=/home/runner/.rustup \
    CARGO_HOME=/home/runner/.cargo
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain ${RUST_TOOLCHAIN} \
       --profile minimal --component rustfmt --component clippy
ENV PATH="/home/runner/.cargo/bin:${PATH}"

# ── sccache (shared compiler cache) ─────────────────────────────────
# Prebuilt musl binary (pinned to SCCACHE_VERSION) — matches the dev machines
# and avoids a ~5-minute from-source `cargo install` on every image build.
# Built natively per-arch (build-amd64 on x86, build-arm64 on aarch64), so
# `uname -m` selects the right asset.
RUN ARCH=$(case "$(uname -m)" in x86_64) echo x86_64;; aarch64) echo aarch64;; *) echo x86_64;; esac) \
  && curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl.tar.gz" \
     | tar -xz -C /tmp \
  && install -m 0755 "/tmp/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl/sccache" "${CARGO_HOME}/bin/sccache" \
  && rm -rf "/tmp/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl" \
  && sccache --version
ENV RUSTC_WRAPPER=sccache

# ── Cache directories (mount these as host volumes) ──────────────────
# These env vars point Cargo/sccache/uv at paths that should be
# bind-mounted or named volumes from the host for persistence.
ENV CARGO_TARGET_DIR=/cache/cargo/target \
    SCCACHE_DIR=/cache/sccache \
    UV_CACHE_DIR=/cache/uv

# ── GitHub Actions runner ────────────────────────────────────────────
USER root
RUN ARCH=$(case ${TARGETARCH} in amd64) echo x64;; arm64) echo arm64;; *) echo x64;; esac) \
  && mkdir -p /opt/actions-runner \
  && cd /opt/actions-runner \
  && curl -sL "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-${ARCH}-${RUNNER_VERSION}.tar.gz" \
     | tar xz \
  && ./bin/installdependencies.sh \
  && chown -R runner:runner /opt/actions-runner

# ── Entrypoint ───────────────────────────────────────────────────────
COPY entrypoint.sh /opt/actions-runner/entrypoint.sh
RUN chmod +x /opt/actions-runner/entrypoint.sh

USER runner
WORKDIR /opt/actions-runner

ENTRYPOINT ["/opt/actions-runner/entrypoint.sh"]
