"""
aster.contract.manifest — ContractManifest and verification.

Spec reference: Aster-ContractIdentity.md §11.4

Provides:
- ContractManifest: dataclass for persisting contract identity info
- FatalContractMismatch: raised when a live contract doesn't match the manifest
- verify_manifest_or_fatal: strict identity check at startup
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field

import blake3


@dataclass
class ContractManifest:
    """Persisted record of a service contract's canonical identity.

    Write with ``json.dumps(asdict(manifest))`` and read back with
    ``ContractManifest(**json.loads(text))``.
    """

    service: str
    """Service name."""

    version: int
    """Service version integer."""

    contract_id: str
    """64-char hex string (full BLAKE3 digest of canonical ServiceContract bytes)."""

    canonical_encoding: str = "fory-xlang/0.15"
    """Encoding scheme identifier."""

    type_count: int = 0
    """Number of user-defined types referenced by this contract."""

    type_hashes: list[str] = field(default_factory=list)
    """Hex-encoded BLAKE3 hashes of each referenced TypeDef (sorted)."""

    method_count: int = 0
    """Number of methods in the service."""

    methods: list[dict] = field(default_factory=list)
    """Method descriptors: [{name, pattern, request_type, response_type, timeout, idempotent, fields}].

    Each entry provides enough information for dynamic invocation and shell
    autocomplete without needing the Python type definitions locally.
    ``fields`` is a list of ``{name, type, required, default}`` dicts describing
    the request type's fields (empty if type info is unavailable).
    """

    serialization_modes: list[str] = field(default_factory=list)
    """Supported serialization mode strings (e.g. ["xlang"])."""

    scoped: str = "shared"
    """Service scope: "shared" or "stream"."""

    deprecated: bool = False
    """Whether this contract version is deprecated."""

    semver: str | None = None
    """Optional semantic version string."""

    vcs_revision: str | None = None
    """Optional VCS commit hash."""

    vcs_tag: str | None = None
    """Optional VCS tag."""

    vcs_url: str | None = None
    """Optional VCS repository URL."""

    changelog: str | None = None
    """Optional free-form changelog entry."""

    published_by: str = ""
    """Identity of the publisher (node public key hex or human name)."""

    published_at_epoch_ms: int = 0
    """Publication timestamp in milliseconds since Unix epoch."""

    def to_json(self, indent: int | None = 2) -> str:
        """Serialize to a JSON string.

        Args:
            indent: JSON indentation level. None for compact.

        Returns:
            JSON string representation.
        """
        return json.dumps(asdict(self), indent=indent)

    @classmethod
    def from_json(cls, text: str) -> "ContractManifest":
        """Deserialize from a JSON string.

        Args:
            text: JSON string.

        Returns:
            ContractManifest instance.
        """
        data = json.loads(text)
        return cls(**data)

    @classmethod
    def from_file(cls, path: str) -> "ContractManifest":
        """Load a ContractManifest from a JSON file.

        Args:
            path: Path to the manifest JSON file.

        Returns:
            ContractManifest instance.
        """
        with open(path, encoding="utf-8") as f:
            return cls.from_json(f.read())

    def save(self, path: str) -> None:
        """Save the manifest to a JSON file.

        Args:
            path: Destination path for the JSON file.
        """
        with open(path, "w", encoding="utf-8") as f:
            f.write(self.to_json())


# ── Method extraction ─────────────────────────────────────────────────────────


def extract_method_descriptors(service_info: object) -> list[dict]:
    """Extract method descriptors from a ServiceInfo for manifest storage.

    Introspects request types to extract field definitions (name, type, required,
    default) so that dynamic clients and the shell can build payloads without
    needing the Python types locally.

    Args:
        service_info: A ServiceInfo object from aster.service.

    Returns:
        List of method descriptor dicts, sorted by name.
    """
    import dataclasses
    import inspect
    from typing import get_type_hints

    methods_out: list[dict] = []

    for method_name, method_info in getattr(service_info, "methods", {}).items():
        fields: list[dict] = []

        # Extract fields from the request type
        req_type = getattr(method_info, "request_type", None)
        if req_type is not None and dataclasses.is_dataclass(req_type):
            try:
                hints = get_type_hints(req_type)
            except Exception:
                hints = {}

            for f in dataclasses.fields(req_type):
                ftype = hints.get(f.name, f.type)
                type_name = _type_display_name(ftype)

                has_default = (
                    f.default is not dataclasses.MISSING
                    or f.default_factory is not dataclasses.MISSING
                )

                default_val = None
                if f.default is not dataclasses.MISSING:
                    default_val = f.default
                elif f.default_factory is not dataclasses.MISSING:
                    # Don't call the factory — just note it has one
                    default_val = None

                fields.append({
                    "name": f.name,
                    "type": type_name,
                    "required": not has_default,
                    "default": default_val if _is_json_safe(default_val) else str(default_val),
                })

        resp_type = getattr(method_info, "response_type", None)

        methods_out.append({
            "name": method_name,
            "pattern": getattr(method_info, "pattern", "unary"),
            "request_type": _type_display_name(req_type) if req_type else "",
            "response_type": _type_display_name(resp_type) if resp_type else "",
            "timeout": getattr(method_info, "timeout", None),
            "idempotent": getattr(method_info, "idempotent", False),
            "fields": fields,
        })

    methods_out.sort(key=lambda m: m["name"])
    return methods_out


def _type_display_name(t: object) -> str:
    """Human-readable name for a type."""
    if t is None:
        return ""
    if hasattr(t, "__name__"):
        return t.__name__
    return str(t)


def _is_json_safe(val: object) -> bool:
    """Check if a value is safely JSON-serializable."""
    return val is None or isinstance(val, (str, int, float, bool))


# ── FatalContractMismatch ─────────────────────────────────────────────────────


class FatalContractMismatch(Exception):
    """Raised when the live contract hash doesn't match the committed manifest.

    This indicates a breaking change that wasn't recorded. The developer
    must rerun ``aster contract gen`` and commit the updated manifest.
    """

    def __init__(
        self,
        service_name: str,
        version: int,
        expected_id: str,
        actual_id: str,
        manifest_path: str,
    ) -> None:
        self.service_name = service_name
        self.version = version
        self.expected_id = expected_id
        self.actual_id = actual_id
        self.manifest_path = manifest_path

        super().__init__(
            f"Contract identity mismatch for {service_name!r} v{version}:\n"
            f"  Expected: {expected_id}\n"
            f"  Actual:   {actual_id}\n"
            f"  Manifest: {manifest_path}\n"
            f"  → The service interface has changed without updating the manifest.\n"
            f"    Rerun `aster contract gen` and commit the updated manifest."
        )


# ── Verification ──────────────────────────────────────────────────────────────


def verify_manifest_or_fatal(
    live_contract_bytes: bytes,
    manifest_path: str,
) -> ContractManifest:
    """Verify that the live contract bytes match the committed manifest.

    Loads the manifest from *manifest_path*, computes BLAKE3 of
    *live_contract_bytes*, and checks for equality.

    Args:
        live_contract_bytes: Canonical bytes of the live ServiceContract.
        manifest_path: Path to the committed manifest JSON file.

    Returns:
        The loaded ContractManifest on success.

    Raises:
        FatalContractMismatch: If the hashes differ.
        FileNotFoundError: If the manifest file does not exist.
        json.JSONDecodeError: If the manifest file is malformed.
    """
    manifest = ContractManifest.from_file(manifest_path)
    actual_id = blake3.blake3(live_contract_bytes).hexdigest()

    if actual_id != manifest.contract_id:
        raise FatalContractMismatch(
            service_name=manifest.service,
            version=manifest.version,
            expected_id=manifest.contract_id,
            actual_id=actual_id,
            manifest_path=manifest_path,
        )

    return manifest
