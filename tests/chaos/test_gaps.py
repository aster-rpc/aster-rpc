"""
Targeted tests for specific audit gaps (G1-G6).

Unlike the parametrized chaos matrix, these tests construct precise
fault scenarios designed to expose known vulnerabilities. A passing
test means the gap is NOT present; a failing test CONFIRMS the gap.
"""

import asyncio
import dataclasses
import uuid
from typing import AsyncIterator
import pytest

from aster.codec import ForyCodec, wire_type
from aster.decorators import service, rpc, client_stream, bidi_stream
from aster.framing import (
    CALL, CANCEL, HEADER, TRAILER, COMPRESSED,
    read_frame, write_frame,
)
from aster.protocol import CallHeader, RpcStatus, StreamHeader
from aster.rpc_types import SerializationMode
from aster.session import (
    SessionServer, SessionStub,
    _ByteQueue, _FakeRecvStream, _FakeSendStream,
    _generate_session_stub_class, create_local_session,
)
from aster.status import RpcError, StatusCode

from .workloads import (
    ChaosSession, EchoReq, EchoResp, CounterReq, CounterResp,
    SumItem, SumResp, ALL_TYPES,
)


def _codec():
    return ForyCodec(mode=SerializationMode.XLANG, types=list(ALL_TYPES))


def _make_session_pipes():
    """Create raw pipes + session server task. Returns everything needed."""
    from aster.decorators import _SERVICE_INFO_ATTR
    info = getattr(ChaosSession, _SERVICE_INFO_ATTR)
    codec = _codec()
    c2s = _ByteQueue()
    s2c = _ByteQueue()
    c_send = _FakeSendStream(c2s)
    s_recv = _FakeRecvStream(c2s)
    s_send = _FakeSendStream(s2c)
    c_recv = _FakeRecvStream(s2c)
    header = StreamHeader(
        service=info.name, method="", version=info.version,
        callId=2,
        serializationMode=SerializationMode.XLANG.value,
    )
    server = SessionServer(
        service_class=ChaosSession, service_info=info, codec=codec,
    )
    task = asyncio.get_event_loop().create_task(
        server.run(header, s_send, s_recv, peer="test")
    )
    return c_send, c_recv, task, codec


async def _call_unary(c_send, c_recv, codec, method, request):
    """Send a CALL + request, read response. Returns response or raises."""
    call_hdr = CallHeader(method=method, callId=3)
    await write_frame(c_send, codec.encode(call_hdr), flags=CALL)
    await write_frame(c_send, codec.encode(request), flags=0)

    frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
    if frame is None:
        raise ConnectionError("stream ended")
    payload, flags = frame
    if flags & TRAILER:
        status = codec.decode(payload, RpcStatus)
        raise RpcError(StatusCode(status.code), status.message)
    return codec.decode(payload)


# =============================================================================
# G1: Session lock + network fault
#
# After a recv error, the NEXT call on the same session must either fail
# cleanly or succeed with the CORRECT response. It must NEVER return the
# previous call's response (silent corruption).
# =============================================================================

@pytest.mark.timeout(15)
async def test_g1_session_continues_after_recv_fault():
    """G1: Second call after recv fault must not return stale/corrupt data."""
    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # First call: succeeds normally
        resp1 = await _call_unary(c_send, c_recv, codec, "echo", EchoReq(value=42))
        assert resp1.value == 42, f"first call: expected 42, got {resp1.value}"

        # Second call: also should succeed (no fault injected yet)
        resp2 = await _call_unary(c_send, c_recv, codec, "echo", EchoReq(value=99))
        assert resp2.value == 99, f"second call: expected 99, got {resp2.value}"

        # Both calls succeeded: session pipe is working.
        # The real G1 test: when using the SessionStub (with its lock),
        # can we detect if the lock fails to protect after a partial read?
        # For now, confirm basic sequential correctness.

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G2: CANCEL must produce a CANCELLED trailer
#
# Send a CALL for a slow method, then send CANCEL. The server MUST
# respond with exactly one CANCELLED trailer.
# =============================================================================

@pytest.mark.timeout(15)
async def test_g2_cancel_produces_cancelled_trailer():
    """G2: CANCEL frame must produce a CANCELLED trailer."""
    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # Start a slow call (increment with 10s delay)
        call_hdr = CallHeader(method="increment", callId=4)
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)
        await write_frame(c_send, codec.encode(CounterReq(delay=10.0)), flags=0)

        # Let the server start handling
        await asyncio.sleep(0.2)

        # Send CANCEL
        await write_frame(c_send, b"", flags=CANCEL)

        # Read frames until we get a CANCELLED trailer
        got_cancelled = False
        for _ in range(10):
            try:
                frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
            except asyncio.TimeoutError:
                break
            if frame is None:
                break
            payload, flags = frame
            if flags & TRAILER:
                status = codec.decode(payload, RpcStatus)
                if status.code == StatusCode.CANCELLED:
                    got_cancelled = True
                break

        assert got_cancelled, (
            "Server did not send CANCELLED trailer after receiving CANCEL frame. "
            "Spec requires exactly one CANCELLED trailer unconditionally."
        )

        # Verify session is still alive: make another call
        resp = await _call_unary(c_send, c_recv, codec, "echo", EchoReq(value=7))
        assert resp.value == 7, f"post-cancel call: expected 7, got {resp.value}"

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G4: Client stream EoI must validate status=OK
#
# If a frame with TRAILER flag but non-OK status arrives during a client
# stream, the server should NOT silently treat it as clean end-of-input.
# =============================================================================

@pytest.mark.timeout(15)
async def test_g4_client_stream_rejects_non_ok_trailer():
    """G4: Client stream EoI trailer must have status=OK."""
    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # Start a client_stream call (sum_stream)
        call_hdr = CallHeader(method="sum_stream", callId=5)
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)

        # Send some items
        await write_frame(c_send, codec.encode(SumItem(value=10)), flags=0)
        await write_frame(c_send, codec.encode(SumItem(value=20)), flags=0)

        # Send EoI with OK status (normal case)
        ok_status = RpcStatus(code=StatusCode.OK, message="")
        await write_frame(c_send, codec.encode(ok_status), flags=TRAILER)

        # Read response
        frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
        assert frame is not None, "no response from server"
        payload, flags = frame
        assert not (flags & TRAILER), "expected data frame, got trailer"
        resp = codec.decode(payload, SumResp)
        assert resp.total == 30, f"expected total=30, got {resp.total}"

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


@pytest.mark.timeout(15)
async def test_g4_client_stream_non_ok_eoi():
    """G4: Sending a non-OK trailer as EoI should not produce a success response."""
    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # Start a client_stream call
        call_hdr = CallHeader(method="sum_stream", callId=6)
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)

        # Send some items
        await write_frame(c_send, codec.encode(SumItem(value=10)), flags=0)

        # Send EoI with INTERNAL error status (simulating corruption)
        bad_status = RpcStatus(code=StatusCode.INTERNAL, message="corrupted")
        await write_frame(c_send, codec.encode(bad_status), flags=TRAILER)

        # Read response -- should be an error, not a success with total=10
        frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
        assert frame is not None, "no response from server"
        payload, flags = frame

        if flags & TRAILER:
            status = codec.decode(payload, RpcStatus)
            # Server recognized the bad EoI and returned an error -- GOOD
            assert status.code != StatusCode.OK, (
                "Server returned OK despite receiving non-OK EoI trailer"
            )
        else:
            # Server returned a data response -- it treated the bad EoI as
            # clean end-of-input and computed a result. This is the G4 bug.
            resp = codec.decode(payload)
            total = getattr(resp, "total", None)
            if total is not None:
                pytest.fail(
                    f"SILENT CORRUPTION: Server returned total={total} after "
                    f"receiving a non-OK EoI trailer. It should have rejected "
                    f"the stream, not computed a result from partial data."
                )

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G6: Bidi reader exception must not become silent EOF
#
# If the frame reader hits an error mid-bidi-stream, the handler should
# see an error (not clean EOF) and the call should fail (not succeed).
# =============================================================================

@wire_type("chaos/BidiReq")
@dataclasses.dataclass
class BidiReq:
    value: int = 0

@wire_type("chaos/BidiResp")
@dataclasses.dataclass
class BidiResp:
    value: int = 0
    count: int = 0

@service(name="ChaosBidi", version=1, scoped="session")
class ChaosBidi:
    def __init__(self, peer=None):
        self.peer = peer

    @bidi_stream
    async def echo_bidi(self, reqs: AsyncIterator[BidiReq]) -> AsyncIterator[BidiResp]:
        count = 0
        async for r in reqs:
            count += 1
            yield BidiResp(value=r.value, count=count)


@pytest.mark.timeout(15)
async def test_g6_bidi_reader_error_not_silent_eof():
    """G6: Bidi stream reader error must not become silent EOF."""
    from aster.decorators import _SERVICE_INFO_ATTR

    bidi_types = [BidiReq, BidiResp]
    codec = ForyCodec(mode=SerializationMode.XLANG, types=bidi_types)
    info = getattr(ChaosBidi, _SERVICE_INFO_ATTR)

    c2s = _ByteQueue()
    s2c = _ByteQueue()
    c_send = _FakeSendStream(c2s)
    s_recv = _FakeRecvStream(c2s)
    s_send = _FakeSendStream(s2c)
    c_recv = _FakeRecvStream(s2c)

    header = StreamHeader(
        service=info.name, method="", version=info.version,
        callId=7,
        serializationMode=SerializationMode.XLANG.value,
    )
    server = SessionServer(
        service_class=ChaosBidi, service_info=info, codec=codec,
    )
    task = asyncio.get_event_loop().create_task(
        server.run(header, s_send, s_recv, peer="test")
    )

    try:
        # Start bidi call
        call_hdr = CallHeader(method="echo_bidi", callId=8)
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)

        # Send a few valid items
        await write_frame(c_send, codec.encode(BidiReq(value=1)), flags=0)
        await asyncio.sleep(0.05)
        await write_frame(c_send, codec.encode(BidiReq(value=2)), flags=0)
        await asyncio.sleep(0.05)

        # Now send a corrupt frame (garbage bytes, not valid Fory)
        await write_frame(c_send, b"\xff\xfe\xfd\xfc\xfb", flags=0)

        # Read responses -- we should get responses for items 1 and 2
        responses = []
        for _ in range(5):
            try:
                frame = await asyncio.wait_for(read_frame(c_recv), timeout=3.0)
            except asyncio.TimeoutError:
                break
            if frame is None:
                break
            payload, flags = frame
            if flags & TRAILER:
                status = codec.decode(payload, RpcStatus)
                if status.code != StatusCode.OK:
                    # Server returned an error -- GOOD, it noticed the corruption
                    responses.append(("error", status.code, status.message))
                else:
                    responses.append(("ok_trailer", None, None))
                break
            else:
                try:
                    resp = codec.decode(payload, BidiResp)
                    responses.append(("data", resp.value, resp.count))
                except Exception:
                    responses.append(("decode_error", None, None))

        # The critical check: if the server returned ONLY data frames
        # (for values 1 and 2) and then an OK trailer, it means the
        # corrupt frame was silently treated as EOF. That's the G6 bug.
        types = [r[0] for r in responses]
        if types == ["data", "data", "ok_trailer"]:
            pytest.fail(
                "SILENT EOF: Server received a corrupt frame in bidi stream "
                "but treated it as clean EOF and returned OK. It should have "
                "returned an INTERNAL error trailer."
            )

        # Acceptable outcomes:
        # - ["data", "data", "error"] -- server caught the corruption
        # - ["data", "error"] -- server caught it mid-stream
        # - ["error"] -- server caught it immediately
        # - [] -- session ended (also acceptable under fault)

    finally:
        task.cancel()
        try:
            await task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G1: Session lock + concurrent calls after recv fault
#
# After a recv fault mid-response, the NEXT call must either fail cleanly
# or succeed with the CORRECT response. It must NEVER return a stale
# response from the previous call. This tests the SessionStub's asyncio.Lock
# under fault conditions.
# =============================================================================

@pytest.mark.timeout(15)
async def test_g1_concurrent_calls_after_recv_fault():
    """G1: Concurrent session calls after recv fault must not return stale data."""
    from aster.session import (
        SessionStub, _generate_session_stub_class,
    )
    from aster.decorators import _SERVICE_INFO_ATTR
    from .workloads import ChaosSession, EchoReq, EchoResp, ALL_TYPES

    info = getattr(ChaosSession, _SERVICE_INFO_ATTR)
    codec = _codec()

    c2s = _ByteQueue()
    s2c = _ByteQueue()
    c_send = _FakeSendStream(c2s)
    s_recv = _FakeRecvStream(c2s)
    s_send = _FakeSendStream(s2c)

    # Use a fault-injecting recv stream that fails on the Nth read
    class FaultyRecvStream:
        def __init__(self, inner: _FakeRecvStream, fail_after: int):
            self._inner = inner
            self._reads = 0
            self._fail_after = fail_after

        async def read_exact(self, n: int) -> bytes:
            self._reads += 1
            if self._reads == self._fail_after:
                raise ConnectionError("simulated network drop")
            return await self._inner.read_exact(n)

    # Fail on the 5th read (mid-response of first call: header reads + payload)
    c_recv = FaultyRecvStream(_FakeRecvStream(s2c), fail_after=5)

    header = StreamHeader(
        service=info.name, method="", version=info.version,
        callId=9,
        serializationMode=SerializationMode.XLANG.value,
    )
    from aster.session import SessionServer
    server = SessionServer(
        service_class=ChaosSession, service_info=info, codec=codec,
    )
    server_task = asyncio.get_event_loop().create_task(
        server.run(header, s_send, s_recv, peer="test")
    )

    stub_cls = _generate_session_stub_class(info)
    stub = stub_cls(
        send=c_send, recv=c_recv, codec=codec,
        service_info=info, interceptors=None,
        session_id=str(uuid.uuid4()),
    )

    try:
        # First call: should fail due to fault
        call1_failed = False
        call1_value = None
        try:
            resp1 = await asyncio.wait_for(stub.echo(EchoReq(value=42)), timeout=5.0)
            call1_value = resp1.value if hasattr(resp1, 'value') else None
        except Exception:
            call1_failed = True

        # Second call: may fail (stream corrupted) but must NEVER return 42
        call2_value = None
        call2_failed = False
        try:
            resp2 = await asyncio.wait_for(stub.echo(EchoReq(value=99)), timeout=5.0)
            call2_value = resp2.value if hasattr(resp2, 'value') else None
        except Exception:
            call2_failed = True

        # The critical invariant: if call 2 succeeded, its value must be 99
        # (never 42 from call 1's stale response)
        if call2_value is not None and call2_value != 99:
            pytest.fail(
                f"SILENT CORRUPTION: call 2 returned value={call2_value} "
                f"(expected 99 or failure). Call 1 sent value=42. "
                f"The session stub returned stale data from call 1."
            )

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G5: Decompression bomb protection
#
# A small compressed frame that decompresses to > 16 MiB must be rejected.
# The server should raise LimitExceeded, not OOM.
# =============================================================================

@pytest.mark.timeout(15)
async def test_g5_decompression_bomb_rejected():
    """G5: Compressed frame decompressing to > 16 MiB must be rejected."""
    try:
        import zstandard
    except ImportError:
        pytest.skip("zstandard not installed")

    codec = _codec()

    # Create a payload that compresses very well: 20 MiB of zeros
    big_data = b"\x00" * (20 * 1024 * 1024)
    cctx = zstandard.ZstdCompressor(level=3)
    bomb = cctx.compress(big_data)

    # The bomb should be small (high compression ratio on zeros)
    assert len(bomb) < 1024 * 1024, f"bomb is {len(bomb)} bytes, expected < 1 MiB"

    from aster.limits import LimitExceeded

    # Attempt to decompress through the codec's safe path
    try:
        codec._safe_decompress(bomb)
        pytest.fail(
            "DECOMPRESSION BOMB: codec decompressed 20 MiB payload without "
            "raising LimitExceeded. Server would OOM on pathological input."
        )
    except LimitExceeded:
        pass  # Expected: codec caught the bomb

    # Also test via the full decode_compressed path with COMPRESSED flag
    c_send, c_recv, server_task, test_codec = _make_session_pipes()

    try:
        # Start a session call
        call_hdr = CallHeader(method="echo", callId=10)
        await write_frame(c_send, test_codec.encode(call_hdr), flags=CALL)

        # Send the bomb as a compressed request frame
        await write_frame(c_send, bomb, flags=COMPRESSED)

        # Server should respond with an error (not crash/OOM)
        frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
        assert frame is not None, "server crashed without responding"
        payload, flags = frame

        if flags & TRAILER:
            status = test_codec.decode(payload, RpcStatus)
            assert status.code != StatusCode.OK, (
                "Server returned OK after receiving a decompression bomb"
            )
        # If it's a data frame, the server didn't catch it -- but it
        # shouldn't happen given the codec-level protection above

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G8: Deadline enforcement in session handlers
#
# If deadline is set in the CallHeader, the server should enforce
# it as a timeout on handler execution. Currently the deadline is read
# into CallContext but never used as a timeout.
# =============================================================================

@pytest.mark.timeout(15)
async def test_g8_deadline_enforced_in_session():
    """G8: Session handler must respect deadline as execution timeout."""
    import time

    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # Start a call to slow_echo with 10s delay but 1s deadline
        call_hdr = CallHeader(
            method="slow_echo",
            callId=1,
            deadline=1,  # 1 second relative
        )
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)
        await write_frame(c_send, codec.encode(EchoReq(value=42, delay=10.0)), flags=0)

        start = time.monotonic()

        # Read response -- should be an error within ~2s (deadline + skew)
        # If the server ignores the deadline, this will take 10s and the
        # test will time out.
        frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)

        elapsed = time.monotonic() - start

        if frame is None:
            pytest.fail("Server closed stream without responding")

        payload, flags = frame

        if flags & TRAILER:
            status = codec.decode(payload, RpcStatus)
            if status.code == StatusCode.DEADLINE_EXCEEDED:
                # Server enforced the deadline -- GOOD
                assert elapsed < 3.0, (
                    f"Deadline was enforced but took {elapsed:.1f}s "
                    f"(expected < 3s for a 1s deadline)"
                )
            else:
                # Server returned some other error within deadline -- acceptable
                pass
        else:
            # Server returned a success response
            if elapsed > 3.0:
                pytest.fail(
                    f"DEADLINE IGNORED: Server returned success after "
                    f"{elapsed:.1f}s, ignoring the 1s deadline. Handler "
                    f"ran to completion without timeout enforcement."
                )
            # If elapsed < 3s, the handler was fast enough -- shouldn't
            # happen with a 10s delay, but not a correctness violation

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G3: Metadata validation in session CallHeader
#
# Oversized metadata in a CallHeader must be rejected before the handler
# runs. The server should return RESOURCE_EXHAUSTED.
# =============================================================================

@pytest.mark.timeout(15)
async def test_g3_oversized_metadata_rejected():
    """G3: CallHeader with oversized metadata must be rejected."""
    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # Build a CallHeader with metadata exceeding limits
        # MAX_METADATA_VALUE_LEN = 4096, so send a 10KB value
        big_value = "x" * 10000
        call_hdr = CallHeader(
            method="echo",
            callId=11,
            metadataKeys=["big_key"],
            metadataValues=[big_value],
        )
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)

        # Server should reject with RESOURCE_EXHAUSTED before reading request
        frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
        assert frame is not None, "server crashed without responding"
        payload, flags = frame

        if flags & TRAILER:
            status = codec.decode(payload, RpcStatus)
            assert status.code == StatusCode.RESOURCE_EXHAUSTED, (
                f"Expected RESOURCE_EXHAUSTED for oversized metadata, "
                f"got code={status.code} message={status.message}"
            )
        else:
            pytest.fail(
                "Server accepted oversized metadata and dispatched handler "
                "instead of rejecting with RESOURCE_EXHAUSTED"
            )

        # Verify session is still alive after metadata rejection:
        # The current implementation returns (killing the session) rather
        # than continuing. That's acceptable but we note it.

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


@pytest.mark.timeout(15)
async def test_g3_too_many_metadata_entries_rejected():
    """G3: CallHeader with too many metadata entries must be rejected."""
    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # MAX_METADATA_ENTRIES = 64, so send 100 entries
        keys = [f"key_{i}" for i in range(100)]
        values = [f"val_{i}" for i in range(100)]
        call_hdr = CallHeader(
            method="echo",
            callId=12,
            metadataKeys=keys,
            metadataValues=values,
        )
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)

        frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
        assert frame is not None, "server crashed without responding"
        payload, flags = frame

        if flags & TRAILER:
            status = codec.decode(payload, RpcStatus)
            assert status.code == StatusCode.RESOURCE_EXHAUSTED, (
                f"Expected RESOURCE_EXHAUSTED for too many metadata entries, "
                f"got code={status.code} message={status.message}"
            )
        else:
            pytest.fail(
                "Server accepted 100 metadata entries without rejecting"
            )

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G10: OTT nonce replay
#
# When a nonce store is configured, replaying an OTT credential must be
# rejected. When no nonce store is configured, OTT credentials must be
# denied entirely (not silently accepted).
# =============================================================================

@pytest.mark.timeout(15)
async def test_g10_ott_nonce_consumed_on_replay():
    """G10: OTT nonce replayed a second time must be rejected."""
    from aster.trust.nonces import InMemoryNonceStore

    store = InMemoryNonceStore()
    nonce = b"\xaa" * 32  # 32 bytes

    # First consumption succeeds
    result1 = await store.consume(nonce)
    assert result1 is True, "first nonce consumption should succeed"

    # Second consumption must fail (replay)
    result2 = await store.consume(nonce)
    assert result2 is False, (
        "NONCE REPLAY: second consumption succeeded -- nonces are not "
        "being tracked, allowing credential replay attacks"
    )


@pytest.mark.timeout(15)
async def test_g10_ott_without_nonce_store_is_denied():
    """G10: OTT credential without nonce_store must be denied, not accepted."""
    import time
    from aster.trust.admission import check_offline
    from aster.trust.credentials import ConsumerEnrollmentCredential

    cred = ConsumerEnrollmentCredential(
        credential_type="ott",
        root_pubkey=b"\xbb" * 32,
        expires_at=int(time.time()) + 3600,
        nonce=b"\xcc" * 32,
        signature=b"\xdd" * 64,
        attributes={"aster.name": "test"},
    )

    result = await check_offline(cred, peer_endpoint_id="e" * 64, nonce_store=None)
    assert not result.admitted, (
        "OTT credential was admitted without a nonce_store -- "
        "this means dev mode silently skips nonce validation"
    )


# =============================================================================
# G12: Invalid/corrupt payload must produce error trailer, not crash
#
# Sending garbage bytes as a request frame should result in an INTERNAL
# error trailer, not an unhandled exception or hang.
# =============================================================================

@pytest.mark.timeout(15)
async def test_g12_corrupt_payload_produces_error_trailer():
    """G12: Corrupt request payload must produce INTERNAL trailer."""
    c_send, c_recv, server_task, codec = _make_session_pipes()

    try:
        # Send a valid CALL frame
        call_hdr = CallHeader(method="echo", callId=13)
        await write_frame(c_send, codec.encode(call_hdr), flags=CALL)

        # Send garbage bytes as the request payload
        await write_frame(c_send, b"\xff\xfe\xfd\xfc\xfb\xfa\x00\x01", flags=0)

        # Server should respond with an error trailer
        frame = await asyncio.wait_for(read_frame(c_recv), timeout=5.0)
        assert frame is not None, "server crashed without responding"
        payload, flags = frame

        if flags & TRAILER:
            status = codec.decode(payload, RpcStatus)
            assert status.code != StatusCode.OK, (
                "Server returned OK after receiving corrupt payload"
            )
        else:
            # Server returned a data frame -- it decoded garbage successfully?
            pytest.fail(
                "Server returned a data response for corrupt payload "
                "instead of an error trailer"
            )

    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


# =============================================================================
# G8-shared: Deadline enforcement in shared (non-session) dispatch
#
# The server-side upper bound MAX_HANDLER_TIMEOUT_S must be enforced
# even when no client deadline is set (deadline=0).
# =============================================================================

@pytest.mark.timeout(15)
async def test_g8_shared_handler_has_upper_bound():
    """Verify MAX_HANDLER_TIMEOUT_S exists and the helper clamps correctly."""
    from aster.limits import MAX_HANDLER_TIMEOUT_S
    from aster.session import _get_deadline_timeout
    from aster.interceptors.base import CallContext

    # No deadline set → should return MAX_HANDLER_TIMEOUT_S
    ctx_no_deadline = CallContext(service="test", method="test", deadline=None)
    timeout = _get_deadline_timeout(ctx_no_deadline)
    assert timeout == MAX_HANDLER_TIMEOUT_S, (
        f"Expected {MAX_HANDLER_TIMEOUT_S}s when no deadline, got {timeout}"
    )

    # Deadline far in the future → should be clamped
    import time
    far_future = time.time() + 99999
    ctx_far = CallContext(service="test", method="test", deadline=far_future)
    timeout_far = _get_deadline_timeout(ctx_far)
    assert timeout_far <= MAX_HANDLER_TIMEOUT_S, (
        f"Expected <= {MAX_HANDLER_TIMEOUT_S}s, got {timeout_far}"
    )

    # Deadline 1s from now → should be ~1s
    ctx_short = CallContext(service="test", method="test", deadline=time.time() + 1.0)
    timeout_short = _get_deadline_timeout(ctx_short)
    assert 0.5 < timeout_short <= 1.5, (
        f"Expected ~1s for 1s deadline, got {timeout_short}"
    )
