# Dev Build Publishing + Runner Cleanup Plan

**Date:** 2026-04-14

## Context

Dev builds (Python wheels, npm packages) run on self-hosted runners on every push to main but the artifacts go nowhere — they're only available as ephemeral GitHub Actions artifacts. Internal developers have no easy way to install the latest dev build. Additionally, the self-hosted runners (macOS, Windows/WSL2, Linux x64 dockerized, Linux ARM64) never clean up, accumulating 10s of GB in `target/`, Docker layers, and package caches.

---

## Part 1: Dev Build Publishing

### 1A. Python — GitHub Releases (keep last 2)

**Modify:** `.github/workflows/build.yml`

Add a `dev-release` job after the build jobs, gated on `is_release == 'false'`:
- Downloads all `wheels-*` artifacts
- Creates a GitHub Release with tag `dev-<run_number>` (e.g. `dev-142`)
- After publishing, uses the GitHub API to delete all dev releases except the 2 most recent
- Needs `permissions: contents: write`

**Retention cleanup step** (runs after creating the new release):
```bash
# List all releases with dev- prefix, sorted newest first, skip first 2, delete the rest
gh api repos/{owner}/{repo}/releases --paginate --jq '
  [.[] | select(.tag_name | startswith("dev-"))] | sort_by(.created_at) | reverse | .[2:][] |
  {id: .id, tag: .tag_name}' |
while read -r line; do
  ID=$(echo "$line" | jq -r '.id')
  TAG=$(echo "$line" | jq -r '.tag')
  gh api -X DELETE "repos/{owner}/{repo}/releases/$ID"
  gh api -X DELETE "repos/{owner}/{repo}/git/refs/tags/$TAG" 2>/dev/null || true
done
```

Since self-hosted runners don't have `gh`, this uses `curl` with `GITHUB_TOKEN` or a small inline script with the `actions/github-script` action instead.

**Consumer install:**
```bash
# Latest dev build (private repo — download via gh CLI)
gh release download --repo aster-rpc/aster-rpc-internal --pattern '*.whl' --dir /tmp/wheels
pip install /tmp/wheels/aster_rpc-*.whl

# Or specific build number
gh release download dev-142 --repo aster-rpc/aster-rpc-internal --pattern '*.whl' --dir /tmp/wheels
```

### 1B. TypeScript/npm — GitHub Packages

**Modify:** `.github/workflows/build-typescript.yml`

Add a `publish-dev` job after all build jobs, gated on `is_release == 'false'`:
- Uses `setup-node` with `registry-url: 'https://npm.pkg.github.com'`
- Injects `publishConfig.registry` into each package.json artifact
- Publishes all 7 packages with `--tag dev`
- Auth: `NODE_AUTH_TOKEN: ${{ secrets.GITHUB_TOKEN }}`

**Retention:** npm versions are immutable once published — no cleanup needed. The `dev` dist-tag always points to the latest. Old dev versions just sit there but don't cost anything meaningful. GitHub Packages has no storage limits for private repos within the org plan.

**Note:** The `@aster-rpc` npm scope matches the `aster-rpc` GitHub org. The private repo lives at `aster-rpc/aster-rpc-internal`.

**Consumer install:**
```bash
# .npmrc (one-time)
@aster-rpc:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}

# Install latest dev
npm install @aster-rpc/aster@dev
```

### 1C. Java — GitHub Packages Maven

**Modify:** `bindings/java/pom.xml` — add `<distributionManagement>` block pointing to `https://maven.pkg.github.com/aster-rpc/aster-rpc-internal`

**Create:** `.github/workflows/build-java.yml`
- Trigger: push to main + `v*` tags, paths filter on `bindings/java/**`, `ffi/**`, `core/**`
- Jobs: `version` -> `build` (mvn package) -> `publish-dev` (mvn deploy, gated on not-release)
- Auth: auto-generated `GITHUB_TOKEN` via maven `settings.xml` server config
- Java builds don't need the native FFI lib yet (the runtime module uses JNI/Panama at a later stage), so just `mvn package` the annotation/codegen/runtime modules

**Retention:** Maven SNAPSHOT versions are overwritten on each deploy (Maven adds a timestamp internally). Only one SNAPSHOT ever exists per version string. No cleanup needed.

**Consumer install:**
```xml
<!-- ~/.m2/settings.xml -->
<servers><server>
  <id>github-aster</id>
  <username>USERNAME</username>
  <password>GITHUB_PAT</password>  <!-- needs read:packages scope -->
</server></servers>

<!-- pom.xml -->
<repositories><repository>
  <id>github-aster</id>
  <url>https://maven.pkg.github.com/aster-rpc/aster-rpc-internal</url>
</repository></repositories>
```

### 1D. .NET — GitHub Packages NuGet

**Modify:** `bindings/dotnet/src/Aster/Aster.csproj` — add NuGet package metadata, fix hardcoded native lib path

**Create:** `.github/workflows/build-dotnet.yml`
- Trigger: push to main + `v*` tags, paths on `bindings/dotnet/**`, `ffi/**`, `core/**`
- Jobs: `version` -> `build-native` (cargo build on each platform) -> `pack` (dotnet pack with multi-platform runtimes layout) -> `publish-dev` (dotnet nuget push to `https://nuget.pkg.github.com/aster-rpc/index.json`)
- Dev versions use `0.1.0-dev.N` (NuGet prerelease format)

**Retention:** NuGet versions are immutable. Old prerelease versions can be unlisted via the GitHub Packages API. Optionally add a cleanup step to unlist all but the 2 most recent `dev.*` versions, but this is low priority since NuGet packages are small (no native code in the package itself until multi-platform packaging is done).

**Consumer install:**
```bash
dotnet nuget add source "https://nuget.pkg.github.com/aster-rpc/index.json" \
  --name github-aster --username USER --password GITHUB_PAT
dotnet add package Aster --prerelease
```

### 1E. Go — No registry needed

Go consumers use `GOPRIVATE=github.com/aster-rpc/*` and `go get` directly from the repo. No publishing workflow needed. The `bindings/go/go.mod` module path (`aster-ffi`) will need updating to `github.com/aster-rpc/aster-rpc-internal/bindings/go` when Go consumers are onboarded, but that's a separate change.

---

## Part 2: Runner Cleanup

### Strategy: Scheduled weekly workflow + lightweight post-job steps

**Create:** `.github/workflows/cleanup-runners.yml`
- Schedule: `cron: '0 3 * * 0'` (Sunday 3 AM UTC) + `workflow_dispatch`
- One job per runner type (Linux x64, Linux ARM64, macOS ARM64, Windows x64)

### Self-hosted runner inventory

| Runner | Location | Access |
|--------|----------|--------|
| macOS ARM64 | `/Users/emrul/dev/aster/local_mac_runner` | Local machine |
| Windows x64 | `ssh emrul@192.168.1.75` | WSL2 |
| Linux x64 | `ssh emrul@192.168.1.140` | Dockerized |
| Linux ARM64 | `ssh ubuntu@132.145.22.10` | Remote |

### What to clean (safe, big wins)
- `target/debug/` — not used in CI (all builds are `--release` or dev-mode maturin)
- `target/release/incremental/` — incremental compilation cache, rebuilt anyway
- `target/release/build/` — build script outputs
- `target/release/.fingerprint/` — change detection cache
- `dist/` — wheel/artifact output directories
- Docker: `docker system prune -f --filter "until=168h"` (Linux only)
- `~/.cache/pip/`, stale npm/bun caches

### What to preserve (for build speed)
- `~/.cargo/registry/` and `~/.cargo/git/` — crate downloads + iroh fork checkouts
- `target/release/deps/` — compiled dependency artifacts (the main incremental benefit)
- `~/.rustup/` — toolchain itself
- sccache directories

### Post-job steps
Add a minimal `rm -rf dist/` cleanup as an `if: always()` step to the build workflows to prevent inter-run accumulation. The heavy cleanup stays in the scheduled workflow.

### Per-runner cleanup commands

**Linux (both x64 and ARM64):**
```bash
cd "$GITHUB_WORKSPACE" 2>/dev/null || cd ~/actions-runner/_work/*/*
rm -rf target/debug target/release/incremental target/release/build target/release/.fingerprint dist
docker system prune -f --filter "until=168h" 2>/dev/null || true
rm -rf ~/.cache/pip 2>/dev/null || true
```

**macOS:**
```bash
cd /Users/emrul/dev/aster/local_mac_runner/_work/*/* 2>/dev/null || true
rm -rf target/debug target/release/incremental target/release/build target/release/.fingerprint dist
rm -rf ~/Library/Caches/pip 2>/dev/null || true
```

**Windows:**
```powershell
$workDir = Get-ChildItem "C:\actions-runner\_work" -Directory | Select-Object -First 1
if ($workDir) { Set-Location $workDir.FullName }
Remove-Item -Recurse -Force target\debug, target\release\incremental, target\release\build, target\release\.fingerprint, dist -ErrorAction SilentlyContinue
```

---

## Implementation Order

1. **`cleanup-runners.yml`** — immediate disk relief, no risk to builds
2. **`build.yml` dev-release job** — Python wheels to GitHub Releases with retention (keep 2)
3. **`build-typescript.yml` publish-dev job** — npm to GitHub Packages
4. **`build-java.yml` + pom.xml changes** — Java Maven to GitHub Packages
5. **`build-dotnet.yml` + csproj changes** — .NET NuGet to GitHub Packages
6. Post-job cleanup steps in build workflows

## Files to create
- `.github/workflows/cleanup-runners.yml`
- `.github/workflows/build-java.yml`
- `.github/workflows/build-dotnet.yml`

## Files to modify
- `.github/workflows/build.yml` — add `dev-release` job + retention cleanup
- `.github/workflows/build-typescript.yml` — add `publish-dev` job
- `bindings/java/pom.xml` — add `<distributionManagement>`
- `bindings/dotnet/src/Aster/Aster.csproj` — add NuGet metadata, fix native lib path

## Verification
- Push to main -> check GitHub Releases for `dev-N` release with Python wheels
- Push 3 times -> confirm only 2 dev releases remain (oldest auto-deleted)
- Push to main -> check GitHub Packages for npm `@aster-rpc/aster@dev`
- Push to main -> check GitHub Packages for Maven `site.aster:aster-*:0.1.0-SNAPSHOT`
- Manual trigger `cleanup-runners.yml` -> verify disk space freed on each runner
- Confirm existing release publishing (PyPI, npm) still works on `v*` tags
