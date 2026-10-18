#!/usr/bin/env bash
# hermetic.sh -- run a test command with outbound network blocked except
# loopback, so a test that reaches the internet fails instead of passing on a
# machine that happens to be online (resilience-test-plan.md §4, FFI-1).
#
#   scripts/ci/hermetic.sh <command> [args...]
#   scripts/ci/hermetic.sh --check        print the enforcement mode and exit
#
# Build and fetch first, outside the guard: `cargo test --no-run`,
# `dotnet build`, `mvn -o` with a primed ~/.m2, `go test` after `go mod
# download`, `uv sync` / `bun install`. The guard is for the test process.
#
# Enforcement, by platform:
#   macOS   sandbox-exec: outbound IP traffic only to localhost (127.0.0.1,
#           ::1); Unix sockets allowed except mDNSResponder, so no name
#           resolution beyond /etc/hosts. Every child inherits the sandbox.
#   Linux   a network namespace with only loopback up: no route anywhere but
#           lo. Unprivileged (user namespace) where the kernel allows it,
#           else via root or passwordless sudo (Ubuntu 24.04 restricts
#           unprivileged user namespaces); the caller's uid is restored
#           inside, so permission checks behave as unguarded.
#   Linux   as root without namespaces (the CI job container, which has no
#   (root)  CAP_SYS_ADMIN/NET_ADMIN): name resolution only. /etc/resolv.conf
#           points at 127.0.0.1 (nothing listens) for the command and is
#           restored after; the Forge registry host is pinned in /etc/hosts
#           so dependency fetches still work. Direct-IP traffic is not
#           blocked in this mode.
# With no mechanism available it fails (exit 3) unless ASTER_HERMETIC=off,
# which runs the command unguarded and says so.
#
# Env: ASTER_HERMETIC=off disables the guard; ASTER_HERMETIC_ALLOW_HOSTS adds
# space-separated host names kept resolvable in root mode (default:
# forge.emrul.dev).
set -euo pipefail

# Test binaries open many endpoints in parallel. CI job containers start with a
# soft open-file limit of 1024, which coordination_client and enrollment_flood
# exceed there ("Too many open files"). Raise the soft limit to the hard limit,
# capped, for every command this wrapper runs.
hard_nofile=$(ulimit -Hn)
if [[ "$hard_nofile" == unlimited || "$hard_nofile" -gt 65536 ]]; then hard_nofile=65536; fi
ulimit -n "$hard_nofile" 2>/dev/null || true

usage() { sed -n '2,8p' "$0" >&2; exit 2; }
[[ $# -gt 0 ]] || usage

here="$(cd "$(dirname "$0")" && pwd)"

mode() {
  if [[ "${ASTER_HERMETIC:-}" == off ]]; then echo off; return; fi
  case "$(uname -s)" in
    Darwin)
      command -v sandbox-exec >/dev/null && { echo sandbox-exec; return; } ;;
    Linux)
      if command -v unshare >/dev/null && command -v ip >/dev/null &&
         unshare --user --map-root-user --net true 2>/dev/null &&
         unshare --help 2>&1 | grep -q -- '--map-user'; then
        echo netns; return
      fi
      # Ubuntu 24.04 restricts unprivileged user namespaces (AppArmor); a
      # root or passwordless-sudo caller gets a plain network namespace.
      if command -v ip >/dev/null && command -v setpriv >/dev/null; then
        if [[ "$(id -u)" == 0 ]] && unshare --net true 2>/dev/null; then
          echo netns-root; return
        fi
        if [[ "$(id -u)" != 0 ]] && sudo -n unshare --net true 2>/dev/null; then
          echo netns-sudo; return
        fi
      fi
      if [[ "$(id -u)" == 0 && -w /etc/resolv.conf && -w /etc/hosts ]]; then
        echo resolv; return
      fi ;;
  esac
  echo none
}

m="$(mode)"
if [[ "$1" == --check ]]; then echo "$m"; exit 0; fi

case "$m" in
  off)
    echo "hermetic.sh: ASTER_HERMETIC=off, running unguarded" >&2
    exec "$@" ;;
  none)
    echo "hermetic.sh: no network-isolation mechanism here (need sandbox-exec," \
      "unshare + ip, or root); set ASTER_HERMETIC=off to run unguarded" >&2
    exit 3 ;;
  sandbox-exec)
    # The profile cannot allow IPv4-mapped IPv6 (::ffff:127.0.0.1), and the
    # JVM and .NET connect to 127.0.0.1 through dual-stack IPv6 sockets
    # (vstest's testhost link included), so make both use IPv4 sockets.
    export JAVA_TOOL_OPTIONS="${JAVA_TOOL_OPTIONS:+$JAVA_TOOL_OPTIONS }-Djava.net.preferIPv4Stack=true"
    export DOTNET_SYSTEM_NET_DISABLEIPV6=1
    exec sandbox-exec -f "$here/hermetic.sb" "$@" ;;
  netns)
    uid="$(id -u)" gid="$(id -g)"
    # Outer namespace: root there, to bring loopback up. Inner: back to the
    # caller's uid, so permission checks in tests behave as unguarded.
    exec unshare --user --map-root-user --net -- \
      sh -c 'u=$0 g=$1; shift; ip link set lo up &&
        exec unshare --user --map-user="$u" --map-group="$g" -- "$@"' \
      "$uid" "$gid" "$@" ;;
  netns-root|netns-sudo)
    sudo=()
    [[ "$m" == netns-sudo ]] && sudo=(sudo -n --preserve-env)
    # sudo resets PATH (secure_path) and may reset HOME: pass both through.
    exec "${sudo[@]}" unshare --net -- \
      sh -c 'u=$0 g=$1 p=$2 h=$3; shift 3; ip link set lo up &&
        exec setpriv --reuid="$u" --regid="$g" --init-groups -- env PATH="$p" HOME="$h" "$@"' \
      "$(id -u)" "$(id -g)" "$PATH" "$HOME" "$@" ;;
  resolv)
    allow="${ASTER_HERMETIC_ALLOW_HOSTS:-forge.emrul.dev}"
    pins=""
    for h in $allow; do
      # The local registry may not resolve from this container; that's fine.
      ip_addr="$(getent ahostsv4 "$h" 2>/dev/null | awk 'NR==1 {print $1}')" || true
      if [[ -n "$ip_addr" ]]; then pins+="$ip_addr $h"$'\n'; fi
    done
    saved_resolv="$(cat /etc/resolv.conf)" saved_hosts="$(cat /etc/hosts)"
    # Docker bind-mounts both files: rewrite in place, never replace.
    restore() {
      printf '%s\n' "$saved_resolv" > /etc/resolv.conf
      printf '%s\n' "$saved_hosts" > /etc/hosts
    }
    trap restore EXIT
    trap 'exit 130' HUP INT TERM
    printf '%s\n%s' "$saved_hosts" "$pins" > /etc/hosts
    printf 'nameserver 127.0.0.1\noptions timeout:1 attempts:1\n' > /etc/resolv.conf
    status=0
    "$@" || status=$?
    exit "$status" ;;
esac
