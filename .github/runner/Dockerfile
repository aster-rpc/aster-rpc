# Self-hosted GitHub Actions runner for aster-python CI.
#
# Multi-arch: builds on both x86_64 and aarch64.
# Host-mounted cache volumes keep Cargo/sccache/uv state across restarts.
#
# Build:
#   docker build -t aster-runner .github/runner/
#
# Run:
#   docker run -d --name aster-runner \
#     -e RUNNER_NAME=linux-x64 \
#     -e GITHUB_URL=https://github.com/<owner>/<repo> \
#     -e RUNNER_TOKEN=<token> \
#     -e RUNNER_LABELS=Linux,X64 \
#     -v aster-cache-cargo:/cache/cargo \
#     -v aster-cache-sccache:/cache/sccache \
#     -v aster-cache-uv:/cache/uv \
#     aster-runner

FROM ubuntu:24.04

ARG RUNNER_VERSION=2.323.0
ARG RUST_TOOLCHAIN=1.94.1
ARG PYTHON_VERSION=3.13
ARG TARGETARCH

# Avoid interactive prompts
ENV DEBIAN_FRONTEND=noninteractive

# ── System packages ──────────────────────────────────────────────────
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    cmake \
    curl \
    git \
    jq \
    libssl-dev \
    lsb-release \
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

# ── Rust toolchain ──────────────────────────────────────────────────
USER runner
ENV RUSTUP_HOME=/home/runner/.rustup \
    CARGO_HOME=/home/runner/.cargo
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain ${RUST_TOOLCHAIN} \
       --component rustfmt clippy
ENV PATH="/home/runner/.cargo/bin:${PATH}"

# ── sccache (shared compiler cache) ─────────────────────────────────
RUN cargo install sccache --locked
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
