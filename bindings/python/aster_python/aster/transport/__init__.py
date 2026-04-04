"""
aster.transport — Transport abstraction layer.

This package provides the transport abstraction that decouples the RPC
layer from the underlying transport mechanism. Two transports are provided:

- `IrohTransport`: Remote transport using QUIC streams over Iroh
- `LocalTransport`: In-process transport using asyncio.Queue

Spec reference: §8.3.1 (Transport protocol), §8.3.2 (LocalTransport),
§8.3.3 (wire-compatible mode)
"""

from aster_python.aster.transport.base import (
    Transport,
    BidiChannel,
)
from aster_python.aster.transport.iroh import IrohTransport
from aster_python.aster.transport.local import LocalTransport

__all__ = [
    "Transport",
    "BidiChannel",
    "IrohTransport",
    "LocalTransport",
]
