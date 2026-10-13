"""Capability roles for Mission Control (Chapter 5).

Each role is a string carried in the aster.role claim of a node's
enrollment credential (aster enroll node --attributes roles.json). With
enrollment configured, the server maps each peer's verified claims to its
roles and Rust checks them against the requires= declarations on service
methods (identical to the Rust example's roles.rs).
"""

from enum import Enum


class Role(str, Enum):
    STATUS = "ops.status"
    LOGS = "ops.logs"
    ADMIN = "ops.admin"
    INGEST = "ops.ingest"
