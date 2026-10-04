"""
aster.transport.iroh -- remote transport over ``aster::rpc::RpcConnection``.

Spec reference: sec. 8.3.1 (Transport protocol); design
``docs/_internal/bindings/dynamic-dispatch-design.md`` sec. 3 (plan P4).

Every call goes through the native ``aster._aster.RpcClient``, the pooled
Rust ``RpcConnection``: framing, zstd, the stream pool, client deadlines and
cancellation all live in Rust, so this module only encodes and decodes
payloads. A unary call is one crossing; streamed responses are read ahead in
Rust and taken in batches.

``session_id`` selects the session (0 = SHARED). Session ids come from the
connection's shared counter (``RpcClient.next_session_id``), so Python and
Rust clients on one connection never collide.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, AsyncIterator

from aster.codec import ForyCodec, ForyConfig
from aster.rpc_types import SerializationMode
from aster.status import RpcError, StatusCode
from aster.transport.base import BidiChannel, Transport, TransportError

if TYPE_CHECKING:
    import aster

_BATCH = 64


def rpc_client(connection: Any) -> Any:
    """The native ``RpcClient`` for ``connection`` (an ``IrohConnection``),
    or ``connection`` itself if it already is one."""
    from aster._aster import RpcClient

    if isinstance(connection, RpcClient):
        return connection
    return RpcClient(connection)


def next_session_id(connection: Any) -> int:
    """A fresh session id from ``connection``'s shared counter (never 0)."""
    return rpc_client(connection).next_session_id()


def _metadata(metadata: dict[str, str] | None) -> list[tuple[str, str]] | None:
    if not metadata:
        return None
    return list(metadata.items())


class IrohTransport(Transport):
    """Transport over one ``aster/1`` connection.

    Args:
        connection: The connection to issue calls on.
        codec: Fory codec for request/response payloads. Defaults to an
            XLANG codec.
        session_id: 0 = SHARED (stateless); non-zero binds every call to that
            session. Normally set via ``AsterClient.open_session``.
        version: Service version written into the call header (per-service
            overrides via :meth:`bind_service`).
    """

    def __init__(
        self,
        connection: "aster.IrohConnection",
        codec: ForyCodec | None = None,
        fory_config: ForyConfig | None = None,
        session_id: int = 0,
        version: int = 1,
    ) -> None:
        self._conn = connection
        self._codec = codec or ForyCodec(
            mode=SerializationMode.XLANG,
            fory_config=fory_config,
        )
        self._session_id = session_id
        self._version = version
        self._versions: dict[str, int] = {}
        self._default_serialization_mode = SerializationMode.XLANG.value
        self._client_obj: Any = None

    @property
    def session_id(self) -> int:
        return self._session_id

    @property
    def _client(self) -> Any:
        c = self._client_obj
        if c is None:
            c = self._client_obj = rpc_client(self._conn)
        return c

    def bind_service(self, service: str, version: int) -> None:
        """Send ``version`` in the header of calls to ``service``."""
        self._versions[service] = version

    def _version_of(self, service: str) -> int:
        return self._versions.get(service, self._version)

    async def close(self) -> None:
        self._conn.close(0, b"normal close")

    def _resolve_mode(self, override: int | None) -> int:
        return override if override is not None else self._default_serialization_mode

    def _decode(self, payload: bytes) -> Any:
        return self._codec.decode(payload)

    # -- Unary ---------------------------------------------------------------

    async def unary(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> Any:
        response = await self._client.unary(
            service,
            method,
            self._codec.encode(request),
            version=self._version_of(service),
            session_id=self._session_id,
            deadline=float(deadline_secs),
            serialization_mode=self._resolve_mode(serialization_mode),
            metadata=_metadata(metadata),
        )
        if not response:
            # OK trailer without a response frame (e.g. a handler that
            # returns nothing). Surface None and let the caller decide.
            return None
        return self._codec.decode(response)

    # -- Server Streaming ---------------------------------------------------

    def server_stream(
        self,
        service: str,
        method: str,
        request: Any,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> AsyncIterator[Any]:
        return self._server_stream_impl(
            service, method, request, metadata,
            deadline_secs, self._resolve_mode(serialization_mode),
        )

    async def _server_stream_impl(
        self,
        service: str,
        method: str,
        request: Any,
        metadata: dict[str, str] | None,
        deadline_secs: int,
        serialization_mode: int,
    ) -> AsyncIterator[Any]:
        stream = await self._client.server_stream(
            service,
            method,
            self._codec.encode(request),
            version=self._version_of(service),
            session_id=self._session_id,
            deadline=float(deadline_secs),
            serialization_mode=serialization_mode,
            metadata=_metadata(metadata),
        )
        decode = self._codec.decode
        try:
            while True:
                batch = await stream.next_batch(_BATCH)
                if not batch:
                    return
                for payload in batch:
                    yield decode(payload)
        finally:
            # A no-op after the trailer; cancels a stream abandoned early.
            stream.cancel()

    # -- Client Streaming ---------------------------------------------------

    async def client_stream(
        self,
        service: str,
        method: str,
        requests: AsyncIterator[Any],
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> Any:
        encode = self._codec.encode
        payloads = [encode(item) async for item in requests]
        if not payloads:
            # The server dispatches a call on its first request frame.
            raise TransportError("client_stream requires at least one request frame")
        response = await self._client.client_stream(
            service,
            method,
            payloads,
            version=self._version_of(service),
            session_id=self._session_id,
            deadline=float(deadline_secs),
            serialization_mode=self._resolve_mode(serialization_mode),
            metadata=_metadata(metadata),
        )
        return self._codec.decode(response)

    # -- Bidirectional Streaming -------------------------------------------

    def bidi_stream(
        self,
        service: str,
        method: str,
        *,
        metadata: dict[str, str] | None = None,
        deadline_secs: int = 0,
        serialization_mode: int | None = None,
    ) -> BidiChannel:
        return IrohBidiChannel(
            transport=self,
            service=service,
            method=method,
            metadata=metadata,
            deadline_secs=deadline_secs,
            serialization_mode=self._resolve_mode(serialization_mode),
        )


class IrohBidiChannel(BidiChannel):
    """Interleaved bidi channel. The call opens on the first ``send`` /
    ``recv``; ``close()`` ends the request stream; the response side stays
    readable until the server's trailer."""

    def __init__(
        self,
        transport: IrohTransport,
        service: str,
        method: str,
        metadata: dict[str, str] | None,
        deadline_secs: int,
        serialization_mode: int,
    ) -> None:
        self._transport = transport
        self._codec = transport._codec
        self._service = service
        self._method = method
        self._metadata = metadata
        self._deadline_secs = deadline_secs
        self._serialization_mode = serialization_mode
        self._sink: Any = None
        self._stream: Any = None
        self._buffer: list[bytes] = []
        self._sent_end_stream = False
        self._last_trailer: tuple[int, str] | None = None

    async def __aenter__(self) -> "IrohBidiChannel":
        await self._ensure_open()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb) -> None:
        await self.close()
        if self._last_trailer is None:
            try:
                await self.wait_for_trailer()
            except Exception:
                pass

    async def _ensure_open(self) -> None:
        if self._stream is not None:
            return
        t = self._transport
        self._sink, self._stream = await t._client.bidi_open(
            self._service,
            self._method,
            version=t._version_of(self._service),
            session_id=t._session_id,
            deadline=float(self._deadline_secs),
            serialization_mode=self._serialization_mode,
            metadata=_metadata(self._metadata),
        )

    async def send(self, msg: Any) -> None:
        if self._sent_end_stream:
            raise TransportError("channel is closed for sending")
        await self._ensure_open()
        await self._sink.send(self._codec.encode(msg))

    async def recv(self) -> Any:
        if self._buffer:
            return self._codec.decode(self._buffer.pop(0))
        if self._last_trailer is not None:
            return None
        await self._ensure_open()
        try:
            batch = await self._stream.next_batch(_BATCH)
        except RpcError as e:
            self._last_trailer = (int(e.code), e.message)
            raise
        if not batch:
            self._last_trailer = (StatusCode.OK, "")
            return None
        if len(batch) > 1:
            self._buffer.extend(batch[1:])
        return self._codec.decode(batch[0])

    async def close(self) -> None:
        """Signal end-of-requests. Idempotent."""
        if self._sent_end_stream or self._sink is None:
            self._sent_end_stream = True
            return
        self._sent_end_stream = True
        try:
            await self._sink.finish()
        except Exception:
            pass

    async def wait_for_trailer(self) -> tuple[int, str]:
        if self._last_trailer is not None:
            return self._last_trailer
        await self._ensure_open()
        self._buffer.clear()
        while self._last_trailer is None:
            try:
                batch = await self._stream.next_batch(_BATCH)
            except RpcError as e:
                self._last_trailer = (int(e.code), e.message)
                break
            if not batch:
                self._last_trailer = (StatusCode.OK, "")
        return self._last_trailer

    def __aiter__(self) -> "IrohBidiChannel":
        return self

    async def __anext__(self) -> Any:
        item = await self.recv()
        if item is None and self._last_trailer is not None and not self._buffer:
            raise StopAsyncIteration
        return item

