# Vendored third-party TypeScript packages

Packages here are vendored because they are not (yet) published to npm or we
want to pin to a specific upstream commit. They are consumed via `file:` refs
from `bindings/typescript/packages/*/package.json`.

## `fory-core/` — Apache Fory JS (@apache-fory/core)

- **Upstream:** https://github.com/apache/fory
- **License:** Apache-2.0 (see `fory-core/LICENSE`, `fory-core/NOTICE`)
- **Why vendored:** Apache Fory has not published its JS package to npm yet.
  Previously `file:../../../../docs/_internal/fory/javascript/packages/core`
  which was gitignored, causing CI-TS to fail for all fresh checkouts.
- **What's here:** only `dist/` (the built JS + d.ts shipped to npm consumers),
  plus a trimmed `package.json` without the upstream `workspaces` / `prepublishOnly`
  / `node-gyp` that aren't needed at runtime.
- **Upgrade process:** rebuild `docs/_internal/fory/javascript/packages/core/dist/`
  upstream, then `cp -r dist/ LICENSE NOTICE` here. Don't edit the vendored
  files in place.
