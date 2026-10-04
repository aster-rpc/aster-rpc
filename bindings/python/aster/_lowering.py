"""
aster._lowering -- decorators -> ``DynamicService`` (plan P4).

Turns a ``@service``-decorated class (its ``ServiceInfo`` / ``MethodInfo``
from :mod:`aster.decorators`) into the JSON registration form of
``aster::rpc::DynamicService`` (design ``dynamic-dispatch-design.md`` sec. 1.5),
which ``aster._aster.DispatchServer`` serves. Rust derives the contract,
runs Gate 3 on the declared ``requires`` and enforces the method timeouts, so
what is enforced is exactly what the ``contract_id`` commits to.

Type hashes come from the existing type-graph walker in
:mod:`aster.contract.identity` (only Python reflection can see the dataclass
graph); the walker already hashes with core's canonical encoder, so the
hashes -- and the contract id -- are cross-binding-correct.

``method_idx`` is the position in :func:`method_names` (the order of
``ServiceInfo.methods``), which is also the index the dispatcher's handler
table uses.
"""

from __future__ import annotations

import enum
import json
import unicodedata
from typing import Any

from aster.contract.identity import (
    CapabilityKind,
    CapabilityRequirement,
    _get_fqn,
    _resolve_type_hash,
    _to_json,
    build_type_graph,
    canonical_xlang_bytes,
    compute_type_hash,
    resolve_with_cycles,
)
from aster.rpc_types import SerializationMode

_CAP_KIND = {
    CapabilityKind.ROLE: "role",
    CapabilityKind.ANY_OF: "any_of",
    CapabilityKind.ALL_OF: "all_of",
}

_MODE_NAME = {
    SerializationMode.XLANG: "xlang",
    SerializationMode.NATIVE: "native",
    SerializationMode.ROW: "row",
}

_PATTERNS = ("unary", "server_stream", "client_stream", "bidi_stream")


def normalize_requirement(req: Any) -> CapabilityRequirement | None:
    """A ``requires=`` value as a ``CapabilityRequirement`` (or ``None``).

    Accepts a ``CapabilityRequirement``, a role string, or a ``str`` enum
    member (one role, ``ROLE`` kind).
    """
    if req is None:
        return None
    if isinstance(req, CapabilityRequirement):
        return req
    if isinstance(req, enum.Enum):
        return CapabilityRequirement(kind=CapabilityKind.ROLE, roles=[str(req.value)])
    if isinstance(req, str):
        return CapabilityRequirement(kind=CapabilityKind.ROLE, roles=[req])
    raise TypeError(f"unsupported requires= value: {req!r}")


def _requirement_json(req: Any) -> dict[str, Any] | None:
    norm = normalize_requirement(req)
    if norm is None:
        return None
    kind = _CAP_KIND.get(CapabilityKind(int(norm.kind)))
    if kind is None:
        raise ValueError(f"unknown capability kind {norm.kind!r}")
    roles = [str(r.value) if isinstance(r, enum.Enum) else str(r) for r in norm.roles]
    return {"kind": kind, "roles": roles}


def _mode_name(mode: Any) -> str:
    if isinstance(mode, str):
        return mode.lower()
    return _MODE_NAME[SerializationMode(int(mode))]


def _type_display(tp: Any) -> str:
    if tp is None:
        return ""
    if isinstance(tp, str):
        return tp
    if isinstance(tp, type):
        tag = getattr(tp, "__wire_type__", None)
        if isinstance(tag, str) and tag:
            return tag
        try:
            return _get_fqn(tp)
        except Exception:  # noqa: BLE001
            return tp.__name__
    return str(tp)


def service_info_of(service: Any) -> Any:
    """The ``ServiceInfo`` of a ``@service`` class or instance (or a
    ``ServiceInfo`` itself)."""
    from aster.service import ServiceInfo

    if isinstance(service, ServiceInfo):
        return service
    cls = service if isinstance(service, type) else type(service)
    info = getattr(cls, "__aster_service_info__", None)
    if info is None:
        raise TypeError(f"{cls!r} is not @service-decorated (missing __aster_service_info__)")
    return info


def method_names(service: Any) -> list[str]:
    """Method names in ``method_idx`` order."""
    return list(service_info_of(service).methods.keys())


def _type_hashes(info: Any) -> tuple[dict[str, bytes], dict[str, Any]]:
    roots: list[type] = []
    for mi in info.methods.values():
        for tp in (mi.request_type, mi.response_type):
            if tp is not None and isinstance(tp, type):
                roots.append(tp)
    type_defs = resolve_with_cycles(build_type_graph(roots))
    hashes = {fqn: compute_type_hash(canonical_xlang_bytes(td)) for fqn, td in type_defs.items()}
    return hashes, type_defs


def lower(
    service: Any,
    *,
    include_type_defs: bool = False,
    manifest_json: str | None = None,
) -> dict[str, Any]:
    """Lower a ``@service`` class / instance / ``ServiceInfo`` to the
    ``DynamicService`` JSON object (as a dict).

    ``include_type_defs`` adds the ``TypeDef``s (needed only to publish a
    contract collection from Rust); ``manifest_json`` passes a binding
    manifest through verbatim (design sec. 1.3).
    """
    info = service_info_of(service)
    hashes, type_defs = _type_hashes(info)

    modes = [_mode_name(m) for m in (info.serialization_modes or [SerializationMode.XLANG])]
    out: dict[str, Any] = {
        "name": unicodedata.normalize("NFC", info.name),
        "version": int(info.version),
        "scope": "session" if info.scoped in ("session", "stream") else "shared",
        "serialization_modes": modes,
    }
    if "native" in modes:
        out["producer_language"] = "python"
    svc_req = _requirement_json(info.requires)
    if svc_req is not None:
        out["requires"] = svc_req

    methods: list[dict[str, Any]] = []
    for name, mi in info.methods.items():
        pattern = mi.pattern if mi.pattern in _PATTERNS else "unary"
        m: dict[str, Any] = {
            "name": unicodedata.normalize("NFC", name),
            "pattern": pattern,
            "request": {
                "hash": _resolve_type_hash(mi.request_type, hashes).hex(),
                "display": _type_display(mi.request_type),
            },
            "response": {
                "hash": _resolve_type_hash(mi.response_type, hashes).hex(),
                "display": _type_display(mi.response_type),
            },
        }
        if mi.idempotent:
            m["idempotent"] = True
        if mi.timeout:
            m["default_timeout"] = float(mi.timeout)
        req = _requirement_json(mi.requires)
        if req is not None:
            m["requires"] = req
        methods.append(m)
    out["methods"] = methods

    if include_type_defs and type_defs:
        out["type_defs"] = [json.loads(_to_json(td)) for td in type_defs.values()]
    if manifest_json is not None:
        out["manifest_json"] = manifest_json
    return out


def lower_json(service: Any, **kwargs: Any) -> str:
    """:func:`lower`, serialized."""
    return json.dumps(lower(service, **kwargs), separators=(",", ":"), ensure_ascii=False)


def contract_id(service: Any) -> str:
    """The contract id Rust derives from the lowered service (it validates
    the registration too)."""
    from aster._aster import dynamic_service_contract_id

    return dynamic_service_contract_id(lower_json(service))
