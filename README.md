# Aster

> **Machines need to authenticate to other machines, often on behalf of a user. Aster makes that safe — without a central authority and without shared secrets.**

Aster is a peer-to-peer RPC framework with identity in the connection. You define typed services in Python or TypeScript, and any machine that holds the right capability credential can call them — across NATs, across organisations, across languages — without DNS, without a load balancer, without a certificate authority, and without an OAuth proxy in the middle.

The 2026 vivid example is AI agents calling tools on remote machines: agent on machine A wants to invoke a function on machine B, you don't want a hosted proxy in between, you don't want to rotate API keys, and you want the call to be scoped to specific methods. That's exactly what Aster solves. The same engineering also covers IoT fleets, edge compute, multi-tenant microservices, and anything else where the machine is the principal and the user is the delegating authority.

Built on [iroh](https://iroh.computer) (QUIC + NAT traversal), [Apache Fory](https://fory.apache.org) (cross-language wire format), and [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) (content-addressed contract identity). Capability-based credentials, four-gate authorization, and an offline ed25519 root key.

- **Website** — [aster.site](https://aster.site)
- **Docs** — [docs.aster.site](https://docs.aster.site)
- **Mission Control walkthrough** — [docs.aster.site/docs/quickstart/mission-control](https://docs.aster.site/docs/quickstart/mission-control) (the canonical 30-minute deep dive — seven chapters, four streaming patterns, auth, gen-client, cross-language)

---

## Install

Two packages: the framework and the CLI.

### Python

```bash
uv pip install aster-rpc aster-cli
# or:
pip install aster-rpc aster-cli
```

`aster-rpc` gives you `from aster import ...` for your service code. `aster-cli` gives you the `aster` command — shell, contract gen-client, trust manager, enrollment, MCP integration. They're versioned independently.

### TypeScript

```bash
bun add @aster-rpc/aster
# or: npm install @aster-rpc/aster
```

The CLI is shared across all language bindings — install it once via Python and it works against any Aster server regardless of what wrote the service:

```bash
uv tool install aster-cli
# or: pip install aster-cli
```

### Verify

```bash
aster --version
aster shell --demo      # interactive shell against a sample peer, no network needed
```

Pre-built wheels for Linux x86_64/aarch64, macOS arm64, and Windows x64. TypeScript native binaries for the same platforms via NAPI-RS.

---

## A working service in 60 seconds

### Python

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
    @rpc
    async def greet(self, req: Request) -> Response:
        return Response(message=f"Hello, {req.name}!")

async def main():
    async with AsterServer(services=[HelloService()]) as srv:
        print(srv.address)   # share this aster1... with consumers
        await srv.serve()
```

Call it from another machine — no DNS, no port forwarding, no shared schema file:

```python
from aster import AsterClient

async def main():
    async with AsterClient(address="aster1...") as c:
        hello = c.proxy("HelloService")
        reply = await hello.greet({"name": "World"})
        print(reply["message"])
```

### TypeScript

```typescript
import { Service, Rpc, AsterServer, AsterClientWrapper } from "@aster-rpc/aster";

class Request {
  name = "";
  constructor(init?: Partial<Request>) { if (init) Object.assign(this, init); }
}

class Response {
  message = "";
  constructor(init?: Partial<Response>) { if (init) Object.assign(this, init); }
}

@Service({ name: "HelloService", version: 1 })
export class HelloService {
  @Rpc({ request: Request, response: Response })
  async greet(req: Request): Promise<Response> {
    return new Response({ message: `Hello, ${req.name}!` });
  }
}

const server = new AsterServer({ services: [new HelloService()] });
await server.start();
console.log(server.address);
await server.serve();
```

The Python service and the TypeScript service speak the same wire format natively — no codegen step, no IDL file. A Python client can call a TypeScript service and vice versa using the dynamic proxy or a generated typed client.

For the full guide — auth, sessions, streaming, cross-language interop — see the [Mission Control walkthrough](https://docs.aster.site/docs/quickstart/mission-control).

---

## What's in this repo

```
aster-rpc/                          PyPI: aster-rpc            (framework + Python bindings)
@aster-rpc/aster                    npm:  @aster-rpc/aster     (framework + TypeScript bindings)
@aster-rpc/transport                npm:  @aster-rpc/transport (NAPI-RS native addon)
aster-cli                           PyPI: aster-cli            (the `aster` command)

bindings/python/                    Python binding source (PyO3 + Python)
bindings/typescript/                TypeScript binding source (NAPI-RS + TypeScript)

cli/                                CLI source (the `aster` command)
core/                               Shared Rust core (iroh wrapper)

examples/python/                    Python example services (incl. mission_control)
examples/typescript/                TypeScript example services (incl. missionControl, hello-world)

tests/                              Unit + integration tests for all bindings
conformance/                        Cross-language conformance vectors
```

Java, .NET, Kotlin, and Go bindings are in progress in the source repo and will land in the public tree once they're shipping. Rust is planned.

---

## Where this is going

Today: typed services, four streaming patterns, capability-based auth, Python and TypeScript bindings shipping at 0.1.2, MCP integration for AI agent tool calling.

Next: identity-aware load balancing and self-healing built from the same primitives. The substrate keeps growing from one consistent identity model — load balancing across endpoints that share an identity, self-healing rooted in the same trust topology. The "no infrastructure" pitch starts as *no DNS, no LB, no certs* and grows into *no DNS, no LB, no certs, no service mesh, no sidecars, no control plane*.

Java, .NET, Kotlin, and Go bindings are in progress. Rust is planned (direct access to the core crate, no FFI overhead).

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
git clone https://github.com/aster-rpc/aster-rpc.git
cd aster-rpc

uv venv
uv pip install maturin pytest pytest-asyncio pytest-timeout pytest-rerunfailures
uv run maturin develop -m bindings/python/rust/Cargo.toml
uv pip install -e cli/
```

Or use the build script which also regenerates type stubs:

```bash
./scripts/build.sh
```

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

# TypeScript
cd bindings/typescript && bun test

# Cross-language matrix (Python ↔ TypeScript)
bash tests/integration/mission_control/run_matrix.sh
```

### Lint and full validation

```bash
cargo fmt --manifest-path bindings/python/rust/Cargo.toml
cargo clippy --manifest-path bindings/python/rust/Cargo.toml -- -D warnings

# Full validation (mirrors CI)
./scripts/validate.sh
```

### Optional: speed up Rust builds with sccache

```bash
brew install sccache       # macOS
# or: cargo install sccache
export RUSTC_WRAPPER=sccache
```

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  Application code (Python / TypeScript / Java / .NET / Go)           │
├──────────────────────────────────────────────────────────────────────┤
│  Aster RPC framework                                                 │
│    @service / @rpc decorators, codegen, interceptors,                │
│    contract identity, session-scoped services, capabilities          │
├──────────────────────────────────────────────────────────────────────┤
│  Wire protocol                                                       │
│    Stream framing, codec (Fory XLANG / NATIVE / ROW / JSON),         │
│    trailers, deadlines                                               │
├──────────────────────────────────────────────────────────────────────┤
│  Trust layer (Gates 0–3)                                             │
│    ed25519 root key, OTT/policy credentials, peer admission,         │
│    role-based capabilities, optional @aster delegated auth           │
├──────────────────────────────────────────────────────────────────────┤
│  Iroh QUIC transport                                                 │
│    NAT traversal, P2P discovery, blobs, docs, gossip                 │
└──────────────────────────────────────────────────────────────────────┘
```

The Python binding is built with [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs); the TypeScript binding uses [NAPI-RS](https://napi.rs/). Both share the same Rust core (`core/` + iroh crates), so a Python client and a TypeScript server (or vice versa) speak the same wire protocol byte-for-byte.

Full conceptual reference, API docs, and the four-gate trust model live on the [docs site](https://docs.aster.site).

---

## License

Apache-2.0
