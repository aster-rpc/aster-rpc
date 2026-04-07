# Aster Public Repo Plan

## Decision log

1. **Open source everything** — Rust code is not a meaningful moat (it's thin wrappers around open-source iroh crates + ports of the Python code). Moat will be elsewhere (aster.site platform, trust infrastructure, hosted services).
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

### Phase 1: GitHub Packages (current — private distribution)

All artifacts published to GitHub Packages for internal consumption and private repos that depend on Aster. See [versioning-and-releases.md](versioning-and-releases.md) for full details.

| Language | Registry | Package |
|----------|----------|---------|
| Python | GitHub Packages (PyPI) | `aster-rpc` |
| TypeScript | GitHub Packages (npm) | `@aster-rpc/transport` |
| CLI | GitHub Packages (PyPI) | `aster-cli` |

### Phase 2: Public registries (when we go public)

Promote from GitHub Packages to public registries:

| Language | Registry | Package |
|----------|----------|---------|
| Python | PyPI | `aster-rpc` |
| TypeScript | npm | `@aster-rpc/transport` |
| CLI | PyPI | `aster-cli` |
| Java | Maven Central | TBD |
| .NET | NuGet | TBD |

Pre-release artifacts (for testing unreleased changes) remain on GitHub Packages. Public releases go to both GitHub Packages and public registries.

---

## Pre-public checklist

Everything that must be done before the public repo goes live.

### Versioning & build (see [versioning-and-releases.md](versioning-and-releases.md))

- [ ] Fix `aster/__init__.py` — use `importlib.metadata` instead of hardcoded `__version__ = "0.2.0"`
- [ ] Align all Track 2/3 versions to `0.1.0` (or whatever we decide the first release is)
- [ ] Set up CI build number stamping (`.devN` for Python, `-build.N` for npm/Cargo)
- [ ] Set up GitHub Packages publishing in CI (Python + TypeScript)
- [ ] Create version bump script (bumps all files in a track atomically)
- [ ] Verify private repos can consume packages from GitHub Packages

### Code hygiene

- [ ] Remove or `.gitignore` any leftover local files (`.claude/`, scratch files)
- [ ] Ensure `CLAUDE.md` is in `.gitignore` (it is, currently)
- [ ] Audit for hardcoded paths, secrets, internal URLs in code and comments
- [ ] Ensure no `docs/_internal/` references leak into public code (imports, comments, links)
- [ ] Review all TODO/FIXME/HACK comments — remove or make them non-embarrassing
- [ ] Ensure tests pass without access to private infrastructure

### Legal & licensing

- [ ] Verify `LICENSE` file is Apache 2.0
- [ ] Add license headers to source files (or decide not to — Apache 2.0 doesn't require it but it's conventional)
- [ ] Review third-party dependency licenses for compatibility (especially Rust deps via `cargo-deny`)
- [ ] Decide on CLA (Contributor License Agreement) for external contributors — or skip for now

### Documentation

- [ ] Write public-facing `README.md` — getting started, install, examples
- [ ] Ensure `docs/end_user/` has enough content for a new user
- [ ] Remove or rewrite any internal jargon in public-facing docs
- [ ] Add `CONTRIBUTING.md` (how to run tests, submit PRs, code style)
- [ ] Add `CODE_OF_CONDUCT.md` (standard Contributor Covenant or similar)

### CI/CD

- [ ] Set up mirror workflow (`.github/workflows/mirror-public.yml`) — see above
- [ ] Create `PUBLIC_REPO_TOKEN` secret in private repo
- [ ] Ensure CI works on public repo (no private runner dependencies for public builds)
- [ ] Set up public repo CI to run tests against published GitHub Packages artifacts
- [ ] Branch protection on public repo `main` (require PR reviews, CI pass)

### Repository setup

- [ ] Create public GitHub repo (name TBD: `aster`, `aster-sdk`, `aster-rpc`)
- [ ] Set repo description, topics, social preview image
- [ ] Enable Discussions (for community Q&A)
- [ ] Enable Issues with templates (bug report, feature request)
- [ ] Disable wiki (docs live in the repo)
- [ ] First mirror push — verify only intended files are present

### Security

- [ ] Ensure `scripts/security-review.sh` pre-commit hook works on public repo
- [ ] No secrets, credentials, or internal API keys in git history
- [ ] Review git history for any accidentally committed sensitive content (if using filter-repo, this is handled; if rsync, history starts clean)

---

## README transparency

Be upfront about the model:

> **Aster** is an Apache 2.0 licensed peer-to-peer RPC framework built on [Iroh](https://github.com/n0-computer/iroh).
>
> This repo contains the full source code. Internal design documents are maintained separately.

---

## Open questions

- [ ] Public repo name: `aster`, `aster-sdk`, `aster-rpc`, other?
- [ ] Mirror approach: rsync (squashed commits) or filter-repo (preserved history)?
- [ ] When to set this up: before or after first public release?
- [ ] CLA: require one for external contributors, or skip?
- [ ] Self-hosted runners: public CI should work on GitHub-hosted runners (no dependency on private infra)
