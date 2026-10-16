# syntax=docker/dockerfile:1
#
# Self-hosted iroh relay for Aster, built from the iroh fork commit Aster pins.
#
# Build from the repository root (the build context must contain
# deploy/relay/build-binary.sh); IROH_REV is required and must be the
# `[forks.iroh] commit` from iroh-fork-manifest.toml:
#
#   rev="$(python3 -c 'import tomllib;print(tomllib.load(open("iroh-fork-manifest.toml","rb"))["forks"]["iroh"]["commit"])')"
#   docker buildx build -f deploy/relay/Dockerfile --build-arg IROH_REV="$rev" \
#     --platform linux/amd64,linux/arm64 -t aster-relay:dev .
#
# Release images are not built from this file: CI job containers have no
# container daemon, so `deploy/relay/assemble-oci.py` assembles the same runtime
# stage (this file's runtime FROM line is its base pin) around binaries built by
# the same build-binary.sh. This file is for local builds and for operators who
# rebuild from source inside their own network. See docs/relay-self-hosted.md.
#
# Why not the fork's docker/Dockerfile `iroh-relay` target: it builds from
# unpinned rust:alpine and alpine:latest, runs as root, and builds the whole
# workspace with --all-features. This file pins both bases by digest, builds
# only the relay, and runs it non-root on distroless.

# Builder: Debian 13 to match the distroless runtime's glibc.
FROM docker.io/library/rust:1.96.0-slim-trixie@sha256:c37af730be4fd8104cbf9aedbd6ab259e51ca2d5437817a0f8680edf66ac6c28 AS build
ARG IROH_REV
RUN test -n "$IROH_REV" || { echo "IROH_REV build-arg is required" >&2; exit 64; }
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*
COPY deploy/relay/build-binary.sh /usr/local/bin/build-relay
RUN case "$(uname -m)" in \
      x86_64) target=x86_64-unknown-linux-gnu ;; \
      aarch64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported architecture $(uname -m)" >&2; exit 64 ;; \
    esac \
 && build-relay "$target" /out "$IROH_REV"

# Runtime: distroless cc (glibc, CA bundle, nonroot user 65532, no shell).
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS runtime
ARG IROH_REV
LABEL org.opencontainers.image.title="aster-relay" \
      org.opencontainers.image.description="iroh-relay built from the iroh fork commit pinned by Aster" \
      org.opencontainers.image.source="https://forge.emrul.dev/Aster/iroh" \
      org.opencontainers.image.revision="${IROH_REV}" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY --from=build --chmod=0555 /out/iroh-relay /usr/local/bin/iroh-relay
USER 65532:65532
WORKDIR /
# Unprivileged ports, so the container needs no capabilities:
# HTTP (captive-portal probe), HTTPS relay, QUIC address discovery, metrics.
EXPOSE 8080/tcp 8443/tcp 7842/udp 9090/tcp
# iroh-relay shuts down cleanly on SIGINT only; PID 1 ignores an unhandled
# SIGTERM. Runtimes that honour the image stop signal (containerd CRI, Docker)
# then stop it promptly instead of waiting out the grace period.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/iroh-relay"]
CMD ["--config-path", "/etc/iroh-relay/config.toml"]
