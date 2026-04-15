"""Secure credential storage for the Aster CLI.

Stores the root private key (the offline trust anchor) in the OS keyring
so it never sits as plaintext on disk. Uses the ``keyring`` package when
available; falls back to a ``--root-key <file>`` escape hatch otherwise.

The root private key is the ONLY secret stored in keyring. Node secret
keys live in the project's ``.aster-identity`` file (0600 permissions).

Keyring keys are scoped by profile:
  ``root_privkey:{profile_name}``
"""

from __future__ import annotations

import logging
from typing import Optional

logger = logging.getLogger(__name__)

_HAS_KEYRING = False
try:
    import keyring

    _HAS_KEYRING = True
except ImportError:
    pass

SERVICE_NAME = "aster"

_last_error: Optional[str] = None


def has_keyring() -> bool:
    """Check if keyring support is available."""
    return _HAS_KEYRING


def last_keyring_error() -> Optional[str]:
    """Return the most recent keyring exception message, if any."""
    return _last_error


def store_root_privkey(profile: str, privkey_hex: str) -> bool:
    """Store the root private key for a profile in the OS keyring.

    Args:
        profile: Profile name (e.g. "prod").
        privkey_hex: Hex-encoded 32-byte ed25519 private key seed.

    Returns:
        True if stored successfully, False if keyring unavailable or write failed.
    """
    global _last_error
    if _HAS_KEYRING:
        try:
            keyring.set_password(SERVICE_NAME, f"root_privkey:{profile}", privkey_hex)
            logger.debug("Root private key stored in keyring for profile '%s'", profile)
            _last_error = None
            return True
        except Exception as e:
            _last_error = str(e)
            logger.warning("Failed to store root key in keyring: %s", e)
    return False


def get_root_privkey(profile: str) -> Optional[str]:
    """Retrieve the root private key for a profile from the OS keyring.

    Returns:
        Hex-encoded private key, or None if not found/keyring unavailable.
    """
    if _HAS_KEYRING:
        try:
            value = keyring.get_password(SERVICE_NAME, f"root_privkey:{profile}")
            if value is not None:
                logger.debug("Root private key loaded from keyring for profile '%s'", profile)
            return value
        except Exception as e:
            logger.warning("Failed to load root key from keyring: %s", e)
    return None


def delete_root_privkey(profile: str) -> bool:
    """Delete the root private key for a profile from the OS keyring.

    Returns:
        True if deleted, False if not found or keyring unavailable.
    """
    if _HAS_KEYRING:
        try:
            keyring.delete_password(SERVICE_NAME, f"root_privkey:{profile}")
            logger.debug("Root private key deleted from keyring for profile '%s'", profile)
            return True
        except Exception as e:
            logger.warning("Failed to delete root key from keyring: %s", e)
    return False
