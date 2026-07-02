# Sealed grants: hand a secret to one node over a public doc

`aster::grants` distributes secrets — typically an iroh-docs namespace
capability — to **individual nodes** through replicated storage. A root node
writes an HPKE-sealed grant into a publicly-replicated policy doc; only the
addressed node can open it, with a key derived from its identity secret.
No pairwise RPC, no requirement that both ends are online together, nothing
extra to publish: the recipient's `aster1` node id (from any ticket) *is*
the encryption address.

The classic use: a root provisions a shared doc namespace and grants each
admitted node read or write access to it, entirely through records.

---

## 1. The pieces

```text
root (granter)                            recipient node
──────────────                            ──────────────
GrantContext { … }                        GrantContext { … }   (identical!)
seal_namespace_grant(ctx, capability)     PolicyDoc::import_read_only(…)
doc.set_bytes(author, grant_key(…), •)    policy.fetch_grant(prefix, my_id)
        │                                 open_namespace_grant(my_secret, ctx, •)
        └── policy doc replicates ──────────────┘
```

- **`GrantContext`** binds the grant to *app label, granter, recipient,
  resource, doc path, and role* via the AEAD's associated data. Seal and
  open must use an identical context — a grant record moved to another
  node, resource, role, or path simply fails to open. You never build the
  AAD yourself.
- **`PolicyDoc`** reads a replicated doc through a **pinned author**. HPKE
  gives confidentiality, not sender authenticity; authenticity comes from
  only trusting records the root wrote. Aster's docs author *is* the node
  identity key, so pinning an author pins a node.
- **`grant_key(prefix, node)`** is the **doc entry key** convention
  (`<prefix>/grants/<node_id>`) — an *address*, not key material. It's
  public on purpose: recipients find their grant by direct lookup; the
  granter enumerates grants by prefix.

### What's public vs. what's secret

Everything in the policy doc is public to anyone who replicates it — the
grant record's location (the doc key) *and* its contents (the HPKE
envelope). That's fine, because the envelope is sealed to the recipient's
X25519 public key, which is derived from its Ed25519 node id. A node id
lets anyone **address** a grant to that node; **opening** one requires the
matching X25519 *private* key, derived from the node's identity **secret**
(the `my_identity_secret` argument below), which never leaves the node.
Same asymmetry as a PGP fingerprint: knowing it lets you encrypt *to*
someone, never read their mail.

## 2. Root side: seal and publish

```rust
use aster::grants::{grant_key, seal_namespace_grant, GrantContext};
use aster::{NamespaceCapability, NamespaceSecret};

// The namespace being shared (e.g. a data doc the fleet should write to).
let shared = NamespaceSecret::from_bytes(secret_32_bytes);
let namespace_id = shared.id();

// The doc *entry key* (a public path string, NOT key material):
let record_key = grant_key("/myapp/v1", &recipient_node_id);
let ctx = GrantContext {
    app: "myapp/data-grant/v1",          // your versioned label
    granter: &root_node.id(),
    recipient: &recipient_node_id,
    resource: &namespace_id.to_bytes(),  // what this grant unlocks
    path: &record_key,                   // the exact doc entry key below
    role: "write",
};
let sealed = seal_namespace_grant(&ctx, &NamespaceCapability::write(shared))?;

let docs = root_node.docs();
let policy_doc = docs.create().await?;               // or reopen the existing one
let author = docs.default_author().await?;           // == the root's node id
policy_doc.set_bytes(&author, record_key.into_bytes(), sealed).await?;
// share the policy doc read-only: policy_doc.share(ShareMode::Read)
```

Role/capability consistency is checked at seal and open: a `Write`
capability under `role: "read"` (or vice versa) is rejected up front.

## 3. Recipient side: fetch and open

```rust
use aster::grants::{grant_key, open_namespace_grant, GrantContext, PolicyDoc};

let docs = my_node.docs();
let policy = PolicyDoc::import_read_only(&docs, policy_namespace_id, root_author).await?;
policy.doc().start_sync(&[root_node_id.clone()]).await?;

let record_key = grant_key("/myapp/v1", &my_node.id());
let ctx = GrantContext {
    app: "myapp/data-grant/v1",
    granter: &root_node_id,
    recipient: &my_node.id(),
    resource: &expected_namespace_id.to_bytes(),
    path: &record_key,
    role: "write",
};

if let Some(record) = policy.fetch_grant("/myapp/v1", &my_node.id()).await? {
    let capability = open_namespace_grant(&my_identity_secret, &ctx, &record)?;
    // Import and use the granted namespace:
    let data_doc = match capability {
        aster::NamespaceCapability::Write(secret) =>
            docs.open_or_import_write_namespace(secret).await?,
        aster::NamespaceCapability::Read(id) =>
            docs.open_or_import_read_namespace(id).await?,
    };
}
```

Watch for changes with `policy.subscribe()`, filtering events with
`policy.is_policy_write(&event)` — only the pinned author's inserts count as
policy.

## 4. Arbitrary payloads and standalone keys

The typed helpers cover the common case (namespace capabilities). For any
other secret, `seal_grant` / `open_grant` take raw bytes under the same
context discipline:

```rust
let sealed = aster::grants::seal_grant(&ctx, b"any secret bytes")?;
let opened = aster::grants::open_grant(&my_identity_secret, &ctx, &sealed)?;
opened.expose_secret(); // &[u8]
```

Grants are addressed to the recipient's **identity-derived** HPKE key by
default — nothing to publish, but rotating the recipient key means rotating
the node identity. If a deployment needs recipient-key rotation without
identity rotation, publish a standalone X25519 key and use
`seal_grant_to_key` / `open_grant_with_key`. **Keep the two schemes
explicit** in whatever record publishes the key: a grant sealed to a key the
daemon doesn't use fails only at open time, which is a miserable thing to
debug.

## 5. Granting to many nodes at once — group identities

Sealing is per-recipient at the crypto layer, but you don't have to seal
every grant N times. For a set of nodes that should all open the same
grants, use a **group identity** — a plain keypair acting as a shared
encryption address:

```rust
// Root, once: mint the group. Its public key is the "recipient".
let group_secret = SecretKey::generate();
// …derive group_id from the public key, as for any node id.

// Once per member (the bootstrap): seal the GROUP SECRET to each member
// with an ordinary per-member grant (app label e.g. "myapp/group-membership/v1").

// Thereafter, every grant to the group is sealed ONCE:
let ctx = GrantContext { recipient: &group_id, /* … */ };
let sealed = seal_namespace_grant(&ctx, &capability)?;
// Any member opens it with the group secret:
let cap = open_namespace_grant(&group_secret, &ctx, &sealed)?;
```

Joins cost one seal (the new member immediately opens all existing group
grants). Leaves cost a group-key rotation — which you'd owe anyway, since a
hostile leaver keeps every secret it learned (see below). The trade: the
AAD binds the *group*, not the individual, so you lose per-member
attribution, and a compromised member exposes every group grant. Use
individual grants where audit or differing capability sets matter; groups
for uniform fleet-wide capabilities.

## 6. Revocation — know the limit

Tombstoning a grant record (write an empty/revoked marker; recipients drop
the capability on their next reconcile) stops an **honest** node. A
malicious node that already learned a namespace secret still has it. True
revocation of a leaked capability = rotate the underlying namespace and
re-grant to the remaining nodes. Design your app's reconcile loop
accordingly: treat "grant absent/unopenable" as "drop local capability and
tear down sessions".

Design background and roadmap (reconcile-loop helper, standalone-key policy
records, namespace rotation): `docs/_internal/aster-sealed-grants.md`.
