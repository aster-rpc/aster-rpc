"""
aster.service — Service registry and metadata types.

Spec reference: §7.1–7.4 (Python decorators)

This module provides the service registry for looking up services and methods,
and the metadata types (ServiceInfo, MethodInfo) that describe service interfaces.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from aster.contract.identity import CapabilityRequirement
from aster.types import SerializationMode

if TYPE_CHECKING:
    from aster.decorators import MethodInfo as DecoratorMethodInfo


# Re-export MethodInfo and ServiceInfo from decorators for backward compatibility
# These are defined in decorators.py but we import them here for convenience
# to avoid circular imports

# ── Service metadata types ─────────────────────────────────────────────────────


@dataclass
class MethodInfo:
    """Method metadata describing a single RPC method.

    Attributes:
        name: The method name.
        pattern: The RPC pattern ("unary", "server_stream", "client_stream", "bidi_stream").
        request_type: The request message type.
        response_type: The response message type.
        timeout: Optional timeout in seconds.
        idempotent: Whether the method is safe to retry.
        serialization: Override serialization mode for this method.
    """

    name: str
    pattern: str
    request_type: type | None = None
    response_type: type | None = None
    timeout: float | None = None
    idempotent: bool = False
    serialization: SerializationMode | None = None
    requires: CapabilityRequirement | None = None


@dataclass
class ServiceInfo:
    """Service metadata describing an RPC service.

    Attributes:
        name: The service name.
        version: The service version.
        scoped: Service scope ("shared" or "stream").
        methods: Dict mapping method names to MethodInfo objects.
        serialization_modes: Supported serialization modes.
        interceptors: Interceptor classes for this service.
        max_concurrent_streams: Maximum concurrent streams (None = unlimited).
    """

    name: str
    version: int
    scoped: str = "shared"
    methods: dict[str, MethodInfo] = field(default_factory=dict)
    serialization_modes: list[SerializationMode] = field(default_factory=list)
    interceptors: list[type] = field(default_factory=list)
    max_concurrent_streams: int | None = None
    requires: CapabilityRequirement | None = None

    def get_method(self, method_name: str) -> MethodInfo | None:
        """Get a method by name.

        Args:
            method_name: The name of the method.

        Returns:
            The MethodInfo for the method, or None if not found.
        """
        return self.methods.get(method_name)

    def has_method(self, method_name: str) -> bool:
        """Check if a method exists.

        Args:
            method_name: The name of the method.

        Returns:
            True if the method exists, False otherwise.
        """
        return method_name in self.methods


# ── Service registry ───────────────────────────────────────────────────────────


class ServiceRegistry:
    """Registry for looking up registered services and dispatching RPC calls.

    The registry holds all services that have been registered via the
    @service decorator, and provides lookup methods for routing incoming
    RPC calls to the appropriate handler.

    Example::

        registry = ServiceRegistry()
        registry.register(MyService)
        registry.register(AnotherService)

        # Look up a service
        svc_info = registry.lookup("MyService")
        if svc_info:
            method_info = svc_info.get_method("MyMethod")

        # Look up by full service path
        result = registry.lookup("MyService", "v1")
    """

    def __init__(self) -> None:
        self._services: dict[tuple[str, int], ServiceInfo] = {}
        self._services_by_name: dict[str, ServiceInfo] = {}

    def register(self, service_class: type) -> ServiceInfo:
        """Register a service class with the registry.

        Args:
            service_class: A class decorated with @service.

        Returns:
            The ServiceInfo for the registered service.

        Raises:
            TypeError: If the class is not decorated with @service.
            ValueError: If a service with the same name and version is already registered.
        """
        from aster.decorators import _SERVICE_INFO_ATTR

        service_info: ServiceInfo | None = getattr(service_class, _SERVICE_INFO_ATTR, None)

        if service_info is None:
            raise TypeError(
                f"Class {service_class.__name__} is not decorated with @service. "
                f"Use @service(name=..., version=...) before registering."
            )

        # Check for duplicate registration
        key = (service_info.name, service_info.version)
        if key in self._services:
            existing = self._services[key]
            raise ValueError(
                f"Service {service_info.name} v{service_info.version} is already "
                f"registered. Register a different version to avoid conflicts."
            )

        # Store by (name, version) and by name
        self._services[key] = service_info
        self._services_by_name[service_info.name] = service_info

        return service_info

    def lookup(
        self, service_name: str, version: int | None = None
    ) -> ServiceInfo | None:
        """Look up a service by name and optionally version.

        Args:
            service_name: The service name.
            version: The service version, or None for the latest (only) version.

        Returns:
            The ServiceInfo for the service, or None if not found.
        """
        if version is not None:
            return self._services.get((service_name, version))

        # If no version specified, look up by name
        return self._services_by_name.get(service_name)

    def lookup_method(
        self, service_name: str, method_name: str, version: int | None = None
    ) -> tuple[ServiceInfo, MethodInfo] | None:
        """Look up a specific method in a service.

        Args:
            service_name: The service name.
            method_name: The method name.
            version: The service version, or None for any version.

        Returns:
            A (ServiceInfo, MethodInfo) tuple, or None if not found.
        """
        service_info = self.lookup(service_name, version)
        if service_info is None:
            return None

        method_info = service_info.get_method(method_name)
        if method_info is None:
            return None

        return service_info, method_info

    def get_all_services(self) -> list[ServiceInfo]:
        """Get all registered services.

        Returns:
            A list of all ServiceInfo objects.
        """
        return list(self._services_by_name.values())

    def clear(self) -> None:
        """Clear all registered services.

        This is primarily useful for testing.
        """
        self._services.clear()
        self._services_by_name.clear()

    def __len__(self) -> int:
        """Return the number of registered services."""
        return len(self._services_by_name)


# ── Global default registry ─────────────────────────────────────────────────────

# This registry is used by the Server class by default
_default_registry: ServiceRegistry | None = None


def get_default_registry() -> ServiceRegistry:
    """Get the default global service registry.

    Returns:
        The default ServiceRegistry instance.
    """
    global _default_registry
    if _default_registry is None:
        _default_registry = ServiceRegistry()
    return _default_registry


def set_default_registry(registry: ServiceRegistry) -> None:
    """Set the default global service registry.

    Args:
        registry: The ServiceRegistry to use as the default.
    """
    global _default_registry
    _default_registry = registry
