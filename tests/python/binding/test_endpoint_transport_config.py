"""
Binding-level tests that endpoint transport tuning reaches the core builder.
"""

import pytest

from aster import EndpointConfig, IrohError, create_endpoint_with_config


ALPN = b"test/binding/transport-config/1"


async def test_transport_config_can_create_endpoint():
    ep = await create_endpoint_with_config(
        EndpointConfig(
            alpns=[ALPN],
            relay_mode="disabled",
            transport_max_concurrent_bidi_streams=64,
            transport_max_concurrent_uni_streams=16,
            transport_stream_receive_window=1_000_000,
            transport_receive_window=4_000_000,
            transport_send_window=4_000_000,
            transport_max_idle_timeout_ms=30_000,
            transport_keep_alive_interval_ms=5_000,
            transport_initial_mtu=1200,
            transport_datagram_receive_buffer_size=0,
            transport_datagram_send_buffer_size=1_000_000,
            transport_send_fairness=True,
            transport_enable_segmentation_offload=False,
        )
    )
    await ep.close()


@pytest.mark.parametrize(
    ("field", "value", "message"),
    [
        ("transport_initial_mtu", 1199, "transport_initial_mtu must be at least 1200"),
        (
            "transport_max_idle_timeout_ms",
            0,
            "transport_max_idle_timeout_ms must be greater than zero",
        ),
        (
            "transport_keep_alive_interval_ms",
            0,
            "transport_keep_alive_interval_ms must be greater than zero",
        ),
    ],
)
async def test_transport_config_validation(field, value, message):
    with pytest.raises(IrohError, match=message):
        await create_endpoint_with_config(
            EndpointConfig(alpns=[ALPN], relay_mode="disabled", **{field: value})
        )
