"""AsterConfig.blob_zstd_level and AsterConfig.reuse_last_port: parsing, and
that turning them off reaches the node (Rust ``aster::AsterConfig``)."""
import asyncio
from dataclasses import dataclass

import pytest

from aster import (
    AsterConfig,
    AsterServer,
    EndpointConfig,
    Node,
    create_endpoint_with_config,
    rpc,
    service,
    wire_type,
)

# The compressed blobs ALPN a node serves unless blob_zstd_level is off
# (aster::alpns::BLOBS_ZSTD).
ZSTD_BLOBS_ALPN = b"/iroh-bytes/4+zstd/1"


def _local(**kw) -> AsterConfig:
    return AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", **kw)


def test_defaults_and_kwargs():
    c = AsterConfig()
    assert c.blob_zstd_level == 3
    assert c.reuse_last_port is True
    for off in (0, None, "off", "none", "False", "disabled"):
        assert AsterConfig(blob_zstd_level=off).blob_zstd_level is None, off
    assert AsterConfig(blob_zstd_level=5).blob_zstd_level == 5
    assert AsterConfig(blob_zstd_level="7").blob_zstd_level == 7
    assert AsterConfig(blob_zstd_level=40).blob_zstd_level == 19
    assert AsterConfig(reuse_last_port=False).reuse_last_port is False
    assert c.replace(blob_zstd_level=0, reuse_last_port=False).to_dict()[
        "blob_zstd_level"
    ] is None
    with pytest.raises(ValueError):
        AsterConfig(blob_zstd_level=-1)
    with pytest.raises(ValueError):
        AsterConfig(blob_zstd_level="fast")
    with pytest.raises(TypeError):
        AsterConfig(blob_zstd_level=True)
    with pytest.raises(TypeError):
        AsterConfig(reuse_last_port="no")


def test_env_and_toml(monkeypatch, tmp_path):
    monkeypatch.setenv("ASTER_BLOB_ZSTD_LEVEL", "off")
    monkeypatch.setenv("ASTER_REUSE_LAST_PORT", "false")
    c = AsterConfig.from_env()
    assert c.blob_zstd_level is None
    assert c.reuse_last_port is False
    assert c._sources["blob_zstd_level"] == "ASTER_BLOB_ZSTD_LEVEL"
    monkeypatch.delenv("ASTER_BLOB_ZSTD_LEVEL")
    monkeypatch.delenv("ASTER_REUSE_LAST_PORT")

    path = tmp_path / "aster.toml"
    path.write_text(
        "[storage]\nblob_zstd_level = 9\n\n[network]\nreuse_last_port = false\n"
    )
    c = AsterConfig.from_file(path)
    assert c.blob_zstd_level == 9
    assert c.reuse_last_port is False
    assert c._sources["blob_zstd_level"].endswith("[storage]")
    assert c._sources["reuse_last_port"].endswith("[network]")


def test_endpoint_config_fields():
    ep = EndpointConfig(alpns=[b"x"])
    assert ep.blob_zstd_level == 3
    assert ep.reuse_last_port is True
    ep = EndpointConfig(alpns=[b"x"], blob_zstd_level=0, reuse_last_port=False)
    assert ep.blob_zstd_level is None
    assert ep.reuse_last_port is False
    assert EndpointConfig(alpns=[b"x"], blob_zstd_level=40).blob_zstd_level == 19
    ep.blob_zstd_level = "off"
    assert ep.blob_zstd_level is None
    ep.blob_zstd_level = 5
    assert ep.blob_zstd_level == 5
    with pytest.raises(ValueError):
        EndpointConfig(alpns=[b"x"], blob_zstd_level=-1)


async def _dial_zstd(server: Node) -> bool:
    ep = await create_endpoint_with_config(
        EndpointConfig(alpns=[b"aster-test/dial"], relay_mode="disabled", bind_addr="127.0.0.1:0")
    )
    try:
        conn = await asyncio.wait_for(
            ep.connect_node_addr(server.node_addr_info(), ZSTD_BLOBS_ALPN), 15
        )
    except asyncio.TimeoutError:
        raise
    except Exception:
        return False
    else:
        conn.close(0, b"")
        return True
    finally:
        await ep.close()


@pytest.mark.network
async def test_blob_zstd_off_drops_the_zstd_alpn():
    async with await Node.start(_local()) as on:
        assert await _dial_zstd(on), "the default node should serve the zstd blobs ALPN"
    async with await Node.start(_local(blob_zstd_level=0)) as off:
        assert not await _dial_zstd(off), "blob_zstd_level=0 must drop the zstd blobs ALPN"


@wire_type("test.cfgtransport/Ping")
@dataclass
class _Ping:
    value: str = ""


@service("CfgTransportService", version=1)
class _Service:
    @rpc
    async def ping(self, req: _Ping) -> _Ping:
        return req


@pytest.mark.network
@pytest.mark.parametrize("reuse,recorded", [(True, True), (False, False)])
async def test_reuse_last_port_off_records_no_bind_ports(tmp_path, reuse, recorded):
    data = tmp_path / "srv"
    srv = AsterServer(services=[_Service()], config=_local(storage_path=str(data), reuse_last_port=reuse))
    await srv.start()
    await srv.close()
    assert (data / "bind-ports.json").exists() is recorded
