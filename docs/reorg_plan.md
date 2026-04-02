# Repository Reorganization Plan (Multi-Language Bindings)

This repository is evolving from Python-first bindings into a multi-language transport workspace.

## Goals

1. Keep stable consumer-facing names where possible (especially Python package/import paths).
2. Make examples and docs language-scoped.
3. Prepare the repo for Java and future bindings without forcing a disruptive big-bang move.

---

## Stage 1 (Now) — Minimal, low-risk structure cleanup - COMPLETE

### Scope

- Keep the Python package/import name as `iroh_python`.
- Move Python examples into a language-scoped location:
  - `examples/*.py` → `examples/python/*.py`
- Update references that mention old example paths.

### Why

- Preserves current Python API and packaging stability.
- Establishes a scalable examples convention (`examples/<language>/...`).

---

## Stage 2 (Post-Java kickoff) — Introduce first-class binding boundaries

### Scope

1. Add Java binding area (`iroh_java/` or `bindings/java/`) and start `examples/java/`.
2. Rewrite top-level README as a multi-language workspace README:
   - `aster_transport_core` as shared backend
   - `aster_transport_ffi` as language-neutral C ABI
   - language bindings as adapters
3. Separate CI responsibilities into clear lanes:
   - core/ffi correctness lane
   - python binding/package lane
   - java binding lane
4. Start language-scoped test layout planning (`tests/python`, future `tests/java` harness integration).
5. Define versioning/release policy across crates/artifacts:
   - aligned release train vs per-binding independent versioning

### Recommended target layout (incremental)

```text
examples/
  python/
  java/

aster_transport_core/
aster_transport_ffi/
iroh_python_rs/
iroh_python/
iroh_java/
```

### Optional Stage 2.5 (only if churn is worth it)

Adopt a `bindings/` umbrella and move language-specific trees under it:

```text
bindings/
  python/
    rust/      # current iroh_python_rs
    package/   # current iroh_python
    tests/
  java/
```

This should only be done after Java is active and CI/docs are ready for a larger move.
