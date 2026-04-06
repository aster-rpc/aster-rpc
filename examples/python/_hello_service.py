"""
Shared HelloService definition used by simple_producer.py and simple_consumer.py.

In production, this file would be the generated client library distributed to consumers.
"""
from __future__ import annotations

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
    """Simple greeting service — the canonical Aster Hello World."""

    @rpc
    async def say_hello(self, req: HelloRequest) -> HelloResponse:
        return HelloResponse(message=f"Hello, {req.name}! (from Aster)")
