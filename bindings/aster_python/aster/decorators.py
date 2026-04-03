"""
aster.decorators — Service and method decorators for defining Aster RPC services.

Spec reference: §7.1–7.4 (Python decorators), §7.6 (language ownership)

This module provides the decorator-based service definition layer that allows
developers to define RPC services with type-safe method signatures.

Example usage::

    from aster_python import service, rpc, server_stream, SerializationMode
    from dataclasses import dataclass

    @service(name="AgentControl", version=1, serialization=[SerializationMode.XLANG])
    class AgentControlService:

        @rpc(timeout=30.0, idempotent=True)
        async def assign_task(self, req: TaskAssignment) -> TaskAck:
            ...

        @server_stream
        async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]:
            ...

        @client_stream
        async def upload_artifacts(self, stream: AsyncIterator[ArtifactChunk]) -> UploadResult:
            ...

        @bidi_stream
        async def approval_loop(
            self, requests: AsyncIterator[ApprovalRequest]
        ) -> AsyncIterator[ApprovalResponse]:
            ...
"""

from __future__ import annotations

import asyncio
import functools
import inspect
import typing
from typing import (
    Any,
    AsyncGenerator,
    AsyncIterator,
    Callable,
    ForwardRef,
    ParamSpec,
    TypeVar,
    overload,
)

from aster_python.aster.types import SerializationMode
from aster_python.aster.service import MethodInfo, ServiceInfo

if typing.TYPE_CHECKING:
    pass

# ── Type variables for preserving generic signatures ────────────────────────────

P = ParamSpec("P")
R = TypeVar("R")
T = TypeVar("T")


# ── RPC Pattern enum ───────────────────────────────────────────────────────────

class RpcPattern:
    """Enumeration of RPC patterns supported by Aster.

    This class mirrors the patterns from the spec:
    - UNARY: single request → single response
    - SERVER_STREAM: single request → multiple responses
    - CLIENT_STREAM: multiple requests → single response
    - BIDI_STREAM: multiple requests ↔ multiple responses
    """

    UNARY = "unary"
    SERVER_STREAM = "server_stream"
    CLIENT_STREAM = "client_stream"
    BIDI_STREAM = "bidi_stream"


# ── Decorator metadata storage ─────────────────────────────────────────────────

# These are set on decorated classes and methods by the decorators.
_SERVICE_INFO_ATTR = "__aster_service_info__"
_METHOD_INFO_ATTR = "__aster_method_info__"
_RPC_DECORATED_ATTR = "__aster_rpc_decorated__"


# ── Helper functions ──────────────────────────────────────────────────────────


def _is_async_function(func: Callable) -> bool:
    """Check if a function is an async function (or async generator)."""
    return asyncio.iscoroutinefunction(func) or inspect.isasyncgenfunction(func)


def _is_method(func: Callable) -> bool:
    """Check if an object is a function or async generator function."""
    return inspect.isfunction(func) or inspect.isasyncgenfunction(func)


# ── Base decorator class ─────────────────────────────────────────────────────
# We use a class instead of a function to avoid the descriptor protocol issues
# that occur when using functions as method decorators.


class _Decorator:
    """Base class for Aster decorators to avoid descriptor protocol issues."""

    def __init__(
        self,
        pattern: str,
        timeout: float | None = None,
        idempotent: bool = False,
        serialization: SerializationMode | None = None,
    ):
        self._pattern = pattern
        self._timeout = timeout
        self._idempotent = idempotent
        self._serialization = serialization

    def __call__(self, method: Callable[P, Any]) -> Callable[P, Any]:
        """Apply the decorator to a method."""
        # Validate the method
        self._validate(method)

        # Create MethodInfo
        method_info = MethodInfo(
            name=method.__name__,
            pattern=self._pattern,
            request_type=None,  # Filled in by @service
            response_type=None,  # Filled in by @service
            timeout=self._timeout,
            idempotent=self._idempotent,
            serialization=self._serialization,
        )

        setattr(method, _METHOD_INFO_ATTR, method_info)
        return method

    def _validate(self, method: Callable) -> None:
        """Override in subclasses to add specific validation."""
        pass


# ── Base decorator classes ─────────────────────────────────────────────────────
# We use callable classes instead of functions to avoid the descriptor protocol
# issues that occur when using functions as method decorators without parens.


class _RpcDecorator:
    """Callable class for @rpc decorator.
    
    Supports both @rpc (no parens) and @rpc() (with parens) syntax.
    """

    def __init__(
        self,
        timeout: float | None = None,
        idempotent: bool = False,
        serialization: SerializationMode | None = None,
    ):
        self._timeout = timeout
        self._idempotent = idempotent
        self._serialization = serialization

    def __call__(
        self,
        method: Callable[P, Any] | None = None,
        timeout: float | None = None,
        idempotent: bool | None = None,
        serialization: SerializationMode | None = None,
        **kwargs: Any,
    ) -> Callable[P, Any]:
        # Handle @rpc(...) - return a new configured decorator instance.
        if method is None:
            return _RpcDecorator(
                timeout=timeout if timeout is not None else self._timeout,
                idempotent=idempotent if idempotent is not None else self._idempotent,
                serialization=serialization if serialization is not None else self._serialization,
            )
        
        # Get the method - merge options
        final_timeout = timeout if timeout is not None else self._timeout
        final_idempotent = idempotent if idempotent is not None else self._idempotent
        final_serial = serialization if serialization is not None else self._serialization
        
        if not asyncio.iscoroutinefunction(method):
            raise TypeError(
                f"@rpc method {method.__name__} must be an async function"
            )

        method_info = MethodInfo(
            name=method.__name__,
            pattern=RpcPattern.UNARY,
            request_type=None,
            response_type=None,
            timeout=final_timeout,
            idempotent=final_idempotent,
            serialization=final_serial,
        )
        setattr(method, _METHOD_INFO_ATTR, method_info)
        return method


# Export the decorator (usable as @rpc or @rpc())
rpc = _RpcDecorator()


# ── @server_stream decorator ───────────────────────────────────────────────────


def server_stream(
    method_or_timeout: Callable[P, AsyncIterator[Any]] | float | None = None,
    timeout: float | None = None,
    serialization: SerializationMode | None = None,
) -> Callable[P, AsyncIterator[Any]]:
    """Decorator to mark a method as a server-streaming RPC.

    Can be used as:
        @server_stream  # without parens
        @server_stream()  # with parens
        @server_stream(timeout=30.0)  # with options

    Args:
        method_or_timeout: Either the method (when used without parens) or a timeout value.
        timeout: Optional timeout in seconds.
        serialization: Override the serialization mode for this method.

    Returns:
        A decorator function or the decorated method.

    Example::

        @service(name="MyService", version=1)
        class MyService:
            @server_stream
            async def watch_items(self, req: WatchRequest) -> AsyncIterator[ItemUpdate]:
                for item in items:
                    yield ItemUpdate(item=item)
    """
    # Handle @server_stream (no parens) - method is passed directly
    if callable(method_or_timeout):
        method = method_or_timeout
        _apply_server_stream_decorator(method, timeout=timeout, serialization=serialization)
        return method

    # Handle @server_stream() or @server_stream(timeout=...)
    actual_timeout = method_or_timeout if method_or_timeout is not None else timeout

    def decorator(method: Callable[P, AsyncIterator[Any]]) -> Callable[P, AsyncIterator[Any]]:
        _apply_server_stream_decorator(method, timeout=actual_timeout, serialization=serialization)
        return method

    return decorator


def _apply_server_stream_decorator(
    method: Callable,
    timeout: float | None = None,
    serialization: SerializationMode | None = None,
) -> None:
    """Apply the server_stream decorator to a method."""
    if not inspect.isasyncgenfunction(method):
        raise TypeError(
            f"@server_stream method {method.__name__} must be an async generator "
            f"(use 'async def' with 'yield')"
        )

    method_info = MethodInfo(
        name=method.__name__,
        pattern=RpcPattern.SERVER_STREAM,
        request_type=None,
        response_type=None,
        timeout=timeout,
        idempotent=False,
        serialization=serialization,
    )
    setattr(method, _METHOD_INFO_ATTR, method_info)


# ── @client_stream decorator ───────────────────────────────────────────────────


def client_stream(
    method_or_timeout: Callable[P, Any] | float | None = None,
    idempotent: bool = False,
    serialization: SerializationMode | None = None,
) -> Callable[P, Any] | Callable:
    """Decorator to mark a method as a client-streaming RPC.

    Can be used as:
        @client_stream  # without parens
        @client_stream()  # with parens
        @client_stream(timeout=30.0)  # with options

    Args:
        method_or_timeout: Either the method (when used without parens) or a timeout value.
        idempotent: Whether the method is safe to retry.
        serialization: Override the serialization mode for this method.

    Returns:
        A decorator function or the decorated method.

    Example::

        @service(name="MyService", version=1)
        class MyService:
            @client_stream
            async def aggregate(self, reqs: AsyncIterator[NumberRequest]) -> SumResponse:
                total = 0
                async for req in reqs:
                    total += req.value
                return SumResponse(total=total)
    """
    # Handle @client_stream (no parens) - method is passed directly
    if callable(method_or_timeout):
        method = method_or_timeout
        _apply_client_stream_decorator(method, idempotent=idempotent, serialization=serialization)
        return method

    # Handle @client_stream() or @client_stream(timeout=...)
    timeout = method_or_timeout

    def decorator(method: Callable[P, Any]) -> Callable[P, Any]:
        _apply_client_stream_decorator(method, timeout=timeout, idempotent=idempotent, serialization=serialization)
        return method

    return decorator


def _apply_client_stream_decorator(
    method: Callable,
    timeout: float | None = None,
    idempotent: bool = False,
    serialization: SerializationMode | None = None,
) -> None:
    """Apply the client_stream decorator to a method."""
    if not asyncio.iscoroutinefunction(method):
        raise TypeError(
            f"@client_stream method {method.__name__} must be an async function"
        )

    method_info = MethodInfo(
        name=method.__name__,
        pattern=RpcPattern.CLIENT_STREAM,
        request_type=None,
        response_type=None,
        timeout=timeout,
        idempotent=idempotent,
        serialization=serialization,
    )
    setattr(method, _METHOD_INFO_ATTR, method_info)


# ── @bidi_stream decorator ────────────────────────────────────────────────────


def bidi_stream(
    method_or_timeout: Callable[P, AsyncIterator[Any]] | float | None = None,
    timeout: float | None = None,
    serialization: SerializationMode | None = None,
) -> Callable[P, AsyncIterator[Any]]:
    """Decorator to mark a method as a bidirectional-streaming RPC.

    Can be used as:
        @bidi_stream  # without parens
        @bidi_stream()  # with parens
        @bidi_stream(timeout=30.0)  # with options

    Args:
        method_or_timeout: Either the method (when used without parens) or a timeout value.
        timeout: Optional timeout in seconds.
        serialization: Override the serialization mode for this method.

    Returns:
        A decorator function or the decorated method.

    Example::

        @service(name="MyService", version=1)
        class MyService:
            @bidi_stream
            async def chat(
                self, requests: AsyncIterator[ChatMessage]
            ) -> AsyncIterator[ChatMessage]:
                async for req in requests:
                    yield ChatMessage(text=f"echo: {req.text}")
    """
    # Handle @bidi_stream (no parens) - method is passed directly
    if callable(method_or_timeout):
        method = method_or_timeout
        _apply_bidi_stream_decorator(method, timeout=timeout, serialization=serialization)
        return method

    # Handle @bidi_stream() or @bidi_stream(timeout=...)
    actual_timeout = method_or_timeout if method_or_timeout is not None else timeout

    def decorator(method: Callable[P, AsyncIterator[Any]]) -> Callable[P, AsyncIterator[Any]]:
        _apply_bidi_stream_decorator(method, timeout=actual_timeout, serialization=serialization)
        return method

    return decorator


def _apply_bidi_stream_decorator(
    method: Callable,
    timeout: float | None = None,
    serialization: SerializationMode | None = None,
) -> None:
    """Apply the bidi_stream decorator to a method."""
    if not inspect.isasyncgenfunction(method):
        raise TypeError(
            f"@bidi_stream method {method.__name__} must be an async generator "
            f"(use 'async def' with 'yield')"
        )

    method_info = MethodInfo(
        name=method.__name__,
        pattern=RpcPattern.BIDI_STREAM,
        request_type=None,
        response_type=None,
        timeout=timeout,
        idempotent=False,
        serialization=serialization,
    )
    setattr(method, _METHOD_INFO_ATTR, method_info)


# ── @service decorator ─────────────────────────────────────────────────────────


def service(
    name: str,
    *,
    version: int = 1,
    serialization: list[SerializationMode] | SerializationMode | None = None,
    scoped: str = "shared",
    interceptors: list[type] | None = None,
    max_concurrent_streams: int | None = None,
) -> Callable[[type], type]:
    """Class decorator to mark a class as an Aster RPC service.

    Args:
        name: The service name (e.g. "AgentControl").
        version: The service version (default: 1).
        serialization: Supported serialization modes. Defaults to [XLANG].
        scoped: Service scope: "shared" or "stream". Default "shared".
        interceptors: List of interceptor classes to apply to all methods.
        max_concurrent_streams: Maximum concurrent streams for this service.

    Returns:
        A decorator function that marks the class as a service and attaches
        a ``__aster_service_info__`` attribute with the service metadata.

    Example::

        @service(name="EchoService", version=1)
        class EchoService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)
    """
    if serialization is None:
        serialization = [SerializationMode.XLANG]
    elif isinstance(serialization, SerializationMode):
        serialization = [serialization]

    def decorator(cls: type) -> type:
        # Check that this is a class
        if not isinstance(cls, type):
            raise TypeError("@service can only be applied to classes")

        # Scan all methods to collect type information
        methods = _scan_service_methods(cls, serialization)

        # Store service info on the class
        service_info = ServiceInfo(
            name=name,
            version=version,
            scoped=scoped,
            methods=methods,
            serialization_modes=list(serialization),
            interceptors=list(interceptors) if interceptors else [],
            max_concurrent_streams=max_concurrent_streams,
        )
        setattr(cls, _SERVICE_INFO_ATTR, service_info)

        # Validate all types in the service have @fory_tag for XLANG mode
        if SerializationMode.XLANG in serialization:
            # Capture caller's local variables AND globals to resolve types defined in any scope
            # (e.g., inside test methods, at module level, or imported)
            try:
                caller_frame = inspect.currentframe()
                if caller_frame is not None:
                    # f_back gives us the frame of the code that called the decorator
                    # (e.g., the test method body)
                    caller_locals = caller_frame.f_back.f_locals
                    caller_globals = caller_frame.f_back.f_globals
                    _validate_xlang_tags_for_service(cls, service_info, _caller_locals=caller_locals, _caller_globals=caller_globals)
                else:
                    _validate_xlang_tags_for_service(cls, service_info)
            finally:
                # Clean up frame reference to avoid reference cycles
                del caller_frame

        return cls

    return decorator


def _scan_service_methods(
    cls: type, serialization_modes: list[SerializationMode]
) -> dict[str, MethodInfo]:
    """Scan a service class and extract method information from decorated methods.

    Args:
        cls: The service class.
        serialization_modes: The service's serialization modes.

    Returns:
        A dict mapping method names to MethodInfo objects.
    """
    methods: dict[str, MethodInfo] = {}

    # Use our custom predicate that includes both regular functions and async generators
    for name, method in inspect.getmembers(cls, predicate=_is_method):
        # Skip private methods and inherited object methods
        if name.startswith("_") and name != "__init__":
            continue

        # Check if the method was decorated with @rpc, @server_stream, etc.
        method_info: MethodInfo | None = getattr(method, _METHOD_INFO_ATTR, None)

        if method_info is not None:
            # Extract types from signature
            request_type, response_type = _extract_types_from_signature(
                method, name, serialization_modes
            )

            # Update the MethodInfo with extracted types
            method_info.name = name
            method_info.request_type = request_type
            method_info.response_type = response_type

            methods[name] = method_info

    return methods


def _extract_types_from_signature(
    method: Callable,
    method_name: str,
    serialization_modes: list[SerializationMode],
) -> tuple[type, type]:
    """Extract request and response types from a method signature.

    For async generators (streaming), the response type is the yielded type.

    Args:
        method: The method to inspect.
        method_name: The method name for error messages.
        serialization_modes: Serialization modes to validate against.

    Returns:
        A (request_type, response_type) tuple.

    Raises:
        TypeError: If type annotations are missing or invalid.
    """
    sig = inspect.signature(method)
    hints = _get_type_hints_safe(method)

    params = list(sig.parameters.values())

    # Skip 'self' parameter
    params = [p for p in params if p.name != "self"]

    # Determine if this is a streaming method based on the MethodInfo
    method_info: MethodInfo | None = getattr(method, _METHOD_INFO_ATTR, None)
    pattern = getattr(method_info, "pattern", RpcPattern.UNARY) if method_info else RpcPattern.UNARY

    # Extract request type
    if pattern in (RpcPattern.UNARY, RpcPattern.SERVER_STREAM):
        # Unary/server-stream: single request
        if not params:
            raise TypeError(
                f"Method {method_name} is marked as {pattern} but has no request parameter"
            )
        request_param = params[0]
        request_type = hints.get(request_param.name, request_param.annotation)
        if request_type is inspect.Parameter.empty:
            raise TypeError(
                f"Method {method_name} has no type annotation for request parameter '{request_param.name}'"
            )
    elif pattern in (RpcPattern.CLIENT_STREAM, RpcPattern.BIDI_STREAM):
        # Client-stream/bidi-stream: request is an async iterator
        if not params:
            raise TypeError(
                f"Method {method_name} is marked as {pattern} but has no request parameter"
            )
        request_param = params[0]
        request_type = hints.get(request_param.name, request_param.annotation)
        if request_type is inspect.Parameter.empty:
            raise TypeError(
                f"Method {method_name} has no type annotation for request parameter '{request_param.name}'"
            )
        # Unwrap AsyncIterator to get the inner type
        request_type = _unwrap_async_iterator(request_type)

    # Extract response type
    if pattern in (RpcPattern.UNARY, RpcPattern.CLIENT_STREAM):
        # Unary/client-stream: single response (return type)
        response_annotation = sig.return_annotation
        if response_annotation is inspect.Signature.empty:
            raise TypeError(
                f"Method {method_name} has no return type annotation"
            )
        response_type = _unwrap_async_iterator(response_annotation)
    else:
        # Server-stream/bidi-stream: response is async iterator
        response_annotation = sig.return_annotation
        if response_annotation is inspect.Signature.empty:
            raise TypeError(
                f"Method {method_name} has no return type annotation"
            )
        response_type = _unwrap_async_iterator(response_annotation)

    return request_type, response_type


def _unwrap_async_iterator(tp: Any) -> Any:
    """Unwrap AsyncIterator[T] or AsyncGenerator[T, ...] to get T.

    Args:
        tp: The type to unwrap.

    Returns:
        The inner type T, or the original type if not an async iterator.
    """
    # Handle AsyncIterator[X] and AsyncGenerator[X, ...]
    origin = getattr(tp, "__origin__", None)

    if origin is None:
        # Check if it's a string (forward reference)
        if isinstance(tp, str):
            return tp
        return tp

    # AsyncIterator[T]
    if origin is AsyncIterator or origin is AsyncGenerator:
        args = getattr(tp, "__args__", ())
        if args:
            return args[0]
        return tp

    return tp


def _get_type_hints_safe(func: Callable) -> dict[str, Any]:
    """Get type hints from a function, handling ForwardRef safely.

    Args:
        func: The function to get hints from.

    Returns:
        A dict mapping parameter names to types.
    """
    try:
        # Try to get hints from the function
        hints = typing.get_type_hints(func)
        return hints
    except Exception:
        # Fallback: get hints directly from annotations
        sig = inspect.signature(func)
        hints = {}
        for name, param in sig.parameters.items():
            if param.annotation is not inspect.Parameter.empty:
                hints[name] = param.annotation
        if sig.return_annotation is not inspect.Signature.empty:
            hints["return"] = sig.return_annotation
        return hints


def _validate_xlang_tags_for_service(cls: type, service_info: Any, _caller_locals: dict | None = None, _caller_globals: dict | None = None) -> None:
    """Validate that all types used in the service have @fory_tag for XLANG mode.

    Args:
        cls: The service class.
        service_info: The ServiceInfo object.
        _caller_locals: Optional dict of local variables from the caller. Used to
            resolve types defined in local scope (e.g., inside test methods).
        _caller_globals: Optional dict of global variables from the caller. Used to
            resolve types defined at module level (e.g., imported types).

    Raises:
        TypeError: If a type lacks @fory_tag.
    """
    import dataclasses

    def check_type(tp: Any, path: str) -> None:
        if tp is None or tp is inspect.Parameter.empty:
            return

        # Skip primitives
        if tp in (int, float, str, bool, bytes, bytearray, type(None)):
            return

        # Handle string types (forward references) - try to resolve them
        if isinstance(tp, str):
            # Try to resolve the forward reference using multiple strategies:
            # 1. Service class namespace
            # 2. Globals from methods in the class
            # 3. Caller's local variables (for types defined in test methods)
            # 4. Caller's globals (for types defined at module level or imported)
            try:
                namespace = dict(vars(cls))
                # Add globals from the methods
                for method_name, method in inspect.getmembers(cls, predicate=_is_method):
                    if hasattr(method, '__globals__'):
                        namespace.update(method.__globals__)
                    # Check if the string name is defined as an attribute on the method
                    if hasattr(method, tp):
                        resolved = getattr(method, tp)
                        check_type(resolved, path)
                        return
                # Try to resolve using caller's local variables first (higher priority)
                if _caller_locals and tp in _caller_locals:
                    resolved = _caller_locals[tp]
                    check_type(resolved, path)
                    return
                # Then try caller's globals (for module-level and imported types)
                if _caller_globals and tp in _caller_globals:
                    resolved = _caller_globals[tp]
                    check_type(resolved, path)
                    return
                # Try to resolve using eval
                resolved = eval(tp, namespace, None)
                check_type(resolved, path)
            except (NameError, SyntaxError):
                # Can't resolve, skip validation for this type
                # Don't catch TypeError - that comes from recursive check_type calls
                # and should propagate up to signal validation failure
                pass
            return

        # Handle typing constructs (Generic, Union, AsyncIterator, etc.)
        origin = getattr(tp, "__origin__", None)
        if origin is not None:
            # Unwrap and check args
            args = getattr(tp, "__args__", ()) or ()
            for arg in args:
                check_type(arg, path)
            return

        # Handle type objects
        if not isinstance(tp, type):
            return

        # Check if it's a dataclass
        if dataclasses.is_dataclass(tp):
            # Check if it has @fory_tag
            if not hasattr(tp, "__fory_tag__"):
                raise TypeError(
                    f"Type {tp.__qualname__} used in service {service_info.name} "
                    f"has no @fory_tag decorator. All types used in XLANG mode "
                    f"must be decorated with @fory_tag."
                )
            # Recursively check fields
            for fld in dataclasses.fields(tp):
                hints = _get_type_hints_safe(tp)
                field_type = hints.get(fld.name, fld.type)
                check_type(field_type, f"{path}.{tp.__name__}.{fld.name}")

    # Check all method types by iterating over decorated methods directly
    for name, method in inspect.getmembers(cls, predicate=_is_method):
        method_info: MethodInfo | None = getattr(method, _METHOD_INFO_ATTR, None)
        if method_info is None:
            continue

        # Get resolved type hints for both request and response types
        # This ensures forward references are resolved correctly
        hints = _get_type_hints_safe(method)

        # Check request type from parameter annotations
        params = list(inspect.signature(method).parameters.values())
        params = [p for p in params if p.name != "self"]
        if params:
            request_param = params[0]
            request_type = hints.get(request_param.name, request_param.annotation)
            if request_type is not inspect.Parameter.empty:
                pattern = getattr(method_info, "pattern", RpcPattern.UNARY)
                if pattern in (RpcPattern.CLIENT_STREAM, RpcPattern.BIDI_STREAM):
                    request_type = _unwrap_async_iterator(request_type)
                check_type(request_type, f"{name}(request)")

        # Check response type from return annotation (use resolved hints, not raw annotation)
        if "return" in hints:
            response_type = hints["return"]
            pattern = getattr(method_info, "pattern", RpcPattern.UNARY)
            if pattern in (RpcPattern.SERVER_STREAM, RpcPattern.BIDI_STREAM):
                response_type = _unwrap_async_iterator(response_type)
            check_type(response_type, f"{name}(response)")
