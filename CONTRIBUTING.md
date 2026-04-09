# Contributing to Aster

Thanks for your interest in contributing to Aster.

## Getting started

```bash
uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout
uv run maturin develop -m bindings/python/rust/Cargo.toml
uv pip install -e cli/
uv run pytest tests/python/ -v --timeout=30
```

See [CLAUDE.md](CLAUDE.md) for build commands and architecture overview.

## Submitting changes

1. Fork the repo and create a branch from `main`.
2. Make your changes. Add tests for new functionality.
3. Run `./scripts/validate.sh` to check formatting, linting, and tests.
4. Open a pull request against `main`.

## Code style

- **Python:** No linter enforced yet. Follow existing patterns.
  Use `Optional[T]` not `T | None` in `@wire_type` dataclasses (pyfory
  requirement). No Unicode em dashes in source (the pre-commit hook
  checks this).
- **Rust:** `cargo fmt` and `cargo clippy -D warnings`.
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
