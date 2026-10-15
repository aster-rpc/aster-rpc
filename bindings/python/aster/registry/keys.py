"""
aster.registry.keys -- Key-schema helpers for the Aster service registry.

All keys are UTF-8 encoded bytes suitable for iroh-docs set_bytes/query calls.
They match ``core/src/registry.rs`` byte for byte.

Key families (§11.2, §12.4):
  contracts/{contract_id}/                               → ArtifactRef JSON
  manifests/{contract_id}/                               → manifest JSON
  services/{name}/versions/v{version}/                   → contract_id (pointer)
  services/{name}/channels/{channel}/                    → contract_id (pointer)
  services/{name}/tags/{tag}/                            → contract_id (pointer)
  services/{name}/contracts/{cid}/endpoints/{eid}/       → EndpointLease JSON
  _aster/acl/{writers|readers|admins|policy}/            → ACL JSON
  _aster/config/{key}/                                   → config value

The keys are **prefix-free**: no key is a byte prefix of another. iroh-docs
lets a write at a key (a value or a deletion marker) supersede the same
author's older entries whose keys extend it, so with ``.../versions/v1`` a
prefix of ``.../versions/v10`` publishing or retracting v1 would remove v10.
Every segment ends in ``/`` and variable segments escape ``%`` and ``/``
(``%25``, ``%2F``), so a segment never holds a raw ``/`` and no family's key
is an interior path of another.
"""

from __future__ import annotations

import re


def key_segment(segment: str) -> str:
    """Escape a variable key segment: ``%`` → ``%25``, ``/`` → ``%2F``."""
    return segment.replace("%", "%25").replace("/", "%2F")


_ESCAPE = re.compile(r"%(25|2F)")


def decode_key_segment(segment: str) -> str | None:
    """Invert :func:`key_segment`; ``None`` for a segment no builder makes."""
    if "/" in segment or "%" in _ESCAPE.sub("", segment):
        return None
    return _ESCAPE.sub(lambda m: "%" if m.group(1) == "25" else "/", segment)


def contract_key(contract_id: str) -> bytes:
    """Key for an ArtifactRef: ``contracts/{contract_id}/``."""
    return f"contracts/{key_segment(contract_id)}/".encode()


def manifest_key(contract_id: str) -> bytes:
    """Key for an inline manifest shortcut: ``manifests/{contract_id}/``.

    The ArtifactRef at ``contracts/{contract_id}/`` points at a blob
    collection; reading ``manifest.json`` from it needs a round-trip.
    The server also writes the manifest JSON inline at this key so
    dynamic consumers can skip the blob download.
    """
    return f"manifests/{key_segment(contract_id)}/".encode()


def version_key(name: str, version: int) -> bytes:
    """Key for a version pointer: ``services/{name}/versions/v{version}/``."""
    return f"services/{key_segment(name)}/versions/v{int(version)}/".encode()


def version_prefix(name: str) -> bytes:
    """Prefix selecting every version pointer of ``name`` (and nothing else):
    ``services/{name}/versions/``. Read the keys with :func:`parse_version_key`."""
    return f"services/{key_segment(name)}/versions/".encode()


_VERSION_KEY = re.compile(r"services/([^/]*)/versions/v(-?(?:0|[1-9][0-9]*))/")


def parse_version_key(key: bytes) -> tuple[str, int] | None:
    """``(name, version)`` of a :func:`version_key`, or ``None`` for any other
    key. Strict: one spelling per pointer (no ``+``, no leading zeros)."""
    try:
        text = bytes(key).decode()
    except UnicodeDecodeError:
        return None
    m = _VERSION_KEY.fullmatch(text)
    if m is None:
        return None
    name = decode_key_segment(m.group(1))
    version = int(m.group(2))
    if name is None or not -(2**31) <= version < 2**31 or m.group(2) == "-0":
        return None
    return name, version


def channel_key(name: str, channel: str) -> bytes:
    """Key for a channel alias: ``services/{name}/channels/{channel}/``."""
    return f"services/{key_segment(name)}/channels/{key_segment(channel)}/".encode()


def tag_key(name: str, tag: str) -> bytes:
    """Key for a tag alias: ``services/{name}/tags/{tag}/``."""
    return f"services/{key_segment(name)}/tags/{key_segment(tag)}/".encode()


def lease_key(name: str, contract_id: str, endpoint_id: str) -> bytes:
    """Key for an endpoint lease.

    ``services/{name}/contracts/{contract_id}/endpoints/{endpoint_id}/``
    """
    return lease_prefix(name, contract_id) + f"{key_segment(endpoint_id)}/".encode()


def lease_prefix(name: str, contract_id: str) -> bytes:
    """Prefix for listing all endpoint leases for a contract.

    ``services/{name}/contracts/{contract_id}/endpoints/``
    """
    return (
        f"services/{key_segment(name)}/contracts/{key_segment(contract_id)}/endpoints/"
    ).encode()


def acl_key(subkey: str) -> bytes:
    """Key for an ACL entry: ``_aster/acl/{subkey}/``."""
    return f"_aster/acl/{key_segment(subkey)}/".encode()


def config_key(subkey: str) -> bytes:
    """Key for a config value: ``_aster/config/{subkey}/``."""
    return f"_aster/config/{key_segment(subkey)}/".encode()


# Registry download-policy prefixes -- all key namespaces that a registry
# client should sync (applied via set_download_policy "nothing_except" by
# RegistryClient). Every family a builder above writes must be covered, or the
# entry syncs without its value; ``endpoints/`` and ``compatibility/`` are
# spec-reserved families with no builder yet.
REGISTRY_PREFIXES: list[bytes] = [
    b"contracts/",
    b"manifests/",
    b"services/",
    b"endpoints/",
    b"compatibility/",
    b"_aster/",
]
