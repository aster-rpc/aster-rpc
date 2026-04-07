# Aster — Versioning, Releases, and Package Distribution

**Status:** Active  
**Date:** 2026-04-07  
**Audience:** Engineering team

---

## Version Tracks

Aster has four independent version tracks. Each track can be released independently, but tracks 1-3 have coordination requirements.

### Track 1: Iroh Upstream (pinned, move together)

The iroh crate dependencies. These are pinned in the workspace `Cargo.toml` and should always be bumped together (except where upstream releases are staggered).

```toml
# Cargo.toml [workspace.dependencies]
iroh = "0.97.0"
iroh-blobs = "0.99.0"
iroh-docs = "0.97.0"
iroh-gossip = "0.97.0"
iroh-tickets = "0.4.0"
```

**When to bump:** When upstream iroh releases a new version and we want to adopt it. All iroh deps should move together. This triggers a Track 2 release (core must be rebuilt against new iroh).

**Who decides:** Engineering lead. Iroh upgrades should be deliberate — review upstream changelogs, run full test suite.

### Track 2: Aster Core (move together)

The proprietary Rust engine. These two crates are always released in lockstep.

| Crate | Cargo.toml | Purpose |
|-------|-----------|---------|
| `aster_transport_core` | `core/Cargo.toml` | Core transport logic, contract identity, signing, framing |
| `aster_transport_ffi` | `ffi/Cargo.toml` | C FFI layer over core |

**Version format:** `MAJOR.MINOR.PATCH`

**When to bump:**
- **MAJOR** — breaking changes to the FFI surface or wire protocol
- **MINOR** — new FFI functions, new features, iroh version bumps
- **PATCH** — bug fixes, performance improvements

**Coordination:** A Track 2 release requires all Track 3 bindings to be rebuilt and re-released (they link against core). Track 2 changes without a Track 3 release means the bindings are stale.

### Track 3: Language Bindings (same major.minor, patch can differ)

The language-specific binding packages. All bindings share the same `MAJOR.MINOR` version to signal wire compatibility. Patch versions are per-language.

| Package | Location | Registry |
|---------|----------|----------|
| `aster-rpc` (Python) | `bindings/python/` + `pyproject.toml` | GitHub Packages (PyPI), eventually PyPI |
| `@aster-rpc/transport` (TypeScript) | `bindings/typescript/` | GitHub Packages (npm), eventually npm |
| `aster-java` (future) | `bindings/java/` | GitHub Packages (Maven) |
| `aster-dotnet` (future) | `bindings/dotnet/` | GitHub Packages (NuGet) |

**Version format:** `MAJOR.MINOR.PATCH[-BUILD]`

- `MAJOR.MINOR` — **must be the same across all languages.** This signals that any `0.3.x` Python client can talk to any `0.3.x` TypeScript service.
- `PATCH` — per-language. Python might be at `0.3.2` while TypeScript is at `0.3.0` because Python had two binding-specific fixes.
- `BUILD` — CI build number for pre-release/internal artifacts: `0.3.0-build.47`. Used for GitHub Packages internal distribution. Never used for public releases.

**When to bump:**
- **MAJOR** — breaking wire protocol changes (all languages must bump together)
- **MINOR** — new features, new API surface, Track 2 core changes (all languages should bump together, even if some bindings haven't changed, to keep major.minor aligned)
- **PATCH** — per-language bug fixes, binding improvements, doc fixes

### Track 4: CLI (independent)

The `aster-cli` package. Released independently — it's a consumer of the Python binding, not part of the binding itself.

| Package | Location | Registry |
|---------|----------|----------|
| `aster-cli` | `cli/` | GitHub Packages (PyPI), eventually PyPI |

**Version format:** `MAJOR.MINOR.PATCH`

**When to bump:** Whenever CLI features or fixes warrant it. The CLI declares a dependency on `aster-rpc >= X.Y.0` (compatible range), so it doesn't need to release in lockstep with Track 3.

**Coordination:** CLI should pin to a minimum Track 3 version in its `pyproject.toml`:

```toml
[project]
dependencies = ["aster-rpc>=0.3.0"]
```

---

## Version File Locations (Source of Truth)

Each version is defined in exactly one place. All other references derive from it.

| Track | Source of truth | Location |
|-------|----------------|----------|
| Track 1 (Iroh) | `[workspace.dependencies]` | `Cargo.toml` (root) |
| Track 2 (Core) | `[package] version` | `core/Cargo.toml` and `ffi/Cargo.toml` |
| Track 3 (Python) | `[project] version` | `pyproject.toml` (root) |
| Track 3 (TypeScript) | `"version"` | `bindings/typescript/packages/transport/package.json` |
| Track 4 (CLI) | `[project] version` | `cli/pyproject.toml` |

**`bindings/python/aster/__init__.py`** should read the version from package metadata at runtime, not hardcode it:

```python
from importlib.metadata import version as _pkg_version
__version__ = _pkg_version("aster-rpc")
```

---

## Build Numbering

CI generates build numbers for internal (pre-release) artifacts. The format is:

```
{MAJOR}.{MINOR}.{PATCH}-build.{CI_RUN_NUMBER}
```

Examples:
- `0.3.0-build.47` — 47th CI build of the 0.3.0 development cycle
- `0.3.1-build.103` — 103rd CI build after the 0.3.1 patch bump

**For Python (PEP 440):** Pre-release versions use `.devN` format:
```
0.3.0.dev47
```

**For npm (semver):** Pre-release versions use `-build.N` format:
```
0.3.0-build.47
```

**For Rust (Cargo):** Pre-release versions use `-build.N` format:
```
0.3.0-build.47
```

Build numbers are only used for GitHub Packages (internal distribution). Public releases on PyPI/npm use clean version numbers.

---

## GitHub Packages Setup

### Why GitHub Packages

- **Private distribution** — artifacts are only accessible to authenticated users with repo access
- **No public registry pollution** — we're not ready for PyPI/npm
- **Works with standard tooling** — `pip install`, `npm install`, Maven, NuGet all support GitHub Packages as an alternate registry
- **Free for private repos** (within GitHub plan storage/bandwidth limits)

### Python (PyPI on GitHub Packages)

**Publishing (CI):**

```yaml
# .github/workflows/build.yml
- name: Publish to GitHub Packages
  env:
    TWINE_USERNAME: __token__
    TWINE_PASSWORD: ${{ secrets.GITHUB_TOKEN }}
  run: |
    twine upload --repository-url https://ghp.pkg.github.com/emrul/iroh-python \
      dist/*.whl dist/*.tar.gz
```

**Consuming (other repos):**

```bash
# Install from GitHub Packages
pip install aster-rpc \
  --index-url https://ghp.pkg.github.com/emrul/iroh-python/simple/ \
  --extra-index-url https://pypi.org/simple/
```

Or in `pyproject.toml` / `requirements.txt`:

```toml
# pyproject.toml of consuming repo
[tool.uv]
index-url = "https://ghp.pkg.github.com/emrul/iroh-python/simple/"
extra-index-url = ["https://pypi.org/simple/"]
```

**Authentication:** Consumers need a GitHub PAT with `read:packages` scope, configured via:

```bash
# ~/.netrc or pip config
pip config set global.index-url https://__token__:ghp_YOURTOKEN@ghp.pkg.github.com/emrul/iroh-python/simple/
```

### TypeScript (npm on GitHub Packages)

**Publishing (CI):**

```yaml
- name: Publish to GitHub Packages
  env:
    NODE_AUTH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  run: |
    echo "//npm.pkg.github.com/:_authToken=${NODE_AUTH_TOKEN}" >> .npmrc
    npm publish --registry=https://npm.pkg.github.com
```

**Consuming (other repos):**

```
# .npmrc in consuming repo
@aster-rpc:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```

```bash
npm install @aster-rpc/transport
```

### Rust (git dependency — no registry needed)

GitHub Packages doesn't support Cargo registries. For private Rust consumers, use git dependencies:

```toml
# Cargo.toml of consuming repo
[dependencies]
aster_transport_core = { git = "https://github.com/emrul/iroh-python", tag = "core-v0.3.0" }
```

Authenticate via git credential helper or `CARGO_NET_GIT_FETCH_WITH_CLI=true` + SSH key.

---

## Release Process

### Internal release (GitHub Packages)

Happens on every push to `main` (or on manual trigger):

1. CI runs tests
2. CI builds artifacts (wheels, npm packages)
3. CI stamps version with build number (e.g., `0.3.0.dev47`)
4. CI publishes to GitHub Packages
5. Other private repos can immediately consume the new build

### Public release (PyPI / npm — future)

Happens when we tag a release (e.g., `v0.3.0`):

1. Engineer bumps version in source-of-truth files (remove `-build.N` / `.devN`)
2. Engineer creates a git tag: `git tag v0.3.0`
3. CI runs full test suite
4. CI builds artifacts with clean version number
5. CI publishes to public registries (PyPI, npm, etc.)
6. CI also publishes to GitHub Packages (for consistency)

### Tag naming convention

| What | Tag format | Example |
|------|-----------|---------|
| Python binding release | `v{VERSION}` | `v0.3.0` |
| TypeScript binding release | `ts-v{VERSION}` | `ts-v0.3.0` |
| Core release | `core-v{VERSION}` | `core-v0.3.0` |
| CLI release | `cli-v{VERSION}` | `cli-v0.3.0` |
| Iroh upgrade | `iroh-{VERSION}` | `iroh-0.98.0` |

---

## Coordination Matrix

When a track is bumped, what else needs to happen:

| When this changes... | ...these must also be updated |
|---------------------|-------------------------------|
| Track 1 (Iroh upstream) | Track 2 (core rebuild), Track 3 (all bindings rebuild) |
| Track 2 (Core) | Track 3 (all bindings rebuild — they link against core) |
| Track 3 Python (binding) | Nothing else required |
| Track 3 TypeScript (binding) | Nothing else required |
| Track 4 (CLI) | Nothing else required (unless it needs a newer Track 3) |
| Wire protocol change | MAJOR bump on Tracks 2 + 3 (all languages) |
| Contract format change | MINOR bump on Tracks 2 + 3 (all languages) |

---

## Current State (as of 2026-04-07)

| Track | Current version | Target first release |
|-------|----------------|---------------------|
| Track 1 (Iroh) | 0.97.0 / 0.99.0 (blobs) | Pin to latest stable when we release |
| Track 2 (Core) | 0.1.0 | 0.1.0 |
| Track 3 (Python) | 0.1.0 (pyproject) / 0.2.0 (__init__.py — stale) | 0.1.0 |
| Track 3 (TypeScript) | 0.1.0-alpha.0 | 0.1.0 |
| Track 4 (CLI) | 0.1.0 | 0.1.0 |

**Action items:**
- [ ] Fix `__init__.py` version to use `importlib.metadata` instead of hardcoded string
- [ ] Set up GitHub Packages publishing in CI
- [ ] Add build number stamping to CI
- [ ] Create version bump script (bumps all files in a track)

---

## References

- [PEP 440 — Version Identification](https://peps.python.org/pep-0440/) — Python version format spec
- [Semantic Versioning 2.0.0](https://semver.org/) — semver spec (used by npm, Cargo)
- [GitHub Packages documentation](https://docs.github.com/en/packages)
