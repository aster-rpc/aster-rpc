"""
Aster RPC — Public API Reference
=================================

This module re-exports the public API surface for documentation.
Import from ``aster`` directly in your code, not from ``aster.public``.

Getting Started
---------------

**Producer** (server)::

    from aster import AsterServer, service, rpc, wire_type
    from dataclasses import dataclass

    @wire_type("myapp/GreetRequest")
    @dataclass
    class GreetRequest:
        name: str = ""

    @wire_type("myapp/GreetResponse")
    @dataclass
    class GreetResponse:
        message: str = ""

    @service(name="Greeter", version=1)
    class Greeter:
        @rpc
        async def greet(self, req: GreetRequest) -> GreetResponse:
            return GreetResponse(message=f"Hello, {req.name}!")

    async with AsterServer(services=[Greeter()]) as srv:
        print(srv.ticket)  # share this with consumers
        await srv.serve()

**Consumer** (client)::

    from aster import AsterClient

    client = AsterClient(address="aster1...")
    await client.connect()

    # Use the shell for exploration:
    #   aster shell aster1...

    # Or generate a typed client:
    #   aster contract gen-client aster1... --out ./clients --package myapp

Decorators
----------

Use these to define RPC services:

- :func:`service` — Declare a class as an RPC service
- :func:`rpc` — Mark a method as a unary RPC endpoint
- :func:`server_stream` — Mark a method as server-streaming
- :func:`client_stream` — Mark a method as client-streaming
- :func:`bidi_stream` — Mark a method as bidirectional streaming
- :func:`wire_type` — Register a dataclass for cross-language serialization

Error Handling
--------------

All RPC failures raise :class:`RpcError` with a :class:`StatusCode`.
"""

from aster.high_level import AsterServer, AsterClient
from aster.decorators import service, rpc, server_stream, client_stream, bidi_stream
from aster.codec import wire_type
from aster.status import RpcError, StatusCode
from aster.config import AsterConfig
from aster.interceptors import CallContext


__all__ = [
    "AsterServer",
    "AsterClient",
    "service",
    "rpc",
    "server_stream",
    "client_stream",
    "bidi_stream",
    "wire_type",
    "RpcError",
    "StatusCode",
    "AsterConfig",
    "CallContext",
]
