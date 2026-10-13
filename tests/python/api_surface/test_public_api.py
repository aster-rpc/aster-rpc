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
    "Node",
    "AsterConfig",
    "SecretKey",
    "PublicKey",
    "NodeId",
    "Hash",
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
    "DownloadPolicy",
    "ShareMode",
    "LocalAccess",
    "AuthorityFingerprint",
    "AuthorityReport",
    "RetireOutcome",
    "BlobFormat",
    "FetchStrategy",
    "FetchReport",
    "ProviderOutcome",
    "PathInfo",
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
    "TrustMode",
    "GatePolicy",
    "PeerAdmission",
    "PeerAdmissionStore",
    "Admission",
    "HandshakeRequest",
    "ConnectRequest",
    "NamespaceCapability",
    "HpkeEnvelope",
    # Plan P7: sealed grants, leases, topology
    "GrantContext",
    "GrantOutcome",
    "GrantRecord",
    "PolicyDoc",
    "AcquireOutcome",
    "Fence",
    "GrantorSerializer",
    "Lease",
    "LeaseGrantorServer",
    "LeaseOp",
    "LeaseOptions",
    "LeaseRowState",
    "LeaseSnapshot",
    "LeaseTransition",
    "MemorySerializer",
    "TransitionOutcome",
    "ClusterView",
    "PeerView",
    "Topology",
    "TopologySnapshot",
    "TopologyTiming",
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


def test_aster_config_loaders_are_callable():
    assert callable(ap.AsterConfig.from_env)
    assert callable(ap.AsterConfig.from_file)
    assert callable(ap.Node.start)


def test_iroh_node_is_gone():
    # Replaced by aster.Node over aster::Node (plan P1).
    assert not hasattr(ap, "IrohNode")
    assert not hasattr(ap, "load_endpoint_config")


@pytest.mark.parametrize(
    "name",
    [
        "namespace_secret_id",
        "hpke_envelope_alg",
        "hpke_generate_keypair",
        "hpke_public_key_from_private",
        "hpke_seal",
        "hpke_open",
        "hpke_x25519_secret_from_identity",
        "hpke_x25519_public_from_identity",
        "hpke_x25519_public_from_node_id",
        "check_grant_role",
        "grant_aad",
        "grant_key",
        "open_grant",
        "open_grant_with_key",
        "open_namespace_grant",
        "reconcile_grant",
        "reconcile_namespace_grant",
        "seal_grant",
        "seal_grant_to_key",
        "seal_namespace_grant",
        "check_fence",
        "lease_transition",
    ],
)
def test_trust_config_primitive_function_is_callable(name):
    assert callable(getattr(ap, name))


def test_p7_errors_are_exceptions():
    assert issubclass(ap.FenceRejectedError, Exception)


def test_p7_node_methods():
    for name in ("topology", "lease_grantor", "serve_lease_grantor"):
        assert callable(getattr(ap.Node, name))


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
    "hook_failure_mode",
    "bind_addr",
    "clear_ip_transports",
    "clear_relay_transports",
    "portmapper_config",
    "proxy_url",
    "proxy_from_env",
    "transport_max_concurrent_bidi_streams",
    "transport_max_concurrent_uni_streams",
    "transport_stream_receive_window",
    "transport_receive_window",
    "transport_send_window",
    "transport_max_idle_timeout_ms",
    "transport_keep_alive_interval_ms",
    "transport_initial_mtu",
    "transport_datagram_receive_buffer_size",
    "transport_datagram_send_buffer_size",
    "transport_send_fairness",
    "transport_enable_segmentation_offload",
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
