#!/usr/bin/env bash
# Install the pinned image tools (crane, trivy, cosign) into <dest-dir>.
#
# Usage: install-image-tools.sh <dest-dir>
#
# Versions and SHA-256 hashes are pinned here; a download whose hash differs
# aborts. The hashes come from each project's published checksum file for that
# release. Bump a version by replacing its version and both hashes together.
set -euo pipefail
IFS=$'\n\t'

log() { printf '%s image-tools %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
die() { log "error: $*"; exit "${2:-1}"; }

[[ $# -eq 1 ]] || die "usage: install-image-tools.sh <dest-dir>" 64
dest="$1"

CRANE_VERSION=0.22.1
TRIVY_VERSION=0.75.0
COSIGN_VERSION=2.6.5
GH=https://github.com

case "$(uname -m)" in
  x86_64)
    crane_asset=go-containerregistry_Linux_x86_64.tar.gz
    crane_sha=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0
    trivy_asset="trivy_${TRIVY_VERSION}_Linux-64bit.tar.gz"
    trivy_sha=c6e65abddb348e25f10549df887045629cf28cc72453cd1c63acb717316b3f3f
    cosign_asset=cosign-linux-amd64
    cosign_sha=c3b4f5410e608af03a5eb0aaac84a4313d8da131248e08ff1759ac70c79d1644
    ;;
  aarch64)
    crane_asset=go-containerregistry_Linux_arm64.tar.gz
    crane_sha=898c0cff975f898a33e8c4580bdafb0e7c02c7faa33374e946762f97c4ab7110
    trivy_asset="trivy_${TRIVY_VERSION}_Linux-ARM64.tar.gz"
    trivy_sha=a1ee9f6ffb7d112b64ff726a2a0717c21175c1114361391f4a132956751a13b3
    cosign_asset=cosign-linux-arm64
    cosign_sha=426193b4c5da4d4d643e822f48fe0cc8a476ca1782a272704831f5a0cef716d7
    ;;
  *) die "unsupported architecture $(uname -m)" 64 ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$dest"

fetch() { # url sha256 output
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$3" "$1"
  echo "$2  $3" | sha256sum -c --quiet - || die "checksum mismatch for $1"
}

fetch "$GH/google/go-containerregistry/releases/download/v${CRANE_VERSION}/${crane_asset}" "$crane_sha" "$work/crane.tgz"
tar -xzf "$work/crane.tgz" -C "$work" crane
install -m 0755 "$work/crane" "$dest/crane"

fetch "$GH/aquasecurity/trivy/releases/download/v${TRIVY_VERSION}/${trivy_asset}" "$trivy_sha" "$work/trivy.tgz"
tar -xzf "$work/trivy.tgz" -C "$work" trivy
install -m 0755 "$work/trivy" "$dest/trivy"

fetch "$GH/sigstore/cosign/releases/download/v${COSIGN_VERSION}/${cosign_asset}" "$cosign_sha" "$work/cosign"
install -m 0755 "$work/cosign" "$dest/cosign"

log "installed crane ${CRANE_VERSION}, trivy ${TRIVY_VERSION}, cosign ${COSIGN_VERSION} into $dest"
