"""
aster.transport.base -- Transport protocol and BidiChannel.

Spec reference: §8.3.1 (Transport protocol)

This module defines the abstract Transport protocol that all transport
implementations must satisfy, and the BidiChannel class for bidirectional
streaming RPCs.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from typing import TYPE_CHECKING, Any, AsyncIterator, Protocol

if TYPE_CHECKING:
    from typing import AsyncContextManager


# ── Transport Protocol ───────────────────────────────────────────────────────

# Note: We use a Protocol class rather than ABC to allow structural subtyping,
# making it easier to implement transports in tests using simple mocks.


class Transport(Protocol):
    """Abstract transport interface for Aster RPC calls.

    All transport implementations must provide these methods. The transport
    handles the mechanics of opening streams, writing/reading frames, and
    handling the wire protocol details.

    Spec reference: §8.3.1
    """

    @abstractmethod
    async def unary(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int = 0,
    ) -> Any:
        """Perform a unary RPC call.

        Args:
            service: The target service name.
            method: The method name within the service.
            request: The serialized request message.
            metadata: Optional key/value pairs sent in StreamHeader.
            deadline_secs: Deadline in relative seconds (0 = no deadline).
            serialization_mode: Serialization mode (XLANG=0, NATIVE=1, ROW=2).


        Returns:
            The deserialized response message.

        Raises:
            RpcError: On protocol errors or handler failures.
        """
        ...

    @abstractmethod
    def server_stream(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int = 0,
    ) -> AsyncIterator[Any]:
        """Initiate a server-streaming RPC.

        Args:
            service: The target service name.
            method: The method name within the service.
            request: The serialized request message.
            metadata: Optional key/value pairs sent in StreamHeader.
            deadline_secs: Deadline in relative seconds (0 = no deadline).
            serialization_mode: Serialization mode.


        Yields:
            Deserialized response messages as they arrive.

        Raises:
            RpcError: On protocol errors or handler failures.
        """
        ...

    @abstractmethod
    async def client_stream(
        self,
        service: str,
        method: str,
        requests: AsyncIterator[Any],
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int = 0,
    ) -> Any:
        """Perform a client-streaming RPC.

        Args:
            service: The target service name.
            method: The method name within the service.
            requests: Async iterator of serialized request messages.
            metadata: Optional key/value pairs sent in StreamHeader.
            deadline_secs: Deadline in relative seconds (0 = no deadline).
            serialization_mode: Serialization mode.


        Returns:
            The deserialized response message.

        Raises:
            RpcError: On protocol errors or handler failures.
        """
        ...

    @abstractmethod
    def bidi_stream(
        self,
        service: str,
        method: str,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int = 0,
    ) -> BidiChannel:
        """Initiate a bidirectional-streaming RPC.

        Args:
            service: The target service name.
            method: The method name within the service.
            metadata: Optional key/value pairs sent in StreamHeader.
            deadline_secs: Deadline in relative seconds (0 = no deadline).
            serialization_mode: Serialization mode.


        Returns:
            A BidiChannel for sending and receiving messages.
        """
        ...

    @abstractmethod
    async def close(self) -> None:
        """Close the transport and release resources."""
        ...


# ── BidiChannel ─────────────────────────────────────────────────────────────


class BidiChannel:
    """Bidirectional channel for streaming RPCs.

    Provides send/recv/close operations for bidirectional and client-streaming
    RPCs. Supports the async context manager protocol for convenient resource
    management.

    Spec reference: §8.3.1

    Example::

        async with transport.bidi_stream("MyService", "chat") as ch:
            await ch.send(request)
            response = await ch.recv()
    """

    @abstractmethod
    async def send(self, msg: Any) -> None:
        """Send a message on the stream.

        Args:
            msg: The serialized message to send.

        Raises:
            RpcError: On protocol errors.
        """
        ...

    @abstractmethod
    async def recv(self) -> Any:
        """Receive the next message from the stream.

        Returns:
            The deserialized message, or None if the stream ended cleanly
            (OK trailer received).

        Raises:
            RpcError: On protocol errors or non-OK trailer status.
        """
        ...

    @abstractmethod
    async def close(self) -> None:
        """Close the sending side of the stream.

        After calling close(), no more messages can be sent. The receive
        side remains open until the server closes it or the stream ends.

        Raises:
            RpcError: On protocol errors.
        """
        ...

    @abstractmethod
    async def wait_for_trailer(self) -> tuple[int, str]:
        """Wait for the trailing status frame.

        Returns:
            A (code, message) tuple from the RpcStatus trailer.

        Raises:
            RpcError: On non-OK status.
        """
        ...


# ── Transport Errors ────────────────────────────────────────────────────────


class TransportError(Exception):
    """Base class for transport-level errors."""
    pass


class ConnectionLostError(TransportError):
    """Raised when the underlying connection is lost."""
    pass


class StreamClosedError(TransportError):
    """Raised when attempting to use a closed stream."""
    pass
