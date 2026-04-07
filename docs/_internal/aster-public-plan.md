# Aster Public Repo Plan

## Decision log

1. **Open source everything** — Rust code is not a meaningful moat (it's thin wrappers around open-source iroh crates + ports of the Python code). Moat will be elsewhere (hosted services, conformance suite, deployment tooling, etc.).
2. **Keep `docs/_internal/` private** — internal specs, RFCs, and analysis docs stay versioned in the private repo, excluded from the public mirror.
3. **Single public multi-language repo** — one repo for all language bindings (Python, TS, Java, .NET), not per-language repos.
4. **Public repo is installable** — has its own pyproject.toml / package.json so consumers can install from GitHub.
5. **Develop bindings in the public repo** — real git history, real community PRs. Not a periodic dump.

## Architecture: private repo (source of truth) → public mirror

The private repo (`iroh-python`) remains the single source of truth with everything, including `docs/_internal/`. The public repo is an auto-mirror of everything *except* `docs/_internal/`.

```
Private repo (iroh-python)          Public repo (aster / aster-sdk)
├── core/                    ──→    ├── core/
├── ffi/                     ──→    ├── ffi/
├── bindings/                ──→    ├── bindings/
├── cli/                     ──→    ├── cli/
├── tests/                   ──→    ├── tests/
├── examples/                ──→    ├── examples/
├── scripts/                 ──→    ├── scripts/
├── docs/                           ├── docs/
│   ├── end_user/            ──→    │   └── end_user/
│   └── _internal/           ✗ EXCLUDED
├── ffi_spec/                ──→    ├── ffi_spec/
├── .github/workflows/       ──→    ├── .github/workflows/
├── LICENSE                  ──→    ├── LICENSE
├── README.md                ──→    ├── README.md
└── ...                      ──→    └── ...
```

## Sync mechanism: GitHub Actions auto-mirror

A GitHub Actions workflow in the **private** repo that, on every push to `main` (and on tags), mirrors to the public repo minus `docs/_internal/`.

### Approach: rsync-based (simplest, one dir excluded)

```yaml
# .github/workflows/mirror-public.yml (in private repo)
name: Mirror to public repo
on:
  push:
    branches: [main]
    tags: ['v*']

jobs:
  mirror:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Clone public repo
        run: |
          git clone https://x-access-token:${{ secrets.PUBLIC_REPO_TOKEN }}@github.com/emrul/aster-sdk.git /tmp/public

      - name: Sync files (exclude _internal)
        run: |
          rsync -av --delete \
            --exclude 'docs/_internal/' \
            --exclude '.git/' \
            ./ /tmp/public/

      - name: Commit and push
        run: |
          cd /tmp/public
          git config user.name "Mirror Bot"
          git config user.email "mirror@aster.dev"
          git add -A
          git diff --cached --quiet && exit 0  # nothing to push
          git commit -m "Mirror from private repo: $(git -C $GITHUB_WORKSPACE log -1 --format='%h %s')"
          git push
```

### Alternative: git filter-repo (preserves individual commits)

More complex but gives the public repo real per-commit history rather than squashed mirror commits. Uses `git filter-repo --path-glob` to exclude `docs/_internal/` from each commit before pushing. Trade-off: harder to set up, but better community perception.

## Native artifact distribution

Private CI builds and publishes compiled native modules to package registries:

| Language | Registry | Package | Contains |
|----------|----------|---------|----------|
| Python | PyPI | `aster-python` | Compiled `.so`/`.pyd` wheel |
| TypeScript | npm | `@aster/native` | `.node` binary |
| Java | Maven Central | `aster-native` | JNI `.so` in JAR |
| .NET | NuGet | `Aster.Native` | Native lib |

Pre-release artifacts (for testing unreleased Rust changes) published as GitHub Release assets on the private repo. Public CI can pull via PAT.

## README transparency

Be upfront about the model:

> **Aster** is an Apache 2.0 licensed peer-to-peer RPC framework built on [Iroh](https://github.com/n0-computer/iroh).
>
> This repo contains the full source code. Internal design documents are maintained separately.

## Open questions

- [ ] Public repo name: `aster`, `aster-sdk`, `aster-rpc`, other?
- [ ] Mirror approach: rsync (squashed commits) or filter-repo (preserved history)?
- [ ] When to set this up: before or after first public release?
