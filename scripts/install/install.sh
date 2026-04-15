#!/usr/bin/env sh
# install.sh -- one-shot installer for the Aster CLI on Linux and macOS.
#
# Usage:
#   curl -fsSL https://aster.site/install.sh | sh
#   curl -fsSL https://aster.site/install.sh | ASTER_VERSION=0.1.2 sh
#   curl -fsSL https://aster.site/install.sh | ASTER_PREFIX=/opt/aster sh
#
# What it does:
#   1. Detects platform (linux-x86_64, linux-aarch64, macos-aarch64).
#   2. Downloads the standalone tarball + SHA256SUMS from GitHub Releases.
#   3. Verifies the tarball checksum.
#   4. Extracts to ~/.local/share/aster/ (atomic via temp dir + mv).
#   5. Symlinks ~/.local/bin/aster -> ~/.local/share/aster/aster.
#   6. Warns if ~/.local/bin is not on PATH.
#
# Why standalone (not onefile): instant startup, no extraction-on-first-run.
# The onefile binary is also published; pass --onefile to fetch that instead.

set -eu

# ── Configuration ──────────────────────────────────────────────────────────
REPO="${ASTER_REPO:-aster-rpc/aster-rpc}"
PREFIX="${ASTER_PREFIX:-$HOME/.local}"
SHARE_DIR="$PREFIX/share/aster"
BIN_DIR="$PREFIX/bin"
USE_ONEFILE=0

# Parse flags
for arg in "$@"; do
  case "$arg" in
    --onefile) USE_ONEFILE=1 ;;
    --help|-h)
      sed -n '2,18p' "$0" | sed 's/^# \?//'
      exit 0
      ;;
    *) printf 'Unknown argument: %s\n' "$arg" >&2; exit 1 ;;
  esac
done

# ── Helpers ────────────────────────────────────────────────────────────────
say() { printf '\033[1;36m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$1" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$1" >&2; exit 1; }

require() {
  command -v "$1" >/dev/null 2>&1 || die "Required tool not found: $1"
}

require curl
require tar
require uname

# ── Platform detection ─────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64|amd64)  SUFFIX=linux-x86_64 ;;
      aarch64|arm64) SUFFIX=linux-aarch64 ;;
      *) die "Unsupported Linux architecture: $ARCH" ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      arm64) SUFFIX=macos-aarch64 ;;
      x86_64) die "Intel macOS is not currently supported. The Aster CLI ships arm64-only on macOS." ;;
      *) die "Unsupported macOS architecture: $ARCH" ;;
    esac
    ;;
  *) die "Unsupported OS: $OS. Use the Windows installer (install.ps1)." ;;
esac

# ── Resolve version ────────────────────────────────────────────────────────
if [ -n "${ASTER_VERSION:-}" ]; then
  VERSION="$ASTER_VERSION"
  TAG="cli-v$VERSION"
else
  say "Resolving latest aster-cli release..."
  TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
         | grep -o '"tag_name": *"cli-v[^"]*"' | head -1 \
         | sed 's/.*"cli-v\(.*\)"/\1/')"
  [ -n "$TAG" ] || die "Could not resolve latest release. Set ASTER_VERSION."
  VERSION="$TAG"
  TAG="cli-v$TAG"
fi
say "Installing aster-cli $VERSION ($SUFFIX)"

# ── Download ───────────────────────────────────────────────────────────────
BASE_URL="https://github.com/$REPO/releases/download/$TAG"
if [ "$USE_ONEFILE" -eq 1 ]; then
  ARCHIVE="aster-$SUFFIX"
else
  ARCHIVE="aster-dist-$SUFFIX.tar.gz"
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT INT TERM

say "Downloading $ARCHIVE..."
curl -fsSL --retry 3 -o "$TMPDIR/$ARCHIVE"   "$BASE_URL/$ARCHIVE"
curl -fsSL --retry 3 -o "$TMPDIR/SHA256SUMS" "$BASE_URL/SHA256SUMS"

# ── Verify ─────────────────────────────────────────────────────────────────
say "Verifying SHA256..."
EXPECTED="$(grep "  $ARCHIVE\$" "$TMPDIR/SHA256SUMS" | awk '{print $1}')"
[ -n "$EXPECTED" ] || die "$ARCHIVE not found in SHA256SUMS"

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$TMPDIR/$ARCHIVE" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "$TMPDIR/$ARCHIVE" | awk '{print $1}')"
else
  die "Need sha256sum or shasum to verify download"
fi
[ "$EXPECTED" = "$ACTUAL" ] || die "Checksum mismatch (expected $EXPECTED, got $ACTUAL)"

# ── Install ────────────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR" "$(dirname "$SHARE_DIR")"

if [ "$USE_ONEFILE" -eq 1 ]; then
  # Onefile: drop the binary directly in BIN_DIR.
  install -m 0755 "$TMPDIR/$ARCHIVE" "$BIN_DIR/aster"
  say "Warming onefile cache..."
  "$BIN_DIR/aster" --version >/dev/null 2>&1 || true
else
  # Standalone: extract dist to share/aster/ atomically.
  say "Extracting to $SHARE_DIR..."
  tar -xzf "$TMPDIR/$ARCHIVE" -C "$TMPDIR"
  EXTRACTED="$TMPDIR/aster-$SUFFIX"
  [ -d "$EXTRACTED" ] || die "Tarball layout unexpected (no aster-$SUFFIX/)"

  # Atomic replace: move new in, remove old.
  if [ -d "$SHARE_DIR" ]; then
    rm -rf "$SHARE_DIR.old" 2>/dev/null || true
    mv "$SHARE_DIR" "$SHARE_DIR.old"
  fi
  mv "$EXTRACTED" "$SHARE_DIR"
  rm -rf "$SHARE_DIR.old" 2>/dev/null || true

  ln -sf "$SHARE_DIR/aster" "$BIN_DIR/aster"
fi

# ── Verify install ─────────────────────────────────────────────────────────
"$BIN_DIR/aster" --version >/dev/null 2>&1 || die "Installed binary failed --version"

say "Installed: $BIN_DIR/aster"

# ── PATH check ─────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    warn "$BIN_DIR is not on your PATH."
    case "$(basename "${SHELL:-/bin/sh}")" in
      bash) RC="~/.bashrc" ;;
      zsh)  RC="~/.zshrc" ;;
      fish) RC="~/.config/fish/config.fish" ;;
      *)    RC="your shell's rc file" ;;
    esac
    printf '   Add to %s:\n     export PATH="%s:$PATH"\n' "$RC" "$BIN_DIR"
    ;;
esac

say "Run: aster --help"
