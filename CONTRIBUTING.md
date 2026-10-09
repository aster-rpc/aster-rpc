# Contributing to Aster

Thanks for your interest in contributing to Aster.

## Getting started

These commands require the complete development workspace, including `ffi/`
and all binding directories. The public mirror currently omits some of those
sources. If you cannot reproduce a problem from the published packages, open
an issue with the details below so the maintainers can help with a source build.

```bash
uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout pytest-rerunfailures
./scripts/build.sh
uv pip install -e cli/
uv run pytest tests/python/ -v --timeout=30
```

See the [README](README.md#working-on-the-sdk) for the workspace layout and
[the Rust guide](docs/aster-rust-getstarted.md) for the public API.

## Submitting changes

1. In a complete development checkout, create a branch from `main`.
2. Make your changes. Add tests for new functionality.
3. Run `./scripts/install-git-hooks.sh` once to enable local hooks.
4. Run `./scripts/validate.sh` to check formatting, linting, and tests.
5. Open a pull request against `main`. Include the commands and results used
   to validate it, and note any checks you could not run with the public mirror.

## Code style

- **Python:** No linter enforced yet. Follow existing patterns.
  Use `Optional[T]` not `T | None` in `@wire_type` dataclasses (pyfory
  requirement). No Unicode em dashes in source (the pre-commit hook
  checks this).
- **Rust:** `cargo fmt --all` and `cargo clippy -D warnings`. The
  pre-commit hook rejects commits when `cargo fmt --all -- --check`
  fails.
- **TypeScript:** Follow existing patterns. camelCase for wire protocol
  field names.

## Contributor License Agreement

By submitting a pull request, you agree to the [CLA](CLA.md). You do
not need to sign a separate document.

## Reporting issues

Open an issue on GitHub. Include:
- What you expected
- What happened
- Steps to reproduce
- Aster version (`python -c "import aster; print(aster.__version__)"`)

## Questions

Open a discussion on GitHub or email emrul@aster.dev.
