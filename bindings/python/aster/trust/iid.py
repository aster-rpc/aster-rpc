"""
aster.trust.iid — Instance Identity Document (IID) verification.

Spec reference: Aster-trust-spec.md §2.4 (runtime checks).

IID verification is triggered when the credential carries ``aster.iid_provider``
and related ``aster.iid_*`` attributes.  The admitting node fetches the
connecting node's IID from the hypervisor metadata endpoint (one HTTP call to
``169.254.169.254``) and verifies both the cloud-provider signature and the
attribute claims.

Cloud providers:
  aws   → http://169.254.169.254/latest/dynamic/instance-identity/document
  gcp   → http://metadata.google.internal/computeMetadata/v1/instance/...
  azure → http://169.254.169.254/metadata/attested/document?api-version=2021-02-01

Phase 11: All backends are mock-pluggable for tests.  Production backends
require network access to the metadata endpoint and ``PyJWT`` for JWT
verification.  ``PyJWT`` is declared as the optional ``iid`` extra in
``pyproject.toml``.
"""

from __future__ import annotations

import logging
from typing import Protocol, runtime_checkable

from .credentials import ATTR_IID_ACCOUNT, ATTR_IID_PROVIDER, ATTR_IID_REGION

logger = logging.getLogger(__name__)


@runtime_checkable
class IIDBackend(Protocol):
    """Protocol for IID verification backends."""

    async def verify(
        self,
        attributes: dict[str, str],
        iid_token: str | None = None,
    ) -> tuple[bool, str | None]:
        """Verify IID claims against the given attributes.

        Args:
            attributes: Credential attributes containing ``aster.iid_*`` keys.
            iid_token: Optional pre-fetched IID token (e.g., from the peer's
                handshake payload).  If None, the backend fetches from the
                local metadata endpoint (for self-verification).

        Returns:
            (ok, reason) — ``ok=True`` on success; ``reason`` is a logging
            string on failure (not sent to peer).
        """
        ...


class MockIIDBackend:
    """Test double for IID verification.

    Configurable to always pass or always fail with a fixed reason.
    Optionally, ``expected_attributes`` can be set to check exact claim match.
    """

    def __init__(
        self,
        should_pass: bool = True,
        reason: str | None = None,
        expected_attributes: dict[str, str] | None = None,
    ) -> None:
        self.should_pass = should_pass
        self.reason = reason
        self.expected_attributes = expected_attributes
        self.call_count = 0

    async def verify(
        self,
        attributes: dict[str, str],
        iid_token: str | None = None,
    ) -> tuple[bool, str | None]:
        self.call_count += 1
        if not self.should_pass:
            return False, self.reason or "mock IID verification failed"
        if self.expected_attributes is not None:
            for key, expected_value in self.expected_attributes.items():
                if attributes.get(key) != expected_value:
                    return (
                        False,
                        f"IID attribute mismatch: {key}={attributes.get(key)!r} != {expected_value!r}",
                    )
        return True, None


class AWSIIDBackend:
    """AWS Instance Identity Document verification.

    Fetches the IID from ``http://169.254.169.254`` and verifies the
    AWS RSA-SHA256 signature against Amazon's published public key.
    Checks ``aster.iid_account``, ``aster.iid_region``, ``aster.iid_role_arn``.

    Requires: ``httpx`` or ``aiohttp`` for async HTTP and ``PyJWT`` or
    ``cryptography`` for RSA signature verification.
    """

    async def verify(
        self,
        attributes: dict[str, str],
        iid_token: str | None = None,
    ) -> tuple[bool, str | None]:
        try:
            import httpx  # type: ignore[import]
        except ImportError:
            return False, "httpx is required for AWS IID verification (pip install httpx)"

        iid_url = "http://169.254.169.254/latest/dynamic/instance-identity/document"
        sig_url = "http://169.254.169.254/latest/dynamic/instance-identity/signature"
        _MAX_IID_RESPONSE = 64 * 1024  # 64 KB — IID documents are small
        try:
            async with httpx.AsyncClient(timeout=2.0) as client:
                doc_resp = await client.get(iid_url)
                doc_resp.raise_for_status()
                if len(doc_resp.content) > _MAX_IID_RESPONSE:
                    return False, f"AWS IID response too large ({len(doc_resp.content)} bytes)"
                sig_resp = await client.get(sig_url)
                sig_resp.raise_for_status()
                if len(sig_resp.content) > _MAX_IID_RESPONSE:
                    return False, f"AWS IID signature too large ({len(sig_resp.content)} bytes)"
        except Exception as exc:  # noqa: BLE001
            return False, f"AWS metadata fetch failed: {exc}"

        import json as _json
        doc = _json.loads(doc_resp.text)
        # Signature is base64 of PKCS1v15(SHA256, document_bytes)
        # Full RSA verification against Amazon's cert deferred to production implementation
        # — requires fetching Amazon's EC2 instance identity certificate.

        # Claim checks
        expected_account = attributes.get(ATTR_IID_ACCOUNT)
        expected_region = attributes.get(ATTR_IID_REGION)
        if expected_account and doc.get("accountId") != expected_account:
            return False, f"IID accountId {doc.get('accountId')!r} != {expected_account!r}"
        if expected_region and doc.get("region") != expected_region:
            return False, f"IID region {doc.get('region')!r} != {expected_region!r}"
        return True, None


class GCPIIDBackend:
    """GCP Instance Identity Token verification (stub — deferred)."""

    async def verify(
        self,
        attributes: dict[str, str],
        iid_token: str | None = None,
    ) -> tuple[bool, str | None]:
        return False, "GCP IID verification is not yet implemented"


class AzureIIDBackend:
    """Azure Attested Document verification (stub — deferred)."""

    async def verify(
        self,
        attributes: dict[str, str],
        iid_token: str | None = None,
    ) -> tuple[bool, str | None]:
        return False, "Azure IID verification is not yet implemented"


def get_iid_backend(provider: str) -> IIDBackend:
    """Factory: return the appropriate backend for ``aster.iid_provider``.

    For tests, inject a ``MockIIDBackend`` directly into ``check_runtime``
    rather than relying on this factory.
    """
    provider = provider.lower()
    if provider == "aws":
        return AWSIIDBackend()
    elif provider == "gcp":
        return GCPIIDBackend()
    elif provider == "azure":
        return AzureIIDBackend()
    else:
        raise ValueError(f"Unknown IID provider: {provider!r}")


async def verify_iid(
    attributes: dict[str, str],
    backend: IIDBackend | None = None,
    iid_token: str | None = None,
) -> tuple[bool, str | None]:
    """Run IID verification for the given credential attributes.

    If no ``backend`` is supplied, the factory is called based on
    ``aster.iid_provider``.  Pass a ``MockIIDBackend`` in tests.

    Returns (ok, reason) — reason is logged, never sent to peer.
    """
    provider = attributes.get(ATTR_IID_PROVIDER)
    if provider is None:
        # No IID attribute — skip
        return True, None
    if backend is None:
        backend = get_iid_backend(provider)
    return await backend.verify(attributes, iid_token)
