"""Per-call compression override interceptor.

Spec reference: §S9.2g

Allows callers to override the default compression threshold on a per-call
basis by injecting metadata into the call context.  The actual compression
is performed by the codec/framing layer -- this interceptor simply sets
the ``_aster_compress_threshold`` and ``_aster_compress_enabled`` metadata
keys so downstream transport code can honour them.
"""

from __future__ import annotations

from aster.codec import DEFAULT_COMPRESSION_THRESHOLD
from aster.interceptors.base import CallContext, Interceptor


class CompressionInterceptor(Interceptor):
    """Standard interceptor for per-call compression configuration.

    Args:
        threshold: Payload size (bytes) above which zstd compression is
            applied.  Defaults to :data:`DEFAULT_COMPRESSION_THRESHOLD`
            (4096).  Set to ``-1`` to disable compression regardless of
            payload size.
        enabled: Master switch.  When ``False``, compression is suppressed
            for every call routed through this interceptor (equivalent to
            ``threshold=-1``).
    """

    def __init__(
        self,
        threshold: int = DEFAULT_COMPRESSION_THRESHOLD,
        enabled: bool = True,
    ) -> None:
        self.threshold = threshold
        self.enabled = enabled

    async def on_request(self, ctx: CallContext, request: object) -> object:
        """Inject compression settings into the call metadata."""
        effective_threshold = self.threshold if self.enabled else -1
        ctx.metadata["_aster_compress_threshold"] = str(effective_threshold)
        ctx.metadata["_aster_compress_enabled"] = str(self.enabled).lower()
        return request

    async def on_response(self, ctx: CallContext, response: object) -> object:
        """Pass through -- response compression uses the same settings."""
        return response
