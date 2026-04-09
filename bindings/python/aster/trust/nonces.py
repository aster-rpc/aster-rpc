"""
aster.trust.nonces -- One-shot nonce store for OTT credentials.

Spec reference: Aster-trust-spec.md §3.1, §3.2.1.

Nonces MUST be exactly 32 bytes (``secrets.token_bytes(32)``).  Storing a
nonce as "consumed" is atomic: the JSON file is written to a temp path then
renamed over the canonical path (``os.replace``), which is atomic on POSIX.
``fsync`` is called on the temp file before renaming to guard against
crash-recovery inconsistencies.

Phase 11 ships the file-backend implementation.  An iroh-docs backend is a
drop-in replacement; the interface (``consume``, ``is_consumed``) is identical.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import tempfile
from pathlib import Path
from typing import Protocol, runtime_checkable

logger = logging.getLogger(__name__)

_DEFAULT_PATH = Path.home() / ".aster" / "nonces.json"


@runtime_checkable
class NonceStoreProtocol(Protocol):
    """Interface for nonce stores -- file, docs, or mock backends."""

    async def consume(self, nonce: bytes) -> bool:
        """Atomically mark nonce as used.

        Returns True on the first call for this nonce, False if it has already
        been consumed.  Raises ValueError if nonce is not exactly 32 bytes.
        """
        ...

    async def is_consumed(self, nonce: bytes) -> bool:
        """Return True if this nonce has already been consumed."""
        ...


class NonceStore:
    """Persistent one-shot nonce store backed by a JSON file.

    Thread-safe via an asyncio.Lock.  All I/O is run in an executor to avoid
    blocking the event loop.

    Args:
        path: Path to the JSON file.  Defaults to ``~/.aster/nonces.json``.
    """

    def __init__(self, path: str | Path | None = None) -> None:
        self._path = Path(path) if path is not None else _DEFAULT_PATH
        self._lock = asyncio.Lock()
        self._cache: set[str] | None = None   # hex-encoded nonces

    # ── Public API ────────────────────────────────────────────────────────────

    async def consume(self, nonce: bytes) -> bool:
        """Atomically mark ``nonce`` as used.

        Returns:
            True on first call for this nonce.
            False if nonce was already consumed.

        Raises:
            ValueError: If nonce is not exactly 32 bytes.
        """
        if len(nonce) != 32:
            raise ValueError(f"Nonce MUST be exactly 32 bytes; got {len(nonce)}")
        key = nonce.hex()
        async with self._lock:
            consumed = await asyncio.get_event_loop().run_in_executor(
                None, self._load
            )
            if key in consumed:
                return False
            consumed.add(key)
            await asyncio.get_event_loop().run_in_executor(
                None, self._save, consumed
            )
            self._cache = consumed
            return True

    async def is_consumed(self, nonce: bytes) -> bool:
        """Return True if ``nonce`` has already been consumed."""
        if len(nonce) != 32:
            raise ValueError(f"Nonce MUST be exactly 32 bytes; got {len(nonce)}")
        key = nonce.hex()
        async with self._lock:
            consumed = await asyncio.get_event_loop().run_in_executor(
                None, self._load
            )
            return key in consumed

    # ── Private helpers ───────────────────────────────────────────────────────

    def _load(self) -> set[str]:
        """Load consumed nonces from file.  Returns empty set if file absent."""
        if self._cache is not None:
            return set(self._cache)
        try:
            with open(self._path) as f:
                data = json.load(f)
            return set(data.get("consumed", []))
        except FileNotFoundError:
            return set()
        except (json.JSONDecodeError, KeyError) as exc:
            logger.warning("Nonce store file corrupted (%s); starting fresh", exc)
            return set()

    def _save(self, consumed: set[str]) -> None:
        """Atomically write consumed nonces to file via rename."""
        self._path.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(
            mode="w",
            dir=self._path.parent,
            delete=False,
            suffix=".tmp",
        ) as tf:
            json.dump({"consumed": sorted(consumed)}, tf)
            tf.flush()
            os.fsync(tf.fileno())
            tmp_name = tf.name
        os.replace(tmp_name, str(self._path))
        # fsync the directory to ensure the rename is durable (not supported on Windows)
        if hasattr(os, 'O_DIRECTORY'):
            dirfd = os.open(str(self._path.parent), os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(dirfd)
            finally:
                os.close(dirfd)


class InMemoryNonceStore:
    """In-memory nonce store for tests (not persistent across restarts)."""

    def __init__(self) -> None:
        self._consumed: set[bytes] = set()
        self._lock = asyncio.Lock()

    async def consume(self, nonce: bytes) -> bool:
        if len(nonce) != 32:
            raise ValueError(f"Nonce MUST be exactly 32 bytes; got {len(nonce)}")
        async with self._lock:
            if nonce in self._consumed:
                return False
            self._consumed.add(nonce)
            return True

    async def is_consumed(self, nonce: bytes) -> bool:
        if len(nonce) != 32:
            raise ValueError(f"Nonce MUST be exactly 32 bytes; got {len(nonce)}")
        async with self._lock:
            return nonce in self._consumed
