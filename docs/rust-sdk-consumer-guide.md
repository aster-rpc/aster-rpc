# Consuming the Aster Rust SDK

This is the dependency and native-interop contract for Rust applications using
Aster. Use [`docs/aster-rust-getstarted.md`](aster-rust-getstarted.md) for the
facade APIs themselves.

## The short version

- Install released Aster crates from the public `Aster` Forgejo Cargo registry.
- Commit `Cargo.lock` in applications and deploy from it.
- Do not copy `iroh-fork-manifest.toml` or an Aster `[patch.crates-io]` block
  into an ordinary consumer.
- Prefer the `aster` facade. Use `aster::native` when you need the exact Iroh,
  Fory, or Salvo types selected by Aster.
- Add a direct dependency from the same source only when a proc macro or an
  extra Cargo feature requires one.

> First-release gate: the commands below are the release contract. Do not
> announce an Aster version until an unauthenticated request to
> `https://forge.emrul.dev/api/packages/Aster/cargo/config.json` returns `200`
> and unauthenticated `git ls-remote` works for every fork URL, and that exact
> Aster plus native dependency wave is present. The native wave may be
> bootstrapped before the first `aster` facade package. If Cargo reports that no
> matching `aster` package exists, the Git fallback at the end is only for
> already-authorized maintainers; that facade release is not public yet.

## Configure Cargo once per project

Add this project-local file so builds do not depend on each developer's home
directory configuration:

```toml
# .cargo/config.toml
[registries.aster]
index = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
```

Reading released crates is anonymous. Consumers must not put a Forgejo token in
their repository, CI variables, Cargo credentials, or dependency URLs.

Then add the compatible Aster series:

```toml
[dependencies]
aster = { version = "0.3", registry = "aster" }
```

Enable only the optional surfaces you use:

```toml
aster = {
  version = "0.3",
  registry = "aster",
  features = ["rpc", "expose-edge"]
}
aster-transport-salvo = { version = "0.3", registry = "aster" }
```

Applications pin the resolved release by committing `Cargo.lock`; use
`cargo update -p aster` deliberately. A reusable library should use a compatible
range such as `0.3` and test the lowest and newest supported releases. Aster's
published internal requirements keep directly listed Aster crates on one
coherent patch release.

## Using Iroh directly

The facade remains Aster's compatibility boundary. The `aster::native` module
reexports the exact native crates carried by the selected release:

```rust
let endpoint: aster::native::iroh::Endpoint = node.native_endpoint();
let store: aster::native::iroh_blobs::api::Store = node.blobs().native_store();
let docs: aster::native::iroh_docs::protocol::Docs = node.docs().native_docs();
let gossip: aster::native::iroh_gossip::net::Gossip =
    node.gossip().native_gossip();
```

These handles are clones of the live objects owned by the Aster node, not a
second stack. Do not close the endpoint, replace its ALPN set, or run a competing
accept loop. Do not call `shutdown` on the native Docs, Blobs store, or Gossip
handles; the Aster node owns their lifecycle. Raw blob writes share Aster's
node-wide garbage collector, so tag content which must remain resident.

For types and the features already enabled by Aster, use the reexports. If your
code needs an extra upstream feature, add the crate directly from the **Aster
registry** at the exact supported version so Cargo unifies it with Aster:

```toml
iroh = { version = "=1.0.1", registry = "aster", features = ["metrics"] }
iroh-blobs = { version = "=0.103.1", registry = "aster" }
```

Do not combine Aster-registry Iroh crates with same-named crates from crates.io
or a Git revision. Cargo treats different sources as different packages; types
which look identical will not be interchangeable.

The raw module follows the upstream crates' compatibility policy. Aster may
move its native stack in a minor release while keeping the facade compatible.
Use the facade for interfaces shared across independently released libraries.

## Fory payloads

Aster currently uses the unmodified `fory-core` and `fory-derive` 1.3.0 crates
from crates.io. They are visible under `aster::native`, but Fory's derive macros
emit an absolute `::fory_core` path. A crate deriving payload types must
therefore list both packages directly:

```toml
[dependencies]
aster = { version = "0.3", registry = "aster", features = ["rpc"] }
fory-core = "=1.3.0"
fory-derive = "=1.3.0"
```

This is a proc-macro name-resolution requirement, not a second Fory runtime.
Keep the exact version: a broad `"1.3"` requirement can now select a newer 1.x
release and silently diverge from Aster's validated wire-codec pair. Cargo
unifies the exact crates.io version. Aster will publish Fory in its own registry
only if it carries an Aster-specific Fory patch.

## Salvo applications

`aster-transport-salvo` and `aster::expose` reexport their exact Salvo crate:

```rust
use aster_transport_salvo::salvo;

let app = salvo::Router::new()
    .push(aster_transport_salvo::router(dispatcher))
    .push(salvo::Router::with_path("healthz").get(health));
```

Salvo proc macros resolve an extern-prelude crate named `salvo`. If you use
those macros, or need an additional supported Salvo feature, add the fork from
the Aster registry rather than crates.io:

```toml
salvo = { version = "=0.93.0", registry = "aster", features = ["quinn"] }
```

Only the Salvo feature closure validated by Aster is part of the release. Ask
the maintainers to expand that closure before enabling a Salvo component which
is not present in the Aster registry.

## Source crates versus native artifacts

Rust libraries are distributed as source `.crate` archives through Cargo.
Precompiled `.rlib` and `.rmeta` files are tied to a Rust compiler, target,
features, profile, and dependency graph and are not a portable SDK format.

Aster does publish target-specific binaries where a native artifact is the
actual product boundary: Python wheels, Node native modules, C ABI libraries and
headers, JVM native libraries, and standalone CLI programs. Linux, macOS, and
Windows builds validate target portability; an anonymous scratch build from the
published registry validates the source-package dependency graph. Neither
replaces source-crate publication.

## Temporary Git fallback

Use this only while the Cargo registry is being bootstrapped or to diagnose a
registry outage:

1. Authorized maintainers pin `aster` to a release commit from its private
   Forgejo repository.
2. Copy the bootstrap `[patch.crates-io]` blocks from that same commit's
   `iroh-fork-manifest.toml` into the consumer workspace root.
3. Commit `Cargo.lock`.

```toml
[dependencies]
aster = {
  git = "https://forge.emrul.dev/Aster/aster-rpc-internal.git",
  rev = "<release-commit>",
  features = ["rpc"]
}
```

Never distribute credentials for this fallback, use `branch = "main"` for a
production build, or combine a manifest from one Aster revision with another
revision. This fallback exists precisely because Cargo does not propagate a
dependency's root `[patch]` section; it is retired once the public source-crate
wave is available.
