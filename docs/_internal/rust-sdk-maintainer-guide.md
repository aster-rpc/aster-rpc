# Maintaining and releasing the Aster Rust SDK

This is the maintainer contract for Aster's Rust crates and its supported
Iroh, Fory, and Salvo stack. Consumer instructions live in
[`docs/rust-sdk-consumer-guide.md`](../rust-sdk-consumer-guide.md).

## Decisions

1. Forgejo is canonical for Aster-owned source, source packages, and releases.
2. Rust libraries ship as source `.crate` packages in the `Aster` Cargo
   registry. We do not ship `.rlib` or `.rmeta` files as dependencies.
3. Native binary matrices remain for Python, Node, C/JVM, and CLI artifacts and
   for validating source-tree portability on Linux, macOS, and Windows. A clean,
   credential-free scratch build separately validates the published graph.
4. Consumers select one Aster version. They do not reproduce our fork graph.
5. `aster` is the stable facade. `aster::native` is the supported escape hatch
   to the exact native stack, but its upstream API is a lower-stability surface.
6. Publish only changed third-party forks. Unmodified dependencies such as the
   current Fory 1.3.0 crates continue to come from crates.io.

## Registry setup and credentials

The sparse index is:

```toml
[registries.aster]
index = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
credential-provider = "cargo:token"
```

Before the first publication, an Aster organization owner must open the
organization's package settings and create Forgejo's special `_cargo-index`
repository and generated configuration. Do not create or edit its contents by
hand; Forgejo updates the index after uploads and provides a rebuild action in
the same settings page if package storage and index metadata diverge.
Forgejo commits each uploaded crate's sparse-index entry as the publishing
user, so the `agents` release account also needs repository write permission on
`Aster/_cargo-index`. A package token without that grant passes Cargo's dry run
but the real upload fails while Forgejo tries to push the index commit.

The Aster owner and Forgejo instance must allow anonymous package reads. The
release acceptance test is deliberately black-box:

```bash
curl --fail https://forge.emrul.dev/api/packages/Aster/cargo/config.json
```

Run it without cookies, Git credentials, or an `Authorization` header. A `401`
means the registry is not public, regardless of how the web UI describes it.
Check the Aster owner visibility and the instance's sign-in-to-view policy
(`service.REQUIRE_SIGNIN_VIEW` on installations which enable it).

Apply the same black-box rule to fork Git source: from a credential-free
environment, `git ls-remote` must work for every fork URL in
`iroh-fork-manifest.toml`. `aster-rpc-internal` remains a private development
repository; its released Rust source is the public `.crate` package. A fork
marked public in the UI is not a public development source if the instance
still redirects or returns `401` to anonymous clients.

Publishing is authenticated. Create a dedicated CI token with
`write:package` (the scope name used by the deployed Forgejo version), store it
as `ASTER_CARGO_TOKEN`, and let
`scripts/release/publish-cargo.sh` add Forgejo's required `Bearer ` prefix.
The repository's `cargo:token` credential provider reads that environment
token for publication. Consumers never receive this token and do not need a
credential provider for anonymous reads.

## What is one release wave?

An Aster release is a coherent set of source identities:

- Modified Iroh/Noq crates needed by the resolved, supported feature graph.
- Modified Salvo/H3 crates needed by `aster-transport-salvo` and
  `aster-expose/edge`.
- `aster_transport_core`, `aster-macros`, `aster-expose`, `aster`, and
  `aster-transport-salvo`, all on the same derived Aster version.
- Unmodified crates.io dependencies, including the exact Fory 1.3.0 pair in
  the current wave.

`iroh-fork-manifest.toml` records fork source provenance and the intended
registry closure. It is a maintainer BOM, not installation text. Always compare
it with the actual all-feature graph:

```bash
cargo metadata --locked --format-version 1
cargo tree --workspace --all-features
```

Do not infer the registry closure from the top-level `[patch]` entries alone.
For example, the current graph also contains `iroh-dns`, Salvo helpers, and the
forked H3 crates. It also has crates.io packages whose published metadata names
a patched package (`irpc`, `netwatch`, `portmapper`, `iroh-mdns-address-lookup`,
`iroh-tickets`, `iroh-util`, and `h3-datagram` in the current graph). Those
bridge crates must be republished unchanged into Aster's registry with their
affected dependencies rewritten to the same registry; a registry package
cannot inherit Aster's root patches. Record them as `registry_bridge_crates` in
the BOM. A missing bridge can pass source-tree tests and still make the
published package select upstream Iroh, Noq, or H3.

## Normal development workflow

### 1. Change a fork

- Base each Aster patch on a named upstream release.
- Keep logically independent patches in reviewable commits/branches.
- Run the fork's format, clippy, and full relevant tests.
- Tag the validated fork commit and push it to its public Forgejo repository.
- Record upstream base, commit, tag, crate versions, and patch purpose in the
  fork ledger and `iroh-fork-manifest.toml`.

Cross-fork dependencies must use Cargo's dual-location form: an exact public
Forgejo `git`/`rev` for source-tree development plus a compatible `version` and
`registry = "aster"` for packaging. Publishable internal workspace dependencies
use `path`, exact `version`, and `registry = "aster"`. Without the `registry`
key Cargo normalizes the fallback to crates.io even when the parent is published
with `--registry aster`. The Git/path location is not embedded in the `.crate`.

### 2. Update Aster

- Update the workspace dependency and every root patch entry to the same fork
  commit. Patches remain a repository-development mechanism until the fork
  workspaces themselves are registry-native.
- Update `Cargo.lock` and the manifest BOM in the same change.
- Use `sem impact` for changed public entities and run the indicated tests.
- Run the workspace feature matrix, not only default features.

The public Forgejo Git URL is the normal fork development source. Do not add a
private token to a Cargo URL. All current Iroh/Noq and Salvo fork repositories,
including `iroh-docs`, are public under the Aster organization.

Unmodified registry bridges are the exception to the Forgejo Git-source rule:
the bootstrap graph pins their exact public upstream commits because Aster
carries no code fork. Their metadata-rewritten `.crate` archive in Forgejo is
the consumer source, and its code must match the recorded crates.io release as
described above.

### 3. Validate source packages before a tag

All publishable workspace crates declare `publish = ["aster"]`, repository and
license metadata, and versions on local path dependencies. Run:

```bash
scripts/release/publish-native-stack.py check
scripts/release/publish-cargo.sh --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
```

The native-stack check clones every fork at the BOM revision, checksum-verifies
every unmodified crates.io bridge archive, applies only temporary dependency
source metadata rewrites, and verifies the complete publisher set. The Aster
package check validates file selection, normalized dependency identities, and
the resolved reverse closure against the bridge-crate BOM without requiring a
live registry. The publish command performs Cargo's real
`--dry-run` package verification immediately before each upload, when that
crate's already-published dependencies are resolvable. A blanket dry run of the
whole Aster workspace cannot validate the first release: later crates depend on
earlier crates that do not exist in the registry yet.

Inspect packaged manifests when dependency-source behavior changes. A published
manifest must point modified packages at the Aster registry, ordinary upstream
packages at crates.io, and contain no unusable local path.

### 4. Publish in dependency order

Registry versions are immutable. Publish leaves first, then parents:

1. Noq family.
2. Bridge crates that name Noq (`irpc`, then `netwatch`, then `portmapper` in
   this wave).
3. Iroh base/relay/dns and Iroh itself.
4. Bridge crates that name Iroh (`iroh-mdns-address-lookup`, `iroh-tickets`, and
   `iroh-util` in this wave).
5. Iroh Blobs, Docs, and Gossip after their cross-fork Iroh dependencies.
6. H3, then the `h3-datagram` bridge, H3-Quinn, and the supported Salvo helper
   closure, then Salvo.
7. Tag Aster, let the disposable release tree stamp the derived version, and
   run `scripts/release/publish-cargo.sh --publish`.

Steps 1 through 6 are encoded in one idempotent command:

```bash
export CARGO_REGISTRIES_ASTER_TOKEN="Bearer <write:package token>"
scripts/release/publish-native-stack.py publish
```

The script stages exact fork revisions and verified bridge archives under a
temporary directory, dry-runs each crate immediately before upload, waits for
the sparse index between dependents, and skips versions already present after
a partial retry. It never changes a fork checkout or crates.io archive in
place. Run it only when the BOM or supported native dependency closure changes;
ordinary Aster-only releases reuse the immutable published dependency wave.

Within the Aster workspace the script publishes core, macros, expose, facade,
and Salvo transport in topological order. It dry-runs each package just in time,
waits for the sparse index before publishing a dependent, and safely skips an
immutable version already present after a partial CI retry. Publish the
dependency wave before the Aster packages: a partial leaf wave is harmless
because no released Aster version references it yet. Never overwrite or delete
a released version to fix a mistake; publish a new Aster patch and yank only a
demonstrably unusable or unsafe version.

The tag workflow always publishes the Rust source wave before public native
artifacts. Its write credential is the repository-scoped `ASTER_CARGO_TOKEN`
secret. Publication refuses an untagged HEAD or package versions which do not
match the derived tag, and any source-publication failure fails the release.

The current root manifests use dual Git/version locations. Before the first
registry release, perform one end-to-end dry run to prove Forgejo records the
target registry for those normalized dependencies. Do not enable automated tag
publication on faith.

### 5. Test like a consumer

After publication, create a scratch project outside every Aster/fork checkout:

- Configure only the sparse registry URL.
- Remove all Cargo credentials.
- Depend on the released `aster` and `aster-transport-salvo` versions.
- Exercise facade APIs, `aster::native` handle types, Fory derives, Salvo route
  composition, and any direct Iroh/Salvo feature documented for consumers.
- Assert `cargo tree` contains one source for each Iroh and Salvo package.
- Build the same locked scratch project on Linux, macOS, and Windows.

The anonymous scratch build is the release gate. A source-tree build is not a
substitute because root patches can hide broken registry metadata.

### 6. Publish native products

After the source-crate smoke tests pass, publish target-specific products:

- Python abi3 wheels plus sdist.
- Node/N-API packages and native modules.
- C ABI libraries and headers; JVM native assets when applicable.
- Standalone executables and checksums.

These artifacts use the same Aster version and Cargo.lock as the source-crate
wave. Runners compile them independently for each supported target. Because
`aster-rpc-internal` is private, consumer binaries are uploaded as public
organization-owned generic packages (`Aster/aster-python/<version>` today),
not exposed through the repository's private Releases page. The private release
remains the signed-off maintainer record. `scripts/download-release.sh` reads
the public package, verifies Forgejo's digest and `SHA256SUMS`, and requires no
token.

## Public surface policy

The facade owns Aster's source-compatibility promise. Native reexports exist so
advanced consumers can work with the real endpoint/store/protocol objects and
avoid duplicate package identities. Keep the escape hatch narrow:

- Reexport Iroh protocol crates that callers intentionally extend.
- Reexport Fory runtime/derive crates under the `rpc` feature, while documenting
  their direct-dependency macro requirement.
- Reexport Salvo from the Salvo transport and edge features; macro users depend
  directly on the same registry package.
- Do not reexport implementation-only crates such as Noq or H3 merely because
  they are transitive. They are available in the registry for graph closure,
  not promised as Aster APIs.

Any native handle must refer to the node's existing object. Never create a
second endpoint/store to simulate interop. Document ownership hazards such as
endpoint closure, ALPN mutation, competing accept loops, and blob GC.

## Release checklist

- [ ] Fork tags and commits match `iroh-fork-manifest.toml`.
- [ ] Resolved all-feature fork closure matches the registry BOM.
- [ ] Bridge crates have been repackaged with same-registry dependency metadata;
      their source matches the recorded crates.io archive byte-for-byte after
      excluding the manifest rewrite and Cargo-generated packaging metadata.
- [ ] Anonymous registry `config.json` returns `200`.
- [ ] Every fork Git URL is anonymously readable, including `Aster/iroh-docs`;
      `aster-rpc-internal` remains private.
- [ ] `publish-native-stack.py check` passes, and any changed fork/bridge source
      crates are published and usable in dependency order.
- [ ] Aster version stamped only in the disposable release tree; `VERSION` not
      hand-edited.
- [ ] `publish-cargo.sh --check`, fmt, clippy, and tests pass.
- [ ] Each package's just-in-time Cargo dry run passes during publication.
- [ ] Aster source crates published in dependency order.
- [ ] Anonymous external scratch consumer passes on Linux/macOS/Windows.
- [ ] Native language artifacts and checksums published.
- [ ] Release notes include Aster version, fork wave/tag provenance, supported
      native crate versions, and any raw-API compatibility changes.
