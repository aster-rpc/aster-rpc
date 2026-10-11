"""Explicit authenticated or public HTTPS/WebTransport listeners and TLS helpers."""

from __future__ import annotations

import inspect
import math
from dataclasses import dataclass, field
from typing import Any, Callable

from .upcalls import UpcallRuntime, get_runtime

__all__ = ["HTTPListener", "RPCAuthRequest", "RPCAuthResponse", "GeneratedCertificate",
           "generate_self_signed_certificate", "generate_webtransport_certificate",
           "verify_certificate_binding"]


@dataclass(frozen=True)
class RPCAuthRequest:
    """Credentials and routing context for one RPC; peer_id is a transport hint."""

    peer_id: str
    service: str
    method: str
    session_id: int
    metadata: dict[str, str]


@dataclass(frozen=True)
class RPCAuthResponse:
    """A nonempty principal accepts; None rejects. Attributes feed Gate 3."""

    principal: str | None = None
    attributes: dict[str, str] = field(default_factory=dict)


class _AuthBinding:
    def __init__(self, handler: Any, runtime: UpcallRuntime) -> None:
        self.handler = handler
        self.upcall_runtime = runtime

    async def authenticate(self, peer, service, method, session_id, metadata):
        handler = getattr(self.handler, "authenticate", self.handler)
        answer = handler(RPCAuthRequest(peer, service, method, session_id, metadata))
        if inspect.isawaitable(answer):
            answer = await answer
        if not isinstance(answer, RPCAuthResponse):
            raise TypeError("authenticator must return RPCAuthResponse")
        if answer.principal is None:
            return None
        if not isinstance(answer.principal, str) or not answer.principal:
            raise ValueError("authenticated principal must be a nonempty string")
        if not isinstance(answer.attributes, dict) or not all(
            isinstance(k, str) and isinstance(v, str) for k, v in answer.attributes.items()
        ):
            raise TypeError("authentication attributes must be a dict of strings")
        return answer.principal, answer.attributes


class HTTPListener:
    """Owned native PEM listener with bound TCP and optional QUIC addresses.

    Await stop/aclose for released ports. Grace bounds application draining;
    QUIC protocol cleanup can extend beyond it. Cancelling stop retains the
    owner for retry. The selected authentication policy applies to routed RPCs;
    configured static files remain public.
    """

    def __init__(self, native, binding: _AuthBinding | None = None) -> None:
        self._native = native
        self._binding = binding
        self._closed = False

    @property
    def tcp_addr(self) -> str:
        return self._native.tcp_addr

    @property
    def quic_addr(self) -> str | None:
        return self._native.quic_addr

    async def stop(self, grace: float = 0.0) -> None:
        """Drain, cancel remaining work and join cleanup. Safe to repeat."""
        if not isinstance(grace, (int, float)) or not math.isfinite(grace) or not 0 <= grace <= 4294967.295:
            raise ValueError("grace must be finite and between 0 and 4294967.295 seconds")
        await self._native.stop(grace)
        if self._binding is not None:
            self._binding.upcall_runtime.unbind(self._binding)
        self._closed = True

    async def aclose(self) -> None:
        """Stop immediately and wait for cleanup, including QUIC drain."""
        await self.stop()

    async def __aenter__(self) -> HTTPListener:
        return self

    async def __aexit__(self, *exc) -> None:
        await self.aclose()

    def __del__(self):
        # Native owner Drop schedules forced cleanup even without a live Python
        # loop. Unbind the dedicated auth channel so its handler is not retained.
        binding = getattr(self, "_binding", None)
        if binding is not None:
            try:
                binding.upcall_runtime.unbind(binding)
            except Exception:
                pass


async def _start_http(
    native_server,
    *,
    authenticator: Callable[[RPCAuthRequest], Any] | Any,
    certificate_pem: bytes,
    private_key_pem: bytes,
    bind_addr: str = "127.0.0.1:0",
    webtransport: bool = False,
    max_pending_auth: int = 256,
    max_auth_payload_bytes: int = 65536,
    upcall_runtime: UpcallRuntime | None = None,
) -> HTTPListener:
    fn = getattr(authenticator, "authenticate", authenticator)
    if not callable(fn):
        raise TypeError("authenticator must be callable or implement authenticate(request)")
    if not bind_addr:
        raise ValueError("bind_addr must be nonempty")
    for name, value in (("max_pending_auth", max_pending_auth), ("max_auth_payload_bytes", max_auth_payload_bytes)):
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise ValueError(f"{name} must be a positive integer")
    runtime = upcall_runtime or getattr(authenticator, "upcall_runtime", None) or get_runtime()
    binding = _AuthBinding(authenticator, runtime)
    try:
        native = await native_server.start_http(
            binding, bind_addr, certificate_pem, private_key_pem,
            webtransport=webtransport,
            max_pending_auth=max_pending_auth,
            max_auth_payload_bytes=max_auth_payload_bytes,
        )
    except BaseException:
        runtime.unbind(binding)
        raise
    return HTTPListener(native, binding)


async def _start_public_http(native_server, *, certificate_pem: bytes, private_key_pem: bytes,
                             bind_addr: str = "127.0.0.1:0", webtransport: bool = False) -> HTTPListener:
    if not bind_addr:
        raise ValueError("bind_addr must be nonempty")
    native = await native_server.start_public_http(bind_addr, certificate_pem, private_key_pem,
                                                  webtransport=webtransport)
    return HTTPListener(native)


@dataclass(frozen=True)
class GeneratedCertificate:
    """PEM certificate/key and raw 32-byte SHA-256 of the certificate DER.

    The digest is suitable for the browser WebTransport serverCertificateHashes
    pin. Keep private_key_pem secret. Generation does not establish CA trust.
    """

    certificate_pem: bytes
    private_key_pem: bytes = field(repr=False)
    sha256: bytes


def generate_self_signed_certificate(sans: list[str] | None = None,
                                     node_secret: bytes | None = None) -> GeneratedCertificate:
    """Generate TLS material, optionally binding its public key to a 32-byte node secret."""
    from . import _aster
    return GeneratedCertificate(*_aster._generate_self_signed_certificate(sans or [], node_secret))


def generate_webtransport_certificate(subject: str, sans: list[str] | None = None,
                                      node_secret: bytes | None = None) -> GeneratedCertificate:
    """Generate a P-256 certificate valid for less than 14 days and its browser pin."""
    from . import _aster
    return GeneratedCertificate(*_aster._generate_webtransport_certificate(subject, sans or [], node_secret))


def verify_certificate_binding(certificate_pem: bytes, expected_node_id: str) -> None:
    """Verify that a node authorized the TLS public key (SPKI); raise ValueError on failure.

    This proof does not cover the entire certificate. Validate certificate trust,
    hostname and validity separately. Obtain expected_node_id through a trusted
    channel, rather than trusting an identity extracted only from the certificate.
    Unsigned certificates have no binding proof.
    """
    from . import _aster
    _aster._verify_certificate_binding(certificate_pem, expected_node_id)
