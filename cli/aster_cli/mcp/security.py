"""
aster_cli.mcp.security -- Tool visibility filtering and human-in-the-loop confirmation.

Three layers of security for MCP tool exposure:

1. Credential-based: methods the credential's role can't access don't appear.
   (Handled upstream by the producer's CapabilityInterceptor -- the MCP server
   only sees methods it's admitted for.)

2. Allow/deny globs: local patterns that filter tools AFTER capability checks.
   Deny wins over allow. If no allow patterns, everything passing deny is visible.

3. Confirm patterns: tool calls matching these globs pause for operator approval.
"""

from __future__ import annotations

import fnmatch
import json
import sys
from typing import Any


class ToolFilter:
    """Filters which Aster methods are exposed as MCP tools.

    Usage::

        filt = ToolFilter(
            allow=["HelloService.*", "StatusService.*"],
            deny=["*:delete_*", "*:admin_*"],
            confirm=["DataService.write_*"],
        )

        if filt.is_visible("HelloService.say_hello"):
            # include in tools/list

        if filt.needs_confirmation("DataService.write_record"):
            approved = await filt.confirm_call("DataService.write_record", {"id": 1})
    """

    _TAG_PATTERN_PREFIX = "tag:"

    def __init__(
        self,
        allow: list[str] | None = None,
        deny: list[str] | None = None,
        confirm: list[str] | None = None,
    ) -> None:
        self._allow = allow or []
        self._deny = deny or []
        self._confirm = confirm or []

    @classmethod
    def _matches(cls, pattern: str, tool_name: str, tags: list[str]) -> bool:
        """Match a pattern against either a tag (``tag:NAME``) or the tool name.

        Tag patterns are exact-case, case-sensitive. Name patterns use
        ``fnmatch`` globbing as before. This lets service authors declare
        semantic policy (``tag:destructive``) instead of enumerating every
        matching method name.
        """
        if pattern.startswith(cls._TAG_PATTERN_PREFIX):
            tag = pattern[len(cls._TAG_PATTERN_PREFIX):]
            return tag in tags
        return fnmatch.fnmatch(tool_name, pattern)

    def is_visible(self, tool_name: str, tags: list[str] | None = None) -> bool:
        """Should this tool appear in tools/list?

        Rules:
        - If tool matches any deny pattern → hidden
        - If allow patterns exist and tool matches none → hidden
        - Otherwise → visible

        Patterns starting with ``tag:`` match against the tool's ``tags`` list
        (exact, case-sensitive); all other patterns glob-match the tool name.
        """
        tags = tags or []

        # Deny wins
        for pattern in self._deny:
            if self._matches(pattern, tool_name, tags):
                return False

        # If allow patterns specified, must match at least one
        if self._allow:
            return any(self._matches(p, tool_name, tags) for p in self._allow)

        return True

    def needs_confirmation(self, tool_name: str, tags: list[str] | None = None) -> bool:
        """Should this tool call require human approval?

        Patterns starting with ``tag:`` match against the tool's ``tags`` list;
        all other patterns glob-match the tool name.
        """
        if not self._confirm:
            return False
        tags = tags or []
        return any(self._matches(p, tool_name, tags) for p in self._confirm)

    async def confirm_call(
        self, tool_name: str, arguments: dict[str, Any]
    ) -> bool:
        """Prompt operator for approval via stderr.

        Returns True if approved, False if denied.
        """
        import asyncio

        args_summary = json.dumps(arguments, default=str, indent=2)
        if len(args_summary) > 500:
            args_summary = args_summary[:497] + "..."

        sys.stderr.write(f"\n{'=' * 60}\n")
        sys.stderr.write(f"  MCP TOOL CALL CONFIRMATION\n")
        sys.stderr.write(f"  Tool: {tool_name}\n")
        sys.stderr.write(f"  Args: {args_summary}\n")
        sys.stderr.write(f"{'=' * 60}\n")
        sys.stderr.write(f"  Approve? [y/N] ")
        sys.stderr.flush()

        loop = asyncio.get_event_loop()
        try:
            response = await loop.run_in_executor(None, lambda: input().strip().lower())
            return response in ("y", "yes")
        except (EOFError, KeyboardInterrupt):
            return False

    @property
    def has_filters(self) -> bool:
        """Whether any filtering is configured."""
        return bool(self._allow or self._deny or self._confirm)
