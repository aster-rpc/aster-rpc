"""
aster.testing.harness — AsterTestHarness factory class.

Spec reference: Aster-SPEC.md §13.2; Plan: §15.3
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from aster_python.aster.server import Server


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
        from aster_python.aster.client import create_local_client

        client = create_local_client(
            service_class,
            implementation,
            wire_compatible=wire_compatible,
        )
        return client, implementation

    async def create_remote_pair(
        self,
        service_class: type,
        implementation: object,
    ) -> tuple[Any, "Server", Any, Any, Any]:
        """Create a client/server pair using real Iroh QUIC endpoints.

        Spins up two in-memory QUIC endpoints, starts the server accept loop
        as a background task, and opens a client connection.

        Args:
            service_class: A class decorated with @service.
            implementation: The service implementation instance.

        Returns:
            (client_stub, server, connection, server_endpoint, client_endpoint)
            Call server_endpoint.close() and client_endpoint.close() in cleanup.
        """
        import asyncio

        import aster_python
        from aster_python.aster.client import create_client
        from aster_python.aster.server import Server

        alpn = b"aster/1"
        server_endpoint = await aster_python.create_endpoint(alpn)
        client_endpoint = await aster_python.create_endpoint(alpn)

        server = Server(endpoint=server_endpoint, services=[implementation])
        serve_task = asyncio.create_task(server.serve())  # noqa: F841

        # Give the server a moment to start
        await asyncio.sleep(0.05)

        conn = await client_endpoint.connect_node_addr(
            server_endpoint.endpoint_addr_info(),
            alpn,
        )
        client = create_client(service_class, conn)

        return client, server, conn, server_endpoint, client_endpoint

    async def create_session_pair(
        self,
        service_class: type,
        implementation: object,
        wire_compatible: bool = True,
    ) -> tuple[Any, Any]:
        """Create an in-process session pair for scoped='stream' services.

        Args:
            service_class: A class decorated with @service(scoped='stream').
            implementation: The implementation class or an instance (class is used).
            wire_compatible: If True, exercises full serialization pipeline.

        Returns:
            (session_stub, implementation)
        """
        from aster_python.aster.session import create_local_session

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
