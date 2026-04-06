# aster-python

Python bindings for the [Iroh](https://iroh.computer) peer-to-peer networking library.

Built with [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs), providing async/await access to Iroh's full protocol suite:

- **QUIC networking** — NAT-traversing connections with bi-directional streams and datagrams
- **Blobs** — Content-addressed storage with BLAKE3 hashing
- **Docs** — Collaborative CRDT documents that sync across peers
- **Gossip** — Topic-based pub-sub messaging

## Quick Start

### Prerequisites

- Python >= 3.9
- Rust toolchain (for building from source)
- [uv](https://docs.astral.sh/uv/) (recommended) or pip

### Install from source

```bash
git clone https://github.com/emrul/iroh-python.git
cd iroh-python

# Using uv (recommended)
uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout
uv run maturin develop -m bindings/python/rust/Cargo.toml

# Or using pip
# python -m venv .venv && source .venv/bin/activate
# pip install maturin pytest pytest-asyncio pytest-timeout
# maturin develop -m bindings/python/rust/Cargo.toml
```

### Optional: speed up local Rust builds with sccache

```bash
# macOS
brew install sccache

# one-off shell session
export RUSTC_WRAPPER=sccache
sccache --start-server || true
```

You can also use `./scripts/validate.sh`, which now auto-enables `sccache` when installed and prints cache stats at the end.

### Usage

#### Store and retrieve blobs

```python
import asyncio
from aster import IrohNode, blobs_client

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
from aster import create_endpoint

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
from aster import IrohNode, gossip_client

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

## Developer Setup

### Full setup (build + CLI)

```bash
uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout pytest-rerunfailures
uv pip install "blake3>=1.0.8" "pyfory==0.16.0" "zstandard>=0.25.0"
uv run maturin develop -m bindings/python/rust/Cargo.toml
uv pip install -e cli/
```

Or use the build script which also regenerates type stubs:

```bash
./scripts/build.sh
```

### Running tests

```bash
# All tests
uv run pytest tests/python/ -v --timeout=30

# Unit tests only (no networking, fast)
uv run pytest tests/python/ -v --timeout=30 -m "not network"

# Single file
uv run pytest tests/python/test_blobs.py -v --timeout=30
```

### Lint / format

```bash
cargo fmt --manifest-path bindings/python/rust/Cargo.toml
cargo clippy --manifest-path bindings/python/rust/Cargo.toml -- -D warnings
```

### Full validation (mirrors CI)

```bash
./scripts/validate.sh
```

### Pre-push hook

A pre-push hook runs `cargo fmt --check` and `cargo clippy` before allowing pushes, preventing CI lint failures:

```bash
git config core.hooksPath .githooks
```

This is checked into the repo at `.githooks/pre-push` — one-time setup per clone.

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
Python:  IrohNode.memory() -> node
         blobs_client(node)  -> BlobsClient
         docs_client(node)   -> DocsClient
         gossip_client(node) -> GossipClient
         net_client(node)    -> NetClient

Rust:    Endpoint::bind(presets::N0)
         + Router (ALPN mux)
         + BlobsProtocol (iroh-blobs 0.99)
         + Docs (iroh-docs 0.97)
         + Gossip (iroh-gossip 0.97)
```

## License

Apache-2.0
