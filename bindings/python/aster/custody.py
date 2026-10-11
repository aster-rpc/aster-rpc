"""Cancellation handover for native blob custody operations."""
from __future__ import annotations

import asyncio


async def _await_operation(pending, cancellation):
    """Wait for actual cleanup; a commit already started may still succeed."""
    from . import _aster

    task = asyncio.ensure_future(pending.start())
    try:
        while True:
            try:
                return await asyncio.shield(task)
            except asyncio.CancelledError:
                if task.cancelled():
                    raise
                cancellation.request_cancel()
                # Keep waiting through repeated Task.cancel requests. Native
                # owns guards until its terminal, including a raced commit.
                continue
            except _aster._BlobOperationCancelled:
                raise asyncio.CancelledError() from None
    finally:
        # An abandoned coroutine still asks its owned native worker to discard
        # precommit results. A completed or already-committing worker is safe.
        cancellation.request_cancel()
