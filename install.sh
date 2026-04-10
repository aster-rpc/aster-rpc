#!/usr/bin/env sh
# Aster CLI installer
# https://aster.site
#
# Installs the `aster` command via `uv tool install`. If `uv` is not
# present, the official Astral installer is run first.
#
# Usage (recommended -- verify hash first):
#
#   curl -LsSfO https://aster.site/install.sh
#   shasum -a 256 install.sh   # compare to the hash on https://aster.site
#   sh install.sh
#
# Quick (one-liner, trusts TLS only):
#
#   curl -LsSf https://aster.site/install.sh | sh
#
# Environment variables:
#   ASTER_VERSION   Specific version to install (default: latest stable)
#   ASTER_PRERELEASE  If set, install the latest pre-release
#   ASTER_NO_MODIFY_PATH  If set, don't print PATH instructions
#
# Requires: curl, sh, a working internet connection.
# Installs: uv (if missing), aster-cli (which pulls in aster-rpc) via
#           `uv tool install`. The result is the `aster` command on PATH.

set -eu

# ── Output helpers ──────────────────────────────────────────────────────────

if [ -t 1 ]; then
    BOLD="$(printf '\033[1m')"
    GREEN="$(printf '\033[32m')"
    YELLOW="$(printf '\033[33m')"
    RED="$(printf '\033[31m')"
    DIM="$(printf '\033[2m')"
    RESET="$(printf '\033[0m')"
else
    BOLD=""; GREEN=""; YELLOW=""; RED=""; DIM=""; RESET=""
fi

say()  { printf "%s\n" "$1"; }
ok()   { printf "%s  ✓ %s%s\n" "${GREEN}" "$1" "${RESET}"; }
warn() { printf "%s  ⚠ %s%s\n" "${YELLOW}" "$1" "${RESET}"; }
err()  { printf "%s  ✗ %s%s\n" "${RED}" "$1" "${RESET}" >&2; }
step() { printf "\n%s── %s ──%s\n" "${BOLD}" "$1" "${RESET}"; }

# ── Pre-flight ──────────────────────────────────────────────────────────────

if ! command -v curl >/dev/null 2>&1; then
    err "curl is required but not installed"
    exit 1
fi

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$OS" in
    linux|darwin) ;;
    *)
        err "Unsupported OS: $OS"
        say "  Aster CLI requires Linux or macOS."
        say "  Windows users: install via 'pip install aster-rpc' or use WSL."
        exit 1
        ;;
esac

case "$ARCH" in
    x86_64|amd64|arm64|aarch64) ;;
    *)
        err "Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

printf "\n%sInstalling Aster CLI%s\n" "${BOLD}" "${RESET}"
printf "%sPeer-to-peer RPC framework + control-plane CLI%s\n" "${DIM}" "${RESET}"
printf "%sPlatform: %s/%s%s\n" "${DIM}" "$OS" "$ARCH" "${RESET}"

# ── Step 1: ensure uv is installed ──────────────────────────────────────────

step "uv (Python tool installer)"

if command -v uv >/dev/null 2>&1; then
    UV_VERSION="$(uv --version 2>/dev/null | awk '{print $2}')"
    ok "uv ${UV_VERSION} found"
else
    warn "uv not found -- installing via the official Astral installer"
    say "    https://docs.astral.sh/uv/getting-started/installation/"
    say ""
    if ! curl -LsSf https://astral.sh/uv/install.sh | sh; then
        err "uv installation failed"
        say "  Try installing uv manually, then re-run this script."
        exit 1
    fi
    # uv installer adds itself to ~/.local/bin (Linux) or ~/.cargo/bin (older).
    # Make it visible to this script's PATH so the next command works.
    for dir in "${HOME}/.local/bin" "${HOME}/.cargo/bin"; do
        if [ -d "$dir" ] && ! echo ":${PATH}:" | grep -q ":${dir}:"; then
            PATH="${dir}:${PATH}"
        fi
    done
    if ! command -v uv >/dev/null 2>&1; then
        err "uv was installed but is not on PATH"
        say "  Restart your shell and re-run this script, or add ~/.local/bin"
        say "  to your PATH manually."
        exit 1
    fi
    ok "uv installed"
fi

# ── Step 2: install aster CLI ───────────────────────────────────────────────

step "aster (CLI + Python bindings)"

# aster-cli is the CLI package; it depends on aster-rpc (the bindings),
# so installing aster-cli pulls in everything.
if [ -n "${ASTER_VERSION:-}" ]; then
    PKG="aster-cli==${ASTER_VERSION}"
elif [ -n "${ASTER_PRERELEASE:-}" ]; then
    PKG="aster-cli"
    UV_FLAGS="--prerelease=allow"
else
    PKG="aster-cli"
    UV_FLAGS=""
fi

# `--force` reinstalls if already present so re-running upgrades cleanly.
# shellcheck disable=SC2086
if uv tool install --force ${UV_FLAGS:-} "$PKG"; then
    ok "aster installed"
else
    err "aster installation failed"
    say "  uv tool install ${PKG} returned non-zero."
    say "  See: https://docs.aster.site/install"
    exit 1
fi

# ── Step 3: verify ──────────────────────────────────────────────────────────

step "Verification"

if command -v aster >/dev/null 2>&1; then
    INSTALLED="$(aster --version 2>/dev/null || echo unknown)"
    ok "aster command available: ${INSTALLED}"
else
    if [ -z "${ASTER_NO_MODIFY_PATH:-}" ]; then
        warn "aster is installed but not yet on your PATH"
        say ""
        say "  uv installs tools to ${HOME}/.local/bin -- add it to your PATH:"
        say ""
        say "    ${BOLD}echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc${RESET}"
        say "    ${BOLD}source ~/.zshrc${RESET}"
        say ""
        say "  Then verify with: ${BOLD}aster --version${RESET}"
    fi
fi

# ── Done ────────────────────────────────────────────────────────────────────

cat <<EOF

${GREEN}${BOLD}Aster CLI installed.${RESET}

Try it:
  ${BOLD}aster --help${RESET}              show all commands
  ${BOLD}aster shell --demo${RESET}        interactive demo (no network)
  ${BOLD}aster init${RESET}                scaffold a new project

Docs:    ${BOLD}https://docs.aster.site${RESET}
Source:  ${BOLD}https://github.com/emrul/iroh-python${RESET}
EOF
