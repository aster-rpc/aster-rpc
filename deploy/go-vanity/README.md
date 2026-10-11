# Aster Go module discovery

The approved module identity is now `sdk.getaster.now/go`. Discovery is live
at `https://sdk.getaster.now/go?go-get=1`, served by Cloudflare Workers Static
Assets as part of the combined SDK site. The publisher is
`aster-rpc/website/.github/workflows/deploy.yml` on GitHub Actions; the successful
end-to-end run is https://github.com/aster-rpc/website/actions/runs/38079993092.
See `~/dev/aster/website/DEPLOYMENT.md` for operations and credentials.

The source remains the public `aster-rpc/aster-rpc` mirror, subdirectory
`bindings/go`. The live discovery page comes from
`website/src/pages/go/index.astro`. This directory's `public/` payload is a matching reference and is not the deployed
source. The SDK's go.mod, imports and release preparer use the same identity;
the changes must be committed and released before public Go publication.
HTTPS hosting and CI authentication are complete; module release is not.

The metadata's fourth field, `bindings/go`, maps the module to its repository
subdirectory. This requires Go 1.25+, which is already the SDK's minimum.
See the [official Go repository discovery rules](https://go.dev/ref/mod#vcs-find).

## Prepare filtered source and release refs

The private repository now contains `scripts/release/prepare-public-mirror.py`.
It prepares and verifies a disposable local clone; it never pushes or changes
private refs, working files or the private pre-push lint hook. Install the pinned
`git-filter-repo==2.47.0` tool before using it. The source must have complete
history, without replacement refs or grafts; the output must be new and outside
the source repository. It reads the latest **committed** `.publicignore`, not
uncommitted edits, and filters every ancestor of the selected source revision.
Policy entries are literal root directory prefixes (ending in `/`), basename
globs at any depth, or root-relative file globs. Negation, absolute paths, dot
path components and wildcard directory-prefix entries are rejected; this is not
a general `.gitignore` parser.

For a local preview after committing the intended source/policy:

```sh
mirror_parent="$(mktemp -d)"
python3 scripts/release/prepare-public-mirror.py \
  --source . --source-ref main --output "$mirror_parent/repo"
```

Add `--required-release v<version>` when checking a particular release. Required
releases must be root tags on main's history, match the original committed
`VERSION`/commit-count derivation and contain the approved Go module identity.
The preparer preserves empty commits and merge history, checks version-critical
counts, and maps eligible releases to both root `v<version>` and
`bindings/go/v<version>` tags on the corresponding **filtered** commit. Tags are
recreated as lightweight refs; private annotated-tag signatures are not carried
onto rewritten commits. A root tag alone does not version the Go submodule.
A required release fails if missing, malformed or ineligible for the Go module,
including unsupported major-version module paths. Valid historical root releases
with an older module identity receive no Go submodule tag. Source tags are never
pushed directly to the public repository.

The prepared clone contains only filtered main and validated release refs, with
no source remote. Its private `.git/public-mirror.json` records the mapping and
`.git/public-refspecs` lists the exact publishable refs. The existing private
GitHub mirror workflow uses this preparer for main/tag events or manual release
selection. It serializes runs and pushes those refs atomically: main uses an
explicit lease, tags are never forced, and a conflicting tag rejects the whole
update. Public history excludes CI/runner configuration; builds and releases
remain on Forgejo. Local preparation does not run the workflow or publish refs.

## Remaining publication requirements

- Commit and validate the source/policy/workflow changes, then verify the private
  mirror workflow runs for the intended release with its scoped
  `PUBLIC_REPO_TOKEN`. Local fixture tests are not a remote CI execution.
- Verify the committed go.mod, imports, tests and release validation match
  the already-live `sdk.getaster.now/go` discovery metadata.
- Verify the filtered public source and `bindings/go/v<version>` tag match the
  matching native release. Versions come from `scripts/release/version.sh`;
  never invent an independent Go patch number or push a private-history tag.
- Publish matching native headers/libraries and confirm anonymous consumer
  downloads. Vanity discovery does not distribute them automatically.
- Complete a fresh public-module install without a local replacement, through
  both the default proxy and direct VCS discovery, then run a native-backed
  consumer. MacOS arm64 staged local-replacement checks passed with ABI 4.16;
  Linux/Windows consumer execution remains pending. These are distinct from
  public registry proof.

Submodule tag prefixes follow the
[Go version mapping rules](https://go.dev/ref/mod#vcs-version).
Nothing in the static payload or local preparer deploys DNS/HTTPS, pushes refs,
or establishes public availability. The new filtering boundary governs exported
reachable history; it makes no claim about historical remote object deletion.

## Verify after deployment and release

```sh
curl -fsSL 'https://sdk.getaster.now/go?go-get=1'
go list -m -versions sdk.getaster.now/go
# Use a real published version in a fresh consumer, without a local replace:
go get sdk.getaster.now/go@v<version>
```

Verify a fresh native-backed consumer with the released header/library and
explicit cgo include/linker paths. Test discovery both through the default Go
proxy and with `GOPROXY=direct`. Do not mark publication complete based on a
checkout build or local `replace` test.
