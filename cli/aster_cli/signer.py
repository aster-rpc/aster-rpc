"""
aster_cli.signer — Pluggable credential signing protocol.

The ``CredentialSigner`` protocol defines the interface for signing
enrollment credentials. The CLI resolves a signer based on the operator
profile's ``signer`` field (default: ``"local"``).

Built-in signers:
  ``local``  — reads the root private key from OS keyring or fallback file.

Future signers (enterprise):
  ``kms``    — AWS KMS / Azure Key Vault / GCP Cloud KMS.
  ``remote`` — HTTP call to a signing service.
  ``offline``— two-step: export unsigned → import signed.
"""

from __future__ import annotations

import json
import os
import sys
from typing import Protocol, runtime_checkable


@runtime_checkable
class CredentialSigner(Protocol):
    """Protocol for signing enrollment credentials.

    Implementations receive the raw credential object and return the
    64-byte ed25519 signature. The signing key is an implementation
    detail — it may come from keyring, a file, a KMS API, or an HSM.
    """

    def sign(self, credential: object, root_pubkey: bytes) -> bytes:
        """Sign a credential and return the 64-byte ed25519 signature.

        Args:
            credential: An ``EnrollmentCredential`` or
                ``ConsumerEnrollmentCredential`` instance.
            root_pubkey: The expected root public key (for validation).

        Returns:
            64-byte ed25519 signature bytes.
        """
        ...

    @property
    def root_pubkey(self) -> bytes:
        """The root public key this signer is associated with."""
        ...


class LocalSigner:
    """Signs credentials using a locally-available root private key.

    Resolution order:
    1. OS keyring (profile-scoped).
    2. ``--root-key`` file path.
    3. Default ``~/.aster/root.key``.
    """

    def __init__(self, profile_name: str, root_key_file: str | None = None) -> None:
        self._profile_name = profile_name
        self._root_key_file = root_key_file
        self._privkey: bytes | None = None
        self._pubkey: bytes | None = None
        self._resolve()

    def _resolve(self) -> None:
        from aster_cli.credentials import get_root_privkey, has_keyring

        # Try keyring first
        if has_keyring():
            hex_key = get_root_privkey(self._profile_name)
            if hex_key is not None:
                self._privkey = bytes.fromhex(hex_key)
                self._pubkey = self._derive_pubkey(self._privkey)
                return

        # Fall back to file
        key_paths = [
            p for p in [self._root_key_file, os.path.expanduser("~/.aster/root.key")]
            if p
        ]
        for path in key_paths:
            if os.path.exists(path):
                with open(path) as f:
                    kd = json.load(f)
                self._privkey = bytes.fromhex(kd["private_key"])
                self._pubkey = bytes.fromhex(kd["public_key"])
                return

        print(
            f"Error: no root private key found for profile '{self._profile_name}'.\n"
            f"  Run: aster keygen root --profile {self._profile_name}\n"
            f"  Or:  pass --root-key <path-to-root.key>",
            file=sys.stderr,
        )
        sys.exit(1)

    @staticmethod
    def _derive_pubkey(privkey: bytes) -> bytes:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
        priv = Ed25519PrivateKey.from_private_bytes(privkey)
        return priv.public_key().public_bytes_raw()

    def sign(self, credential: object, root_pubkey: bytes) -> bytes:
        assert self._privkey is not None
        from aster.trust.signing import sign_credential
        return sign_credential(credential, self._privkey)

    @property
    def root_pubkey(self) -> bytes:
        assert self._pubkey is not None
        return self._pubkey


def resolve_signer(profile_name: str, root_key_file: str | None = None,
                   signer_type: str | None = None) -> CredentialSigner:
    """Resolve a signer based on the profile config or explicit type.

    Args:
        profile_name: Operator profile name.
        root_key_file: Fallback file path for the root key.
        signer_type: Explicit signer type (default: read from profile or "local").

    Returns:
        A ``CredentialSigner`` implementation.
    """
    if signer_type is None:
        # Check profile config for a signer field
        from aster_cli.profile import _load_config
        config = _load_config()
        profile = config.get("profiles", {}).get(profile_name, {})
        signer_type = profile.get("signer", "local")

    if signer_type == "local":
        return LocalSigner(profile_name, root_key_file)

    # Future: "kms", "remote", "offline"
    print(f"Error: unknown signer type '{signer_type}'.", file=sys.stderr)
    print("  Supported: local", file=sys.stderr)
    print("  Future: kms, remote, offline", file=sys.stderr)
    sys.exit(1)
