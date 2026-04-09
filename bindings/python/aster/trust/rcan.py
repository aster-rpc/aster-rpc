"""
aster.trust.rcan -- rcan capability evaluation and grant serialization.

Provides:
- ``evaluate_capability``: runtime check of a caller's roles against a
  ``CapabilityRequirement`` (ROLE, ANY_OF, ALL_OF).
- Opaque encode/decode/validate helpers for rcan grant bytes (stub until
  upstream spec pins down the serialization format, §14.12).
"""

from __future__ import annotations

from aster.contract.identity import CapabilityKind, CapabilityRequirement


# ── Capability evaluation (Gate 3 -- method-level access control) ────────────


def _extract_caller_roles(caller_attributes: dict[str, str]) -> set[str]:
    """Extract the set of roles from caller admission attributes.

    The ``aster.role`` attribute may be:
    - A comma-separated string (e.g. ``"admin,editor"``).
    - Already a list (if the admission layer passed one through).

    Returns a set of stripped, non-empty role strings.
    """
    raw = caller_attributes.get("aster.role", "")
    if isinstance(raw, list):
        return {r.strip() for r in raw if r.strip()}
    if isinstance(raw, str):
        return {r.strip() for r in raw.split(",") if r.strip()}
    return set()


def evaluate_capability(
    requirement: CapabilityRequirement,
    caller_attributes: dict[str, str],
) -> bool:
    """Check whether *caller_attributes* satisfy *requirement*.

    Args:
        requirement: The ``CapabilityRequirement`` from the service or method
            contract (``ROLE``, ``ANY_OF``, or ``ALL_OF``).
        caller_attributes: The admission attributes dict carried on
            ``CallContext.attributes``.  The key ``aster.role`` holds the
            caller's role(s).

    Returns:
        ``True`` if the caller satisfies the requirement, ``False`` otherwise.
    """
    caller_roles = _extract_caller_roles(caller_attributes)

    if requirement.kind == CapabilityKind.ROLE:
        # ROLE: caller must have the single required role.
        if not requirement.roles:
            return True  # vacuously satisfied -- no role demanded
        return requirement.roles[0] in caller_roles

    if requirement.kind == CapabilityKind.ANY_OF:
        # ANY_OF: caller must have at least one of the listed roles.
        if not requirement.roles:
            return True
        return bool(caller_roles & set(requirement.roles))

    if requirement.kind == CapabilityKind.ALL_OF:
        # ALL_OF: caller must have every listed role.
        if not requirement.roles:
            return True
        return set(requirement.roles) <= caller_roles

    # Unknown kind -- fail closed.
    return False


# ── Opaque grant helpers (stub) ─────────────────────────────────────────────


def validate_rcan(rcan_bytes: bytes) -> tuple[bool, str | None]:
    """Validate an rcan grant.

    Phase 12: all non-empty grants are accepted (opaque).  Empty bytes -> invalid.
    """
    if not rcan_bytes:
        return False, "rcan grant must not be empty"
    # TODO: implement full rcan validation once upstream format is specified (§14.12)
    return True, None
