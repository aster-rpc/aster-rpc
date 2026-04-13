"""
aster.testing.harness -- AsterTestHarness factory class.

Spec reference: Aster-SPEC.md §13.2; Plan: §15.3
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from aster.server import Server


class AsterTestHarness:
    """Test harness for Aster RPC services.

    Provides factory methods for creating client/server pairs in different
    configurations: local (in-process), remote (real Iroh), and session.

    Spec reference: Aster-SPEC.md §13.2; Plan: §15.3
    """

    async def create_local_pair(
        self,
        service_class: type,
        implementation: object,
        wire_compatible: bool = True,
    ) -> tuple[Any, Any]:
        """Create an in-process LocalTransport client+server pair.

        Args:
            service_class: A class decorated with @service.
            implementation: The service implementation instance.
            wire_compatible: If True, exercises full frame+Fory serialization path.

        Returns:
            (client_stub, implementation) - client has typed method stubs.
        """
        from aster.client import create_local_client

        client = create_local_client(
            service_class,
            implementation,
            wire_compatible=wire_compatible,
        )
        return client, implementation

    async def create_session_pair(
        self,
        service_class: type,
        implementation: object,
        wire_compatible: bool = True,
    ) -> tuple[Any, Any]:
        """Create an in-process session pair for scoped='session' services.

        Args:
            service_class: A class decorated with @service(scoped='session').
            implementation: The implementation class or an instance (class is used).
            wire_compatible: If True, exercises full serialization pipeline.

        Returns:
            (session_stub, implementation)
        """
        from aster.session import create_local_session

        impl_class = (
            implementation
            if isinstance(implementation, type)
            else type(implementation)
        )
        stub = create_local_session(
            service_class,
            impl_class,
            wire_compatible=wire_compatible,
        )
        return stub, implementation
