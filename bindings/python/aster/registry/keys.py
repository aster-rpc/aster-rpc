"""
aster.registry.keys -- Key-schema helpers for the Aster service registry.

All keys are UTF-8 encoded bytes suitable for iroh-docs set_bytes/query calls.

Key prefixes (§11.2, §12.4):
  contracts/{contract_id}                                → ArtifactRef JSON
  services/{name}/versions/v{version}                    → contract_id (pointer)
  services/{name}/channels/{channel}                     → contract_id (pointer)
  services/{name}/tags/{tag}                             → contract_id (pointer)
  services/{name}/contracts/{cid}/endpoints/{eid}        → EndpointLease JSON
  _aster/acl/{writers|readers|admins|policy}             → ACL JSON
  _aster/config/{key}                                    → config value
"""

from __future__ import annotations


def contract_key(contract_id: str) -> bytes:
    """Key for an ArtifactRef: ``contracts/{contract_id}``."""
    return f"contracts/{contract_id}".encode()


def manifest_key(contract_id: str) -> bytes:
    """Key for an inline manifest shortcut: ``manifests/{contract_id}``.

    The ArtifactRef at ``contracts/{contract_id}`` points at a blob
    collection; reading ``manifest.json`` from it needs a round-trip.
    The server also writes the manifest JSON inline at this key so
    dynamic consumers can skip the blob download.
    """
    return f"manifests/{contract_id}".encode()


def version_key(name: str, version: int) -> bytes:
    """Key for a version pointer: ``services/{name}/versions/v{version}``."""
    return f"services/{name}/versions/v{version}".encode()


def channel_key(name: str, channel: str) -> bytes:
    """Key for a channel alias: ``services/{name}/channels/{channel}``."""
    return f"services/{name}/channels/{channel}".encode()


def tag_key(name: str, tag: str) -> bytes:
    """Key for a tag alias: ``services/{name}/tags/{tag}``."""
    return f"services/{name}/tags/{tag}".encode()


def lease_key(name: str, contract_id: str, endpoint_id: str) -> bytes:
    """Key for an endpoint lease.

    ``services/{name}/contracts/{contract_id}/endpoints/{endpoint_id}``
    """
    return f"services/{name}/contracts/{contract_id}/endpoints/{endpoint_id}".encode()


def lease_prefix(name: str, contract_id: str) -> bytes:
    """Prefix for listing all endpoint leases for a contract.

    ``services/{name}/contracts/{contract_id}/endpoints/``
    """
    return f"services/{name}/contracts/{contract_id}/endpoints/".encode()


def acl_key(subkey: str) -> bytes:
    """Key for an ACL entry: ``_aster/acl/{subkey}``."""
    return f"_aster/acl/{subkey}".encode()


def config_key(subkey: str) -> bytes:
    """Key for a config value: ``_aster/config/{subkey}``."""
    return f"_aster/config/{subkey}".encode()


# Registry download-policy prefixes -- all key namespaces that a registry
# client should sync (applied via set_download_policy "nothing_except").
REGISTRY_PREFIXES: list[bytes] = [
    b"contracts/",
    b"services/",
    b"endpoints/",
    b"compatibility/",
    b"_aster/",
]
