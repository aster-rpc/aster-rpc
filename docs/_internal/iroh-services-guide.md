# Iroh Services Guide

How Iroh's three core services — Docs, Blobs, and Gossip — work, how they replicate, and how they relate to each other.

## Docs (KV Store)

Docs is a CRDT-based distributed key-value store.

### Access model

- Each document has a unique **namespace ID**
- Connecting to an endpoint alone grants no visibility into any document
- Access is controlled via **capability tickets**: a write ticket grants read+write, a read ticket grants read-only
- One node creates the document, then shares a ticket with peers who join using it

### Setup for distributed KV

**Node A (bootstrap):**
```rust
let doc = docs.create().await?;
let ticket = doc.share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses).await?;
// distribute this ticket to all peers

let author = docs.create_author().await?;
doc.set_bytes(author, "greeting", "hello from A").await?;
```

**Every other node (join with ticket):**
```rust
let doc = docs.import(ticket).await?;
let author = docs.create_author().await?;
doc.set_bytes(author, "greeting", "hello from B").await?;
```

### Multi-writer semantics

Keys are actually keyed by `(author, key)`. If A and B both write `"greeting"`, those are two separate entries. To get a single-value-per-key semantic, query all authors for a key and pick the latest by timestamp.

### Replication

- Writes propagate automatically via **gossip** to all peers who have joined the document
- When a peer goes offline and comes back, other peers sync the full document state back (including entries the returning peer originally wrote)
- **Memory store caveat**: author keypairs are lost on restart (you'll create a new author ID), and if ALL peers use memory stores and ALL go offline simultaneously, everything is lost
- For durable setups, use `Docs::persistent(path)` on at least some nodes

### Recovering documents after restart (persistent store)

With `Docs::persistent(path)`, documents and author keypairs survive restarts. To reopen them:

**By saved NamespaceId** (preferred — save the ID to your own config/state on creation):
```rust
// On creation
let ns_id = doc.id();  // NamespaceId — save this somewhere
save_to_config("doc1", ns_id);

// After restart
let ns_id = load_from_config("doc1");
let doc = docs.open(ns_id).await?.expect("doc should exist");
```

**By listing all docs** (if you didn't save the IDs):
```rust
let mut stream = docs.list().await?;
while let Some((ns_id, capability)) = stream.try_next().await? {
    // match on ns_id or inspect capability to find your docs
}
```

Note: `open()` returns `Result<Option<Doc>>`, but in practice `None` indicates a backend error, not "not found."

---

## Blobs (Content-Addressed Storage)

Blobs is a content-addressed storage layer. Data is identified by its **BLAKE3 hash**.

### How it works

```rust
// Add bytes locally, get back a hash
let outcome = blobs.add_bytes(b"hello world").await?;
let hash = outcome.hash;

// Another peer fetches by hash + node ID
blobs.download(hash, node_id).await?;
```

### Key properties

- **Immutable**: a hash always maps to the same content. New content = new hash.
- **Deduplication**: same content on multiple peers = same hash, only transferred once.
- **Verified streaming**: content is verified against the hash as it streams in (BLAKE3 supports this natively).
- **No discovery**: unlike BitTorrent, there's no DHT. You need to know which peer has the blob.
- **Collections (HashSeq)**: a blob can be an ordered list of hashes, representing directories or file sets.

### Replication

**Not automatically replicated.** Blobs are pull-based.

Adding a blob to your node does NOT push it to other peers. They must explicitly request it by hash.

**Exception**: blobs referenced by docs entries. When a doc entry syncs to another peer, that peer automatically fetches the referenced blob content. This is docs triggering the download, not blobs itself.

---

## Gossip (Pub-Sub)

Gossip is fire-and-forget pub-sub messaging. No persistence, no replay.

### How it works

```rust
// Subscribe to a topic (32-byte ID)
let (sender, receiver) = gossip.subscribe(topic_id, peer_list).await?;

// Broadcast
sender.broadcast(b"hey everyone".into()).await?;

// Receive
while let Some(event) = receiver.try_next().await? {
    match event {
        Event::Received(msg) => { /* got a message */ }
        Event::NeighborUp(peer) => { /* peer joined */ }
        Event::NeighborDown(peer) => { /* peer left */ }
    }
}
```

### Key properties

- **Online only**: if you're subscribed, you get messages. If you're offline, they're gone.
- **No history, no catch-up**: no log to replay. Joining a topic only shows messages from that point forward.
- **Role in the stack**: gossip is the transport layer for docs sync. Change notifications go out over gossip; peers then fetch blob content. But gossip itself doesn't know about docs or blobs.

---

## How they fit together

```
Docs  ──uses──>  Gossip  (to broadcast entry changes)
Docs  ──uses──>  Blobs   (to store/fetch entry values)
Gossip           (standalone pub-sub, no persistence)
Blobs            (standalone content store, pull-only)
```

When you `doc.set_bytes(author, key, value)`:
1. The value is stored as a **blob** (hashed, content-addressed)
2. The doc entry stores `(author, key) -> (hash, size, timestamp)`
3. The entry change is broadcast via **gossip**
4. Peers receive the entry, then fetch the **blob** content

## Comparison

| | Blobs | Docs | Gossip |
|---|---|---|---|
| Model | Content-addressed store | CRDT key-value | Pub-sub messaging |
| Replication | Pull (on-demand) | Push (automatic via gossip) | Broadcast (live only) |
| Persistence | Yes (if persistent store) | Yes (entries + blob refs) | None |
| Offline catch-up | Fetch by hash if peer available | Full re-sync from peers | No replay |
