"""
aster.contract.generate -- Build contract manifests from service classes.

The language-side half of manifest generation: the Rust ``aster`` CLI cannot
import Python code, so ``aster contract gen`` runs
``python -m aster.contract gen`` (see ``__main__.py``) and adds what is not
language-specific, such as version-control details.
"""

from __future__ import annotations

import importlib
import time
from typing import Any

from aster.contract.identity import (
    ServiceContract,
    build_type_graph,
    canonical_xlang_bytes,
    compute_contract_id,
    compute_type_hash,
    resolve_with_cycles,
)
from aster.contract.manifest import ContractManifest, extract_method_descriptors
from aster.decorators import _SERVICE_INFO_ATTR


def import_service(spec: str) -> type:
    """Import the class named by ``module.path:ClassName``.

    Raises:
        ValueError: The spec is malformed, or the module or class is missing.
    """
    module_path, sep, class_name = spec.rpartition(":")
    if not sep or not module_path or not class_name:
        raise ValueError(f"expected MODULE:CLASS, got {spec!r}")
    try:
        module = importlib.import_module(module_path)
    except ImportError as exc:
        raise ValueError(f"cannot import module {module_path!r}: {exc}") from exc
    cls = getattr(module, class_name, None)
    if cls is None:
        raise ValueError(f"module {module_path!r} has no attribute {class_name!r}")
    return cls


def _mode_name(mode: Any) -> str:
    if hasattr(mode, "name"):
        return mode.name.lower()
    if hasattr(mode, "value") and isinstance(mode.value, str):
        return mode.value
    return str(mode)


def build_manifest(
    cls: type,
    *,
    semver: str | None = None,
    vcs_revision: str | None = None,
    vcs_tag: str | None = None,
    vcs_url: str | None = None,
) -> ContractManifest:
    """Build the manifest of an ``@service`` class.

    Raises:
        ValueError: ``cls`` is not decorated with ``@service``.
    """
    service_info = getattr(cls, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        raise ValueError(f"class {cls.__name__} is not decorated with @service")

    root_types: list[type] = []
    for method_info in service_info.methods.values():
        if method_info.request_type is not None:
            root_types.append(method_info.request_type)
        if method_info.response_type is not None:
            root_types.append(method_info.response_type)

    type_defs = resolve_with_cycles(build_type_graph(root_types))
    type_hashes = {
        fqn: compute_type_hash(canonical_xlang_bytes(td)) for fqn, td in type_defs.items()
    }
    contract = ServiceContract.from_service_info(service_info, type_hashes)
    contract_id = compute_contract_id(canonical_xlang_bytes(contract))

    modes = [_mode_name(m) for m in service_info.serialization_modes] or ["xlang"]
    metadata = getattr(service_info, "metadata", None)

    return ContractManifest(
        service=service_info.name,
        version=service_info.version,
        contract_id=contract_id,
        canonical_encoding="fory-xlang/0.15",
        type_count=len(type_defs),
        type_hashes=sorted(h.hex() for h in type_hashes.values()),
        method_count=len(contract.methods),
        methods=extract_method_descriptors(service_info),
        serialization_modes=modes,
        # Spec 11.3.2.3: required with "native", empty otherwise.
        producer_language="python" if "native" in modes else "",
        scoped=getattr(service_info, "scoped", "shared"),
        description=str(getattr(metadata, "description", "") or ""),
        tags=list(getattr(metadata, "tags", []) or []),
        deprecated=False,
        semver=semver,
        vcs_revision=vcs_revision,
        vcs_tag=vcs_tag,
        vcs_url=vcs_url,
        changelog=None,
        published_by="",
        published_at_epoch_ms=int(time.time() * 1000),
    )
