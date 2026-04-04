"""
aster.trust — Trust Foundations (Phases 11 & 12).

Provides offline root-key authorization, enrollment credentials, Gate 0
connection-level admission filtering, producer mesh gossip, and clock drift
detection.

Spec references:
  Aster-trust-spec.md §2.2, §2.4, §2.9, §3.1, §3.2, §3.3 (Phase 11)
  Aster-trust-spec.md §2.1, §2.3, §2.5, §2.6, §2.7, §2.10 (Phase 12)
  ASTER_PLAN.md §13, §14

Quick start::

    from aster_python.aster.trust import (
        EnrollmentCredential,
        ConsumerEnrollmentCredential,
        AdmissionResult,
        MeshEndpointHook,
        ALPN_PRODUCER_ADMISSION,
        ALPN_CONSUMER_ADMISSION,
        admit,
        generate_root_keypair,
        sign_credential,
        verify_signature,
        InMemoryNonceStore,
        NonceStore,
        # Phase 12
        ProducerMessage,
        ProducerMessageType,
        MeshState,
        ClockDriftConfig,
        ClockDriftDetector,
        sign_producer_message,
        verify_producer_message,
        derive_gossip_topic,
    )
"""

from .admission import admit, check_offline, check_runtime
from .credentials import (
    ATTR_IID_ACCOUNT,
    ATTR_IID_PROVIDER,
    ATTR_IID_REGION,
    ATTR_IID_ROLE_ARN,
    ATTR_NAME,
    ATTR_ROLE,
    AdmissionResult,
    ConsumerEnrollmentCredential,
    EnrollmentCredential,
)
from .drift import ClockDriftDetector
from .gossip import (
    derive_gossip_topic,
    encode_contract_published_payload,
    encode_depart_payload,
    encode_introduce_payload,
    encode_lease_update_payload,
    handle_producer_message,
    producer_message_signing_bytes,
    run_lease_heartbeat,
    sign_producer_message,
    start_lease_heartbeat,
    verify_producer_message,
)
from .hooks import ALPN_CONSUMER_ADMISSION, ALPN_PRODUCER_ADMISSION, MeshEndpointHook
from .iid import IIDBackend, MockIIDBackend, get_iid_backend
from .mesh import (
    AdmissionRequest,
    AdmissionResponse,
    ClockDriftConfig,
    ContractPublishedPayload,
    DepartPayload,
    IntroducePayload,
    LeaseUpdatePayload,
    MeshState,
    ProducerMessage,
    ProducerMessageType,
)
from .nonces import InMemoryNonceStore, NonceStore
from .signing import (
    canonical_json,
    canonical_signing_bytes,
    generate_root_keypair,
    load_private_key,
    load_public_key,
    sign_credential,
    verify_signature,
)

__all__ = [
    # Credentials
    "EnrollmentCredential",
    "ConsumerEnrollmentCredential",
    "AdmissionResult",
    # Attribute key constants
    "ATTR_ROLE",
    "ATTR_NAME",
    "ATTR_IID_PROVIDER",
    "ATTR_IID_ACCOUNT",
    "ATTR_IID_REGION",
    "ATTR_IID_ROLE_ARN",
    # Signing
    "canonical_json",
    "canonical_signing_bytes",
    "generate_root_keypair",
    "load_private_key",
    "load_public_key",
    "sign_credential",
    "verify_signature",
    # Admission
    "admit",
    "check_offline",
    "check_runtime",
    # IID
    "IIDBackend",
    "MockIIDBackend",
    "get_iid_backend",
    # Nonces
    "NonceStore",
    "InMemoryNonceStore",
    # Hooks
    "MeshEndpointHook",
    "ALPN_PRODUCER_ADMISSION",
    "ALPN_CONSUMER_ADMISSION",
    # Phase 12: Producer mesh
    "ProducerMessage",
    "ProducerMessageType",
    "IntroducePayload",
    "DepartPayload",
    "ContractPublishedPayload",
    "LeaseUpdatePayload",
    "MeshState",
    "ClockDriftConfig",
    "AdmissionRequest",
    "AdmissionResponse",
    # Phase 12: Gossip
    "producer_message_signing_bytes",
    "sign_producer_message",
    "verify_producer_message",
    "derive_gossip_topic",
    "handle_producer_message",
    "encode_introduce_payload",
    "encode_depart_payload",
    "encode_contract_published_payload",
    "encode_lease_update_payload",
    "run_lease_heartbeat",
    "start_lease_heartbeat",
    # Phase 12: Drift
    "ClockDriftDetector",
]
