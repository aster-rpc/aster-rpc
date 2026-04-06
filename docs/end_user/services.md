# Defining Services and Types

This guide covers everything you need to define Aster RPC services: the `@service` and `@rpc` decorators, streaming patterns, request/response types, wire identity, contract identity, and serialization modes.

## The `@service` decorator

`@service` marks a class as an Aster RPC service. It scans the class for methods decorated with `@rpc`, `@server_stream`, `@client_stream`, or `@bidi_stream`, extracts their type signatures, and attaches a `ServiceInfo` metadata object to the class.

Three calling forms are supported:

```python
from aster.decorators import service

# Bare -- service name defaults to the class name ("Greeter").
@service
class Greeter:
    ...

# Explicit name.
@service("AgentControl")
class AgentControlService:
    ...

# Keyword arguments -- name still defaults to the class name.
@service(version=2, scoped="stream")
class ChatSession:
    def __init__(self, peer: str):
        self.peer = peer
    ...
```

### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `name` (positional) | `str` | Class name | The service name used on the wire and in service discovery. |
| `version` | `int` | `1` | Service version. Consumers specify which version they want. |
| `serialization` | `SerializationMode` or `list[SerializationMode]` | `[XLANG]` | Which serialization modes this service supports. |
| `scoped` | `str` | `"shared"` | `"shared"` (one instance serves all connections) or `"stream"` (a new instance is created per session -- the class `__init__` must accept a `peer` parameter). |
| `interceptors` | `list[type]` | `[]` | Interceptor classes applied to every method in the service. |
| `max_concurrent_streams` | `int` or `None` | `None` | Limits concurrent streams per connection. `None` means unlimited. |

## The `@rpc` decorator

`@rpc` marks a method as a unary RPC (one request in, one response out). It is the most common pattern.

```python
from aster.decorators import service, rpc

@service
class Calculator:
    # Bare -- no options.
    @rpc
    async def add(self, req: AddRequest) -> AddResponse:
        return AddResponse(result=req.a + req.b)

    # With options.
    @rpc(timeout=10.0, idempotent=True)
    async def multiply(self, req: MulRequest) -> MulResponse:
        return MulResponse(result=req.a * req.b)
```

### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `timeout` | `float` or `None` | `None` | Default timeout in seconds. Consumers can override per-call. |
| `idempotent` | `bool` | `False` | Marks the method as safe to retry. Used by the retry interceptor. |
| `serialization` | `SerializationMode` or `None` | `None` | Override the service-level serialization mode for this method. |

All `@rpc` methods must be `async def` coroutines. They receive a single request argument (after `self`) and return a single response.

## Streaming patterns

Aster supports three streaming patterns beyond unary.

### Server streaming (`@server_stream`)

The server sends multiple responses for one request. The method must be an async generator (use `yield`).

```python
from typing import AsyncIterator
from aster.decorators import service, server_stream


@service
class Ticker:
    @server_stream
    async def watch(self, req: WatchRequest) -> AsyncIterator[PriceUpdate]:
        while True:
            price = await get_latest_price(req.symbol)
            yield PriceUpdate(symbol=req.symbol, price=price)
            await asyncio.sleep(1.0)
```

Consumers iterate over the result:

```python
async for update in ticker.watch(WatchRequest(symbol="AAPL")):
    print(update.price)
```

### Client streaming (`@client_stream`)

The client sends multiple requests; the server returns one response. The method receives an `AsyncIterator` of requests.

```python
from typing import AsyncIterator
from aster.decorators import service, client_stream


@service
class Aggregator:
    @client_stream
    async def sum(self, reqs: AsyncIterator[NumberRequest]) -> SumResponse:
        total = 0
        async for req in reqs:
            total += req.value
        return SumResponse(total=total)
```

### Bidirectional streaming (`@bidi_stream`)

Both sides stream concurrently. The method is an async generator that receives an `AsyncIterator` of requests and yields responses.

```python
from typing import AsyncIterator
from aster.decorators import service, bidi_stream


@service
class Chat:
    @bidi_stream
    async def converse(
        self, messages: AsyncIterator[ChatMessage]
    ) -> AsyncIterator[ChatMessage]:
        async for msg in messages:
            yield ChatMessage(text=f"echo: {msg.text}")
```

All three streaming decorators accept `timeout` and `serialization` keyword arguments, same as `@rpc`. `@client_stream` also accepts `idempotent`.

## Request and response types

Request and response types are plain Python dataclasses. No base class or schema definition is required.

```python
from dataclasses import dataclass


@dataclass
class AddRequest:
    a: int = 0
    b: int = 0


@dataclass
class AddResponse:
    result: int = 0
```

### Nested dataclasses

Dataclasses can contain other dataclasses. The type graph is walked automatically.

```python
from dataclasses import dataclass, field


@dataclass
class Address:
    street: str = ""
    city: str = ""
    zip_code: str = ""


@dataclass
class Person:
    name: str = ""
    age: int = 0
    address: Address = field(default_factory=Address)
```

### Supported field types

Aster supports all types that Apache Fory can serialize:

- Primitives: `int`, `float`, `str`, `bool`, `bytes`, `bytearray`
- Collections: `list[T]`, `dict[K, V]`, `set[T]`
- Optional: `T | None` or `Optional[T]`
- Nested dataclasses
- Enums (via `IntEnum`)

All fields should have default values so that deserialization can construct partial objects.

## `@wire_type` -- stable wire identity

When Aster serializes a dataclass across the wire, it needs a string tag to identify the type. By default, the `@service` decorator auto-derives this tag from the Python module and class name (e.g., `hello_service.HelloRequest`). This is fine for development, but it breaks if you rename or move the module.

For production services, use `@wire_type` to declare an explicit, stable tag:

```python
from dataclasses import dataclass
from aster.codec import wire_type


@wire_type("hello/HelloRequest")
@dataclass
class HelloRequest:
    name: str = ""


@wire_type("hello/HelloResponse")
@dataclass
class HelloResponse:
    message: str = ""
```

The tag string is split on the last `/` into a namespace and type name. If there is no `/`, the namespace is empty. The tag must be unique across all types in your system.

When you use auto-derived tags (no `@wire_type`), the `@service` decorator emits a warning:

```
UserWarning: Type HelloRequest auto-tagged as 'hello_service.HelloRequest';
use @wire_type for stable wire identity
```

This is harmless during development but serves as a reminder to add explicit tags before deploying to production.

### When to use `@wire_type`

- **Development / prototyping**: skip it. Auto-derivation works fine.
- **Production / multi-service systems**: use it. Wire tags are part of the serialization contract. If a consumer and producer disagree on the tag for a type, deserialization will fail.
- **Cross-language interop**: required. The Fory XLANG serializer uses these tags to match types across languages.

## Contract identity

Every `@service`-decorated class gets a deterministic **contract ID** -- a BLAKE3 hash derived from:

- The service name and version.
- Each method's name, RPC pattern, and parameter types.
- The serialization mode.

Two services with the same name, version, methods, types, and serialization mode will produce the same contract ID, regardless of the implementation. This enables service discovery: a consumer can ask "give me a service with contract ID X" and know it will get a compatible interface.

The contract ID is computed automatically. You do not need to set it. It is available on the `ServiceInfo` metadata:

```python
from aster.contract.identity import contract_id_from_service

cid = contract_id_from_service(HelloService)
print(cid)  # e.g. "a1b2c3d4..."
```

If you change a method signature, add a method, or change the serialization mode, the contract ID changes. This is intentional -- it signals a breaking change in the service interface.

## Serialization modes

Aster supports two serialization modes via Apache Fory:

| Mode | Enum value | Description |
|------|-----------|-------------|
| **XLANG** | `SerializationMode.XLANG` | Cross-language mode. Types are registered with string tags (via `@wire_type` or auto-derivation). Works across Python, Java, Go, Rust, etc. This is the default. |
| **NATIVE** | `SerializationMode.NATIVE` | Python-only mode. Uses Fory's native Python serialization, which is faster but only works between Python peers. No `@wire_type` tags needed. |

Most users never need to change the serialization mode. The default (XLANG) works for both Python-to-Python and cross-language communication. Use NATIVE only when you are certain both sides are Python and you need the performance boost.

To change the mode:

```python
from aster.decorators import service
from aster.types import SerializationMode


# All methods in this service use NATIVE serialization.
@service(serialization=SerializationMode.NATIVE)
class FastService:
    ...


# Override for a single method.
from aster.decorators import rpc

@service
class MixedService:
    @rpc(serialization=SerializationMode.NATIVE)
    async def fast_path(self, req: Req) -> Resp:
        ...

    @rpc
    async def normal_path(self, req: Req) -> Resp:
        ...
```

## Putting it together

Here is a complete service definition with explicit wire types, a server stream, and a unary method:

```python
import asyncio
from dataclasses import dataclass
from typing import AsyncIterator

from aster.codec import wire_type
from aster.decorators import service, rpc, server_stream


@wire_type("todo/Task")
@dataclass
class Task:
    id: str = ""
    title: str = ""
    done: bool = False


@wire_type("todo/CreateTaskRequest")
@dataclass
class CreateTaskRequest:
    title: str = ""


@wire_type("todo/ListTasksRequest")
@dataclass
class ListTasksRequest:
    pass


@service("TodoService", version=1)
class TodoService:
    def __init__(self):
        self._tasks: dict[str, Task] = {}
        self._counter = 0

    @rpc(idempotent=False)
    async def create_task(self, req: CreateTaskRequest) -> Task:
        self._counter += 1
        task = Task(id=str(self._counter), title=req.title, done=False)
        self._tasks[task.id] = task
        return task

    @server_stream
    async def list_tasks(self, req: ListTasksRequest) -> AsyncIterator[Task]:
        for task in self._tasks.values():
            yield task
```
