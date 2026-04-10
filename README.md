# Aster

**Peer-to-peer RPC framework + control-plane CLI, built on [Iroh](https://iroh.computer).**

Write a service in Python or TypeScript, get a typed client in another language, talk to it from anywhere on the public internet without a CA, port-forwarding, or central server. Aster handles the trust, transport, and contract identity for you.

- **Website**: [aster.site](https://aster.site)
- **Docs**: [docs.aster.site](https://docs.aster.site)
- **Quickstart**: [Mission Control in 30 minutes](https://docs.aster.site/quickstart/mission-control)

---

## Install the CLI

The Aster CLI (`aster`) gives you a control plane for your services: connect to a peer, browse its contracts, call methods, stream logs, manage credentials, generate typed clients, and more.

### One-line install (recommended)

```bash
curl -LsSf https://aster.site/install.sh | sh
```

This installs [`uv`](https://docs.astral.sh/uv/) (if not already present) and then runs `uv tool install aster-cli`. The result is the `aster` command on your `PATH`, with everything isolated in its own environment so it never conflicts with your system Python.

### Verify the installer first (security-conscious)

```bash
curl -LsSfO https://aster.site/install.sh
shasum -a 256 install.sh
# Expected: 27536761d65bd03df2a770ef124121f531406fb56861d7049c9e7c30668d8a5c
sh install.sh
```

The expected hash is published on [aster.site](https://aster.site) and updated with every release. If the hash doesn't match, **don't run the script** — it may have been tampered with.

### Already have `uv`?

```bash
uv tool install aster-cli
```

### Already have `pip`?

```bash
pip install aster-cli
```

We strongly recommend `uv tool install` over `pip install --user` because it isolates the CLI from your system Python.

### Platforms

- **Linux** (x86_64, aarch64) — full support
- **macOS** (Intel, Apple Silicon) — full support
- **Windows** — install via `pip install aster-cli` (no `install.sh` yet; PRs welcome)

### Verify

```bash
aster --version
aster --help
aster shell --demo      # interactive demo, no network needed
```

---

## Use Aster from Python

```bash
pip install aster-rpc
# or: uv add aster-rpc
```

Define a service:

```python
from dataclasses import dataclass
from aster import service, rpc, wire_type, AsterServer

@wire_type("hello/Request")
@dataclass
class Request:
    name: str = ""

@wire_type("hello/Response")
@dataclass
class Response:
    message: str = ""

@service(name="HelloService", version=1)
class HelloService:
    @rpc()
    async def greet(self, req: Request) -> Response:
        return Response(message=f"Hello, {req.name}!")

async def main():
    async with AsterServer(services=[HelloService()]) as srv:
        print(srv.address)  # Share this aster1... ticket
        await srv.serve()
```

Call it from another machine:

```python
from aster import AsterClient

async def main():
    client = AsterClient(address="aster1...")
    await client.connect()
    hello = client.proxy("HelloService")
    result = await hello.greet({"name": "World"})
    print(result["message"])
```

That's a working RPC service running on Iroh's NAT-traversing QUIC transport, with content-addressed contract identity. No port forwarding, no TLS certificate, no shared schema file.

For the full guide, see [docs.aster.site/quickstart/mission-control](https://docs.aster.site/quickstart/mission-control).

---

## Use Aster from TypeScript / Bun

```bash
bun add @aster-rpc/aster
```

```typescript
import { Service, Rpc, WireType, AsterServer, AsterClientWrapper } from "@aster-rpc/aster";

@WireType("hello/Request")
class Request { name: string = ""; }

@WireType("hello/Response")
class Response { message: string = ""; }

@Service({ name: "HelloService", version: 1 })
class HelloService {
  @Rpc()
  async greet(req: Request): Promise<Response> {
    return { message: `Hello, ${req.name}!` };
  }
}

const srv = new AsterServer({ services: [new HelloService()] });
await srv.start();
console.log(srv.address);
await srv.serve();
```

---

## What's in this repo

```
aster-rpc/                          PyPI: aster-rpc          (Python bindings + RPC framework)
@aster-rpc/aster                    npm:  @aster-rpc/aster   (TypeScript bindings + RPC framework)
aster-cli                           PyPI: aster-cli          (the `aster` command)

bindings/python/                    Python binding source (PyO3 + Python)
bindings/typescript/                TypeScript binding source (NAPI-RS + TypeScript)
bindings/java/                      Java binding (in progress)
bindings/go/                        Go binding (in progress)

cli/                                CLI source (the `aster` command)
core/                               Shared Rust core (iroh wrapper)
ffi/                                C FFI for non-Python language bindings

examples/python/                    Python examples (mission_control, etc.)
examples/typescript/                TypeScript examples
examples/mission-control/           Mission Control walkthrough (the quickstart)

docs/_internal/                     Implementation flows, design notes, perf
                                    investigation. Not published.
ffi_spec/                           Wire-protocol specification documents
conformance/                        Cross-language conformance vectors
tests/                              Unit + integration tests for all bindings
```

---

## Building from source

You only need this if you're contributing to Aster itself. Most users should `pip install aster-rpc`.

### Prerequisites

- Python ≥ 3.9
- Rust toolchain (1.94+ recommended)
- [`uv`](https://docs.astral.sh/uv/) (recommended) or `pip`
- For TypeScript binding: [`bun`](https://bun.sh/) or Node.js 20+

### Python binding

```bash
git clone https://github.com/emrul/iroh-python.git
cd iroh-python

uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout pytest-rerunfailures
uv run maturin develop -m bindings/python/rust/Cargo.toml
uv pip install -e cli/
```

Or use the build script which also regenerates type stubs:

```bash
./scripts/build.sh
```

### Optional: speed up Rust builds with sccache

```bash
brew install sccache       # macOS
# or: cargo install sccache

export RUSTC_WRAPPER=sccache
sccache --start-server || true
```

`./scripts/validate.sh` auto-enables `sccache` when installed and prints cache stats at the end.

### TypeScript binding

```bash
cd bindings/typescript
bun install
bun run build
```

### Run tests

```bash
# Python: all tests (some need network, may take a few minutes)
uv run pytest tests/python/ -v --timeout=30

# Python: unit tests only (fast)
uv run pytest tests/python/ -v --timeout=30 -m "not network"

# Single file
uv run pytest tests/python/test_blobs.py -v --timeout=30

# TypeScript
cd bindings/typescript && bun test

# Cross-language matrix (Python ↔ TypeScript)
bash tests/integration/mission_control/run_matrix.sh
```

### Lint / format

```bash
# Rust
cargo fmt --manifest-path bindings/python/rust/Cargo.toml
cargo clippy --manifest-path bindings/python/rust/Cargo.toml -- -D warnings

# Full validation (mirrors CI)
./scripts/validate.sh
```

### Pre-push hook

A pre-push hook runs `cargo fmt --check` and `cargo clippy` before allowing pushes:

```bash
git config core.hooksPath .githooks
```

Checked into the repo at `.githooks/pre-push` — one-time setup per clone.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  Application code (Python / TypeScript / Java / Go)                  │
├──────────────────────────────────────────────────────────────────────┤
│  Aster RPC framework                                                 │
│    @service / @rpc decorators, codegen, interceptors,                │
│    contract identity, session-scoped services, capabilities          │
├──────────────────────────────────────────────────────────────────────┤
│  Wire protocol                                                       │
│    Stream framing, codec (Fory XLANG / JSON), trailers, deadlines    │
├──────────────────────────────────────────────────────────────────────┤
│  Trust layer (Gate 0/1/3)                                            │
│    ed25519 root key, OTT/policy credentials, peer admission,         │
│    role-based capabilities, optional @aster delegated auth           │
├──────────────────────────────────────────────────────────────────────┤
│  Iroh QUIC transport                                                 │
│    NAT traversal, P2P discovery, blobs, docs, gossip                 │
└──────────────────────────────────────────────────────────────────────┘
```

The Python binding is built with [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs); the TypeScript binding uses [NAPI-RS](https://napi.rs/). Both share the same Rust core (`core/` + iroh crates), so a Python client and a TypeScript server (or vice versa) speak the same wire protocol byte-for-byte.

For implementation details, see [`docs/_internal/implementation_flows/INDEX.md`](docs/_internal/implementation_flows/INDEX.md).

---

## License

Apache-2.0
