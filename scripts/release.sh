#!/bin/bash
set -euo pipefail

# Aster release script — bumps versions across all tracks and tags.
#
# Usage:
#   ./scripts/release.sh           # interactive
#   ./scripts/release.sh 0.2.0     # set version directly

BOLD="\033[1m"
DIM="\033[2m"
GREEN="\033[32m"
YELLOW="\033[33m"
RESET="\033[0m"

# ── Current versions ──────────────────────────────────────────────────────
CORE_VERSION=$(grep '^version' core/Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
FFI_VERSION=$(grep '^version' ffi/Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
PYTHON_VERSION=$(grep '^version' pyproject.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
CLI_VERSION=$(grep '^version' cli/pyproject.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
TS_VERSION=$(grep '"version"' bindings/typescript/packages/aster/package.json | head -1 | sed 's/.*"\(.*\)".*/\1/')

echo -e "${BOLD}Current versions:${RESET}"
echo -e "  Core (Track 2):     ${GREEN}${CORE_VERSION}${RESET}"
echo -e "  FFI (Track 2):      ${GREEN}${FFI_VERSION}${RESET}"
echo -e "  Python (Track 3):   ${GREEN}${PYTHON_VERSION}${RESET}"
echo -e "  TypeScript (Track 3): ${GREEN}${TS_VERSION}${RESET}"
echo -e "  CLI (Track 4):      ${GREEN}${CLI_VERSION}${RESET}"
echo ""

# ── Get new version ───────────────────────────────────────────────────────
if [ "${1:-}" != "" ]; then
    NEW_VERSION="$1"
else
    echo -e "${BOLD}What type of release?${RESET}"
    echo "  1) Patch  (bug fixes, no new features)"
    echo "  2) Minor  (new features, backward compatible)"
    echo "  3) Major  (breaking changes)"
    echo "  4) Custom (enter version manually)"
    echo ""
    read -p "Choice [1-4]: " CHOICE

    IFS='.' read -r MAJOR MINOR PATCH <<< "$PYTHON_VERSION"
    case "$CHOICE" in
        1) NEW_VERSION="$MAJOR.$MINOR.$((PATCH + 1))" ;;
        2) NEW_VERSION="$MAJOR.$((MINOR + 1)).0" ;;
        3) NEW_VERSION="$((MAJOR + 1)).0.0" ;;
        4) read -p "Enter version: " NEW_VERSION ;;
        *) echo "Invalid choice"; exit 1 ;;
    esac
fi

echo ""
echo -e "${BOLD}New version: ${GREEN}${NEW_VERSION}${RESET}"
echo ""

# ── Confirm ───────────────────────────────────────────────────────────────
echo -e "${BOLD}Files to update:${RESET}"
echo "  core/Cargo.toml                                    ${CORE_VERSION} -> ${NEW_VERSION}"
echo "  ffi/Cargo.toml                                     ${FFI_VERSION} -> ${NEW_VERSION}"
echo "  pyproject.toml                                     ${PYTHON_VERSION} -> ${NEW_VERSION}"
echo "  cli/pyproject.toml                                 ${CLI_VERSION} -> ${NEW_VERSION}"
echo "  bindings/typescript/packages/aster/package.json    ${TS_VERSION} -> ${NEW_VERSION}"
echo "  bindings/typescript/native/package.json            (if exists)"
echo ""
echo -e "${BOLD}Tags to create:${RESET}"
echo "  v${NEW_VERSION}         (Python binding + PyPI publish)"
echo "  core-v${NEW_VERSION}    (Rust core)"
echo ""
read -p "Proceed? [y/N] " CONFIRM
if [ "$CONFIRM" != "y" ] && [ "$CONFIRM" != "Y" ]; then
    echo "Aborted."
    exit 0
fi

# ── Update versions ───────────────────────────────────────────────────────
echo ""
echo -e "${DIM}Updating version files...${RESET}"

# Core (Cargo.toml)
sed -i '' "s/^version = \"${CORE_VERSION}\"/version = \"${NEW_VERSION}\"/" core/Cargo.toml
sed -i '' "s/^version = \"${FFI_VERSION}\"/version = \"${NEW_VERSION}\"/" ffi/Cargo.toml

# Python (pyproject.toml)
sed -i '' "s/^version = \"${PYTHON_VERSION}\"/version = \"${NEW_VERSION}\"/" pyproject.toml

# CLI (cli/pyproject.toml)
sed -i '' "s/^version = \"${CLI_VERSION}\"/version = \"${NEW_VERSION}\"/" cli/pyproject.toml

# TypeScript
for pkg_json in bindings/typescript/packages/aster/package.json bindings/typescript/native/package.json; do
    if [ -f "$pkg_json" ]; then
        sed -i '' "s/\"version\": \"${TS_VERSION}\"/\"version\": \"${NEW_VERSION}\"/" "$pkg_json"
    fi
done

# Also update the TS native binding Cargo.toml if it exists
if [ -f "bindings/typescript/native/Cargo.toml" ]; then
    NATIVE_VERSION=$(grep '^version' bindings/typescript/native/Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
    sed -i '' "s/^version = \"${NATIVE_VERSION}\"/version = \"${NEW_VERSION}\"/" bindings/typescript/native/Cargo.toml
fi

# ── Verify ────────────────────────────────────────────────────────────────
echo -e "${DIM}Verifying...${RESET}"
echo ""
echo -e "${BOLD}Updated versions:${RESET}"
echo "  Core:       $(grep '^version' core/Cargo.toml | head -1)"
echo "  FFI:        $(grep '^version' ffi/Cargo.toml | head -1)"
echo "  Python:     $(grep '^version' pyproject.toml | head -1)"
echo "  CLI:        $(grep '^version' cli/pyproject.toml | head -1)"
echo "  TypeScript: $(grep '"version"' bindings/typescript/packages/aster/package.json | head -1)"
echo ""

# ── Commit and tag ────────────────────────────────────────────────────────
read -p "Commit and tag? [y/N] " TAG_CONFIRM
if [ "$TAG_CONFIRM" = "y" ] || [ "$TAG_CONFIRM" = "Y" ]; then
    git add -A
    git commit -m "Release v${NEW_VERSION}"
    git tag "v${NEW_VERSION}"
    git tag "core-v${NEW_VERSION}"
    echo ""
    echo -e "${GREEN}${BOLD}Tagged v${NEW_VERSION} and core-v${NEW_VERSION}${RESET}"
    echo ""
    echo -e "Push with:"
    echo -e "  git push origin main --tags"
    echo ""
    echo -e "This will trigger:"
    echo -e "  - CI build + test"
    echo -e "  - PyPI publish (trusted publishing)"
    echo -e "  - GitHub Release with wheel artifacts"
else
    echo ""
    echo -e "${YELLOW}Version files updated but not committed.${RESET}"
    echo "Review with: git diff"
    echo "Commit with: git add -A && git commit -m 'Release v${NEW_VERSION}'"
fi
