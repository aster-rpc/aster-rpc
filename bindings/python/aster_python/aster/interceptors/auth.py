"""Authentication interceptor."""

from __future__ import annotations

from collections.abc import Callable

from aster_python.aster.interceptors.base import CallContext, Interceptor
from aster_python.aster.status import RpcError, StatusCode


class AuthInterceptor(Interceptor):
    """Injects and/or validates auth metadata."""

    def __init__(
        self,
        *,
        token_provider: str | Callable[[], str] | None = None,
        validator: str | Callable[[str], bool] | None = None,
        metadata_key: str = "authorization",
        scheme: str | None = "Bearer",
    ) -> None:
        self._token_provider = token_provider
        self._validator = validator
        self._metadata_key = metadata_key
        self._scheme = scheme

    async def on_request(self, ctx: CallContext, request: object) -> object:
        if self._token_provider is not None and self._metadata_key not in ctx.metadata:
            token = self._token_provider() if callable(self._token_provider) else self._token_provider
            ctx.metadata[self._metadata_key] = (
                f"{self._scheme} {token}" if self._scheme else str(token)
            )

        if self._validator is not None:
            raw_token = ctx.metadata.get(self._metadata_key, "")
            token = raw_token
            prefix = f"{self._scheme} " if self._scheme else ""
            if prefix and raw_token.startswith(prefix):
                token = raw_token[len(prefix):]
            valid = self._validator(token) if callable(self._validator) else token == self._validator
            if not valid:
                raise RpcError(StatusCode.UNAUTHENTICATED, "authentication failed")

        return request