"""
aster.metadata -- Extensible metadata for services, methods, and fields.

Provides semantic documentation that AI agents (via MCP) and humans
can use to understand what services do and how to populate request fields.

Metadata is NON-CANONICAL -- it does NOT affect contract identity (BLAKE3 hash)
and does NOT appear in the wire protocol.
"""

from __future__ import annotations

from dataclasses import dataclass

from aster.codec import wire_type


@wire_type("_aster/Metadata")
@dataclass
class Metadata:
    """Extensible metadata for describing services, methods, and fields.

    Currently holds a description string. Future extensions may add:
    - tags: list[str]
    - examples: list[dict]
    - constraints: dict
    - deprecated: bool
    - since_version: int
    """

    description: str = ""
