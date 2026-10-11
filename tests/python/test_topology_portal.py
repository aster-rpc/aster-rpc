"""Native topology baseline opt-in, Gate 3, and safe timing validation."""

from dataclasses import dataclass

import pyfory
import pytest

from aster import (
    AsterClient, AsterConfig, AsterServer, ForyCodec, Node, TopologyTiming,
    all_of, any_of, rpc, service, wire_type,
)
from aster._aster import DispatchServer, RpcClient
from aster.status import RpcError, StatusCode


@wire_type("test.topology/Probe")
@dataclass
class Probe:
    text: str = ""


@service(name="TopologyProbe", version=1)
class TopologyProbe:
    @rpc()
    async def echo(self, request: Probe) -> Probe:
        return request


@wire_type("aster/Empty")
@dataclass
class Empty:
    pass


@wire_type("aster/ClusterView")
@dataclass
class RpcCluster:
    members: list[str]
    bridge: str
    witnesses: list[str]


@wire_type("aster/ClusterList")
@dataclass
class RpcClusters:
    clusters: list[RpcCluster]


@wire_type("aster/SeparatedQuery")
@dataclass
class SeparatedQuery:
    node_a: str
    node_b: str


@wire_type("aster/SeparatedVerdict")
@dataclass
class SeparatedVerdict:
    verdict: pyfory.Int32


def config(*, monitoring=True):
    return AsterConfig(
        relay_mode="disabled", bind_addr="127.0.0.1:0", enable_monitoring=monitoring,
    )


async def test_topology_baseline_is_off_by_default():
    async with AsterServer(
        [TopologyProbe()], config=config(), allow_all_consumers=True,
    ) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            stub = await client.client(TopologyProbe)
            assert await stub.echo(Probe("ready")) == Probe("ready")
            native = RpcClient(stub._transport._conn)
            with pytest.raises(RpcError) as error:
                await native.unary("aster.net.Topology", "clusters", b"\x00")
            assert error.value.code == StatusCode.NOT_FOUND


async def test_topology_baseline_requires_monitoring():
    node = await Node.start(config(monitoring=False), alpns=[b"aster/1"])
    try:
        with pytest.raises(ValueError, match="monitoring"):
            DispatchServer.start(node, [], builtin_topology=True)
    finally:
        await node.shutdown()


@pytest.mark.parametrize("requirement,ops_allowed", [
    ("ops", True), (any_of("ops", "admin"), True), (all_of("ops", "audit"), False),
])
async def test_topology_baseline_native_fory_and_gate3(requirement, ops_allowed):
    codec = ForyCodec(types=[Empty, RpcClusters, SeparatedQuery, SeparatedVerdict])
    async with AsterServer(
        [TopologyProbe()], config=config(), allow_all_consumers=True,
        builtin_topology=True, topology_requires=requirement,
    ) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            stub = await client.client(TopologyProbe)
            assert await stub.echo(Probe("ready")) == Probe("ready")
            native = RpcClient(stub._transport._conn)
            empty = codec.encode(Empty())
            with pytest.raises(RpcError) as error:
                await native.unary("aster.net.Topology", "clusters", empty)
            assert error.value.code == StatusCode.PERMISSION_DENIED

            # This uses the same admission store as ordinary Python services.
            # No application method requires roles, so the topology requirement
            # alone must activate the admission-to-native-authenticator bridge.
            server.gate.admit(client._node.node_id(), attributes={"aster.role": "ops"})
            if ops_allowed:
                result = await native.unary("aster.net.Topology", "clusters", empty)
                assert codec.decode(result, RpcClusters).clusters == []
            else:
                with pytest.raises(RpcError) as error:
                    await native.unary("aster.net.Topology", "clusters", empty)
                assert error.value.code == StatusCode.PERMISSION_DENIED
            server.gate.admit(client._node.node_id(), attributes={"aster.role": "ops,audit"})
            result = await native.unary("aster.net.Topology", "clusters", empty)
            assert codec.decode(result, RpcClusters).clusters == []
            query = SeparatedQuery(server._node.node_id(), client._node.node_id())
            result = await native.unary("aster.net.Topology", "separated", codec.encode(query))
            # Neither endpoint has joined a shared topology. Absence is Unknown.
            assert codec.decode(result, SeparatedVerdict).verdict == 0


@pytest.mark.parametrize("option", ["position", "timing"])
async def test_topology_unrepresentable_duration_is_value_error(option):
    node = await Node.start(config())
    try:
        kwargs = {"position_refresh": 1e300} if option == "position" else {
            "timing": TopologyTiming(min_hold=1e300),
        }
        with pytest.raises(ValueError):
            await node.topology().enable_shared(bytes(32), **kwargs)
    finally:
        await node.shutdown()


async def test_topology_snapshot_survives_node_close_without_restarting_publisher():
    node = await Node.start(config())
    topology = node.topology()
    try:
        await topology.enable_shared(bytes([31]) * 32)
        snapshot = await topology.snapshot()
        clusters = snapshot.clusters
    finally:
        await node.shutdown()
    assert snapshot.clusters == clusters
    assert snapshot.cluster_of("00" * 32) is None
    assert snapshot.separated(node.id, "00" * 32) == "unknown"
    with pytest.raises(Exception, match="closed node"):
        await topology.enable_shared(bytes([32]) * 32)
