"""
aster.session -- Session-scoped service support (Phase 8).

Spec reference: Aster-session-scoped-services.md

Session-scoped services multiplex multiple RPC calls over a single QUIC
bi-directional stream.  The stream is established with a StreamHeader whose
``method`` field is empty (``""``); subsequent calls are demarcated by
per-call CALL frames (flags=0x10) carrying a serialised ``CallHeader``.

Wire protocol summary
---------------------
- Client opens a bidi-stream and sends a StreamHeader(method="").
- Each RPC call starts with a CALL frame whose payload is a Fory-encoded
  ``CallHeader`` (method name, call_id, …).
- After the CALL frame the client sends the request payload (for unary /
  server-stream) or a stream of payloads ending with an explicit
  TRAILER(status=OK) (for client-stream / bidi).
- Server responses follow per-pattern rules (see spec §4.6):
    - unary:         response payload frame only (NO trailer on success)
    - server-stream: response frame(s) + TRAILER(OK)
    - client-stream: response payload frame only
    - bidi:          response frame(s) + TRAILER(OK)
- Errors always produce a TRAILER with the relevant status code.
- CANCEL frame (flags=0x20, empty payload): cancels the in-flight call;
  server writes CANCELLED trailer and session remains open.
- Client closes the session by calling ``session.close()`` which calls
  ``send_stream.finish()``.
"""

from __future__ import annotations

import asyncio
import logging
import uuid
from typing import Any, AsyncIterator

from aster.codec import ForyCodec, ForyConfig
from aster.framing import (
    CALL,
    CANCEL,
    COMPRESSED,
    HEADER,
    TRAILER,
    read_frame,
    write_frame,
)
from aster.interceptors.base import (
    apply_request_interceptors,
    build_call_context,
)
from aster.limits import LimitExceeded, validate_metadata
from aster.protocol import CallHeader, RpcStatus, StreamHeader
from aster.service import ServiceInfo, MethodInfo
from aster.status import RpcError, StatusCode
from aster.rpc_types import SerializationMode

logger = logging.getLogger(__name__)


# ── In-process fake stream helpers ──────────────────────────────────────────


class _ByteQueue:
    """asyncio.Queue-backed byte pipe used for local (in-process) sessions."""

    def __init__(self) -> None:
        self._queue: asyncio.Queue[bytes | None] = asyncio.Queue()
        self._buf = b""
        self._finished = False

    async def write(self, data: bytes) -> None:
        await self._queue.put(data)

    async def finish(self) -> None:
        if not self._finished:
            self._finished = True
            await self._queue.put(None)  # EOF sentinel

    async def read_exact(self, n: int) -> bytes:
        while len(self._buf) < n:
            chunk = await self._queue.get()
            if chunk is None:
                # EOF -- return whatever we have (may be < n, caller detects short read)
                return self._buf
            self._buf += chunk
        result = self._buf[:n]
        self._buf = self._buf[n:]
        return result


class _FakeRecvStream:
    """Implements the RecvStream protocol backed by a _ByteQueue."""

    def __init__(self, queue: _ByteQueue) -> None:
        self._q = queue

    async def read_exact(self, n: int) -> bytes:
        return await self._q.read_exact(n)


class _FakeSendStream:
    """Implements the SendStream protocol backed by a _ByteQueue.

    Also supports ``finish()`` to signal end-of-stream.
    """

    def __init__(self, queue: _ByteQueue) -> None:
        self._q = queue

    async def write_all(self, data: bytes) -> None:
        await self._q.write(data)

    async def finish(self) -> None:
        await self._q.finish()


# ── Internal helper: write trailers / response frames ───────────────────────


async def _write_trailer(send: Any, codec: ForyCodec, code: StatusCode, message: str = "") -> None:
    status = RpcStatus(code=int(code), message=message)
    payload = codec.encode(status)
    await write_frame(send, payload, flags=TRAILER)


async def _write_ok_trailer(send: Any, codec: ForyCodec) -> None:
    await _write_trailer(send, codec, StatusCode.OK)


def _resolve_type(tp: Any) -> type | None:
    """Return *tp* if it is a concrete type, else None.

    Forward-reference strings and typing constructs (AsyncIterator[X], …)
    are not valid ``isinstance`` targets, so we fall back to ``None`` so
    Fory can use its built-in type detection instead.
    """
    if tp is None:
        return None
    if isinstance(tp, type):
        return tp
    return None


async def _write_response(send: Any, codec: ForyCodec, response: Any) -> None:
    payload, compressed = codec.encode_compressed(response)
    flags = COMPRESSED if compressed else 0
    await write_frame(send, payload, flags)


def _get_deadline_timeout(call_ctx: Any) -> float:
    """Return remaining seconds until deadline, clamped to server max.

    If the client set no deadline, returns MAX_HANDLER_TIMEOUT_S.
    If the client set a deadline further than MAX_HANDLER_TIMEOUT_S,
    returns MAX_HANDLER_TIMEOUT_S.
    """
    from aster.limits import MAX_HANDLER_TIMEOUT_S
    remaining = getattr(call_ctx, "remaining_seconds", None)
    if remaining is None:
        return MAX_HANDLER_TIMEOUT_S
    return max(0.0, min(remaining, MAX_HANDLER_TIMEOUT_S))


# ── Server side ──────────────────────────────────────────────────────────────


class SessionServer:
    """Server-side handler for a session-scoped service stream.

    One ``SessionServer`` instance is created per incoming QUIC stream whose
    ``StreamHeader.method`` is empty (``""``), indicating a session stream.
    The server instantiates the service class once per stream and dispatches
    successive CALL frames to the appropriate handler methods.
    """

    def __init__(
        self,
        service_class: type,
        service_info: ServiceInfo,
        codec: ForyCodec,
        interceptors: list[Any] | None = None,
        peer_store: Any | None = None,
    ) -> None:
        self._service_class = service_class
        self._service_info = service_info
        self._codec = codec
        self._interceptors = list(interceptors) if interceptors else []
        self._peer_store = peer_store

    async def run(
        self,
        stream_header: StreamHeader,
        send: Any,
        recv: Any,
        peer: str | None = None,
    ) -> None:
        """Drive the session lifecycle for one stream."""
        session_id = str(stream_header.callId) if stream_header.callId else str(uuid.uuid4())
        serialization_mode = stream_header.serializationMode

        # Instantiate the service class with peer= kwarg
        try:
            instance = self._service_class(peer=peer)
        except Exception as e:
            logger.error("Failed to instantiate session service: %s", e)
            await _write_trailer(send, self._codec, StatusCode.INTERNAL, str(e))
            return

        try:
            await self._session_loop(instance, session_id, serialization_mode, send, recv, peer)
        finally:
            # Fire on_session_close lifecycle hook if present
            close_hook = getattr(instance, "on_session_close", None)
            if close_hook is not None:
                try:
                    result = close_hook()
                    if asyncio.iscoroutine(result):
                        await result
                except Exception as e:
                    logger.debug("on_session_close raised: %s", e)

    async def _session_loop(
        self,
        instance: Any,
        session_id: str,
        serialization_mode: int,
        send: Any,
        recv: Any,
        peer: str | None,
    ) -> None:
        """Main receive loop for a session stream.

        Uses a shared ``_FrameQueue`` that is pumped by a background reader task.
        All dispatch handlers read from the frame queue (not directly from ``recv``),
        so control frames (CANCEL, next CALL) arriving while a handler is running
        are detected by the session loop without consuming frames out-of-order.
        """
        # Create a shared frame queue and start the pump task.
        frame_q: asyncio.Queue[tuple[bytes, int] | None] = asyncio.Queue()
        pump_task = asyncio.create_task(self._pump_frames(recv, frame_q))

        try:
            while True:
                frame = await frame_q.get()

                if frame is None:
                    # Clean EOF from pump
                    return

                payload, flags = frame

                # ── CANCEL without an in-flight call: just ack ───────────
                if flags & CANCEL:
                    await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
                    continue

                # ── Only CALL frames are valid at top of loop ─────────────
                if not (flags & CALL):
                    await _write_trailer(send, self._codec, StatusCode.FAILED_PRECONDITION, "Expected CALL frame")
                    return

                # ── Decode CallHeader ─────────────────────────────────────
                try:
                    call_header: CallHeader = self._codec.decode(payload, CallHeader)
                except Exception as e:
                    await _write_trailer(send, self._codec, StatusCode.INTERNAL, f"Invalid CallHeader: {e}")
                    return

                method_name = call_header.method
                method_info = self._service_info.get_method(method_name)
                if method_info is None:
                    await _write_trailer(send, self._codec, StatusCode.UNIMPLEMENTED, f"Method not found: {method_name}")
                    return

                handler_method = getattr(instance, method_name, None)
                if handler_method is None:
                    await _write_trailer(send, self._codec, StatusCode.INTERNAL, f"Handler missing: {method_name}")
                    return

                # Build call context
                _keys = call_header.metadataKeys or []
                _vals = call_header.metadataValues or []
                try:
                    validate_metadata(_keys, _vals)
                except LimitExceeded as exc:
                    await _write_trailer(send, self._codec, StatusCode.RESOURCE_EXHAUSTED, str(exc))
                    return
                metadata = dict(zip(_keys, _vals))
                attributes = {}
                if self._peer_store is not None and peer:
                    attributes = self._peer_store.get_attributes(peer)
                call_ctx = build_call_context(
                    service=self._service_info.name,
                    method=method_name,
                    metadata=metadata,
                    deadline_secs=call_header.deadline,
                    peer=peer,
                    is_streaming=method_info.pattern != "unary",
                    pattern=method_info.pattern,
                    idempotent=method_info.idempotent,
                    call_id=call_header.callId,
                    session_id=session_id,
                    attributes=attributes,
                )

                # Run authorization-style interceptors BEFORE dispatching the
                # method. This is the session-server equivalent of the upfront
                # check in server.py -- without it, session-scoped services
                # bypass Gate 3 capability checks entirely.
                if self._interceptors:
                    try:
                        await apply_request_interceptors(self._interceptors, call_ctx, None)
                    except RpcError as auth_err:
                        await _write_trailer(send, self._codec, auth_err.code, auth_err.message)
                        # Continue the session loop -- the client may make
                        # other calls that pass auth.
                        continue

                pattern = method_info.pattern

                if pattern == "unary":
                    interrupted = await self._dispatch_unary_with_cancel(
                        send, frame_q, call_ctx, handler_method, method_info
                    )
                    if interrupted is _SESSION_CLOSED:
                        return
                    if interrupted is _SESSION_NEXT_CALL:
                        # frame_q has the already-read CALL/CANCEL frame as next
                        continue
                    # interrupted is None → normal completion, loop for next CALL

                elif pattern == "server_stream":
                    interrupted = await self._dispatch_server_stream_with_cancel(
                        send, frame_q, call_ctx, handler_method, method_info
                    )
                    if interrupted is _SESSION_CLOSED:
                        return
                    if interrupted is _SESSION_NEXT_CALL:
                        continue

                elif pattern == "client_stream":
                    result = await self._dispatch_client_stream(send, frame_q, call_ctx, handler_method, method_info)
                    if result is _SESSION_CLOSED:
                        return

                elif pattern == "bidi_stream":
                    result = await self._dispatch_bidi_stream(send, frame_q, call_ctx, handler_method, method_info)
                    if result is _SESSION_CLOSED:
                        return

                else:
                    await _write_trailer(send, self._codec, StatusCode.INTERNAL, f"Unknown pattern: {pattern}")
                    return

        finally:
            pump_task.cancel()
            try:
                await pump_task
            except (asyncio.CancelledError, Exception):
                pass

    async def _pump_frames(
        self,
        recv: Any,
        queue: asyncio.Queue,
    ) -> None:
        """Read frames from ``recv`` and put them into ``queue``; puts ``None`` on EOF."""
        try:
            while True:
                frame = await read_frame(recv)
                await queue.put(frame)
                if frame is None:
                    return
        except asyncio.CancelledError:
            await queue.put(None)
            raise
        except Exception as e:
            logger.debug("Frame pump error: %s", e)
            await queue.put(None)

    async def _dispatch_unary_with_cancel(
        self,
        send: Any,
        frame_q: asyncio.Queue,
        call_ctx: Any,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> Any:
        """Dispatch unary call, handling CANCEL frames that arrive during handler.

        Returns:
            None: normal completion
            _SESSION_CLOSED: peer closed stream
            _SESSION_NEXT_CALL: a CANCEL or CALL frame arrived; it's already in frame_q
        """
        # Read request frame from queue
        frame = await frame_q.get()
        if frame is None:
            return _SESSION_CLOSED

        payload, flags = frame

        if flags & CANCEL:
            await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
            return None  # consumed the cancel, loop for next CALL

        if flags & CALL:
            # Out-of-order CALL → FAILED_PRECONDITION, put CALL frame back
            await frame_q.put(frame)
            await _write_trailer(send, self._codec, StatusCode.FAILED_PRECONDITION, "CALL frame before request")
            return _SESSION_NEXT_CALL

        compressed = bool(flags & COMPRESSED)
        try:
            request = self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.request_type))
        except Exception as e:
            await _write_trailer(send, self._codec, StatusCode.INTERNAL, f"Decode error: {e}")
            return None

        cancel_event = asyncio.Event()
        deadline_timeout = _get_deadline_timeout(call_ctx)

        # Run handler as task, concurrently wait for a CANCEL frame
        handler_task = asyncio.create_task(
            self._run_handler(handler_method, request, call_ctx, method_info.accepts_ctx)
        )
        cancel_reader_task = asyncio.create_task(self._read_cancel_frame(frame_q, cancel_event))

        deadline_task = asyncio.create_task(asyncio.sleep(deadline_timeout))
        wait_set: set[asyncio.Task] = {handler_task, cancel_reader_task, deadline_task}

        done, pending = await asyncio.wait(
            wait_set,
            return_when=asyncio.FIRST_COMPLETED,
        )

        if deadline_task in done:
            for t in (handler_task, cancel_reader_task):
                t.cancel()
                try:
                    await t
                except (asyncio.CancelledError, Exception):
                    pass
            await _write_trailer(send, self._codec, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
            return None

        if not deadline_task.done():
            deadline_task.cancel()
            try:
                await deadline_task
            except (asyncio.CancelledError, Exception):
                pass

        if cancel_reader_task in done:
            # CANCEL or unexpected frame arrived
            cancel_reader_task.result()  # propagate exception if any
            if cancel_event.is_set():
                # Cancel the handler
                handler_task.cancel()
                try:
                    await handler_task
                except (asyncio.CancelledError, Exception):
                    pass
                await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
                return None
            else:
                # A non-cancel control frame was placed back in frame_q by cancel_reader_task
                # Cancel the handler (mid-call CALL → FAILED_PRECONDITION)
                handler_task.cancel()
                try:
                    await handler_task
                except (asyncio.CancelledError, Exception):
                    pass
                await _write_trailer(send, self._codec, StatusCode.FAILED_PRECONDITION, "CALL received during active call")
                return _SESSION_NEXT_CALL
        else:
            # Handler completed first
            cancel_reader_task.cancel()
            try:
                await cancel_reader_task
            except (asyncio.CancelledError, Exception):
                pass

            try:
                response = handler_task.result()
            except RpcError as e:
                await _write_trailer(send, self._codec, e.code, e.message)
                return None
            except asyncio.CancelledError:
                await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
                return None
            except Exception as e:
                logger.error("Session unary handler error: %s", e)
                await _write_trailer(send, self._codec, StatusCode.INTERNAL, str(e))
                return None

            if cancel_event.is_set():
                await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
            else:
                await _write_response(send, self._codec, response)
            return None

    async def _dispatch_server_stream_with_cancel(
        self,
        send: Any,
        frame_q: asyncio.Queue,
        call_ctx: Any,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> Any:
        """Dispatch server_stream call, handling CANCEL."""
        # Read request frame from queue
        frame = await frame_q.get()
        if frame is None:
            return _SESSION_CLOSED

        payload, flags = frame

        if flags & CANCEL:
            await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
            return None

        if flags & CALL:
            await frame_q.put(frame)
            await _write_trailer(send, self._codec, StatusCode.FAILED_PRECONDITION, "CALL frame before request")
            return _SESSION_NEXT_CALL

        compressed = bool(flags & COMPRESSED)
        try:
            request = self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.request_type))
        except Exception as e:
            await _write_trailer(send, self._codec, StatusCode.INTERNAL, f"Decode error: {e}")
            return None

        cancel_event = asyncio.Event()
        deadline_timeout = _get_deadline_timeout(call_ctx)
        deadline_time = asyncio.get_event_loop().time() + deadline_timeout

        from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
        _cv_token = None
        try:
            response_iter, _cv_token = invoke_handler_with_ctx(
                handler_method, request, call_ctx, method_info.accepts_ctx,
            )
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter

            async for response in response_iter:
                if asyncio.get_event_loop().time() > deadline_time:
                    await _write_trailer(send, self._codec, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                    return None
                if cancel_event.is_set():
                    await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
                    return None
                # Check frame_q for cancel without blocking
                try:
                    ctrl_frame = frame_q.get_nowait()
                    _, ctrl_flags = ctrl_frame
                    if ctrl_flags & CANCEL:
                        cancel_event.set()
                        await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
                        return None
                    elif ctrl_flags & CALL:
                        # Put back and error
                        await frame_q.put(ctrl_frame)
                        await _write_trailer(send, self._codec, StatusCode.FAILED_PRECONDITION, "CALL received during stream")
                        return _SESSION_NEXT_CALL
                    elif ctrl_frame is None:
                        return _SESSION_CLOSED
                except asyncio.QueueEmpty:
                    pass
                await _write_response(send, self._codec, response)

            await _write_ok_trailer(send, self._codec)
            return None

        except asyncio.CancelledError:
            try:
                await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
            except Exception:
                pass
            raise
        except RpcError as e:
            await _write_trailer(send, self._codec, e.code, e.message)
            return None
        except Exception as e:
            logger.error("Session server_stream handler error: %s", e)
            await _write_trailer(send, self._codec, StatusCode.INTERNAL, str(e))
            return None
        finally:
            reset_call_context(_cv_token)

    async def _run_handler(self, handler_method: Any, request: Any, call_ctx: Any = None, accepts_ctx: bool = False) -> Any:
        """Run a unary handler and return its result.

        If ``accepts_ctx`` is True, the ``call_ctx`` is injected as the
        second positional argument. The ``CallContext._current`` contextvar
        is set for the duration of the call so handlers can also access the
        context via ``CallContext.current()``.
        """
        from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
        response, _token = invoke_handler_with_ctx(handler_method, request, call_ctx, accepts_ctx)
        try:
            if asyncio.iscoroutine(response):
                response = await response
            return response
        finally:
            reset_call_context(_token)

    async def _read_cancel_frame(
        self,
        frame_q: asyncio.Queue,
        cancel_event: asyncio.Event,
    ) -> None:
        """Read one frame from frame_q.

        If it is CANCEL, set cancel_event.
        If it is any other control frame (CALL, EOF), put it back in the queue
        (so the session loop can process it) and return without setting cancel_event.
        Data-only frames are silently dropped (should not arrive during unary).
        """
        frame = await frame_q.get()
        if frame is None:
            await frame_q.put(None)  # put EOF back
            return
        _, flags = frame
        if flags & CANCEL:
            cancel_event.set()
            return
        if flags & CALL:
            # Put the CALL frame back so session_loop handles it
            await frame_q.put(frame)
            return
        if flags & TRAILER:
            await frame_q.put(frame)
            return
        # Unknown/data frame during unary: put it back
        await frame_q.put(frame)

    async def _dispatch_client_stream(
        self,
        send: Any,
        frame_q: asyncio.Queue,
        call_ctx: Any,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> Any:
        """Handle a client-stream call within a session.

        Reads frames from ``frame_q`` until TRAILER(OK) EoI.
        Returns ``_SESSION_CLOSED`` on EOF, else ``None``.
        """
        try:
            from aster.limits import MAX_CLIENT_STREAM_ITEMS
            requests: list[Any] = []
            cancelled = False

            while True:
                frame = await frame_q.get()
                if frame is None:
                    return _SESSION_CLOSED

                payload, flags = frame

                if flags & CANCEL:
                    cancelled = True
                    break

                if flags & TRAILER:
                    try:
                        eoi_status = self._codec.decode(payload, RpcStatus)
                        if eoi_status.code != StatusCode.OK:
                            await _write_trailer(
                                send, self._codec, StatusCode.INTERNAL,
                                f"client sent non-OK EoI trailer (code={eoi_status.code})",
                            )
                            return None
                    except Exception:
                        await _write_trailer(
                            send, self._codec, StatusCode.INTERNAL,
                            "malformed EoI trailer",
                        )
                        return None
                    break

                if len(requests) >= MAX_CLIENT_STREAM_ITEMS:
                    await _write_trailer(
                        send, self._codec, StatusCode.RESOURCE_EXHAUSTED,
                        f"client stream exceeded {MAX_CLIENT_STREAM_ITEMS} items",
                    )
                    return None

                compressed = bool(flags & COMPRESSED)
                item = self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.request_type))
                requests.append(item)

            if cancelled:
                await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
                return None

            async def request_iter() -> AsyncIterator[Any]:
                for item in requests:
                    yield item

            deadline_timeout = _get_deadline_timeout(call_ctx)
            from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
            _cv_token = None
            try:
                try:
                    coro, _cv_token = invoke_handler_with_ctx(
                        handler_method, request_iter(), call_ctx, method_info.accepts_ctx,
                    )
                    if asyncio.iscoroutine(coro):
                        response = await asyncio.wait_for(coro, timeout=deadline_timeout)
                    else:
                        response = coro
                except asyncio.TimeoutError:
                    await _write_trailer(send, self._codec, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                    return None

                # SUCCESS: write response payload only (no trailer, matches unary rule)
                await _write_response(send, self._codec, response)
                return None
            finally:
                reset_call_context(_cv_token)

        except asyncio.CancelledError:
            try:
                await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
            except Exception:
                pass
            raise
        except RpcError as e:
            await _write_trailer(send, self._codec, e.code, e.message)
            return None
        except Exception as e:
            logger.error("Session client_stream handler error: %s", e)
            await _write_trailer(send, self._codec, StatusCode.INTERNAL, str(e))
            return None

    async def _dispatch_bidi_stream(
        self,
        send: Any,
        frame_q: asyncio.Queue,
        call_ctx: Any,
        handler_method: Any,
        method_info: MethodInfo,
    ) -> Any:
        """Handle a bidi-stream call within a session.

        Reads frames from ``frame_q`` concurrently with streaming responses.
        Returns ``_SESSION_CLOSED`` on EOF, else ``None``.
        """
        try:
            request_queue: asyncio.Queue[Any] = asyncio.Queue()
            cancel_event = asyncio.Event()

            async def request_iter() -> AsyncIterator[Any]:
                while True:
                    item = await request_queue.get()
                    if item is _BIDI_EOF:
                        break
                    yield item

            from aster.interceptors.base import invoke_handler_with_ctx, reset_call_context
            response_iter, _cv_token = invoke_handler_with_ctx(
                handler_method, request_iter(), call_ctx, method_info.accepts_ctx,
            )
            if asyncio.iscoroutine(response_iter):
                response_iter = await response_iter

            reader_error: Exception | None = None

            async def reader() -> None:
                nonlocal reader_error
                try:
                    while True:
                        frame = await frame_q.get()
                        if frame is None:
                            break
                        payload, flags = frame
                        if flags & CANCEL:
                            cancel_event.set()
                            break
                        if flags & TRAILER:
                            break
                        compressed = bool(flags & COMPRESSED)
                        item = self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.request_type))
                        await request_queue.put(item)
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    # Record the error so the dispatch loop can surface it
                    # instead of returning OK on partial data.
                    reader_error = e
                    logger.warning("Session bidi reader error: %s", e)
                finally:
                    await request_queue.put(_BIDI_EOF)

            reader_task = asyncio.create_task(reader())
            deadline_timeout = _get_deadline_timeout(call_ctx)
            deadline_time = asyncio.get_event_loop().time() + deadline_timeout

            try:
                async for response in response_iter:
                    if asyncio.get_event_loop().time() > deadline_time:
                        reader_task.cancel()
                        try:
                            await reader_task
                        except (asyncio.CancelledError, Exception):
                            pass
                        await _write_trailer(send, self._codec, StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
                        return None
                    if cancel_event.is_set():
                        await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
                        return None
                    await _write_response(send, self._codec, response)

                # Wait for reader to finish so we can check its error state
                reader_task.cancel()
                try:
                    await reader_task
                except (asyncio.CancelledError, Exception):
                    pass

                if reader_error is not None:
                    await _write_trailer(
                        send, self._codec, StatusCode.INTERNAL,
                        f"bidi stream reader error: {reader_error}",
                    )
                else:
                    await _write_ok_trailer(send, self._codec)
                return None
            finally:
                if not reader_task.done():
                    reader_task.cancel()
                    try:
                        await reader_task
                    except (asyncio.CancelledError, Exception):
                        pass

        except asyncio.CancelledError:
            try:
                await _write_trailer(send, self._codec, StatusCode.CANCELLED, "cancelled")
            except Exception:
                pass
            raise
        except RpcError as e:
            await _write_trailer(send, self._codec, e.code, e.message)
            return None
        except Exception as e:
            logger.error("Session bidi_stream handler error: %s", e)
            await _write_trailer(send, self._codec, StatusCode.INTERNAL, str(e))
            return None
        finally:
            try:
                reset_call_context(_cv_token)
            except NameError:
                pass


_BIDI_EOF = object()
# Sentinels returned by dispatch methods to signal session_loop control flow
_SESSION_CLOSED = object()

_next_session_id = 0


def _alloc_session_id() -> int:
    global _next_session_id
    _next_session_id += 1
    return _next_session_id
_SESSION_NEXT_CALL = object()


# ── Client side ──────────────────────────────────────────────────────────────


class SessionStub:
    """Client stub for a session-scoped service.

    Calls are serialised via an internal asyncio.Lock (one in-flight call at
    a time, as required by the BIDI constraint in the spec).
    """

    def __init__(
        self,
        send: Any,
        recv: Any,
        codec: ForyCodec,
        service_info: ServiceInfo,
        interceptors: list[Any] | None,
        session_id: str,
    ) -> None:
        self._send = send
        self._recv = recv
        self._codec = codec
        self._service_info = service_info
        self._interceptors = list(interceptors) if interceptors else []
        self._session_id = session_id
        self._lock = asyncio.Lock()
        self._call_id_counter = 0

    def _next_call_id(self) -> int:
        self._call_id_counter += 1
        return self._call_id_counter

    async def close(self) -> None:
        """Close the session (client-side)."""
        await self._send.finish()

    async def cancel(self) -> None:
        """Cancel the current in-flight call (SS5.5).

        Sends a CANCEL frame on the session stream, then drains and
        discards response frames until the server responds with a
        TRAILER carrying StatusCode.CANCELLED.  This ensures the
        session stream is left in a clean state for subsequent calls.
        """
        async with self._lock:
            # Send CANCEL frame (empty payload)
            await write_frame(self._send, b"", flags=CANCEL)

            # Drain frames until we receive a CANCELLED trailer
            while True:
                frame = await read_frame(self._recv)
                if frame is None:
                    # Stream ended without a CANCELLED trailer
                    break
                payload, flags = frame
                if flags & TRAILER:
                    # Got the trailer -- verify it's CANCELLED and stop draining
                    try:
                        status: RpcStatus = self._codec.decode(payload, RpcStatus)
                        if status.code != StatusCode.CANCELLED:
                            logger.debug(
                                "Expected CANCELLED trailer after cancel, got code=%s",
                                status.code,
                            )
                    except Exception:
                        pass
                    break
                # Discard any data frames that arrive before the trailer

    async def _call_unary(self, method_info: MethodInfo, request: Any, metadata: dict | None = None, timeout: float | None = None) -> Any:
        async with self._lock:
            # Write CALL frame
            await self._write_call_frame(method_info, metadata)
            # Write request payload
            await _write_response(self._send, self._codec, request)
            # Read response -- success is a plain payload frame (no trailer)
            frame = await read_frame(self._recv)
            if frame is None:
                raise RpcError(StatusCode.UNAVAILABLE, "Session stream ended")
            payload, flags = frame
            if flags & TRAILER:
                status: RpcStatus = self._codec.decode(payload, RpcStatus)
                raise RpcError(StatusCode(status.code), status.message)
            compressed = bool(flags & COMPRESSED)
            return self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.response_type))

    async def _call_server_stream(self, method_info: MethodInfo, request: Any, metadata: dict | None = None, timeout: float | None = None) -> AsyncIterator[Any]:
        """Async generator yielding server-stream responses."""
        async with self._lock:
            await self._write_call_frame(method_info, metadata)
            await _write_response(self._send, self._codec, request)

            # Read responses until TRAILER(OK)
            responses: list[Any] = []
            error: RpcError | None = None
            while True:
                frame = await read_frame(self._recv)
                if frame is None:
                    break
                payload, flags = frame
                if flags & TRAILER:
                    status = self._codec.decode(payload, RpcStatus)
                    if status.code != StatusCode.OK:
                        error = RpcError(StatusCode(status.code), status.message)
                    break
                compressed = bool(flags & COMPRESSED)
                responses.append(self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.response_type)))

        if error:
            raise error

        for item in responses:
            yield item

    async def _call_client_stream(self, method_info: MethodInfo, requests: AsyncIterator[Any], metadata: dict | None = None, timeout: float | None = None) -> Any:
        async with self._lock:
            await self._write_call_frame(method_info, metadata)
            # Send request frames
            async for item in requests:
                await _write_response(self._send, self._codec, item)
            # Send explicit EoI trailer
            await _write_ok_trailer(self._send, self._codec)
            # Read response payload
            frame = await read_frame(self._recv)
            if frame is None:
                raise RpcError(StatusCode.UNAVAILABLE, "Session stream ended")
            payload, flags = frame
            if flags & TRAILER:
                status = self._codec.decode(payload, RpcStatus)
                raise RpcError(StatusCode(status.code), status.message)
            compressed = bool(flags & COMPRESSED)
            return self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.response_type))

    async def _call_bidi_stream_collect(self, method_info: MethodInfo, requests: AsyncIterator[Any], metadata: dict | None = None) -> list[Any]:
        """Send all requests, then collect all responses (simple bidi usage)."""
        async with self._lock:
            await self._write_call_frame(method_info, metadata)
            # Send all requests
            async for item in requests:
                await _write_response(self._send, self._codec, item)
            # Send EoI
            await _write_ok_trailer(self._send, self._codec)
            # Collect responses until TRAILER
            responses: list[Any] = []
            while True:
                frame = await read_frame(self._recv)
                if frame is None:
                    break
                payload, flags = frame
                if flags & TRAILER:
                    status = self._codec.decode(payload, RpcStatus)
                    if status.code != StatusCode.OK:
                        raise RpcError(StatusCode(status.code), status.message)
                    break
                compressed = bool(flags & COMPRESSED)
                responses.append(self._codec.decode_compressed(payload, compressed, _resolve_type(method_info.response_type)))
            return responses

    async def _write_call_frame(self, method_info: MethodInfo, metadata: dict | None) -> None:
        meta_keys = list((metadata or {}).keys())
        meta_vals = list((metadata or {}).values())
        call_header = CallHeader(
            method=method_info.name,
            callId=self._next_call_id(),
            deadline=0,
            metadataKeys=meta_keys,
            metadataValues=meta_vals,
        )
        payload = self._codec.encode(call_header)
        await write_frame(self._send, payload, flags=CALL)


def _generate_session_stub_class(service_info: ServiceInfo) -> type:
    """Generate a SessionStub subclass with typed method stubs."""

    class GeneratedSessionStub(SessionStub):
        pass

    for method_name, method_info in service_info.methods.items():
        _add_session_method_stub(GeneratedSessionStub, method_name, method_info)

    GeneratedSessionStub.__name__ = f"{service_info.name}SessionStub"
    GeneratedSessionStub.__doc__ = f"Session stub for {service_info.name} v{service_info.version}"
    return GeneratedSessionStub


def _add_session_method_stub(cls: type, method_name: str, method_info: MethodInfo) -> None:
    pattern = method_info.pattern

    if pattern == "unary":
        async def stub(
            self: SessionStub,
            request: Any,
            *,
            metadata: dict | None = None,
            timeout: float | None = None,
        ) -> Any:
            return await self._call_unary(method_info, request, metadata=metadata, timeout=timeout)

    elif pattern == "server_stream":
        async def stub(  # type: ignore[misc]
            self: SessionStub,
            request: Any,
            *,
            metadata: dict | None = None,
            timeout: float | None = None,
        ) -> AsyncIterator[Any]:
            results = []
            async for item in self._call_server_stream(method_info, request, metadata=metadata, timeout=timeout):
                results.append(item)
            return results

    elif pattern == "client_stream":
        async def stub(  # type: ignore[misc]
            self: SessionStub,
            requests: AsyncIterator[Any],
            *,
            metadata: dict | None = None,
            timeout: float | None = None,
        ) -> Any:
            return await self._call_client_stream(method_info, requests, metadata=metadata, timeout=timeout)

    elif pattern == "bidi_stream":
        async def stub(  # type: ignore[misc]
            self: SessionStub,
            requests: AsyncIterator[Any],
            *,
            metadata: dict | None = None,
        ) -> list[Any]:
            return await self._call_bidi_stream_collect(method_info, requests, metadata=metadata)

    else:
        return

    stub.__name__ = method_name
    stub.__doc__ = f"{pattern} session RPC {method_name}"
    setattr(cls, method_name, stub)


# ── Public factory functions ─────────────────────────────────────────────────


async def create_session(
    service_class: type,
    connection: Any = None,
    codec: ForyCodec | None = None,
    fory_config: ForyConfig | None = None,
    interceptors: list[Any] | None = None,
) -> SessionStub:
    """Open a session-scoped connection to a remote service.

    Opens a QUIC bi-directional stream and sends a StreamHeader with
    ``method=""`` to indicate session mode.

    Args:
        service_class: A class decorated with ``@service(scoped='session')``.
        connection: An ``IrohConnection`` instance.
        codec: Optional pre-built ``ForyCodec``.
        fory_config: Optional codec configuration.
        interceptors: Optional list of interceptor instances.

    Returns:
        A ``SessionStub`` instance.
    """
    from aster.decorators import _SERVICE_INFO_ATTR
    from aster.client import _collect_service_types

    service_info: ServiceInfo | None = getattr(service_class, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        raise ValueError(f"Class {service_class.__name__} is not decorated with @service")

    if codec is None:
        types = _collect_service_types(service_class, service_info)
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=list(types) if types else None,
            fory_config=fory_config,
        )

    if connection is None:
        raise ValueError("create_session requires a connection")

    send, recv = await connection.open_bi()
    wire_session_id = _alloc_session_id()
    session_id = str(wire_session_id)

    # Pick the wire serialization mode from the codec being used. If the
    # caller passed a JsonProxyCodec (e.g. because the server only speaks
    # JSON), the StreamHeader must declare mode 3 so the server takes the
    # JSON path; otherwise the service's declared mode wins.
    from aster.json_codec import JsonProxyCodec
    if isinstance(codec, JsonProxyCodec):
        ser_mode = SerializationMode.JSON.value
    elif service_info.serialization_modes:
        ser_mode = service_info.serialization_modes[0].value
    else:
        ser_mode = 0

    header = StreamHeader(
        service=service_info.name,
        method="",
        version=service_info.version,
        callId=wire_session_id,
        serializationMode=ser_mode,
    )
    header_payload = codec.encode(header)
    await write_frame(send, header_payload, flags=HEADER)

    stub_cls = _generate_session_stub_class(service_info)
    return stub_cls(
        send=send,
        recv=recv,
        codec=codec,
        service_info=service_info,
        interceptors=interceptors,
        session_id=session_id,
    )


class SessionProxyClient:
    """Dynamic proxy client for a session-scoped service.

    Created via ``AsterClient.session("ServiceName")``. Works with dicts
    instead of typed dataclasses. Maintains a lock for one-call-at-a-time
    (spec requirement).
    """

    def __init__(self, send: Any, recv: Any, codec: Any, session_id: str) -> None:
        self._send = send
        self._recv = recv
        self._codec = codec
        self._session_id = session_id
        self._lock = asyncio.Lock()
        self._call_id_counter = 0

    async def call(self, method: str, request: dict | None = None) -> dict | Any:
        """Call a unary method on this session."""
        import struct as _struct

        async with self._lock:
            self._call_id_counter += 1
            call_header = CallHeader(
                method=method,
                callId=self._call_id_counter,
                deadline=0,
            )
            ch_payload = self._codec.encode(call_header)
            req_payload = self._codec.encode(request or {})

            call_header_frame = (
                _struct.pack("<I", len(ch_payload) + 1)
                + bytes([CALL])
                + ch_payload
            )
            request_frame = (
                _struct.pack("<I", len(req_payload) + 1)
                + bytes([0])
                + req_payload
            )

            # Use single-crossing session call if available (IrohStreams)
            if hasattr(self._send, '__class__') and hasattr(self._recv, '__class__'):
                try:
                    from aster._aster import session_unary_call as _session_unary_call
                    (
                        resp_payload,
                        resp_flags,
                        trailer_payload,
                        trailer_flags,
                    ) = await _session_unary_call(
                        self._send, self._recv,
                        call_header_frame, request_frame,
                    )
                    if trailer_flags & TRAILER and trailer_payload:
                        status = self._codec.decode(trailer_payload, RpcStatus)
                        if status.code != StatusCode.OK:
                            raise RpcError(StatusCode(status.code), status.message)
                    return self._codec.decode(resp_payload)
                except ImportError:
                    pass

            # Fallback: per-frame writes + reads
            await write_frame(self._send, ch_payload, flags=CALL)
            await write_frame(self._send, req_payload, flags=0)

            frame = await read_frame(self._recv)
            if frame is None:
                raise RpcError(StatusCode.UNAVAILABLE, "Session stream ended")
            resp_payload, flags = frame
            if flags & TRAILER:
                status = self._codec.decode(resp_payload, RpcStatus)
                raise RpcError(StatusCode(status.code), status.message)
            return self._codec.decode(resp_payload)

    async def close(self) -> None:
        """Close the session."""
        try:
            await self._send.finish()
        except Exception:
            pass

    def __getattr__(self, name: str) -> Any:
        if name.startswith("_"):
            raise AttributeError(name)

        async def method_stub(request: dict | None = None, **kwargs: Any) -> dict | Any:
            if kwargs and request is None:
                request = kwargs
            return await self.call(name, request)

        method_stub.__name__ = name
        return method_stub


async def create_proxy_session(
    service_name: str,
    connection: Any,
    codec: Any = None,
    aster_client: Any = None,
) -> SessionProxyClient:
    """Open a session-scoped proxy connection to a remote service.

    Like create_session() but works without local type definitions.
    Uses JSON codec by default.
    """
    if codec is None:
        from aster.json_codec import JsonProxyCodec
        codec = JsonProxyCodec()

    send, recv = await connection.open_bi()
    wire_session_id = _alloc_session_id()
    session_id = str(wire_session_id)

    from aster.json_codec import JsonProxyCodec
    ser_mode = SerializationMode.JSON.value if isinstance(codec, JsonProxyCodec) else 0

    header = StreamHeader(
        service=service_name,
        method="",
        version=1,
        callId=wire_session_id,
        serializationMode=ser_mode,
    )
    header_payload = codec.encode(header)
    await write_frame(send, header_payload, flags=HEADER)

    return SessionProxyClient(send=send, recv=recv, codec=codec, session_id=session_id)


def create_local_session(
    service_class: type,
    service_class_impl_class: type | None = None,
    wire_compatible: bool = True,
    codec: ForyCodec | None = None,
    fory_config: ForyConfig | None = None,
    interceptors: list[Any] | None = None,
) -> SessionStub:
    """Create an in-process session stub for a session-scoped service.

    Spins up a background ``SessionServer`` task connected to the stub via
    in-memory ``_ByteQueue`` pipes.

    Args:
        service_class: The service interface class (decorated with ``@service``).
        service_class_impl_class: The implementation class (must accept ``peer=``
            in ``__init__``). If ``None``, ``service_class`` itself is used.
        wire_compatible: If ``True``, exercises full serialisation pipeline.
        codec: Optional pre-built ``ForyCodec``.
        fory_config: Optional codec configuration.
        interceptors: Optional list of interceptor instances.

    Returns:
        A ``SessionStub`` bound to the local server.
    """
    from aster.decorators import _SERVICE_INFO_ATTR
    from aster.client import _collect_service_types

    service_info: ServiceInfo | None = getattr(service_class, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        raise ValueError(f"Class {service_class.__name__} is not decorated with @service")

    impl_class = service_class_impl_class or service_class

    if codec is None:
        types = _collect_service_types(service_class, service_info)
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=list(types) if types else None,
            fory_config=fory_config,
        )

    # Two queues: client→server (c2s) and server→client (s2c)
    c2s = _ByteQueue()
    s2c = _ByteQueue()

    c2s_send = _FakeSendStream(c2s)
    c2s_recv = _FakeRecvStream(c2s)
    s2c_send = _FakeSendStream(s2c)
    s2c_recv = _FakeRecvStream(s2c)

    wire_session_id = _alloc_session_id()
    session_id = str(wire_session_id)

    # Build a StreamHeader that the SessionServer will receive
    stream_header = StreamHeader(
        service=service_info.name,
        method="",
        version=service_info.version,
        callId=wire_session_id,
        serializationMode=(
            service_info.serialization_modes[0].value
            if service_info.serialization_modes
            else 0
        ),
    )

    session_server = SessionServer(
        service_class=impl_class,
        service_info=service_info,
        codec=codec,
        interceptors=interceptors,
    )

    # Spawn server background task
    loop = asyncio.get_event_loop()
    loop.create_task(
        session_server.run(stream_header, s2c_send, c2s_recv, peer="local")
    )

    stub_cls = _generate_session_stub_class(service_info)
    return stub_cls(
        send=c2s_send,
        recv=s2c_recv,
        codec=codec,
        service_info=service_info,
        interceptors=interceptors,
        session_id=session_id,
    )
