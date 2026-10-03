"""
aster.trust -- Trust Foundations.

Provides offline root-key authorization, enrollment credentials, and Gate 0
connection-level admission filtering. (Producer mesh, mesh gossip, mesh
bootstrap and clock-drift detection have been removed.)

Spec references:
  Aster-trust-spec.md §2.2, §2.4, §2.9, §3.1, §3.2, §3.3
  ASTER_PLAN.md §13

Quick start::

    from aster.trust import (
        EnrollmentCredential,
        ConsumerEnrollmentCredential,
        AdmissionResult,
        MeshEndpointHook,
        ALPN_PRODUCER_ADMISSION,
        ALPN_CONSUMER_ADMISSION,
        ALPN_DELEGATED_ADMISSION,
        admit,
        generate_root_keypair,
        sign_credential,
        verify_signature,
        InMemoryNonceStore,
        NonceStore,
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
from .hooks import (
    ALPN_CONSUMER_ADMISSION,
    ALPN_DELEGATED_ADMISSION,
    ALPN_PRODUCER_ADMISSION,
    MeshEndpointHook,
)
from .iid import IIDBackend, MockIIDBackend, get_iid_backend
from .nonces import InMemoryNonceStore, NonceStore
from .signing import (
    generate_root_keypair,
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
    # Signing (canonical_json/canonical_signing_bytes removed -- use Rust core)
    "generate_root_keypair",
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
    "ALPN_DELEGATED_ADMISSION",
]
