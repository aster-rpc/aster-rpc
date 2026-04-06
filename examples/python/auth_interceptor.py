"""
Interceptor Example — Aster RPC.

Demonstrates how to add cross-cutting concerns (logging, auth, metrics,
error handling) to Aster RPC calls using the interceptor API.

Interceptors implement the ``Interceptor`` base class with three hooks:
  - ``on_request(ctx, request)``  — runs before the handler (can modify/reject)
  - ``on_response(ctx, response)`` — runs after the handler (can modify/enrich)
  - ``on_error(ctx, error)``      — runs when an error occurs (can suppress/transform)

Interceptors compose: they are applied in order for requests, in reverse
order for errors.

This example defines:
  1. ``LoggingInterceptor`` — logs every call with timing
  2. ``TokenAuthInterceptor`` — rejects calls without a valid token
  3. ``ErrorMappingInterceptor`` — transforms internal errors to friendlier messages

It also shows how to use the built-in ``AuthInterceptor`` on the client side
to inject auth tokens into metadata automatically.

Usage (two terminals):

  # Terminal 1 — producer (server-side interceptors)
  python auth_interceptor.py producer

  # Terminal 2 — consumer (client-side interceptors)
  ASTER_ENDPOINT_ADDR=<printed by producer> python auth_interceptor.py consumer
"""
from __future__ import annotations

import asyncio
import logging
import os
import sys
import time
from dataclasses import dataclass

from aster import (
    AsterServer,
    AsterClient,
    Interceptor,
    CallContext,
    AuthInterceptor,
    RpcError,
    StatusCode,
)
from aster.codec import wire_type
from aster.decorators import service, rpc

# Enable logging so interceptor output is visible.
logging.basicConfig(level=logging.INFO, format="%(name)s: %(message)s")


# ── Message types ────────────────────────────────────────────────────────────


@wire_type("example.secure/GetSecretRequest")
@dataclass
class GetSecretRequest:
    key: str = ""


@wire_type("example.secure/GetSecretResponse")
@dataclass
class GetSecretResponse:
    value: str = ""
    accessed_by: str = ""


@wire_type("example.secure/PingRequest")
@dataclass
class PingRequest:
    pass


@wire_type("example.secure/PingResponse")
@dataclass
class PingResponse:
    message: str = ""


# ── Custom interceptors ─────────────────────────────────────────────────────


class LoggingInterceptor(Interceptor):
    """Logs every RPC call with method name, peer, and elapsed time.

    This interceptor demonstrates the ``on_request`` / ``on_response`` /
    ``on_error`` lifecycle hooks.
    """

    async def on_request(self, ctx: CallContext, request: object) -> object:
        # Store start time in context metadata for timing
        ctx.metadata["_log_start"] = str(time.monotonic())
        print(f"  [LOG] -> {ctx.service}.{ctx.method} from peer={ctx.peer or 'unknown'}")
        return request

    async def on_response(self, ctx: CallContext, response: object) -> object:
        start = float(ctx.metadata.get("_log_start", "0"))
        elapsed_ms = (time.monotonic() - start) * 1000
        print(f"  [LOG] <- {ctx.service}.{ctx.method} OK ({elapsed_ms:.1f}ms)")
        return response

    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        start = float(ctx.metadata.get("_log_start", "0"))
        elapsed_ms = (time.monotonic() - start) * 1000
        print(f"  [LOG] !! {ctx.service}.{ctx.method} ERROR: {error.code} {error.message} ({elapsed_ms:.1f}ms)")
        # Return the error to propagate it (return None to suppress)
        return error


class TokenAuthInterceptor(Interceptor):
    """Server-side interceptor that validates a bearer token in metadata.

    Rejects requests that lack a valid ``authorization`` header with
    ``UNAUTHENTICATED``.  This is a simplified version of the built-in
    ``AuthInterceptor`` to show how custom auth logic works.
    """

    VALID_TOKENS = {"secret-token-123", "admin-token-456"}

    async def on_request(self, ctx: CallContext, request: object) -> object:
        auth_header = ctx.metadata.get("authorization", "")
        token = auth_header.removeprefix("Bearer ").strip()

        if token not in self.VALID_TOKENS:
            print(f"  [AUTH] REJECTED: invalid token for {ctx.service}.{ctx.method}")
            raise RpcError(StatusCode.UNAUTHENTICATED, "invalid or missing auth token")

        print(f"  [AUTH] Authenticated token: {token[:8]}...")
        return request


class ErrorMappingInterceptor(Interceptor):
    """Transforms internal error messages to user-friendly versions.

    Demonstrates how ``on_error`` can modify or suppress errors before
    they reach the client.
    """

    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        # Map internal errors to friendlier messages
        if error.code == StatusCode.INTERNAL:
            return RpcError(
                StatusCode.INTERNAL,
                "An internal error occurred. Please try again later.",
            )
        if error.code == StatusCode.UNAUTHENTICATED:
            return RpcError(
                StatusCode.UNAUTHENTICATED,
                "Access denied. Please provide valid credentials.",
            )
        return error


# ── Service definition ───────────────────────────────────────────────────────


@service("SecureVault")
class SecureVaultService:
    """A vault service that requires authentication."""

    SECRETS = {
        "db-password": "hunter2",
        "api-key": "sk-aster-example-key",
        "jwt-secret": "my-signing-secret",
    }

    @rpc
    async def get_secret(self, req: GetSecretRequest) -> GetSecretResponse:
        """Retrieve a secret by key. Requires valid auth token."""
        value = self.SECRETS.get(req.key)
        if value is None:
            raise RpcError(StatusCode.NOT_FOUND, f"Secret '{req.key}' not found")
        return GetSecretResponse(value=value, accessed_by="authenticated-user")

    @rpc
    async def ping(self, req: PingRequest) -> PingResponse:
        """Simple health check (also requires auth via interceptor)."""
        return PingResponse(message="pong")


# ── Producer ─────────────────────────────────────────────────────────────────


async def run_producer() -> None:
    # Compose interceptors: logging first, then auth, then error mapping.
    # Request flow:  LoggingInterceptor -> TokenAuthInterceptor -> handler
    # Response flow: LoggingInterceptor <- handler
    # Error flow:    ErrorMappingInterceptor -> LoggingInterceptor (reversed)
    server_interceptors = [
        LoggingInterceptor(),
        TokenAuthInterceptor(),
        ErrorMappingInterceptor(),
    ]

    async with AsterServer(
        services=[SecureVaultService()],
        interceptors=server_interceptors,
    ) as srv:
        print()
        print("=== Secure Vault Producer (with interceptors) ===")
        print(f"  endpoint_addr : {srv.endpoint_addr_b64}")
        print(f"  interceptors  : {[type(i).__name__ for i in server_interceptors]}")
        print()
        print("  Run consumer with:")
        print(f"    ASTER_ENDPOINT_ADDR={srv.endpoint_addr_b64} python auth_interceptor.py consumer")
        print()
        print("  Waiting for connections... (Ctrl+C to stop)")
        try:
            await srv.serve()
        except (KeyboardInterrupt, asyncio.CancelledError):
            pass
    print("\n[producer] Stopped.")


# ── Consumer ─────────────────────────────────────────────────────────────────


async def run_consumer() -> None:
    async with AsterClient() as c:
        print(f"[consumer] Connected. Services: {[s.name for s in c.services]}")

        # ── 1. Client with valid auth token ──────────────────────────────────
        # Use the built-in AuthInterceptor to inject a bearer token into
        # every outgoing request's metadata.
        print("\n--- 1. Authenticated client (valid token) ---")
        vault = await c.client(
            SecureVaultService,
            interceptors=[
                AuthInterceptor(token_provider="secret-token-123"),
            ],
        )

        resp = await vault.ping(PingRequest())
        print(f"  ping -> {resp.message}")

        resp = await vault.get_secret(GetSecretRequest(key="api-key"))
        print(f"  get_secret('api-key') -> {resp.value}")

        resp = await vault.get_secret(GetSecretRequest(key="db-password"))
        print(f"  get_secret('db-password') -> {resp.value}")

        # ── 2. Request for non-existent secret ──────────────────────────────
        print("\n--- 2. Request for missing secret ---")
        try:
            await vault.get_secret(GetSecretRequest(key="does-not-exist"))
        except RpcError as e:
            print(f"  Expected error: [{e.code}] {e.message}")

        # ── 3. Client with invalid token ─────────────────────────────────────
        print("\n--- 3. Unauthenticated client (bad token) ---")
        bad_vault = await c.client(
            SecureVaultService,
            interceptors=[
                AuthInterceptor(token_provider="wrong-token"),
            ],
        )
        try:
            await bad_vault.ping(PingRequest())
        except RpcError as e:
            print(f"  Expected error: [{e.code}] {e.message}")

    print("\n[consumer] Done.")


# ── Entry point ──────────────────────────────────────────────────────────────


def main() -> None:
    if len(sys.argv) < 2 or sys.argv[1] not in ("producer", "consumer"):
        print("Usage: python auth_interceptor.py <producer|consumer>")
        print()
        print("  producer  — start the vault service with interceptors")
        print("  consumer  — connect and test auth/error flows")
        sys.exit(1)

    role = sys.argv[1]
    try:
        if role == "producer":
            asyncio.run(run_producer())
        else:
            asyncio.run(run_consumer())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
