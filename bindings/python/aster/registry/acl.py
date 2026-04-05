"""
aster.registry.acl — ACL enforcement for the Aster service registry.

Spec reference: Aster-SPEC.md §11.2.3.

ACL entries are stored in the registry doc at:
  _aster/acl/writers  → JSON list[str] of trusted writer AuthorIds
  _aster/acl/readers  → JSON list[str] of reader AuthorIds
  _aster/acl/admins   → JSON list[str] of admin AuthorIds

Phase 10 filters at READ TIME (post-read filter). Untrusted writes persist
locally but are excluded from all registry reads. True sync-time rejection
is deferred pending a future FFI hook.
"""

from __future__ import annotations

import json
import logging
from typing import TYPE_CHECKING

from .keys import acl_key

if TYPE_CHECKING:
    pass  # DocHandle imported lazily

logger = logging.getLogger(__name__)


class RegistryACL:
    """In-memory ACL cache for the registry doc.

    Starts in *open mode* (all authors trusted) until an ACL entry is
    explicitly configured. Open mode is appropriate for local/dev deployments.

    When ``add_writer()`` is first called, the ACL switches to restricted mode
    and only explicitly listed writers are trusted.
    """

    def __init__(self, doc: object, author_id: str) -> None:
        """
        Args:
            doc: A ``DocHandle`` for the registry doc.
            author_id: The local author's ID (used to write ACL entries).
        """
        self._doc = doc
        self._author_id = author_id
        self._open = True               # All-trusted until a writer is set
        self._writers: set[str] = set()
        self._readers: set[str] = set()
        self._admins: set[str] = set()

    # ── Read-time filtering ───────────────────────────────────────────────────

    def is_trusted_writer(self, author_id: str) -> bool:
        """Return True if this author's writes should be trusted.

        Open mode: always True.
        Restricted mode: True only if author_id is in the writers set.
        """
        if self._open:
            return True
        return author_id in self._writers

    def filter_trusted(self, entries: list) -> list:
        """Filter a list of DocEntry objects to only trusted-author entries."""
        return [e for e in entries if self.is_trusted_writer(e.author_id)]

    # ── Reload from doc ────────────────────────────────────────────────────────

    async def reload(self) -> None:
        """Reload ACL state from the registry doc.

        If no ACL entries exist, stay in open mode. If entries are present,
        switch to restricted mode.
        """
        writers = await self._read_list("writers")
        if writers is None:
            # No ACL configured — remain open
            return
        readers = await self._read_list("readers") or []
        admins = await self._read_list("admins") or []
        self._writers = set(writers)
        self._readers = set(readers)
        self._admins = set(admins)
        self._open = False
        logger.debug(
            "ACL reloaded: %d writers, %d readers, %d admins",
            len(self._writers),
            len(self._readers),
            len(self._admins),
        )

    async def _read_list(self, subkey: str) -> list[str] | None:
        """Read a JSON list from the doc at ``_aster/acl/{subkey}``.

        Returns None if the key does not exist.
        """
        entries = await self._doc.query_key_exact(acl_key(subkey))
        # Prefer the entry written by ourselves (the admin)
        our_entries = [e for e in entries if e.author_id == self._author_id]
        target = our_entries[0] if our_entries else (entries[0] if entries else None)
        if target is None:
            return None
        raw = await self._doc.read_entry_content(target.content_hash)
        return json.loads(raw)

    # ── Admin operations ───────────────────────────────────────────────────────

    async def add_writer(self, author_id: str) -> None:
        """Add an author to the trusted writers set.

        Persists the updated list to the doc. Switches to restricted mode if
        this is the first explicit writer entry.
        """
        self._writers.add(author_id)
        self._open = False
        await self._persist("writers", list(self._writers))

    async def remove_writer(self, author_id: str) -> None:
        """Remove an author from the trusted writers set."""
        self._writers.discard(author_id)
        await self._persist("writers", list(self._writers))

    async def get_writers(self) -> list[str]:
        return list(self._writers)

    async def get_readers(self) -> list[str]:
        return list(self._readers)

    async def get_admins(self) -> list[str]:
        return list(self._admins)

    async def _persist(self, subkey: str, values: list[str]) -> None:
        await self._doc.set_bytes(
            self._author_id,
            acl_key(subkey),
            json.dumps(values).encode(),
        )
