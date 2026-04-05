# CI/CD Overview

This repository uses **GitHub Actions** with two workflows:

- **`.github/workflows/ci.yml`**
  - Runs on pushes to `main`, pull requests, and manual dispatch.
  - Focused on quality + integration checks:
    - Rust formatting/lint (`cargo fmt`, `cargo clippy`)
    - Build + Python test matrix (`maturin develop` + `pytest`) across Linux/macOS/Windows.

- **`.github/workflows/build.yml`**
  - Runs on pull requests, manual dispatch, and version tags (`v*`).
  - Builds distributables:
    - Wheels for Linux/macOS/Windows
    - Source distribution (`sdist`)
  - Publishes to PyPI on tag pushes.

## Build architecture (what is being built)

Rust workspace crates:

- `aster_transport_core` (core transport)
- `aster_transport_ffi` (FFI layer)
- `aster_rs` (PyO3 Python extension)

Python packaging uses `maturin` (`pyproject.toml`), so wheel jobs compile Rust and produce Python artifacts.

## Caching in use

We now use two complementary cache layers:

1. **Cargo/build cache** via `Swatinem/rust-cache@v2`
   - Restores/saves Cargo registry/git + `target` build outputs.
   - Keys are split by job/target (for example `linux-${{ matrix.target }}`) to avoid cross-target cache pollution.

2. **Compiler artifact cache** via `sccache`
   - Enabled globally in workflows with:
     - `RUSTC_WRAPPER=sccache`
     - `SCCACHE_GHA_ENABLED=true`
   - Installed with `mozilla-actions/sccache-action`.

`setup-uv` cache is also used in CI test jobs for Python dependency setup.

## How to validate cache behavior in logs

Each Rust job includes cache diagnostics before and after build/lint.

Check for:

- `Cache Rust dependencies/build artifacts` step:
  - Shows cache restore/save behavior for `rust-cache`.
- `Cache diagnostics (pre-...)` step:
  - Prints `RUSTC_WRAPPER`, `SCCACHE_GHA_ENABLED`, `sccache --version`, `rustc -Vv`.
- `Cache diagnostics (post-...)` step:
  - Prints `sccache --show-stats` with hit/miss counters.

On a warm cache (subsequent runs), you should see increasing sccache hits and faster compile phases.

## Quick troubleshooting

- **No sccache hits**: verify `RUSTC_WRAPPER=sccache` appears in diagnostics.
- **Unexpected cache misses**: expected after `Cargo.lock` changes, target changes, or major rustc/toolchain updates.
- **Different platforms/targets**: caches are intentionally segmented by OS/target.

## Typical change flow

1. Open a PR -> CI runs lint + matrix tests.
2. Build workflow runs wheel/sdist builds for validation.
3. Push tag like `v0.1.0` -> publish job uploads built artifacts to PyPI.
