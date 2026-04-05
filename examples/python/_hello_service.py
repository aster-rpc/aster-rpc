"""
Shared HelloService definition used by simple_producer.py and simple_consumer.py.

In production, this file would be the generated client library distributed to consumers.
"""
from __future__ import annotations

from dataclasses import dataclass

from aster.codec import aster_tag
from aster.decorators import service, rpc
from aster.types import SerializationMode


@aster_tag("demo/HelloRequest")
@dataclass
class HelloRequest:
    name: str = ""


@aster_tag("demo/HelloResponse")
@dataclass
class HelloResponse:
    message: str = ""


@service(name="HelloService")
class HelloService:
    """Simple greeting service — the canonical Aster Hello World."""

    @rpc()
    async def say_hello(self, req: HelloRequest) -> HelloResponse:
        return HelloResponse(message=f"Hello, {req.name}! (from Aster)")
