"""
Binding-layer tests for EndpointConfig.

These tests exercise the PyO3 object directly -- constructor, field access,
field mutation -- without touching any network I/O.  They catch:
  - Wrong PyO3 field names or types
  - Missing keyword arguments
  - Default value drift between Rust and Python
"""

import pytest
from aster import EndpointConfig


# ---------------------------------------------------------------------------
# Construction
# ---------------------------------------------------------------------------

def test_minimal_construction():
    cfg = EndpointConfig(alpns=[])
    assert cfg.alpns == []


def test_all_defaults():
    cfg = EndpointConfig(alpns=[])
    assert cfg.relay_mode is None
    assert cfg.secret_key is None
    assert cfg.enable_monitoring is False
    assert cfg.enable_hooks is False
    assert cfg.hook_timeout_ms == 5000
    assert cfg.hook_failure_mode == "fail_open"
    assert cfg.bind_addr is None
    assert cfg.clear_ip_transports is False
    assert cfg.clear_relay_transports is False
    assert cfg.portmapper_config is None
    assert cfg.proxy_url is None
    assert cfg.proxy_from_env is False
    assert cfg.transport_max_concurrent_bidi_streams is None
    assert cfg.transport_max_concurrent_uni_streams is None
    assert cfg.transport_stream_receive_window is None
    assert cfg.transport_receive_window is None
    assert cfg.transport_send_window is None
    assert cfg.transport_max_idle_timeout_ms is None
    assert cfg.transport_keep_alive_interval_ms is None
    assert cfg.transport_initial_mtu is None
    assert cfg.transport_datagram_receive_buffer_size is None
    assert cfg.transport_datagram_send_buffer_size is None
    assert cfg.transport_send_fairness is None
    assert cfg.transport_enable_segmentation_offload is None


def test_alpns_roundtrip():
    alpns = [b"myproto/1", b"myproto/2"]
    cfg = EndpointConfig(alpns=alpns)
    assert cfg.alpns == alpns


def test_relay_mode():
    for mode in ("default", "disabled", "staging", "custom"):
        cfg = EndpointConfig(alpns=[], relay_mode=mode)
        assert cfg.relay_mode == mode


def test_secret_key_roundtrip():
    key = bytes(range(32))
    cfg = EndpointConfig(alpns=[], secret_key=key)
    assert cfg.secret_key == key


def test_secret_key_none():
    cfg = EndpointConfig(alpns=[], secret_key=None)
    assert cfg.secret_key is None


def test_enable_monitoring():
    cfg = EndpointConfig(alpns=[], enable_monitoring=True)
    assert cfg.enable_monitoring is True


def test_enable_hooks():
    cfg = EndpointConfig(alpns=[], enable_hooks=True)
    assert cfg.enable_hooks is True


def test_hook_timeout_ms():
    cfg = EndpointConfig(alpns=[], hook_timeout_ms=1000)
    assert cfg.hook_timeout_ms == 1000


def test_hook_failure_mode():
    cfg = EndpointConfig(alpns=[], hook_failure_mode="fail_closed")
    assert cfg.hook_failure_mode == "fail_closed"


def test_hook_failure_mode_rejects_invalid_value():
    with pytest.raises(ValueError, match="hook_failure_mode"):
        EndpointConfig(alpns=[], hook_failure_mode="closed-ish")


def test_bind_addr():
    cfg = EndpointConfig(alpns=[], bind_addr="127.0.0.1:0")
    assert cfg.bind_addr == "127.0.0.1:0"


def test_clear_ip_transports():
    cfg = EndpointConfig(alpns=[], clear_ip_transports=True)
    assert cfg.clear_ip_transports is True


def test_clear_relay_transports():
    cfg = EndpointConfig(alpns=[], clear_relay_transports=True)
    assert cfg.clear_relay_transports is True


def test_portmapper_config():
    for val in ("enabled", "disabled"):
        cfg = EndpointConfig(alpns=[], portmapper_config=val)
        assert cfg.portmapper_config == val


def test_proxy_url():
    url = "http://proxy.corp:8080"
    cfg = EndpointConfig(alpns=[], proxy_url=url)
    assert cfg.proxy_url == url


def test_proxy_from_env():
    cfg = EndpointConfig(alpns=[], proxy_from_env=True)
    assert cfg.proxy_from_env is True


def test_all_fields_together():
    key = bytes(range(32))
    cfg = EndpointConfig(
        alpns=[b"proto/1"],
        relay_mode="default",
        secret_key=key,
        enable_monitoring=True,
        enable_hooks=True,
        hook_timeout_ms=2000,
        hook_failure_mode="fail_closed",
        bind_addr="0.0.0.0:9000",
        clear_ip_transports=False,
        clear_relay_transports=False,
        portmapper_config="disabled",
        proxy_url="socks5://localhost:1080",
        proxy_from_env=False,
        transport_max_concurrent_bidi_streams=512,
        transport_max_concurrent_uni_streams=128,
        transport_stream_receive_window=2_000_000,
        transport_receive_window=8_000_000,
        transport_send_window=8_000_000,
        transport_max_idle_timeout_ms=60_000,
        transport_keep_alive_interval_ms=10_000,
        transport_initial_mtu=1452,
        transport_datagram_receive_buffer_size=1_000_000,
        transport_datagram_send_buffer_size=1_000_000,
        transport_send_fairness=True,
        transport_enable_segmentation_offload=False,
    )
    assert cfg.alpns == [b"proto/1"]
    assert cfg.relay_mode == "default"
    assert cfg.secret_key == key
    assert cfg.enable_monitoring is True
    assert cfg.enable_hooks is True
    assert cfg.hook_timeout_ms == 2000
    assert cfg.hook_failure_mode == "fail_closed"
    assert cfg.bind_addr == "0.0.0.0:9000"
    assert cfg.clear_ip_transports is False
    assert cfg.clear_relay_transports is False
    assert cfg.portmapper_config == "disabled"
    assert cfg.proxy_url == "socks5://localhost:1080"
    assert cfg.proxy_from_env is False
    assert cfg.transport_max_concurrent_bidi_streams == 512
    assert cfg.transport_max_concurrent_uni_streams == 128
    assert cfg.transport_stream_receive_window == 2_000_000
    assert cfg.transport_receive_window == 8_000_000
    assert cfg.transport_send_window == 8_000_000
    assert cfg.transport_max_idle_timeout_ms == 60_000
    assert cfg.transport_keep_alive_interval_ms == 10_000
    assert cfg.transport_initial_mtu == 1452
    assert cfg.transport_datagram_receive_buffer_size == 1_000_000
    assert cfg.transport_datagram_send_buffer_size == 1_000_000
    assert cfg.transport_send_fairness is True
    assert cfg.transport_enable_segmentation_offload is False


# ---------------------------------------------------------------------------
# Mutation via setters
# ---------------------------------------------------------------------------

def test_field_mutation():
    cfg = EndpointConfig(alpns=[])
    cfg.alpns = [b"new/1"]
    cfg.relay_mode = "disabled"
    cfg.enable_monitoring = True
    cfg.hook_timeout_ms = 999
    cfg.hook_failure_mode = "fail_closed"
    cfg.bind_addr = "127.0.0.1:0"
    cfg.portmapper_config = "disabled"
    cfg.proxy_url = "http://p:80"
    cfg.proxy_from_env = True
    cfg.clear_ip_transports = True
    cfg.clear_relay_transports = True
    cfg.transport_receive_window = 8_000_000
    cfg.transport_initial_mtu = 1452
    cfg.transport_send_fairness = False
    cfg.transport_enable_segmentation_offload = False

    assert cfg.alpns == [b"new/1"]
    assert cfg.relay_mode == "disabled"
    assert cfg.enable_monitoring is True
    assert cfg.hook_timeout_ms == 999
    assert cfg.hook_failure_mode == "fail_closed"
    assert cfg.bind_addr == "127.0.0.1:0"
    assert cfg.portmapper_config == "disabled"
    assert cfg.proxy_url == "http://p:80"
    assert cfg.proxy_from_env is True
    assert cfg.clear_ip_transports is True
    assert cfg.clear_relay_transports is True
    assert cfg.transport_receive_window == 8_000_000
    assert cfg.transport_initial_mtu == 1452
    assert cfg.transport_send_fairness is False
    assert cfg.transport_enable_segmentation_offload is False


def test_clear_optional_fields():
    cfg = EndpointConfig(
        alpns=[b"proto/1"],
        relay_mode="disabled",
        secret_key=bytes(32),
        bind_addr="0.0.0.0:0",
        proxy_url="http://p:80",
        portmapper_config="disabled",
        transport_receive_window=8_000_000,
        transport_send_fairness=True,
    )
    cfg.relay_mode = None
    cfg.secret_key = None
    cfg.bind_addr = None
    cfg.proxy_url = None
    cfg.portmapper_config = None
    cfg.transport_receive_window = None
    cfg.transport_send_fairness = None

    assert cfg.relay_mode is None
    assert cfg.secret_key is None
    assert cfg.bind_addr is None
    assert cfg.proxy_url is None
    assert cfg.portmapper_config is None
    assert cfg.transport_receive_window is None
    assert cfg.transport_send_fairness is None
