"""
Tests for the trust helpers that remain after consumer admission was retired.

Covers:
- canonical JSON (shared by the @aster delegated-token verifier)
- rcan stub fails closed
- LocalTransport bypass: CallContext.peer is None, attributes == {}
- Auth interceptor: peer is None -> allow (in-process trust)

Enrollment (the replacement for consumer admission) is covered end to end in
``test_enrollment_aster_server.py``.
"""

from __future__ import annotations

import json

from aster.trust.signing import canonical_json_value_bytes as canonical_json


# ── canonical_json ─────────────────────────────────────────────────────────────


def test_canonical_json_sorted_keys():
    result = canonical_json({"z": "last", "a": "first", "m": "mid"})
    parsed = json.loads(result)
    assert list(parsed.keys()) == ["a", "m", "z"]


def test_canonical_json_empty():
    assert canonical_json({}) == b"{}"


def test_canonical_json_unicode():
    result = canonical_json({"k": "café"})
    assert "café".encode("utf-8") in result


def test_delegated_canonical_json_matches_rust_core():
    """Delegated-token canonical JSON must use the same form as Rust core
    (raw UTF-8, not \\uXXXX escapes) -- previously it used ensure_ascii=True."""
    from aster.trust.delegated import _canonical_json_bytes

    attrs = {"z": "naïve ✓", "a": "café", "ctl": "a\nb\x01"}
    assert _canonical_json_bytes(attrs) == canonical_json(attrs)
    assert "café".encode("utf-8") in _canonical_json_bytes(attrs)


def test_validate_rcan_fails_closed():
    """rcan format is unspecified; the stub must reject every grant."""
    from aster.trust.rcan import validate_rcan

    for grant in [b"", b"\x01", b"anything-at-all" * 10]:
        ok, reason = validate_rcan(grant)
        assert ok is False
        assert reason


# ── LocalTransport bypass ─────────────────────────────────────────────────────


def test_local_transport_call_context_peer_is_none():
    """LocalTransport CallContext.peer is None -- Gates 0 and 1 are bypassed."""
    from aster.interceptors import CallContext

    ctx = CallContext(service="TestService", method="DoSomething")
    assert ctx.peer is None
    assert ctx.attributes == {}


def test_auth_interceptor_allows_when_peer_is_none():
    """Auth interceptor canonical behaviour: peer is None → allow (in-process trust)."""
    from aster.interceptors import AuthInterceptor, CallContext

    interceptor = AuthInterceptor()
    ctx = CallContext(service="TestService", method="DoSomething")
    assert ctx.peer is None

    # The auth interceptor should not raise when peer is None
    # (in-process LocalTransport calls are trusted by construction)
    import asyncio

    async def run():
        # on_request is the hook used by server dispatch; peer=None means in-process
        result = await interceptor.on_request(ctx, b"request")
        return result

    # Should complete without raising
    loop = asyncio.new_event_loop()
    loop.run_until_complete(run())
    loop.close()
