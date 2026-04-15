"""
aster.inline_params -- Mode 2 inline request parameters for @rpc handlers.

Spec reference: ffi_spec/handler-context-design.md §"Inline Request Parameters"

Detects Mode 1 (explicit @wire_type request) vs Mode 2 (inline params) at
@service scanning time and synthesizes a {Method}Request wire type from the
handler's parameter list for Mode 2. The synthesized type is indistinguishable
from an equivalent hand-written @wire_type dataclass at the contract-identity
layer: producer using Mode 2 and a consumer reading the manifest must agree on
the contract_id.

The dispatch layer continues to call handler(request, [ctx]); a tiny adapter
installed on the class unpacks the synthesized request's fields into the
user's handler signature before invoking it.
"""

from __future__ import annotations

import asyncio
import dataclasses
import inspect
from dataclasses import dataclass
from typing import Any, Callable

from aster.codec import wire_type
from aster.interceptors.base import CallContext


# ── Mode classification ────────────────────────────────────────────────────────


class RequestStyle:
    """String enum for the request_style field on MethodInfo."""

    EXPLICIT = "explicit"  # Mode 1: single @wire_type param
    INLINE = "inline"      # Mode 2: synthesized from param list
    NO_REQUEST = "inline"  # Mode 2 degenerate: no input fields, still "inline" in the manifest


@dataclass
class InlineParam:
    """A single inline parameter on a Mode 2 handler.

    The ``description`` / ``tags`` fields capture any ``Description(...)``
    marker found in an ``Annotated[T, Description(...)]`` annotation so the
    metadata can be attached to the synthesized request type's fields.
    """
    name: str
    annotation: Any
    default: Any  # inspect.Parameter.empty means "no default provided"
    description: str = ""
    tags: list[str] = dataclasses.field(default_factory=list)


# ── Detection and synthesis ────────────────────────────────────────────────────


_INLINE_PRIMITIVES = frozenset({str, int, float, bool, bytes, bytearray})


def is_wire_type_class(tp: Any) -> bool:
    """Return True if *tp* is a dataclass (explicit or auto-tagged later).

    The classifier treats any dataclass as an explicit-Mode-1 candidate.
    The ``@wire_type`` tag may or may not have been applied yet by the
    time classification runs -- auto-tagging happens in the xlang validator
    later in ``@service`` processing.
    """
    if not isinstance(tp, type):
        return False
    return dataclasses.is_dataclass(tp)


def _looks_inline(tp: Any) -> bool:
    """Return True if *tp* is a type we can synthesize an inline field for.

    Mode 2 only kicks in when the parameter's type is something we know how
    to give a default value to: primitives and the standard containers. All
    other types (dataclasses, forward-ref strings, unresolved generics,
    unknown types) fall back to Mode 1 pass-through so existing code that
    predates inline params continues to work.
    """
    if _is_optional(tp):
        inner = _unwrap_optional(tp)
        return _looks_inline(inner)
    if isinstance(tp, type) and tp in _INLINE_PRIMITIVES:
        return True
    origin = getattr(tp, "__origin__", None)
    if origin in (list, set, frozenset, dict):
        return True
    return False


def _split_annotated(annotation: Any) -> tuple[Any, str, list[str]]:
    """Strip ``Annotated[T, Description(...), ...]`` -> ``(T, description, tags)``.

    Non-Annotated annotations pass through unchanged with empty description.
    """
    from aster.metadata import Description

    if not hasattr(annotation, "__metadata__"):
        return annotation, "", []
    import typing
    args = typing.get_args(annotation)
    if not args:
        return annotation, "", []
    inner = args[0]
    for marker in args[1:]:
        if isinstance(marker, Description):
            return inner, marker.text, list(marker.tags)
    return inner, "", []


def classify_method(
    method: Callable,
    hints: dict[str, Any],
) -> tuple[str, list[InlineParam], str | None]:
    """Classify a handler method as Mode 1 (EXPLICIT) or Mode 2 (INLINE).

    Returns ``(request_style, inline_params, ctx_param_name)``:
    - ``request_style`` is "explicit" or "inline"
    - ``inline_params`` is empty for EXPLICIT mode; for INLINE mode it
      contains one entry per non-self, non-CallContext parameter in
      declaration order.
    - ``ctx_param_name`` is the name of the CallContext parameter if the
      handler declared one (so the adapter can pass it back through by the
      user's chosen name, e.g. ``ctx`` / ``context`` / ``call_ctx``), else
      None.
    """
    # Resolve hints with include_extras=True so Annotated[T, Description(...)]
    # wrappers survive and we can strip them while preserving the marker.
    import typing
    try:
        extras_hints = typing.get_type_hints(method, include_extras=True)
    except Exception:
        extras_hints = hints

    sig = inspect.signature(method)
    params: list[inspect.Parameter] = []
    ctx_param_name: str | None = None
    for name, p in sig.parameters.items():
        if name == "self":
            continue
        ann = hints.get(name, p.annotation)
        # Exclude CallContext params -- they are framework injection.
        if ann is CallContext or (isinstance(ann, str) and ann.split(".")[-1] == "CallContext"):
            ctx_param_name = name
            continue
        params.append(p)

    # Zero params → Mode 2 with empty synthesized request.
    if len(params) == 0:
        return RequestStyle.INLINE, [], ctx_param_name

    def _build_inline_param(p: inspect.Parameter) -> InlineParam:
        """Build an InlineParam, stripping any Annotated[T, Description(...)] wrapper."""
        raw_ann = extras_hints.get(p.name, p.annotation)
        inner, desc, tags = _split_annotated(raw_ann)
        return InlineParam(
            name=p.name,
            annotation=inner,
            default=p.default,
            description=desc,
            tags=tags,
        )

    # One param → Mode 2 only if the (unwrapped) type is a primitive/container
    # we can safely default. Dataclasses, forward refs, and unknown types
    # default to Mode 1 pass-through for backward compatibility.
    if len(params) == 1:
        sole = params[0]
        ip = _build_inline_param(sole)
        if _looks_inline(ip.annotation):
            return RequestStyle.INLINE, [ip], ctx_param_name
        return RequestStyle.EXPLICIT, [], ctx_param_name

    # Multiple params → Mode 2. Each param must be something we can synthesize.
    inline: list[InlineParam] = [_build_inline_param(p) for p in params]
    return RequestStyle.INLINE, inline, ctx_param_name


# ── Default value synthesis ────────────────────────────────────────────────────


_PRIMITIVE_DEFAULTS: dict[type, Any] = {
    str: "",
    int: 0,
    float: 0.0,
    bool: False,
    bytes: b"",
    bytearray: bytearray,  # sentinel, handled below
}


def _is_optional(ann: Any) -> bool:
    """Return True for Optional[X] / X | None / Union[X, None]."""
    import types as _types
    import typing

    origin = getattr(ann, "__origin__", None)
    args = getattr(ann, "__args__", ()) or ()
    if origin is _types.UnionType or origin is typing.Union:
        return type(None) in args
    return False


def _unwrap_optional(ann: Any) -> Any:
    """Return X from Optional[X] (or the original annotation if not optional)."""
    if _is_optional(ann):
        args = [a for a in getattr(ann, "__args__", ()) if a is not type(None)]
        if len(args) == 1:
            return args[0]
    return ann


def _field_default(name: str, annotation: Any) -> Any:
    """Return a dataclasses.field(...) entry suitable for make_dataclass().

    Prefers user-provided defaults; otherwise infers from the annotation.
    Dataclass types must be Optional (so default None is valid); non-Optional
    dataclass params raise TypeError at decoration time.
    """
    if _is_optional(annotation):
        return dataclasses.field(default=None)

    # Primitive defaults
    if annotation in _PRIMITIVE_DEFAULTS:
        dv = _PRIMITIVE_DEFAULTS[annotation]
        if dv is bytearray:
            return dataclasses.field(default_factory=bytearray)
        return dataclasses.field(default=dv)

    # list[X], dict[K,V], set[X]
    origin = getattr(annotation, "__origin__", None)
    if origin in (list, set, frozenset):
        return dataclasses.field(default_factory=list if origin is list else set)
    if origin is dict:
        return dataclasses.field(default_factory=dict)

    # Dataclass @wire_type type -- require Optional
    if dataclasses.is_dataclass(annotation):
        raise TypeError(
            f"Inline parameter {name!r} has dataclass type "
            f"{getattr(annotation, '__qualname__', annotation)!r} but is not "
            f"Optional. Use ``Optional[{getattr(annotation, '__qualname__', 'T')}]`` "
            f"so the synthesized request type can have a safe default, or "
            f"switch to an explicit @wire_type request class."
        )

    # Fallback -- no safe default available
    raise TypeError(
        f"Inline parameter {name!r} has unsupported type "
        f"{annotation!r}; cannot synthesize a default for the inline request. "
        f"Use a primitive type (str/int/float/bool/bytes), a container "
        f"(list/dict/set), or Optional[<@wire_type class>]."
    )


# ── Synthesis ──────────────────────────────────────────────────────────────────


def synthesize_request_type(
    service_cls: type,
    method_name: str,
    inline_params: list[InlineParam],
) -> type:
    """Build a dataclass representing the wire request for a Mode 2 handler.

    The class is:
    - Placed in the service class's module (``__module__``)
    - Named ``{MethodName}Request`` in PascalCase
    - Decorated with @wire_type tag ``{module}/{MethodName}Request``
    - Dataclass with one field per inline parameter, in declaration order
    - Every field has a default so Fory XLANG can serialize it

    The resulting class is byte-compatible with an equivalent hand-written
    @wire_type dataclass that has the same field names, types, and order.
    """
    type_name = _pascal_case(method_name) + "Request"

    # Build field list for make_dataclass
    fields_spec: list[tuple[str, Any, Any]] = []
    for p in inline_params:
        if p.default is not inspect.Parameter.empty:
            # User supplied default in the signature; wrap in a dataclasses.field()
            fld = dataclasses.field(default=p.default)
        else:
            fld = _field_default(p.name, p.annotation)
        # Attach Aster metadata from any Description(...) marker on the
        # original annotation so it flows into the manifest.
        if p.description or p.tags:
            aster_meta = {"description": p.description, "tags": list(p.tags)}
            # dataclasses.field doesn't let us update metadata after the fact,
            # so rebuild the field with the original default/factory + metadata.
            kwargs: dict[str, Any] = {"metadata": {"aster": aster_meta}}
            if fld.default is not dataclasses.MISSING:
                kwargs["default"] = fld.default
            if fld.default_factory is not dataclasses.MISSING:
                kwargs["default_factory"] = fld.default_factory
            fld = dataclasses.field(**kwargs)
        fields_spec.append((p.name, p.annotation, fld))

    module = getattr(service_cls, "__module__", "") or ""

    synthesized = dataclasses.make_dataclass(
        type_name,
        fields_spec,
        bases=(),
        namespace={},
        frozen=False,
        eq=True,
    )

    # Re-home the class to the service module so contract-identity FQN is
    # deterministic and stable across sessions.
    synthesized.__module__ = module
    synthesized.__qualname__ = type_name

    # Apply @wire_type so the codec registers the type and the contract
    # identity system picks up the namespace/typename.
    tag = f"{module}/{type_name}" if module else type_name
    wire_type(tag)(synthesized)

    # Mark as synthesized so downstream tooling can tell at a glance.
    synthesized.__aster_inline_synthesized__ = True
    synthesized.__aster_inline_param_names__ = tuple(p.name for p in inline_params)

    return synthesized


def _pascal_case(s: str) -> str:
    """Convert snake_case or camelCase to PascalCase."""
    if not s:
        return s
    if "_" in s:
        return "".join(part[:1].upper() + part[1:] for part in s.split("_") if part)
    return s[:1].upper() + s[1:]


# ── Handler adapter ────────────────────────────────────────────────────────────


def make_inline_adapter(
    original_method: Callable,
    inline_params: list[InlineParam],
    accepts_ctx: bool,
    ctx_param_name: str | None = None,
) -> Callable:
    """Wrap a Mode 2 handler so dispatch can call it with (self, request[, ctx]).

    The wrapper:
    - Accepts (self, request, [ctx]) positionally -- same shape as a Mode 1
      handler, so ``server.py``/``session.py``/``local.py`` dispatch don't
      need to know about inline mode.
    - Extracts the inline fields from ``request`` in declaration order and
      invokes ``original_method(self, *field_values, [ctx=ctx])``.
    - Preserves coroutine / async-generator semantics of the original.

    The number of positional parameters the wrapper declares is chosen to
    match what ``handler_accepts_ctx`` and dispatch expect:
    - accepts_ctx=False → ``async def wrapper(self, request)``
    - accepts_ctx=True  → ``async def wrapper(self, request, ctx)``

    ``Function.length`` / ``inspect.signature`` on the wrapper must match
    the original's CallContext-awareness so detection logic elsewhere still
    works consistently.
    """
    param_names = [p.name for p in inline_params]
    is_async_gen = inspect.isasyncgenfunction(original_method)
    ctx_kw = ctx_param_name or "ctx"

    if is_async_gen:
        if accepts_ctx:
            async def wrapper_gen_ctx(self, request, ctx):
                kwargs = {name: getattr(request, name) for name in param_names}
                kwargs[ctx_kw] = ctx
                async for item in original_method(self, **kwargs):
                    yield item
            _attach_meta(wrapper_gen_ctx, original_method)
            return wrapper_gen_ctx
        else:
            async def wrapper_gen(self, request):
                kwargs = {name: getattr(request, name) for name in param_names}
                async for item in original_method(self, **kwargs):
                    yield item
            _attach_meta(wrapper_gen, original_method)
            return wrapper_gen

    # Regular async coroutine (unary / client-stream)
    if accepts_ctx:
        async def wrapper_ctx(self, request, ctx):
            kwargs = {name: getattr(request, name) for name in param_names}
            kwargs[ctx_kw] = ctx
            return await original_method(self, **kwargs)
        _attach_meta(wrapper_ctx, original_method)
        return wrapper_ctx
    else:
        async def wrapper(self, request):
            kwargs = {name: getattr(request, name) for name in param_names}
            return await original_method(self, **kwargs)
        _attach_meta(wrapper, original_method)
        return wrapper


def _attach_meta(wrapper: Callable, original: Callable) -> None:
    """Copy the original handler's important attributes onto the wrapper.

    Keeps ``__aster_method_info__`` so the @service scan can still find the
    MethodInfo, and copies ``__name__``, ``__qualname__``, ``__doc__``,
    ``__module__`` for introspection/tracebacks. The wrapper also remembers
    the original under ``__aster_inline_original__`` so codegen/debug tools
    can recover the user-written signature.
    """
    import functools
    functools.update_wrapper(wrapper, original)
    setattr(wrapper, "__aster_inline_original__", original)
