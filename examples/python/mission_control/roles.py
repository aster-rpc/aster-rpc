"""Capability roles for Mission Control (Chapter 5).

Each role maps to a string that appears in the aster.role attribute
of enrollment credentials. The CapabilityInterceptor matches these
against the requires= declarations on service methods.
"""

from enum import Enum


class Role(str, Enum):
    STATUS = "ops.status"
    LOGS = "ops.logs"
    ADMIN = "ops.admin"
    INGEST = "ops.ingest"
