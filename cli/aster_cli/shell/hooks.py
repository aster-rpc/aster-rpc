"""
aster_cli.shell.hooks -- Extension hooks for the shell.

Provides the hook protocol and registry for plugging in:
  - Input builders (construct RPC payloads from user intent)
  - Output renderers (format RPC responses for display)
  - Session lifecycle events

The LLM plugin will implement InputBuilder and OutputRenderer to handle
complex types conversationally rather than field-by-field.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, Protocol, runtime_checkable


# ── Hook protocols ────────────────────────────────────────────────────────────


@runtime_checkable
class InputBuilder(Protocol):
    """Builds an RPC request payload from user input and method schema.

    The default implementation prompts field-by-field. An LLM plugin
    replaces this with conversational input construction:

    1. Receives the method schema (fields, types, nested types, constraints)
    2. Receives any raw user input (partial key=value pairs, natural language)
    3. Constructs the complete JSON payload, asking follow-up questions if needed

    For complex nested types, the LLM can:
    - Explain what each field means in context
    - Suggest reasonable defaults
    - Validate constraints before sending
    - Build nested objects conversationally ("What file do you want to get?")
    """

    async def build_payload(
        self,
        method_schema: MethodSchema,
        user_input: dict[str, Any],
        ask: AskFn,
    ) -> dict[str, Any] | None:
        """Build a complete RPC payload.

        Args:
            method_schema: Full type information for the method.
            user_input: Any args the user already provided (may be partial/empty).
            ask: Callable to prompt the user for more information.
                 Signature: ask(prompt: str) -> str | None (None = cancelled)

        Returns:
            Complete payload dict, or None if cancelled.
        """
        ...


@runtime_checkable
class OutputRenderer(Protocol):
    """Renders an RPC response for display.

    The default implementation dumps JSON. An LLM plugin replaces this
    with intelligent rendering:

    1. Receives the response data and its type schema
    2. Decides the best presentation:
       - Simple scalar → inline display
       - List of records → table
       - Nested object → tree or summarized view
       - Large payload → paginated or summarized
       - Error/status → highlighted with explanation
    3. Can explain what the response means in context

    The renderer also receives the display object so it can use
    rich tables, trees, panels, etc.
    """

    async def render_response(
        self,
        method_schema: MethodSchema,
        result: Any,
        display: Any,
    ) -> bool:
        """Render an RPC response.

        Args:
            method_schema: Full type information for the method.
            result: The response data (already deserialized).
            display: The Display instance for output.

        Returns:
            True if the response was rendered (suppresses default JSON dump).
            False to fall through to default rendering.
        """
        ...


@runtime_checkable
class SessionHook(Protocol):
    """Lifecycle hook for session-scoped services.

    Called when entering/exiting a session subshell.
    """

    async def on_session_start(self, service_name: str, ctx: Any) -> None: ...
    async def on_session_end(self, service_name: str, ctx: Any) -> None: ...


# ── Data types ────────────────────────────────────────────────────────────────


@dataclass
class FieldSchema:
    """Schema for a single field in a request/response type."""

    name: str
    type_name: str  # "str", "int", "list[str]", "MyNestedType", etc.
    required: bool = True
    default: Any = None
    description: str = ""
    nested_fields: list[FieldSchema] | None = None  # for complex types


@dataclass
class MethodSchema:
    """Full schema for a method -- everything a hook needs to build input or render output."""

    service_name: str
    method_name: str
    pattern: str  # "unary", "server_stream", "client_stream", "bidi_stream"
    request_type: str = ""
    response_type: str = ""
    request_fields: list[FieldSchema] | None = None
    response_fields: list[FieldSchema] | None = None
    timeout: float | None = None
    description: str = ""
    # "explicit" (Mode 1, single request class) or "inline" (Mode 2, method
    # takes inline params that the framework packs into a synthesized
    # request class). For "inline" methods the shell renders
    # ``method(name: type, ...)`` instead of ``method(RequestType)``.
    request_style: str = "explicit"


# Type alias for the ask function
AskFn = Any  # Callable[[str], Awaitable[str | None]] -- avoid complex type for 3.9


# ── Hook registry ─────────────────────────────────────────────────────────────


class HookRegistry:
    """Central registry for shell extension hooks."""

    def __init__(self) -> None:
        self._input_builders: list[InputBuilder] = []
        self._output_renderers: list[OutputRenderer] = []
        self._session_hooks: list[SessionHook] = []

    def register_input_builder(self, builder: InputBuilder) -> None:
        """Register an input builder (e.g., LLM-powered)."""
        self._input_builders.append(builder)

    def register_output_renderer(self, renderer: OutputRenderer) -> None:
        """Register an output renderer (e.g., LLM-powered)."""
        self._output_renderers.append(renderer)

    def register_session_hook(self, hook: SessionHook) -> None:
        """Register a session lifecycle hook."""
        self._session_hooks.append(hook)

    @property
    def input_builder(self) -> InputBuilder | None:
        """The active input builder (last registered wins)."""
        return self._input_builders[-1] if self._input_builders else None

    @property
    def output_renderer(self) -> OutputRenderer | None:
        """The active output renderer (last registered wins)."""
        return self._output_renderers[-1] if self._output_renderers else None

    @property
    def session_hooks(self) -> list[SessionHook]:
        """All registered session hooks."""
        return list(self._session_hooks)


# ── Default implementations ──────────────────────────────────────��────────────


class DefaultInputBuilder:
    """Field-by-field interactive prompting (the built-in fallback)."""

    async def build_payload(
        self,
        method_schema: MethodSchema,
        user_input: dict[str, Any],
        ask: AskFn,
    ) -> dict[str, Any] | None:
        if not method_schema.request_fields:
            return user_input or {}

        result = dict(user_input)  # start with what user already provided

        for f in method_schema.request_fields:
            if f.name in result:
                continue  # already provided

            prompt = f"  ▸ {f.name}"
            if f.type_name:
                prompt += f" ({f.type_name})"
            if f.default is not None:
                prompt += f" [{f.default}]"
            prompt += ": "

            value = await ask(prompt)
            if value is None:
                return None  # cancelled

            value = value.strip()
            if not value and f.default is not None:
                result[f.name] = f.default
            elif not value and not f.required:
                continue
            elif not value:
                return None  # required field missing
            else:
                # Try JSON parse
                import json
                try:
                    result[f.name] = json.loads(value)
                except (json.JSONDecodeError, ValueError):
                    result[f.name] = value

        return result


class DefaultOutputRenderer:
    """JSON dump with timing (the built-in fallback)."""

    async def render_response(
        self,
        method_schema: MethodSchema,
        result: Any,
        display: Any,
    ) -> bool:
        # Return False to use default rendering in invoker
        return False


# ── Singleton ─────────────────────────────────────────────────────────────────

_registry = HookRegistry()


def get_hook_registry() -> HookRegistry:
    """Get the global hook registry."""
    return _registry
