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

## ⚠️ Depending on this crate (required patch block)

Aster pins `iroh` / `noq` to patched forks via `[patch.crates-io]`. Cargo patches
**do not propagate across repositories**, so an external project depending on
`aster` via git **must** copy the patch block below into its own root
`Cargo.toml` (keep it in sync with this repo's root `Cargo.toml`):

```toml
[dependencies]
aster = { git = "https://github.com/aster-rpc/aster-rpc-internal", branch = "main" }

[patch.crates-io]
iroh        = { git = "https://github.com/aster-rpc/iroh",        rev = "77a68a5477c15db261c66e7740a77ef752d9685f" }
iroh-base   = { git = "https://github.com/aster-rpc/iroh",        rev = "77a68a5477c15db261c66e7740a77ef752d9685f" }
iroh-relay  = { git = "https://github.com/aster-rpc/iroh",        rev = "77a68a5477c15db261c66e7740a77ef752d9685f" }
iroh-blobs  = { git = "https://github.com/aster-rpc/iroh-blobs",  rev = "ede454c774b24e1e4674aa713a37573b4144517d" }
iroh-docs   = { git = "https://github.com/aster-rpc/iroh-docs",   rev = "be04181e05e566adb033af786e09c4d20ac9390f" }
iroh-gossip = { git = "https://github.com/aster-rpc/iroh-gossip", rev = "d7d13582ba81d1b266cce758daccacad3ab5357c" }
noq         = { git = "https://github.com/aster-rpc/noq",         rev = "617899fe7e983f794e0162d15d8558258fa8c375" }
noq-udp     = { git = "https://github.com/aster-rpc/noq",         rev = "617899fe7e983f794e0162d15d8558258fa8c375" }
noq-proto   = { git = "https://github.com/aster-rpc/noq",         rev = "617899fe7e983f794e0162d15d8558258fa8c375" }
```

Without this block the build fails to resolve the forked iroh/noq revisions.

## Status

Step 1 (node + blobs/docs/gossip + admission + deterministic namespaces) is
implemented. Rust-native **RPC services** are designed but not yet shipped — see
`docs/_internal/aster-rust.md`.
