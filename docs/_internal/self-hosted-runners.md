# Self-Hosted GitHub Actions Runners

**Last updated:** 2026-04-06

## Overview

CI tests run on self-hosted runners in Emrul's home lab + OCI. Wheel builds for PyPI remain on GitHub-hosted runners (manylinux reproducibility).

Runners are registered at: **Settings > Actions > Runners** on `github.com/emrul/iroh-python`

---

## Runners

### aster-linux-x64

| | |
|---|---|
| **Machine** | mlserver-ubuntu (home lab) |
| **IP** | `192.168.1.140` |
| **SSH** | `ssh emrul@192.168.1.140` |
| **OS** | Ubuntu 24.04, kernel 6.17.0, x86_64 |
| **Runner path** | `/home/emrul/aster-runner/` |
| **Work dir** | `/home/emrul/aster-runner/_work/` |
| **Service** | systemd: `actions.runner.emrul-iroh-python.aster-linux-x64` |
| **Labels** | `self-hosted, Linux, X64` |
| **Rust** | 1.94.1 (`~/.cargo/bin/`) |
| **Python** | 3.13.12 (deadsnakes PPA) |
| **uv** | 0.11.3 (`~/.local/bin/`) |
| **sccache** | installed (`~/.cargo/bin/`) |

**Note:** v4l2loopback-dkms was removed on 2026-04-06 (was causing half-configured kernel packages). All kernel packages now clean. Disk at 89% — consider cleanup if it grows.

### aster-linux-arm64

| | |
|---|---|
| **Machine** | emrul-002 (OCI cloud instance) |
| **IP** | `132.145.22.10` |
| **SSH** | `ssh ubuntu@132.145.22.10` |
| **OS** | Ubuntu 24.04, kernel 6.17.0-oracle, aarch64 |
| **Runner path** | `/home/ubuntu/aster-runner/` |
| **Work dir** | `/home/ubuntu/aster-runner/_work/` |
| **Service** | systemd: `actions.runner.emrul-iroh-python.aster-linux-arm64` |
| **Labels** | `self-hosted, Linux, ARM64` |
| **Rust** | 1.94.1 (`~/.cargo/bin/`) |
| **Python** | 3.13.12 (deadsnakes PPA) |
| **uv** | 0.11.3 (`~/.local/bin/`) |
| **sccache** | installed (`~/.cargo/bin/`) |

### aster-macos-arm64

| | |
|---|---|
| **Machine** | Local Mac (Emrul's dev machine) |
| **IP** | localhost |
| **OS** | Darwin 24.5.0, Apple Silicon |
| **Runner path** | `/Users/emrul/dev/aster/local_mac_runner/` |
| **Work dir** | `/Users/emrul/dev/aster/local_mac_runner/_work/` |
| **Service** | launchd: `actions.runner.emrul-iroh-python.aster-macos-arm64` |
| **Plist** | `~/Library/LaunchAgents/actions.runner.emrul-iroh-python.aster-macos-arm64.plist` |
| **Labels** | `self-hosted, macOS, ARM64` |
| **Rust** | 1.94.1 (system rustup) |
| **Python** | 3.13.5 (pyenv, shims in runner `.path`) |
| **uv** | system (`~/.local/bin/`) |

### aster-windows-x64

| | |
|---|---|
| **Machine** | mav-win-001 (home lab) |
| **IP** | `192.168.1.75` |
| **SSH** | `ssh emrul@192.168.1.75` (lands in WSL2; use `powershell.exe` to reach Windows) |
| **OS** | Windows 11 Pro for Workstations, AMD64 |
| **Runner path** | `C:\actions-runner\` |
| **Work dir** | `C:\actions-runner\_work\` |
| **Service** | Windows Service: `actions.runner.emrul-iroh-python.aster-windows-x64` |
| **Labels** | `self-hosted, Windows, X64` |
| **Rust** | 1.94.1 (`%USERPROFILE%\.cargo\bin\`) |
| **Python** | 3.13.12 (`%LOCALAPPDATA%\Programs\Python\Python313\`) |
| **uv** | 0.11.3 (`%USERPROFILE%\.local\bin\`) |

**Note:** SSH to this machine drops into WSL2 (Ubuntu on WSL). To run Windows commands remotely: `powershell.exe -Command "..."` or write a `.ps1` to `/mnt/c/Users/emrul/` and run `powershell.exe -File C:\Users\emrul\script.ps1`.

---

## Common Operations

### Check runner status
```bash
gh api repos/emrul/iroh-python/actions/runners \
  --jq '.runners[] | "\(.name) — \(.status)"'
```

### Restart a Linux runner
```bash
ssh emrul@192.168.1.140 "sudo systemctl restart actions.runner.emrul-iroh-python.aster-linux-x64"
```

### Restart the Mac runner
```bash
cd /Users/emrul/dev/aster/local_mac_runner
./svc.sh stop && ./svc.sh start
```

### Restart the Windows runner
```bash
ssh emrul@192.168.1.75 "powershell.exe -Command 'Restart-Service actions.runner.emrul-iroh-python.aster-windows-x64'"
```

### Re-register a runner (token expired)
```bash
# Generate a new token
TOKEN=$(gh api -X POST repos/emrul/iroh-python/actions/runners/registration-token --jq '.token')

# On the runner machine:
cd ~/aster-runner  # or C:\actions-runner on Windows
./config.sh remove --token "$TOKEN"
./config.sh --unattended --url https://github.com/emrul/iroh-python --token "$TOKEN" \
  --name <runner-name> --labels "self-hosted,<OS>,<Arch>" --work _work --replace
```

### Update Rust toolchain on all runners
```bash
# Update the pinned version in .github/workflows/ci.yml (RUST_TOOLCHAIN env var)
# Then on each runner:
rustup default <new-version>
rustup component add rustfmt clippy
```

---

## Architecture

```
ci.yml (tests)          → self-hosted runners (all 4 machines)
build.yml (wheels)      → GitHub-hosted runners (manylinux/macOS/Windows)
mobile.yml (iOS/Android)→ self-hosted Mac (iOS) + self-hosted Linux ARM64 (Android)
```

Wheel builds stay on GitHub-hosted for manylinux container reproducibility and PyPI trusted publishing. Tests run on self-hosted for speed, cost savings, and reliable networking.

---

## Docker Runner Image

A Docker-based runner setup exists at `.github/runner/` (Dockerfile, docker-compose.yml, entrypoint.sh) but is not currently in use. The runners were set up as native installs for simplicity. The Docker image is available if containerization is needed later.
