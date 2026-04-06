"""
Session-scoped Chat Service — Aster RPC Example.

Demonstrates session-scoped services (``@service(scoped="stream")``), where
each connection gets its own service instance with private state.

Key concepts:
  - ``@service(scoped="stream")`` creates one service instance per connection
  - ``__init__(self, peer)`` receives the peer identity on construction
  - Multiple sequential RPC calls share the same session state
  - ``on_session_close()`` fires when the session stream ends

Architecture:
  - A chat room service tracks per-session state (nickname, message history)
  - The producer hosts the service
  - The consumer opens a session, sets a nickname, sends messages, and
    retrieves history — all on the same session-scoped instance

Usage (two terminals):

  # Terminal 1 — producer
  python session_chat.py producer

  # Terminal 2 — consumer
  ASTER_ENDPOINT_ADDR=<printed by producer> python session_chat.py consumer
"""
from __future__ import annotations

import asyncio
import os
import sys
from dataclasses import dataclass, field
from typing import AsyncIterator

from aster import AsterServer, AsterClient
from aster.codec import wire_type
from aster.decorators import service, rpc, server_stream


# ── Message types ────────────────────────────────────────────────────────────


@wire_type("example.chat/SetNicknameRequest")
@dataclass
class SetNicknameRequest:
    nickname: str = ""


@wire_type("example.chat/SetNicknameResponse")
@dataclass
class SetNicknameResponse:
    greeting: str = ""


@wire_type("example.chat/SendMessageRequest")
@dataclass
class SendMessageRequest:
    text: str = ""


@wire_type("example.chat/SendMessageResponse")
@dataclass
class SendMessageResponse:
    echo: str = ""
    message_number: int = 0


@wire_type("example.chat/HistoryRequest")
@dataclass
class HistoryRequest:
    """Empty request — just asking for the history."""
    pass


@wire_type("example.chat/HistoryItem")
@dataclass
class HistoryItem:
    nickname: str = ""
    text: str = ""
    sequence: int = 0


# ── Session-scoped service ───────────────────────────────────────────────────


@service("ChatRoom", scoped="stream")
class ChatRoomService:
    """A chat room where each connection gets its own session.

    The service is instantiated once per QUIC stream. The ``peer`` parameter
    in ``__init__`` receives the remote peer's endpoint ID (hex string).
    Instance attributes persist across all RPC calls within the session.
    """

    def __init__(self, peer: str | None = None):
        self.peer = peer or "unknown"
        self.nickname = f"anon-{self.peer[:8]}"
        self.messages: list[tuple[str, str]] = []  # (nickname, text)
        print(f"  [session] New session opened for peer {self.peer[:16]}...")

    @rpc
    async def set_nickname(self, req: SetNicknameRequest) -> SetNicknameResponse:
        """Set the nickname for this session."""
        old = self.nickname
        self.nickname = req.nickname
        print(f"  [session] Peer {self.peer[:8]} changed nickname: {old} -> {req.nickname}")
        return SetNicknameResponse(
            greeting=f"Welcome, {req.nickname}! (was {old})"
        )

    @rpc
    async def send_message(self, req: SendMessageRequest) -> SendMessageResponse:
        """Send a message, stored in this session's history."""
        self.messages.append((self.nickname, req.text))
        seq = len(self.messages)
        print(f"  [session] [{self.nickname}] #{seq}: {req.text}")
        return SendMessageResponse(
            echo=f"[{self.nickname}] {req.text}",
            message_number=seq,
        )

    @server_stream
    async def get_history(self, req: HistoryRequest) -> AsyncIterator[HistoryItem]:
        """Stream the full message history for this session."""
        for i, (nick, text) in enumerate(self.messages, 1):
            yield HistoryItem(nickname=nick, text=text, sequence=i)

    async def on_session_close(self) -> None:
        """Called when the session stream ends (peer disconnects or closes)."""
        print(
            f"  [session] Session closed for {self.nickname} "
            f"(peer {self.peer[:8]}, {len(self.messages)} messages)"
        )


# ── Producer ─────────────────────────────────────────────────────────────────


async def run_producer() -> None:
    # For session-scoped services, pass the CLASS (not an instance).
    # The server will instantiate it per connection with peer= kwarg.
    async with AsterServer(services=[ChatRoomService]) as srv:
        print()
        print("=== Chat Room Producer ===")
        print(f"  endpoint_addr : {srv.endpoint_addr_b64}")
        print()
        print("  Run consumer with:")
        print(f"    ASTER_ENDPOINT_ADDR={srv.endpoint_addr_b64} python session_chat.py consumer")
        print()
        print("  Waiting for connections... (Ctrl+C to stop)")
        try:
            await srv.serve()
        except (KeyboardInterrupt, asyncio.CancelledError):
            pass
    print("\n[producer] Stopped.")


# ── Consumer ─────────────────────────────────────────────────────────────────


async def run_consumer() -> None:
    from aster.session import create_session

    async with AsterClient() as c:
        print(f"[consumer] Connected. Services: {[s.name for s in c.services]}")

        # Open a session to the ChatRoomService.
        # create_session returns a SessionStub with typed methods.
        conn = await c._rpc_conn_for(c.services[0].channels["rpc"])
        session = await create_session(ChatRoomService, connection=conn)

        try:
            # 1. Set a nickname (first RPC call on this session)
            resp1 = await session.set_nickname(SetNicknameRequest(nickname="Alice"))
            print(f"  set_nickname -> {resp1.greeting}")

            # 2. Send some messages (same session instance on the server)
            for text in ["Hello everyone!", "How's the weather?", "Goodbye!"]:
                resp2 = await session.send_message(SendMessageRequest(text=text))
                print(f"  send_message -> #{resp2.message_number}: {resp2.echo}")

            # 3. Stream the history back (server_stream within the session)
            items = await session.get_history(HistoryRequest())
            print(f"\n  --- Message History ({len(items)} messages) ---")
            for item in items:
                print(f"    #{item.sequence} [{item.nickname}] {item.text}")

        finally:
            await session.close()

    print("[consumer] Done.")


# ── Entry point ──────────────────────────────────────────────────────────────


def main() -> None:
    if len(sys.argv) < 2 or sys.argv[1] not in ("producer", "consumer"):
        print("Usage: python session_chat.py <producer|consumer>")
        print()
        print("  producer  — start the chat room service")
        print("  consumer  — connect and chat (requires ASTER_ENDPOINT_ADDR)")
        sys.exit(1)

    role = sys.argv[1]
    try:
        if role == "producer":
            asyncio.run(run_producer())
        else:
            asyncio.run(run_consumer())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
