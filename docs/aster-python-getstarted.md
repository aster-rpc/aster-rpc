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

`Node.start(config=None, *, alpns=None)` is a coroutine that returns a running
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
| `take_hook_receiver()`, `has_hooks()` | admission hooks (needs `enable_hooks=True`) |
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
to both. `AsterServer` turns on admission hooks (failing closed) when the
consumer gate is enabled.
