"""
API surface tests for aster.

These tests verify that every name listed in __all__ is:
  - importable from aster
  - the right kind (class, function, exception)
  - has the expected attributes for key types

They run in milliseconds with zero network I/O.  Their purpose is to catch
accidental removals or renames caused by Rust refactors, PyO3 registration
changes, or __init__.py edits -- before any protocol-level test would notice.
"""

import inspect
import pytest
import aster
import aster as ap


# ---------------------------------------------------------------------------
# Everything in __all__ is importable
# ---------------------------------------------------------------------------

def test_all_names_importable():
    missing = []
    for name in aster.__all__:
        if not hasattr(aster, name):
            missing.append(name)
    assert missing == [], f"Names in __all__ but not importable: {missing}"


def test_no_name_in_all_is_none():
    nones = [
        name for name in aster.__all__
        if getattr(aster, name, "SENTINEL") is None
    ]
    assert nones == [], f"Names in __all__ resolved to None: {nones}"


# ---------------------------------------------------------------------------
# Exception hierarchy
# ---------------------------------------------------------------------------

_EXCEPTION_NAMES = [
    "IrohError",
    "BlobNotFound",
    "DocNotFound",
    "ConnectionError",
    "TicketError",
]

@pytest.mark.parametrize("name", _EXCEPTION_NAMES)
def test_exception_is_exception_subclass(name):
    cls = getattr(ap, name)
    assert issubclass(cls, Exception), f"{name} is not an Exception subclass"


def test_specific_errors_are_iroh_error_subclasses():
    for name in ("BlobNotFound", "DocNotFound", "ConnectionError", "TicketError"):
        cls = getattr(ap, name)
        assert issubclass(cls, ap.IrohError), f"{name} does not inherit from IrohError"


# ---------------------------------------------------------------------------
# Classes exist and are classes
# ---------------------------------------------------------------------------

_CLASS_NAMES = [
    "IrohNode",
    "BlobsClient",
    "BlobStatusResult",
    "BlobObserveResult",
    "BlobLocalInfo",
    "TagInfo",
    "DocsClient",
    "DocHandle",
    "DocEntry",
    "DocEvent",
    "DocEventReceiver",
    "DocDownloadPolicy",
    "GossipClient",
    "GossipTopicHandle",
    "NodeAddr",
    "EndpointConfig",
    "ConnectionInfo",
    "RemoteInfo",
    "NetClient",
    "IrohConnection",
    "IrohSendStream",
    "IrohRecvStream",
    "HookConnectInfo",
    "HookHandshakeInfo",
    "HookDecision",
    "HookReceiver",
    "HookRegistration",
    "HookManager",
]

@pytest.mark.parametrize("name", _CLASS_NAMES)
def test_is_class(name):
    obj = getattr(ap, name)
    assert inspect.isclass(obj), f"{name} is not a class"


# ---------------------------------------------------------------------------
# Callables (factory functions / coroutine functions)
# ---------------------------------------------------------------------------

_ASYNC_FUNCTION_NAMES = [
    "blobs_client",
    "docs_client",
    "gossip_client",
    "net_client",
    "create_endpoint",
    "create_endpoint_with_config",
]

@pytest.mark.parametrize("name", _ASYNC_FUNCTION_NAMES)
def test_is_callable(name):
    obj = getattr(ap, name)
    assert callable(obj), f"{name} is not callable"


def test_load_endpoint_config_is_callable():
    assert callable(ap.load_endpoint_config)


# ---------------------------------------------------------------------------
# EndpointConfig has all expected fields
# ---------------------------------------------------------------------------

_ENDPOINT_CONFIG_FIELDS = [
    "alpns",
    "relay_mode",
    "secret_key",
    "enable_monitoring",
    "enable_hooks",
    "hook_timeout_ms",
    "bind_addr",
    "clear_ip_transports",
    "clear_relay_transports",
    "portmapper_config",
    "proxy_url",
    "proxy_from_env",
]

def test_endpoint_config_has_all_fields():
    cfg = ap.EndpointConfig(alpns=[])
    missing = [f for f in _ENDPOINT_CONFIG_FIELDS if not hasattr(cfg, f)]
    assert missing == [], f"EndpointConfig missing fields: {missing}"


# ---------------------------------------------------------------------------
# NodeAddr has expected fields and methods
# ---------------------------------------------------------------------------

def test_node_addr_fields():
    addr = ap.NodeAddr(endpoint_id="a" * 64)
    assert hasattr(addr, "endpoint_id")
    assert hasattr(addr, "relay_url")
    assert hasattr(addr, "direct_addresses")


def test_node_addr_has_serialization_methods():
    addr = ap.NodeAddr(endpoint_id="a" * 64)
    assert callable(getattr(addr, "to_bytes", None))
    assert callable(getattr(addr, "to_dict", None))
    assert callable(getattr(ap.NodeAddr, "from_bytes", None))
    assert callable(getattr(ap.NodeAddr, "from_dict", None))


# ---------------------------------------------------------------------------
# __version__ is present and a string
# ---------------------------------------------------------------------------

def test_version_is_string():
    assert isinstance(aster.__version__, str)
    assert len(aster.__version__) > 0
