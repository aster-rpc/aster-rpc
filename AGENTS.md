# AGENTS.md

This file provides guidance to coding agents working in this repository.

## Overview

Aster is a multi-language peer-to-peer SDK built on Iroh, Apache Fory, and
Salvo. `core` is the shared Rust engine; `aster` is the first-class Rust facade;
Python, TypeScript, C, Java, and Kotlin surfaces wrap the same engine. Python is
one binding, not the architectural top layer.

Rust distribution and native-stack policy are canonical in:

- `docs/rust-sdk-consumer-guide.md` — how downstream Rust projects depend on
  Aster and use the Iroh/Fory/Salvo escape hatch.
- `docs/_internal/rust-sdk-maintainer-guide.md` — fork, registry, source-crate,
  testing, and release workflow.

`iroh-fork-manifest.toml` is a maintainer provenance/BOM file. Do not tell an
ordinary consumer to copy its pin blocks once the public Cargo registry is
live.

## Commands

### Setup

```bash
uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout pytest-rerunfailures
uv run maturin develop -m bindings/python/rust/Cargo.toml
uv pip install -e cli/
```

### Build (required after any Rust change)

```bash
./scripts/build.sh
```

This runs `maturin develop` and regenerates `bindings/python/aster/_aster.pyi` (native module type stubs for IDE support).

### Get prebuilt Aster binaries (preferred on developer Macs)

Forgejo CI builds Linux x86-64, Windows x86-64, and Apple Silicon macOS Python wheels. Before compiling Rust locally, download a released build from the Forge box:

```bash
# Latest release, or replace `latest` with a version such as 0.3.0 / v0.3.0.
./scripts/download-release.sh latest /tmp/aster-release

# Inspect and install the wheel appropriate to the current target/Python.
ls /tmp/aster-release
# On this Apple Silicon Mac:
uv pip install /tmp/aster-release/aster_rpc-*macosx*arm64.whl
python -c 'import aster; print(aster.VERSION)'
```

The release contract requires artifacts and source crates to be publicly
readable without a consumer token; check the consumer guide's first-release
gate before claiming a version is live. Tagged binary assets are public
organization packages at
<https://forge.emrul.dev/Aster/-/packages>; the private repository's Releases
page is only the maintainer record. Release packages contain Linux, Windows,
and Apple Silicon macOS abi3 wheels, a source distribution, and `SHA256SUMS`.
Use
`*manylinux*x86_64.whl` on Linux and `*win_amd64.whl` on Windows; wheels are
platform-specific.

For an unreleased `main` commit, use the artifacts from its Forgejo Actions
run. The same builds are retained temporarily as generic packages under
`Aster/aster-python/dev-<run-number>` and can be fetched with, for example,
`./scripts/download-release.sh dev-20 /tmp/aster-dev-20`. Prefer either source
over running a redundant Linux/Windows compilation on a developer Mac.

### Versioning and releases

Aster versions are derived as `<major>.<minor>.<commit-count - offset>`. `VERSION` stores the major/minor base and its commit-count offset, so the patch number increases once per commit and resets to zero on a deliberate major/minor bump.

```bash
./scripts/release/version.sh          # exact version for HEAD
./scripts/release.sh                  # tag HEAD and trigger its Forgejo release
./scripts/release/bump-version.sh 0.4 # future bump; creates the 0.4.0 commit
```

Only deliberate releases are tagged (`v0.3.0`, `v0.3.1`, ...). Never hand-edit the patch component or create a tag that differs from `scripts/release/version.sh`. Committed Cargo manifests retain the series baseline (for example `0.3.0`) because committing a derived patch would itself increment that patch. `scripts/build.sh` and Forgejo CI temporarily stamp all eight Aster library/binding Cargo packages, their versioned path dependencies, `Cargo.lock`, and Python metadata to the exact derived build version. Non-publishable examples and fuzz fixtures keep their own independent versions. The exact Aster version is also embedded as `aster_transport_core::VERSION`, re-exported as `aster::VERSION`, exposed to Python as `aster.VERSION` / `aster.__version__`, exposed to TypeScript by `version()`, and available from C as `aster_version()`.

### Run tests

```bash
uv run pytest tests/python/ -v --timeout=30
# Single test file:
uv run pytest tests/python/test_blobs.py -v --timeout=30
# Single test:
uv run pytest tests/python/test_blobs.py::test_add_and_get -v --timeout=30
```

### Lint / format

```bash
cargo fmt --manifest-path bindings/python/rust/Cargo.toml
cargo clippy --manifest-path bindings/python/rust/Cargo.toml -- -D warnings
```

### Full validation (mirrors CI)

```bash
./scripts/validate.sh
```

## Architecture

The stack has four layers:

```
Rust facade (`aster`) + language APIs (Python/TS/C/Java/Kotlin)
    ↓ thin wrappers
core/src/lib.rs — authoritative shared transport/business logic
    ↓ async Rust (Tokio)
Iroh protocols + Fory codec + optional Salvo HTTP/WebTransport
```

**Rule**: business logic belongs in `core`, not in the PyO3 layer. The PyO3 wrappers in `bindings/python/rust/src/` should be thin.

### Key source locations

| Path | Purpose |
| --- | --- |
| `aster/src/` | Supported Rust facade, RPC framework, and `aster::native` escape hatch |
| `core/src/lib.rs` | Shared transport logic; CoreNode, CoreNetClient, CoreBlobsClient, CoreDocsClient, CoreGossipClient |
| `aster-expose/`, `aster-transport-salvo/` | HTTP relay/edge and Salvo HTTP/H3/WebTransport transport |
| `bindings/python/rust/src/` | PyO3 wrappers: `lib.rs` (module init + tokio runtime), `node.rs`, `net.rs`, `blobs.rs`, `docs.rs`, `gossip.rs`, `hooks.rs`, `monitor.rs`, `error.rs` |
| `bindings/python/python/` | Python package: `__init__.py` (public API), `aster/` (RPC framework) |
| `bindings/python/python/aster/` | Aster RPC: `client.py`, `server.py`, `codec.py`, `decorators.py`, `transport/`, `interceptors/` |
| `bindings/java/` | Java binding (future) |
| `ffi/` | C FFI layer over `core` (for non-Python interop) |
| `cli/` | CLI tool (Python now, Go eventually) |
| `tests/python/` | All tests; `conftest.py` provides `node`, `node_pair`, `endpoint_pair` fixtures |

### Key documentation locations

Start with `docs/rust-sdk-consumer-guide.md`,
`docs/_internal/rust-sdk-maintainer-guide.md`, and the feature-specific guides
under `docs/`. Protocol and binding specifications live under `ffi_spec/` and
`docs/_internal/`.

### Async model

The PyO3 module init (`lib.rs`) starts a single shared Tokio runtime. All async Rust functions are bridged to Python awaitables via `pyo3-async-runtimes`. Python tests use `asyncio_mode = "auto"` (pytest-asyncio).

### Cargo workspace

`Cargo.toml` at the root is a workspace. Always pass `-m bindings/python/rust/Cargo.toml` to maturin, or use the workspace root for `cargo fmt`/`cargo clippy` across all crates.

## CI

- `.forgejo/workflows/linux.yml` is the active CI/release workflow. It validates
  Rust and Python, builds target-specific artifacts, and publishes tagged
  Forgejo releases. GitHub workflows are not the release source of truth.

## Semantic tooling (`sem`)

`sem` is an entity-level (function/struct/impl/trait/method) view over Git, built on tree-sitter — it covers both the Rust crates and the Phase-5 Bash scripts here. Prefer it over line-diff/grep when the question is "what *entity* changed" or "what depends on this". Check `sem --help` for the installed version's flags; all commands below take `--json` for machine output.

- `sem impact <name>` — direct deps, direct dependents, transitive impact, and affected **tests** for an entity. The highest-value command in this multi-crate workspace: it resolves cross-crate edges (e.g. a `portal-store` fn used by `portal-sync-session` tests) so you know exactly which crates to retest. Add `--file <path>` or `--entity-id <id>` to disambiguate a name with &gt;1 match; `--tests` for just the test impact; `--depth 0` for unlimited traversal.
- `sem diff [refs/paths]` — entity-level diff (added/modified/deleted/renamed), separating cosmetic (whitespace/comment) from structural changes. Good for reviewing a commit/branch before a PR. `--staged`, `--from <a> --to <b>`, `--commit <sha>`, `--no-cosmetics`, `--format json`. Note: untracked files are excluded (matches git), so newly-added scripts won't show until staged.
- `sem graph [path]` — whole-graph entity dependency view (`--file-exts .rs`).
- `sem blame <file>` — last-modifier per entity, not per line.
