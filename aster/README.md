# aster

Native Rust API for **Aster** — peer-to-peer transport built on
[iroh](https://github.com/n0-computer/iroh): content-addressed blob storage,
CRDT documents, and gossip pub-sub over QUIC.

This is the supported entry point for Rust consumers. It is an idiomatic facade
over the internal `aster_transport_core` backend (the Python / TypeScript /
Java / Kotlin bindings wrap the same core).

## Quick start

```rust
use aster::{AsterConfig, Node, RelayMode};

let node = Node::start(
    AsterConfig::builder()
        .persistent("/var/lib/my-app")
        .relay(RelayMode::Default)
        .build(),
)
.await?;

// Blobs
let hash = node.blobs().add_bytes(b"hello".to_vec()).await?;
let bytes = node.blobs().read_to_bytes(&hash).await?;

// Docs (deterministic config namespace)
let secret = aster::NamespaceSecret::from_bytes(my_32_byte_seed);
let doc = node.docs().open_or_import_write_namespace(secret).await?;
let author = node.docs().default_author().await?;
doc.set_bytes(&author, b"key".to_vec(), b"val".to_vec()).await?;

node.shutdown().await;
```

See `examples/quickstart.rs` for an end-to-end persistence round-trip.

## Cargo features

| Feature | Default | Gates |
|---------|---------|-------|
| `blobs` | ✅ | `Node::blobs()` |
| `docs` | ✅ | `Node::docs()` |
| `gossip` | ✅ | `Node::gossip()` |
| `discovery` | — | mDNS local discovery config |
| `hooks` | — | (reserved) |
| `metrics` | — | `Node::metrics_prometheus()` |

Admission (`Node::take_admission()`) additionally requires the node to be
started with `AsterConfig::builder().hooks(true)`.

## Depending on this crate

Released crates come from Aster's public Forgejo Cargo registry. Configure it
once per project:

```toml
# .cargo/config.toml
[registries.aster]
index = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
```

```toml
# Cargo.toml
[dependencies]
aster = { version = "0.3", registry = "aster" }
```

Use the selected release version rather than copying Aster's private fork
graph. Advanced Rust consumers can access the exact Iroh/Fory/Salvo stack via
`aster::native`. See `docs/rust-sdk-consumer-guide.md` in the repository for
the registry rollout state, direct-dependency exceptions for proc macros, and
the maintainer-only private-Git fallback.

## Status

Step 1 (node + blobs/docs/gossip + admission + deterministic namespaces) is
implemented. Rust-native **RPC services** are designed but not yet shipped — see
`docs/_internal/aster-rust.md`.
