# Aster for Python: getting started

The Python binding (`pip install aster-rpc`, `import aster`) wraps the Rust
`aster` crate. Configuration, the node, and node identity are the Rust ones;
this page covers the Python surface. Fuller RPC coverage follows with the rest
of the binding catch-up (plan P10).

## 1. Start a node

```python
import asyncio
from aster import AsterConfig, Node, blobs_client

async def main():
    config = AsterConfig(storage_path="/var/lib/my-app")  # omit for in-memory
    async with await Node.start(config) as node:          # shut down on exit
        print("node id:", node.id)
        blobs = blobs_client(node)
        ...

asyncio.run(main())
```

`Node.start(config=None, *, alpns=None, gate=None)` is a coroutine that returns a running
`Node`. The node is an async context manager, so `async with await
Node.start(...)` closes it on exit (the same shape as `asyncio.start_server`).
Without `async with`, call `await node.shutdown()` yourself; it flushes
persistent state before closing.

| `Node` member | Returns |
|---|---|
| `id` (property) | `NodeId`, which compares and hashes equal to its 64-char hex string |
| `node_id()` | the id as a hex `str` |
| `node_addr_info()` | `NodeAddr` (id + relay + direct addresses) |
| `secret_key()` / `export_secret_key()` | `SecretKey` / 32 raw bytes |
| `accept_aster()` | `(alpn: bytes, IrohConnection)` for custom ALPNs passed via `alpns` / `config.alpns` |
| `admission(*, outbound=False)` | Gate 0 requests as an async iterator (needs `enable_hooks=True`, no `gate`); see section 5 |
| `gate` (property), `has_hooks()` | the attached `GatePolicy` (or `None`); whether admission hooks are on |
| `add_node_addr(other)` | teach this node another node's address (tests, local setups) |
| `transport_metrics_prometheus()` | transport metrics in Prometheus text format |
| `shutdown()` / `close()` | clean shutdown |

The protocol clients take the node: `blobs_client(node)`, `docs_client(node)`,
`gossip_client(node)`, `net_client(node)`.

## 2. Configure: `AsterConfig`

`AsterConfig` is a frozen dataclass mirroring the Rust `aster::AsterConfig`.
Rust loads and checks every value. Settings resolve in this order, later
sources overriding earlier ones:

1. built-in defaults
2. `aster.toml`, read by `AsterConfig.from_file(path)`
3. `ASTER_*` environment variables, read by `AsterConfig.from_env()` (and by
   `from_file`, which applies them on top of the file)
4. code: keyword arguments, or `config.replace(**changes)` for a modified copy

```python
config = AsterConfig.from_file("aster.toml").replace(bind_addr="0.0.0.0:9000")
```

The keyword names are the Rust field names, which are also the TOML keys. The
complete builder / env / TOML table is in
[aster-rust-getstarted.md section 2.2](./aster-rust-getstarted.md#22-configuration-reference).
Two spellings differ in Python:

- `gc_interval_ms: int | None` stands in for the builder's `gc_interval(Duration)`.
  `None` or `0` turns GC off.
- `relay_mode` takes `"default"`, `"staging"`, `"disabled"` or `"custom"`.
  Setting `relay_urls=[...]` selects `"custom"`, and so does a bare URL passed
  as `relay_mode`.

Invalid values raise when the config is built: `ValueError` for a bad value,
`TypeError` for a wrong type or an unknown field, and `FileNotFoundError` when
`from_file` cannot find its file. `config.print_config()` prints the resolved
settings with the source of each one and masks secrets.

## 3. Node identity

A node's identity is its secret key. On disk, that key is a **secret key
file**: 64 hex characters, written with mode 0600. The Rust SDK and every
binding use this same format.

```python
from aster import AsterConfig, SecretKey

config = AsterConfig(secret_key_file="/var/lib/my-app/node.key")
```

- When the file exists, the node loads its key from it. When it does not,
  `Node.start` creates it, using `secret_key` if one is set and a fresh key
  otherwise.
- The start fails with `ValueError` if the file is malformed, or if
  `secret_key` is also set to a different key.
- `SecretKey` provides `generate()`, `from_hex()`, `from_bytes()`,
  `read_file()`, `read_file_if_exists()`, `write_file()` (which never
  overwrites an existing file), `load_or_generate()`, `public_key()` and
  `node_id()`. Its `repr` never shows the key.
- `aster keygen producer --out PATH` writes a key file in this format and
  prints the node id it gives.

A persistent node started directly with `Node.start` gets a fresh identity on
every start unless you set `secret_key` or `secret_key_file`. `AsterServer` and
`AsterClient` handle this for you: when `storage_path` is set and no key is
configured, they use `<storage_path>/node.key`.

The `.aster-identity` TOML file, selected by `identity_file` or found
automatically in the working directory, is a *credential* file. It holds peers,
the root key and enrollment material, not the node key file. If it has a
`[node] secret_key`, that key is used when the config does not set one.

## 4. `AsterServer` / `AsterClient`

Both take `config=AsterConfig(...)`, or read `AsterConfig.from_env()` when no
config is given, and start their node with `Node.start`. Persistent storage,
relay settings, transport tuning and the identity rules above therefore apply
to both.

`AsterServer` always attaches a Rust Gate 0 (section 5), exposed as
`server.gate`. It is `PROTECTED` (failing closed) when consumer admission is
on (`allow_all_consumers=False`) and `OPEN_DEV` otherwise; pass
`AsterServer(..., gate=GatePolicy(...))` to supply your own. A peer that
passes credential admission is admitted into `server.gate` with the
credential's attributes and expiry (capped by `ASTER_PEER_TTL_S`, default 24
hours), and RPC handlers read those attributes from the same store.
`server.gate.revoke(peer_id)` shuts the peer out of new connections.

## 5. Gate 0: connection-level admission

Gate 0 runs in Rust (the `aster` crate's `GatePolicy` and
`PeerAdmissionStore`). Admission ALPNs (`ALPN_CONSUMER_ADMISSION`,
`ALPN_PRODUCER_ADMISSION`, `ALPN_DELEGATED_ADMISSION`) are always open, so an
unknown peer can present a credential. In `TrustMode.PROTECTED` every other
ALPN (RPC, blobs, docs, gossip, your own) needs an unexpired admission. In
`TrustMode.OPEN_DEV` everything is open.

```python
from aster import AsterConfig, GatePolicy, Node, TrustMode

gate = GatePolicy(TrustMode.PROTECTED)    # or GatePolicy.open_dev()
gate.set_reject(403, b"peer not admitted")  # QUIC close code + reason
node = await Node.start(AsterConfig(), gate=gate)

gate.admit(peer_id, attributes={"aster.role": "reader"}, ttl=3600)
gate.is_admitted(peer_id)       # True
gate.attributes(peer_id)        # {"aster.role": "reader"}
gate.revoke(peer_id)            # new connections are rejected again
```

With a gate attached, every inbound connection is decided in Rust and nothing
reaches Python. Rejected peers see the gate's close code and reason. Setters
(`trust_mode`, `admission_alpns`, `set_reject`) and admissions apply to the
next connection. `gate.store` is the live `PeerAdmissionStore` (`admit`,
`revoke`, `get` returning a `PeerAdmission` record, `attributes`,
`prune_expired`, `len`, `in`); a store can be shared between gates with
`GatePolicy(store=...)`. `admit` takes `attributes`, `expires_at` (epoch
seconds) or `ttl` (seconds), `source`, `owner_id`, `proof_hash` and
`policy_epoch`. Peer ids are `NodeId`s or 64-hex strings.

To decide in Python instead, start the node with `enable_hooks=True` and no
gate, then iterate its admission requests:

```python
node = await Node.start(AsterConfig(enable_hooks=True))
async for req in node.admission():      # HandshakeRequest per inbound peer
    if allowed(req.peer, req.alpn):
        req.accept()
    else:
        req.reject(4031, b"not on the list")
```

`node.admission(outbound=True)` also yields a `ConnectRequest` for each of
this node's own dials (otherwise they are allowed). A request left unanswered
falls back to `hook_failure_mode` after `hook_timeout_ms`. The iterator ends
when the node closes.

## 6. Blobs, docs and the network layer

These wrap the Rust SDK's `Blobs`, `Docs`/`Doc` and `Connection`. Every hash
or node-id argument takes a typed `Hash` / `NodeId` or a hex string; namespace
arguments take 32 bytes or 64 hex characters.

Blobs (`blobs_client(node)`):

| Call | Returns |
|---|---|
| `read_range(hash, offset, length)` | bytes |
| `download_hash(hash, provider, format=BlobFormat.RAW)` | bytes |
| `download_hash_to_store(hash, provider, format=...)` | `None` (the blob is stored, not copied out) |
| `download_hash_from(hash, providers, format=..., strategy=FetchStrategy.WHOLE)` | `(bytes, FetchReport)` |
| `download_hash_to_store_from(hash, providers, format=..., strategy=...)` | `FetchReport` |
| `download_collection_from(hash, providers, strategy=...)` | `([(name, bytes)], FetchReport)` |
| `tag_set(name, hash, format=...)`, `tag_delete(name)`, `tag_delete_prefix(prefix)` | `None` / count removed |
| `gc_run_once()` | `None`, after one GC pass over every untagged blob |

`providers` is an ordered list: each is tried in turn, and a later one fetches
only what is still missing. `FetchStrategy.SPLIT_CHILDREN` fetches each child
of a HashSeq as its own request and needs `BlobFormat.HASH_SEQ`. A
`FetchReport` has `providers` (`ProviderOutcome`: `node_id`, `attempts`,
`failures`, `completed_requests`), `attempted()`, `failed()`, `served()` and
`bytes_transferred`. `format` also accepts `"raw"` / `"hash_seq"`.

Docs (`docs_client(node)` and its `DocHandle`s):

- `DocsClient`: `default_author()` (equals the node id), `open(namespace)`
  (`None` if not local), `open_or_import_read_namespace(namespace)`,
  `open_or_import_write_namespace(secret)`, `authority_report(namespace)`,
  `retire_local_write_authority(namespace)`.
- `DocHandle`: `set_bytes_durable(author, key, value)` (survives a hard kill),
  `entry_exists(author, key)` (true for a deletion tombstone too),
  `entry_content_status(hash)` (`BlobStatusResult`),
  `read_entry_contents_if_complete(hashes)`, `authority_report()`,
  `shutdown_for_role_swap(budget_seconds)`, `share(ShareMode.READ)`,
  `set_download_policy(DownloadPolicy.nothing_except([b"prefix/"]))`,
  `get_download_policy()`.
- `AuthorityReport` has `namespace_id`, `namespace_hex`, `access`
  (`LocalAccess.WRITE` / `READ`, as stored) and `fingerprint`
  (`AuthorityFingerprint`, comparable with `AuthorityFingerprint.of(namespace,
  access)`). `RetireOutcome` has `retired`, `access` and `handles` (open
  references that made it refuse; release them with
  `shutdown_for_role_swap`).
- Events are async iterators: `async for ev in doc.events(): ...` (or
  `await doc.subscribe()`); `ev.kind` says which fields are set.

Network: `IrohConnection.paths()` lists every open `PathInfo` (`kind`
`"direct"` / `"relay"` / `"other"`, `addr`, `relay_url`, `is_selected`,
`rtt_micros`) and `selected_path()` returns the one carrying data.
`IrohSendStream.set_priority(n)` / `priority()` order send streams on one
connection (higher first, default 0).
