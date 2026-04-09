"""
aster.registry -- Aster service registry (Phase 10).

Provides a docs-based registry for publishing service contracts, advertising
live endpoints, and resolving services by name/version/channel.

Spec references:
  Aster-SPEC.md §11.2, §11.2.1, §11.2.3, §11.5, §11.6, §11.7, §11.8, §11.9, §11.10
  ASTER_PLAN.md §12
"""

from .acl import RegistryACL
from .client import RegistryClient
from .gossip import RegistryGossip
from .keys import (
    REGISTRY_PREFIXES,
    acl_key,
    channel_key,
    config_key,
    contract_key,
    lease_key,
    lease_prefix,
    tag_key,
    version_key,
)
from .models import (
    ArtifactRef,
    DEGRADED,
    DRAINING,
    EndpointLease,
    GossipEvent,
    GossipEventType,
    READY,
    STARTING,
)
from .publisher import RegistryPublisher

__all__ = [
    # Data models
    "ArtifactRef",
    "EndpointLease",
    "GossipEvent",
    "GossipEventType",
    # HealthStatus constants
    "STARTING",
    "READY",
    "DEGRADED",
    "DRAINING",
    # Key helpers
    "contract_key",
    "version_key",
    "channel_key",
    "tag_key",
    "lease_key",
    "lease_prefix",
    "acl_key",
    "config_key",
    "REGISTRY_PREFIXES",
    # Classes
    "RegistryACL",
    "RegistryGossip",
    "RegistryPublisher",
    "RegistryClient",
]
