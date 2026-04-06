# Quickstart

Get a working Aster RPC service running in under two minutes.

## Install

```bash
pip install aster-rpc
```

Or with uv:

```bash
uv pip install aster-rpc
```

## Define a service

Create a file called `hello_service.py`:

```python
from dataclasses import dataclass
from aster.decorators import service, rpc


@dataclass
class HelloRequest:
    name: str = ""


@dataclass
class HelloResponse:
    message: str = ""


@service
class HelloService:
    @rpc
    async def say_hello(self, req: HelloRequest) -> HelloResponse:
        return HelloResponse(message=f"Hello, {req.name}!")
```

That's the entire service definition. Three decorators: `@dataclass` (standard Python), `@service` (marks the class as an Aster service), `@rpc` (marks a method as a callable endpoint). No schema files, no code generation, no base classes.

## Run a producer

A producer is the node that hosts the service. Create `producer.py`:

```python
import asyncio
from hello_service import HelloService
from aster import AsterServer


async def main():
    async with AsterServer(services=[HelloService()]) as srv:
        print("Producer ready at:", srv.endpoint_addr_b64)
        await srv.serve()


asyncio.run(main())
```

Run it:

```bash
python producer.py
```

In dev mode (no `ASTER_*` environment variables set), `AsterServer`:

- Generates an ephemeral root key and node identity (no files needed).
- Opens the consumer gate (`allow_all_consumers=True`) so consumers can connect without enrollment credentials.
- Serves RPC, blobs, docs, and gossip on a single endpoint.
- Prints the endpoint address for consumers to connect to.

## Run a consumer

Create `consumer.py`:

```python
import asyncio
from hello_service import HelloService, HelloRequest
from aster import AsterClient


async def main():
    async with AsterClient() as c:
        hello = await c.client(HelloService)
        resp = await hello.say_hello(HelloRequest(name="World"))
        print(resp.message)  # Hello, World!


asyncio.run(main())
```

Run it, passing the producer's endpoint address:

```bash
ASTER_ENDPOINT_ADDR=<paste from producer output> python consumer.py
```

`AsterClient` reads `ASTER_ENDPOINT_ADDR` from the environment, connects to the producer's admission endpoint (to discover available services), then opens an RPC connection. `c.client(HelloService)` returns a typed client stub — call methods on it like regular async functions.

## Dev mode vs production

The quickstart above runs in **dev mode** — ephemeral keys, open gates, no credential files. Everything works out of the box for local development.

In **production**, you configure trust and admission:

```bash
# Operator's machine (offline):
aster keygen root --out root.key
aster keygen pubkey --in root.key --out root_pub.key
aster authorize consumer --root-key root.key --type policy --out consumer.token

# Producer node:
ASTER_ROOT_PUBKEY_FILE=root_pub.key python producer.py

# Consumer node:
ASTER_ENDPOINT_ADDR=<producer addr> \
ASTER_ENROLLMENT_CREDENTIAL=consumer.token \
python consumer.py
```

See [Configuration](configuration.md) for the full list of environment variables and TOML settings, and [Trust](trust.md) for the security model.

## What's next

- [Defining services and types](services.md) — streaming RPCs, `@wire_type`, contract identity.
- [Configuration](configuration.md) — `AsterConfig`, TOML files, environment variables.
- [Trust and admission](trust.md) — root keys, enrollment credentials, the Gate model.
- [AsterServer](server.md) — blobs, docs, gossip, Gate 0 wiring.
- [AsterClient](client.md) — admission flow, typed clients, error handling.
