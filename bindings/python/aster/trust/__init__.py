"""
aster.trust -- trust helpers.

Admission is Enrollment, configured through :class:`aster.AsterConfig`
(``[trust]`` + ``[enrollment]``) and run in Rust by :class:`aster.AsterServer`
and :class:`aster.AsterClient`; credentials are issued by the ``aster`` CLI
(``aster enroll node``). The former consumer admission (``aster.consumer_
admission``, consumer credential JSON, ``allow_all_consumers``) has been
removed.

Gate 0 primitives for custom admission are the Rust implementation:
``aster.GatePolicy``, ``aster.PeerAdmissionStore``, ``aster.TrustMode`` and
``Node.admission()``. This package keeps the ed25519 / canonical-JSON helpers,
the @aster delegated-token verifier (``aster.trust.delegated``) and the
capability evaluator (``aster.trust.rcan``).
"""

from aster._aster import (
    ALPN_DELEGATED_ADMISSION,
    ALPN_PRODUCER_ADMISSION,
)
from .signing import canonical_json_value_bytes, generate_root_keypair

__all__ = [
    "canonical_json_value_bytes",
    "generate_root_keypair",
    # Gate-0 admission ALPNs
    "ALPN_PRODUCER_ADMISSION",
    "ALPN_DELEGATED_ADMISSION",
]
