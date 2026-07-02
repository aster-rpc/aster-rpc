# Baseline services: `aster.ops.NodeInfo`, typed values, and union payloads

Every Aster node can expose a small set of **baseline services** — the same
RPCs, under the reserved `aster.*` namespace, on every node regardless of
which application runs there. This is what lets a control panel, CLI, or
monitor dial any node and ask "who are you?" without out-of-band knowledge.

Shipped today (Rust; other bindings follow with the FFI updates):

| Service | Method | Returns |
|---|---|---|
| `aster.ops.NodeInfo` (v1) | `describe()` (idempotent) | `aster/NodeIdentity` |

> Status: Rust↔Rust, like the rest of the Rust RPC layer. The contract
> identity (including the `aster/Value` union) is cross-binding-correct;
> union *payload* codecs in Python/TS/Java land with the cross-binding
> payload work.

---

## 1. The reserved namespace

`aster.*` service names and the `aster/…` wire-type package are **reserved
for the framework** and enforced at compile time:

```rust
#[aster::service(name = "aster.ops.MyThing", version = 1)]  // ✗ compile error
trait MyThing { /* … */ }

#[derive(ForyStruct, aster::AsterType)]
#[aster(wire = "aster/MyType")]                              // ✗ compile error
struct MyType { /* … */ }
```

Both fail with "reserved: the `aster` namespace is for baseline
services/types shipped with the framework". Only the exact namespace is
reserved — `asterix.Foo` or `aster-app/Bar` are fine. Use your own package
(`myapp.MyThing`, `myapp/MyType`).

## 2. `aster.ops.NodeInfo` — on by default

An `AsterServer` serves `aster.ops.NodeInfo` automatically. Give it an
identity to serve:

```rust
use aster::rpc::baseline::{Attr, NodeIdentity};
use aster::rpc::AsterServer;

let srv = AsterServer::builder()
    .node_identity(NodeIdentity {
        node_name: "worker-7".into(),
        tags: vec!["blue".into(), "eu".into()],
        roles: vec!["worker".into()],
        attributes: vec![
            Attr::new("region", "eu-west"),      // Text
            Attr::new("capacity", 512i64),       // Int
            Attr::new("beta", true),             // Bool
            Attr::null("draining"),              // declared null
        ],
        ..Default::default()                     // node_id is stamped for you
    })
    // .service(MyService)  ← optional; a bare node is still startable & describable
    .start()
    .await?;
```

`node_id` is always stamped from the node's key at start — whatever you set
is overwritten, so the served identity can't lie about who it is.

Dial it from anywhere:

```rust
use aster::rpc::baseline::NodeInfoClient;

let conn = client_node.rpc_connect(&server_id).await?;
let info = NodeInfoClient::new(conn);
let identity = info.describe().await?;
println!("{} ({})", identity.node_name, identity.node_id);
for attr in &identity.attributes {
    match &attr.value {
        Some(v) => println!("  {} = {:?}", attr.name, v),
        None => println!("  {} = null", attr.name),
    }
}
```

### Updating the identity at runtime

The builder value is the *initial* record, not a frozen snapshot. The server
holds a live, clone-shared handle (same pattern as `AttributeStore`) — grab
it and update the served identity while the node runs:

```rust
let identity = srv.node_identity().expect("baseline enabled"); // NodeIdentityHandle, cheap to clone

// During a rolling deploy:
identity.update(|id| {
    id.attributes.retain(|a| a.name != "draining");
    id.attributes.push(Attr::new("draining", true));
});

// Or replace wholesale:
identity.set(NodeIdentity { node_name: "worker-7b".into(), ..Default::default() });
```

The next `describe()` call serves the updated record. `node_id` stays pinned
through both `set` and `update` — whatever you write into that field is
ignored, so the identity can never claim to be another node.
`srv.node_identity()` returns `None` when the baseline service was disabled.

### Gating and disabling

By default `describe` is open to any dialer the transport admits. To require
a role (Gate 3), or to turn the service off entirely:

```rust
use aster::rpc::require_role;

AsterServer::builder()
    .node_info_requires(require_role("operator"))  // gate every method
    // …or…
    .builtin_node_info(false)                      // disable entirely
```

With the baseline service disabled and no `.service(...)` registered,
`start()` refuses with `InvalidArgument` (nothing to serve).

`NodeIdentity` is deliberately **generic identity only**: id, name, tags,
roles, typed attributes. Domain policy — admission state, expiry, your app's
records — belongs in your own service, which can embed `NodeIdentity` in its
own response types rather than re-declaring `node_id`.

## 3. `aster/Value` — the typed name/value primitive

Baseline payloads share one typed value primitive: `Value` is a
cross-binding **union** of `Int(i64) | Float(f64) | Bool(bool) |
Text(String)`, and `Attr` pairs it with a name. Nullability is positional —
`Attr.value` is `Option<Value>`; there is no `Null` variant.

```rust
use aster::rpc::baseline::Value;

let v = Value::from(42i64);
assert_eq!(v.as_int(), Some(42));
assert_eq!(v.as_text(), None);      // no cross-kind coercion

let t: Value = "eu-west".into();
assert_eq!(t.as_text(), Some("eu-west"));
```

Under the hood each variant wraps a one-field message (`aster/IntValue`
etc.) rather than a bare primitive — ContractIdentity §11.3.3 v1 admits
only message-typed union variants, and that constraint is what keeps the
contract identity byte-identical across all four bindings today. The `From`
/ `as_*` helpers mean you never touch the wrappers directly.

## 4. Defining your own union payloads

`Value` is built on machinery you can use for your own RPC payloads:
`#[derive(AsterType)]` now accepts data-carrying enums, paired with Fory's
`ForyUnion` derive for serialization:

```rust
use fory_derive::{ForyStruct, ForyUnion};

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "myapp/Circle")]
pub struct Circle { pub radius: f64 }

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "myapp/Rect")]
pub struct Rect { pub w: f64, pub h: f64 }

#[derive(ForyUnion, aster::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "myapp/Shape")]
pub enum Shape {
    #[fory(default)]
    Circle(Circle),
    Rect(Rect),
    /// Mandatory ForyUnion forward-compat variant. Runtime-only:
    /// it is NOT part of the cross-binding contract.
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}
```

The rules (all enforced with compile errors that say so):

- **Every variant wraps exactly one message** (`#[derive(AsterType)]`
  struct). Primitive payloads like `Int(i64)` are outside the v1
  cross-binding contract — wrap them, as `Value` does.
- **No unit variants.** Absence is `Option<Shape>` on the field that uses
  the union, not a variant.
- **One `#[fory(default)]` variant** (schema-evolution fallback) and the
  trailing `#[fory(unknown)] Unknown(fory_core::UnknownCase)` variant are
  ForyUnion requirements. The `Unknown` variant is excluded from the
  contract.
- **Case ids** default to declaration order (0-based, `Unknown` excluded);
  pin them with `#[fory(id = N)]` if you plan to reorder variants — the
  contract hash sorts by id, so explicit ids make declaration order
  irrelevant.

A union used in a service's request/response types registers itself and its
variant messages transitively — nothing extra to wire up.

## 5. What's next

The baseline catalog grows from here (`aster.ops.Health`, `aster.ops.Manifest`
reflection, `aster.ops.Logs`, …) — see `docs/_internal/aster-baseline-services.md`
for the roadmap and design rationale. Python/TS/Java/.NET surfaces for
`NodeInfo` arrive with the binding FFI updates.
