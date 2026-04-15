"""
aster.metadata -- Extensible metadata for services, methods, and fields.

Provides semantic documentation that AI agents (via MCP) and humans
can use to understand what services do and how to populate request fields.

Metadata is NON-CANONICAL -- it does NOT affect contract identity (BLAKE3 hash)
and does NOT appear in the wire protocol.

See ``docs/_internal/rich_metadata/`` for design and roll-out plan.
"""

from __future__ import annotations

import dataclasses
from dataclasses import dataclass, field
from typing import Any

from aster.codec import wire_type


@wire_type("_aster/Metadata")
@dataclass
class Metadata:
    """Extensible metadata for describing services, methods, and fields.

    Attached to ``ServiceInfo`` / ``MethodInfo`` / dataclass field metadata.
    Flows through ``ContractManifest`` JSON; never appears in canonical
    contract bytes.

    Fields:
        description: Free-text description (typically first docstring paragraph).
        tags: Open-vocabulary semantic tags. See
            ``docs/_internal/rich_metadata/README.md`` for the conventional
            vocabulary. Framework enforces service/method tags; field tags
            are advisory only.
        deprecated: Whether this service/method/field is deprecated.
        since_version: Optional service-version integer in which this was
            introduced. Useful for generated clients to surface availability.
    """

    description: str = ""
    tags: list[str] = field(default_factory=list)
    deprecated: bool = False
    since_version: int | None = None


# ── Field authoring helpers ───────────────────────────────────────────────────


_MISSING = dataclasses.MISSING


def describe(
    text: str,
    *,
    tags: tuple[str, ...] | list[str] = (),
    default: Any = _MISSING,
    default_factory: Any = _MISSING,
) -> Any:
    """Declare a dataclass field with Aster metadata attached.

    Use in place of ``dataclasses.field(...)`` on a ``@wire_type`` dataclass
    to attach a description and optional tags to a wire field. The
    description flows into ``ContractManifest`` and surfaces in MCP tool
    schemas, generated client docstrings, and the shell's contract view.

    Tags are advisory for field-level use -- the framework guarantees
    round-trip through the manifest but does not enforce anything.

    Example::

        @wire_type("Hello/HelloRequest")
        @dataclass
        class HelloRequest:
            name: str = describe("Name of the person to greet.")
            locale: str = describe("BCP 47 locale tag.", default="en-US")
            api_key: str = describe("API key.", tags=("secret",))

    Args:
        text: Description of the field.
        tags: Optional semantic tags (e.g. ``("pii",)``, ``("secret",)``).
        default: Default value. Mutually exclusive with ``default_factory``.
        default_factory: Zero-arg callable returning the default. Use for
            mutable defaults (lists, dicts, etc.).

    Returns:
        A ``dataclasses.field(...)`` entry whose ``metadata["aster"]`` mapping
        carries the description and tags.
    """
    aster_meta: dict[str, Any] = {"description": text, "tags": list(tags)}
    meta = {"aster": aster_meta}
    kwargs: dict[str, Any] = {"metadata": meta}
    if default is not _MISSING:
        kwargs["default"] = default
    if default_factory is not _MISSING:
        kwargs["default_factory"] = default_factory
    return field(**kwargs)


@dataclass(frozen=True)
class Description:
    """Marker for ``Annotated[T, Description(...)]`` on Mode 2 inline params.

    Attach a description (and optional tags) to an individual parameter of
    a Mode 2 handler. The description surfaces via the same manifest path
    as explicit dataclass field descriptions.

    Example::

        from typing import Annotated
        from aster.metadata import Description

        @rpc
        async def greet(
            name: Annotated[str, Description("Name to greet.")],
            locale: Annotated[str, Description("BCP 47 locale.")] = "en-US",
        ) -> Greeting: ...
    """

    text: str
    tags: tuple[str, ...] = ()


# ── Extraction helpers ────────────────────────────────────────────────────────


def field_metadata(f: dataclasses.Field) -> tuple[str, list[str]]:
    """Extract Aster (description, tags) from a dataclass Field's metadata.

    Returns ``("", [])`` when no Aster metadata is attached.
    """
    meta = f.metadata or {}
    aster = meta.get("aster") if isinstance(meta, (dict,)) or hasattr(meta, "get") else None
    if not aster:
        return "", []
    desc = aster.get("description", "") if hasattr(aster, "get") else ""
    tags = list(aster.get("tags", []) or []) if hasattr(aster, "get") else []
    return desc, tags


def unwrap_annotated(annotation: Any) -> tuple[Any, Description | None]:
    """Split ``Annotated[T, Description(...)]`` into ``(T, Description | None)``.

    If the annotation is not an ``Annotated[...]`` wrapper or contains no
    ``Description`` marker, returns ``(annotation, None)``.

    Also strips the Annotated wrapper when the metadata doesn't contain a
    Description marker -- callers that care about the inner type still get it.
    """
    import typing

    origin = typing.get_origin(annotation)
    # typing.Annotated's origin is the inner type itself under older stdlib;
    # typing.get_args returns (T, m1, m2, ...). Check via get_args.
    args = typing.get_args(annotation)

    # An Annotated annotation survives get_args with >= 2 args and origin
    # being the inner type. Non-Annotated get_args can also return args
    # (e.g. List[int]), so we also require at least one non-type marker.
    # Most reliable probe: check __metadata__ attribute.
    if not hasattr(annotation, "__metadata__"):
        return annotation, None

    # Annotated[T, *markers]: first arg is T, rest is metadata.
    if not args:
        return annotation, None
    inner = args[0]
    for marker in args[1:]:
        if isinstance(marker, Description):
            return inner, marker
    return inner, None
