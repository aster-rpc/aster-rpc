"""
aster_cli.shell.invoker -- Dynamic RPC invocation from the shell.

Invokes service methods by name, handling:
  - Argument building from key=value pairs or interactive prompting
  - Unary, server-stream, client-stream, and bidi-stream patterns
  - Rich result formatting with timing
  - Error display
"""

from __future__ import annotations

import asyncio
import json
import time
from typing import Any, AsyncIterator

from aster_cli.shell.hooks import FieldSchema, MethodSchema, get_hook_registry
from aster_cli.shell.plugin import CommandContext
from aster_cli.shell.vfs import NodeKind, resolve_path


def _build_method_schema(
    service_name: str, method_name: str, method_meta: dict[str, Any]
) -> MethodSchema:
    """Build a MethodSchema from VFS metadata for hook consumption."""
    fields = method_meta.get("fields", [])
    request_fields = [
        FieldSchema(
            name=f.get("name", ""),
            type_name=f.get("kind", "") or f.get("type", "str"),
            required=f.get("required", True),
            default=f.get("default_value") if f.get("default_kind") == "value" else f.get("default"),
            description=f.get("description", ""),
        )
        for f in fields
    ] if fields else None

    return MethodSchema(
        service_name=service_name,
        method_name=method_name,
        pattern=method_meta.get("pattern", "unary"),
        request_type=method_meta.get("request_type", ""),
        response_type=method_meta.get("response_type", ""),
        request_fields=request_fields,
        timeout=method_meta.get("timeout"),
    )


async def invoke_method(
    ctx: CommandContext,
    service_name: str,
    method_name: str,
    payload: dict[str, Any],
) -> None:
    """Invoke a service method and display the result.

    Args:
        ctx: Command context with connection and display.
        service_name: Name of the service.
        method_name: Name of the method.
        payload: Arguments dict (from _parse_call_args).
    """
    display = ctx.display
    hooks = get_hook_registry()

    # Look up method metadata from VFS
    services_node = ctx.vfs_root.child("services")
    method_meta: dict[str, Any] = {}
    if services_node:
        svc_node = services_node.child(service_name)
        if svc_node:
            m_node = svc_node.child(method_name)
            if m_node:
                method_meta = m_node.metadata

    pattern = method_meta.get("pattern", "unary")
    schema = _build_method_schema(service_name, method_name, method_meta)

    # ── Input building (hook point) ───────────────────────────────────────
    # If no payload and method has parameters, use input builder hook
    if not payload and ctx.interactive and method_meta.get("fields"):
        builder = hooks.input_builder
        if builder:
            async def _ask(prompt: str) -> str | None:
                loop = asyncio.get_event_loop()
                try:
                    return await loop.run_in_executor(None, lambda: input(prompt))
                except (KeyboardInterrupt, EOFError):
                    return None

            payload = await builder.build_payload(schema, payload, _ask)
        else:
            payload = await _prompt_for_args(ctx, method_meta)

        if payload is None:
            return  # user cancelled

    display.info(f"-> {service_name}.{method_name}({_summarize_args(payload)})")

    # Fire guide event
    if hasattr(ctx, "guide") and ctx.guide:
        ctx.guide.fire("invoke")

    try:
        t0 = time.monotonic()

        # When the user is inside a `session <Service>` subshell, route
        # every call through the persistent SessionProxyClient instead of
        # opening a new bidi stream per invocation. This is the only way
        # to call methods on session-scoped services from the shell --
        # the per-call ``connection.invoke`` path explicitly rejects them.
        active_session = getattr(ctx, "session", None)
        if active_session is not None:
            result = await active_session.call(method_name, payload)
            elapsed = (time.monotonic() - t0) * 1000
            renderer = hooks.output_renderer
            rendered = False
            if renderer:
                rendered = await renderer.render_response(
                    schema, _to_serializable(result), display
                )
            if not rendered:
                display.rpc_result(_to_serializable(result), elapsed_ms=elapsed)
            return

        if pattern == "unary":
            result = await ctx.connection.invoke(service_name, method_name, payload)
            elapsed = (time.monotonic() - t0) * 1000

            # ── Output rendering (hook point) ─────────────────────────────
            renderer = hooks.output_renderer
            rendered = False
            if renderer:
                rendered = await renderer.render_response(
                    schema, _to_serializable(result), display
                )
            if not rendered:
                display.rpc_result(_to_serializable(result), elapsed_ms=elapsed)

        elif pattern == "server_stream":
            stream = await ctx.connection.server_stream(service_name, method_name, payload)
            await _display_stream(ctx, stream, t0)

        elif pattern == "client_stream":
            # Client stream: read values from user until empty line
            display.info("Enter values (JSON, one per line). Empty line to send:")
            values = await _read_stream_input(ctx)
            result = await ctx.connection.client_stream(service_name, method_name, values)
            elapsed = (time.monotonic() - t0) * 1000

            renderer = hooks.output_renderer
            rendered = False
            if renderer:
                rendered = await renderer.render_response(
                    schema, _to_serializable(result), display
                )
            if not rendered:
                display.rpc_result(_to_serializable(result), elapsed_ms=elapsed)

        elif pattern == "bidi_stream":
            await _handle_bidi(ctx, service_name, method_name, payload, t0)

        else:
            display.error(f"unknown pattern: {pattern}")

    except KeyboardInterrupt:
        display.info("(cancelled)")
    except Exception as e:
        msg = str(e)
        # Provide actionable hints for common errors
        if "Expected" in msg and "got dict" in msg:
            display.error(
                f"RPC failed: the server expected a typed object but received a dict.\n"
                f"  This usually means the shell is sending JSON to a Fory-only service.\n"
                f"  Try: aster call <addr> {service_name}.{method_name} '<json>'"
            )
        elif "FAILED_PRECONDITION" in msg or "scope mismatch" in msg.lower():
            display.error(
                f"RPC failed: '{service_name}' is session-scoped.\n"
                f"  Try: cd /services && session {service_name}"
            )
        elif "PERMISSION_DENIED" in msg:
            display.error(
                f"RPC failed: permission denied for {service_name}.{method_name}.\n"
                f"  Check your credential has the required role."
            )
        elif "DEADLINE_EXCEEDED" in msg:
            display.error(
                f"RPC failed: request timed out ({service_name}.{method_name})."
            )
        elif "UNAVAILABLE" in msg:
            display.error(
                f"RPC failed: service unavailable. The connection may have dropped.\n"
                f"  Try: refresh"
            )
        else:
            display.error(f"RPC failed: {e}")


async def _display_stream(
    ctx: CommandContext,
    stream: AsyncIterator[Any],
    t0: float,
) -> None:
    """Display a server-streaming response."""
    count = 0
    try:
        async for value in stream:
            ctx.display.streaming_value(count, _to_serializable(value))
            count += 1
    except KeyboardInterrupt:
        pass

    elapsed = (time.monotonic() - t0) * 1000
    ctx.display.info(f"({count} items, {elapsed:.0f}ms)")


async def _handle_bidi(
    ctx: CommandContext,
    service_name: str,
    method_name: str,
    initial_payload: dict[str, Any],
    t0: float,
) -> None:
    """Handle a bidirectional streaming call.

    Runs two concurrent tasks:
    - Reader: displays incoming values from the server
    - Writer: reads user input and sends to the server

    Type a JSON value and press Enter to send. Ctrl+D or empty line to stop sending.
    """
    display = ctx.display

    display.info("Bidi stream open. Type JSON values to send, Ctrl+D or empty line to close input.")
    display.info("─" * 40)

    send_queue: asyncio.Queue[Any] = asyncio.Queue()

    # Seed with initial payload if non-empty
    if initial_payload:
        await send_queue.put(initial_payload)

    async def input_producer() -> AsyncIterator[Any]:
        """Yield values from the send queue until sentinel."""
        while True:
            value = await send_queue.get()
            if value is _SENTINEL:
                return
            yield value

    async def read_user_input() -> None:
        """Read lines from stdin and push to send queue."""
        loop = asyncio.get_event_loop()
        try:
            while True:
                line = await loop.run_in_executor(None, _read_line_sync)
                if line is None or line.strip() == "":
                    break
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    # Try key=value
                    if "=" in line:
                        parts = line.strip().split("=", 1)
                        value = {parts[0]: parts[1]}
                    else:
                        display.error("invalid JSON -- enter a JSON value or key=value")
                        continue
                await send_queue.put(value)
        finally:
            await send_queue.put(_SENTINEL)

    async def display_responses(stream: AsyncIterator[Any]) -> None:
        """Display incoming stream values."""
        count = 0
        async for value in stream:
            display.streaming_value(count, _to_serializable(value))
            count += 1

    try:
        stream = ctx.connection.bidi_stream(
            service_name, method_name, input_producer()
        )
        await asyncio.gather(
            read_user_input(),
            display_responses(stream),
        )
    except KeyboardInterrupt:
        pass

    elapsed = (time.monotonic() - t0) * 1000
    display.info(f"(bidi stream closed, {elapsed:.0f}ms)")


async def _prompt_for_args(
    ctx: CommandContext,
    method_meta: dict[str, Any],
) -> dict[str, Any] | None:
    """Interactively prompt for method arguments.

    Uses method metadata (fields list) to prompt for each parameter.
    """
    fields = method_meta.get("fields", [])
    if not fields:
        return {}

    result: dict[str, Any] = {}
    ctx.display.print("[dim]Enter arguments (Ctrl+C to cancel):[/dim]")

    try:
        loop = asyncio.get_event_loop()
        for f in fields:
            name = f.get("name", f"arg{len(result)}")
            ftype = f.get("type", "str")
            required = f.get("required", True)
            default = f.get("default")

            prompt = f"  ▸ {name}"
            if ftype:
                prompt += f" ({ftype})"
            if default is not None:
                prompt += f" [{default}]"
            prompt += ": "

            value = await loop.run_in_executor(None, lambda p=prompt: input(p))
            value = value.strip()

            if not value and default is not None:
                result[name] = default
                continue
            elif not value and not required:
                continue
            elif not value:
                ctx.display.error(f"{name} is required")
                return None

            # Try to parse as JSON
            try:
                result[name] = json.loads(value)
            except (json.JSONDecodeError, ValueError):
                result[name] = value

    except (KeyboardInterrupt, EOFError):
        ctx.display.info("(cancelled)")
        return None

    return result


async def _read_stream_input(ctx: CommandContext) -> list[Any]:
    """Read a sequence of JSON values from stdin for client streaming."""
    values: list[Any] = []
    loop = asyncio.get_event_loop()

    try:
        while True:
            line = await loop.run_in_executor(None, lambda: input("  > "))
            line = line.strip()
            if not line:
                break
            try:
                values.append(json.loads(line))
            except json.JSONDecodeError:
                ctx.display.error(f"invalid JSON: {line}")
    except (KeyboardInterrupt, EOFError):
        pass

    return values


def _read_line_sync() -> str | None:
    """Read a line synchronously, returning None on EOF."""
    try:
        return input("  ⇢ ")
    except EOFError:
        return None


class _SentinelType:
    pass


_SENTINEL = _SentinelType()


def _to_serializable(obj: Any) -> Any:
    """Convert an object to a JSON-serializable form."""
    if obj is None or isinstance(obj, (str, int, float, bool)):
        return obj
    if isinstance(obj, bytes):
        try:
            return obj.decode("utf-8")
        except UnicodeDecodeError:
            return obj.hex()
    if isinstance(obj, dict):
        return {str(k): _to_serializable(v) for k, v in obj.items()}
    if isinstance(obj, (list, tuple)):
        return [_to_serializable(v) for v in obj]
    if hasattr(obj, "__dict__"):
        return {k: _to_serializable(v) for k, v in obj.__dict__.items()
                if not k.startswith("_")}
    return str(obj)


def _summarize_args(payload: dict[str, Any]) -> str:
    """Short summary of call arguments for display."""
    if not payload:
        return ""
    items = []
    for k, v in list(payload.items())[:3]:
        if isinstance(v, str) and len(v) > 20:
            v = v[:17] + "…"
        items.append(f"{k}={v!r}")
    s = ", ".join(items)
    if len(payload) > 3:
        s += f", … +{len(payload) - 3} more"
    return s
