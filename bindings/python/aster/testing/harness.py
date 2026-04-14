"""
aster.testing.harness -- AsterTestHarness factory class.

Spec reference: Aster-SPEC.md §13.2; Plan: §15.3
"""

from __future__ import annotations

from typing import Any


class AsterTestHarness:
    """Test harness for Aster RPC services.

    Provides factory methods for creating client/server pairs.

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
