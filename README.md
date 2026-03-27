# iroh-python

Python bindings for the [Iroh](https://iroh.computer) peer-to-peer networking library.

Built with [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs), providing async/await access to Iroh's full protocol suite:

- **QUIC networking** — NAT-traversing connections with bi-directional streams and datagrams
- **Blobs** — Content-addressed storage with BLAKE3 hashing
- **Docs** — Collaborative CRDT documents that sync across peers
- **Gossip** — Topic-based pub-sub messaging

## Quick Start

### Prerequisites

- Python ≥ 3.9
- Rust toolchain (for building from source)
- [uv](https://docs.astral.sh/uv/) (recommended) or pip

### Install from source

```bash
git clone https://github.com/user/iroh-python.git
cd iroh-python

# Using uv (recommended)
uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout
uv run maturin develop -m iroh_python_rs/Cargo.toml

# Or using pip
# python -m venv .venv && source .venv/bin/activate
# pip install maturin pytest pytest-asyncio pytest-timeout
# maturin develop -m iroh_python_rs/Cargo.toml
```

### Usage

#### Store and retrieve blobs

```python
import asyncio
from iroh_python import IrohNode, blobs_client

async def main():
    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_str = await blobs.add_bytes(b"Hello, Iroh!")
    data = await blobs.read_to_bytes(hash_str)
    print(data)  # b"Hello, Iroh!"

    await node.shutdown()

asyncio.run(main())
```

#### QUIC echo server & client

```python
import asyncio
from iroh_python import create_endpoint

ALPN = b"iroh/echo/1"

async def main():
    server = await create_endpoint(ALPN)
    client = await create_endpoint(ALPN)

    server_id = await server.endpoint_id()
    conn = await client.connect(server_id, ALPN)

    send, recv = await conn.open_bi()
    await send.write_all(b"ping")
    await send.finish()

    server_conn = await server.accept()
    s_recv, s_send = await server_conn.accept_bi()
    data = await s_recv.read_to_end(1024)
    await s_send.write_all(data)
    await s_send.finish()

    reply = await recv.read_to_end(1024)
    print(reply)  # b"ping"

asyncio.run(main())
```

#### Gossip messaging

```python
import asyncio
from iroh_python import IrohNode, gossip_client

TOPIC = bytes(range(32))

async def main():
    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()
    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    g1, g2 = gossip_client(node1), gossip_client(node2)
    id1, id2 = await node1.node_id(), await node2.node_id()

    t1, t2 = await asyncio.gather(
        g1.subscribe(TOPIC, [id2]),
        g2.subscribe(TOPIC, [id1]),
    )

    await t1.broadcast(b"hello from node1")
    event_type, data = await t2.recv()
    print(data)  # b"hello from node1"

    await node1.shutdown()
    await node2.shutdown()

asyncio.run(main())
```

## Running Tests

```bash
uv run pytest tests/ -v --timeout=30
```

## Run lint locally

```bash
cargo fmt --manifest-path iroh_python_rs/Cargo.toml --check
cargo clippy --manifest-path iroh_python_rs/Cargo.toml -- -D warnings
```

## Enable pre-push checks

```bash
./scripts/install-git-hooks.sh
```

After that, every `git push` will run `cargo fmt --check` and `cargo clippy` before pushing.

## API Overview

| Class / Function | Description |
|---|---|
| `IrohNode.memory()` | Create an in-memory Iroh node |
| `blobs_client(node)` | Get blob storage client |
| `docs_client(node)` | Get CRDT documents client |
| `gossip_client(node)` | Get gossip pub-sub client |
| `net_client(node)` | Get low-level QUIC networking client |
| `create_endpoint(alpn)` | Create a bare QUIC endpoint |

## Architecture

```
Python:  IrohNode.memory() → node
         blobs_client(node)  → BlobsClient
         docs_client(node)   → DocsClient
         gossip_client(node) → GossipClient
         net_client(node)    → NetClient

Rust:    Endpoint::bind(presets::N0)
         + Router (ALPN mux)
         + BlobsProtocol (iroh-blobs 0.99)
         + Docs (iroh-docs 0.97)
         + Gossip (iroh-gossip 0.97)
```

## License

Apache-2.0