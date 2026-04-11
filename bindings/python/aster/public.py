"""
Aster RPC -- Public API Reference
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
        print(srv.address)  # share this with consumers
        await srv.serve()

**Consumer** (client)::

    from aster import AsterClient

    client = AsterClient(address="aster1...")
    await client.connect()

    # Use the shell for exploration:
    #   aster shell aster1...

    # Or generate a typed client:
    #   aster contract gen-client aster1... --out ./clients --lang python --package myapp

Decorators
----------

Use these to define RPC services:

- :func:`service` -- Declare a class as an RPC service
- :func:`rpc` -- Mark a method as a unary RPC endpoint
- :func:`server_stream` -- Mark a method as server-streaming
- :func:`client_stream` -- Mark a method as client-streaming
- :func:`bidi_stream` -- Mark a method as bidirectional streaming
- :func:`wire_type` -- Register a dataclass for cross-language serialization

Authorization
-------------

Compose capability requirements:

- :func:`any_of` -- caller must have at least one of the listed capabilities
- :func:`all_of` -- caller must have every listed capability

Serialization
-------------

- :class:`SerializationMode` -- pick between XLANG (cross-language Fory),
  NATIVE (single-language Fory), ROW (zero-copy Fory rows for data-heavy
  workloads), and JSON (human-readable wire format for debugging and the
  dynamic proxy client).

Error Handling
--------------

All RPC failures raise :class:`RpcError` with a :class:`StatusCode`.
Specific subclasses give actionable diagnostics:

- :class:`AdmissionDeniedError` -- raised when the server refuses
  consumer admission (open-gate vs trusted-mode mismatch, expired
  credential, endpoint id mismatch). The error message enumerates the
  common causes.
- :class:`ContractViolationError` -- raised when the strict-mode codec
  rejects a payload whose shape doesn't match the contract.

Interceptors
------------

Built-in middleware that wraps every RPC call. Configure them on the
``AsterServer`` or ``AsterClient`` constructor.

- :class:`CallContext` -- per-call context object passed through the chain
- :class:`Interceptor` -- base class for custom interceptors
- :class:`DeadlineInterceptor` -- enforce per-call deadlines
- :class:`AuthInterceptor` -- token-based authentication
- :class:`RetryInterceptor` -- automatic retry for idempotent methods
- :class:`CircuitBreakerInterceptor` -- circuit breaker for failing endpoints
- :class:`AuditLogInterceptor` -- log every RPC call for audit
- :class:`MetricsInterceptor` -- collect call latency and error metrics
"""

from aster.runtime import AsterServer, AsterClient, AdmissionDeniedError
from aster.decorators import service, rpc, server_stream, client_stream, bidi_stream
from aster.codec import wire_type
from aster.status import RpcError, StatusCode, ContractViolationError
from aster.rpc_types import SerializationMode
from aster.config import AsterConfig
from aster.capabilities import any_of, all_of
from aster.interceptors import (
    CallContext,
    Interceptor,
    DeadlineInterceptor,
    AuthInterceptor,
    RetryInterceptor,
    CircuitBreakerInterceptor,
    AuditLogInterceptor,
    MetricsInterceptor,
)


__all__ = [
    # Server / client
    "AsterServer",
    "AsterClient",
    "AsterConfig",
    # Decorators
    "service",
    "rpc",
    "server_stream",
    "client_stream",
    "bidi_stream",
    "wire_type",
    # Authorization
    "any_of",
    "all_of",
    # Serialization
    "SerializationMode",
    # Errors
    "RpcError",
    "StatusCode",
    "AdmissionDeniedError",
    "ContractViolationError",
    # Interceptors
    "CallContext",
    "Interceptor",
    "DeadlineInterceptor",
    "AuthInterceptor",
    "RetryInterceptor",
    "CircuitBreakerInterceptor",
    "AuditLogInterceptor",
    "MetricsInterceptor",
]
