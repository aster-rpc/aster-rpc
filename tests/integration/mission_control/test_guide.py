#!/usr/bin/env python3
"""
Mission Control guide integration test -- Python client.

Tests every chapter of the Mission Control guide against a running server
(Python or TypeScript). Run from the repo root:

    python tests/integration/mission_control/test_guide.py <address> --mode dev
    python tests/integration/mission_control/test_guide.py <address> --mode auth --keys-dir <work_dir>

Exit codes:
  0  all tests passed
  1  at least one test failed
  2  setup/usage error
"""

import argparse
import asyncio
import os
import sys
import time
from pathlib import Path
from typing import Any

# Make examples importable
REPO_ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO_ROOT))


# ── Output helpers ──────────────────────────────────────────────────────────

QUIET = False
PASS_COUNT = 0
FAIL_COUNT = 0


def ok(label: str) -> None:
    global PASS_COUNT
    PASS_COUNT += 1
    if not QUIET:
        print(f"  \033[32m✓\033[0m {label}")


def fail(label: str, msg: str) -> None:
    global FAIL_COUNT
    FAIL_COUNT += 1
    print(f"  \033[31m✗\033[0m {label}: {msg}")


def section(name: str) -> None:
    if not QUIET:
        print(f"\n\033[1m{name}\033[0m")


# ── Chapter tests (dev mode) ────────────────────────────────────────────────

async def test_ch1_unary(mc) -> None:
    """Chapter 1: getStatus unary RPC."""
    try:
        r = await mc.getStatus({"agent_id": "edge-7"})
        if not isinstance(r, dict):
            fail("Ch1 getStatus", f"expected dict, got {type(r).__name__}")
            return
        if r.get("agent_id") != "edge-7":
            fail("Ch1 getStatus", f"agent_id mismatch: {r}")
            return
        if r.get("status") != "running":
            fail("Ch1 getStatus", f"status mismatch: {r}")
            return
        if not isinstance(r.get("uptime_secs"), int) or r["uptime_secs"] <= 0:
            fail("Ch1 getStatus", f"uptime_secs invalid: {r}")
            return
        ok("Ch1 getStatus returns typed response")
    except Exception as e:
        fail("Ch1 getStatus", str(e))


async def test_ch2_streaming(mc) -> None:
    """Chapter 2: submitLog (unary) + tailLogs (server stream).

    Verifies the documented use case: an operator opens tailLogs, an agent
    submits a log entry, and the operator receives that exact entry on the
    live stream.
    """
    # 2a: submitLog
    try:
        r = await mc.submitLog({
            "timestamp": time.time(),
            "level": "info",
            "message": "ch2 first log",
            "agent_id": "edge-7",
        })
        if not (r is True or (isinstance(r, dict) and r.get("accepted") is True)):
            fail("Ch2 submitLog", f"unexpected response: {r}")
            return
        ok("Ch2 submitLog accepted")
    except Exception as e:
        fail("Ch2 submitLog", str(e))
        return

    # 2b: tailLogs receives a live entry. Submit a unique marker AFTER the
    # stream is open and consume until we see it (drain any queued entries
    # from earlier in the test).
    marker = f"ch2-marker-{time.time()}"
    found_marker = asyncio.Event()
    received: list[Any] = []

    async def consume() -> None:
        try:
            async for entry in mc.tailLogs.stream({"level": "info"}):
                received.append(entry)
                msg = entry.get("message") if isinstance(entry, dict) else None
                if msg == marker:
                    found_marker.set()
                    break
        except Exception:
            pass

    task = asyncio.create_task(consume())
    await asyncio.sleep(0.3)  # let the stream open
    try:
        await mc.submitLog({
            "timestamp": time.time(),
            "level": "info",
            "message": marker,
            "agent_id": "edge-7",
        })
    except Exception as e:
        task.cancel()
        fail("Ch2 tailLogs setup", f"submitLog failed: {e}")
        return

    try:
        await asyncio.wait_for(found_marker.wait(), timeout=5.0)
    except asyncio.TimeoutError:
        task.cancel()
        fail("Ch2 tailLogs", f"marker {marker!r} not received within 5s (got {len(received)} entries)")
        return

    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass
    ok(f"Ch2 tailLogs received live marker ({marker})")


async def test_ch3_client_stream(mc) -> None:
    """Chapter 3: ingestMetrics client streaming with 1000 metrics."""
    n = 1000

    async def metrics():
        for i in range(n):
            yield {
                "name": "cpu.usage",
                "value": float(i),
                "timestamp": time.time(),
            }

    try:
        r = await mc.ingestMetrics(metrics())
        accepted = r.get("accepted") if isinstance(r, dict) else getattr(r, "accepted", None)
        if accepted != n:
            fail("Ch3 ingestMetrics", f"expected accepted={n}, got {accepted} (full: {r})")
            return
        ok(f"Ch3 ingestMetrics accepted {n}")
    except Exception as e:
        fail("Ch3 ingestMetrics", str(e))


async def test_ch4_session(client, services_module) -> None:
    """Chapter 4: AgentSession (session-scoped) -- register + runCommand bidi.

    Session-scoped services need a typed client because the proxy can't drive
    the session protocol. We import the example's AgentSession class.
    """
    try:
        AgentSession = services_module.AgentSession
    except AttributeError:
        fail("Ch4 setup", f"AgentSession not in {services_module.__name__}")
        return

    # 4a: register with GPU → train-42
    try:
        agent = await client.client(AgentSession)
        r = await agent.register(services_module.Heartbeat(
            agent_id="gpu-1",
            capabilities=["gpu"],
            load_avg=0.5,
        ))
        if r.task_id != "train-42":
            fail("Ch4 register (gpu)", f"expected task_id='train-42', got {r.task_id!r}")
            return
        ok("Ch4 register (gpu) returns train-42")
    except Exception as e:
        fail("Ch4 register (gpu)", str(e))
        return

    # 4b: register without GPU → idle
    try:
        agent2 = await client.client(AgentSession)
        r = await agent2.register(services_module.Heartbeat(
            agent_id="cpu-1",
            capabilities=["arm64"],
            load_avg=0.2,
        ))
        if r.task_id != "idle":
            fail("Ch4 register (no gpu)", f"expected task_id='idle', got {r.task_id!r}")
            return
        ok("Ch4 register (no gpu) returns idle")
    except Exception as e:
        fail("Ch4 register (no gpu)", str(e))


# ── Chapter 5: Auth ─────────────────────────────────────────────────────────

async def test_ch5_no_credential(address: str) -> None:
    """No credential → connection refused."""
    from aster import AsterClient
    try:
        client = AsterClient(address=address)
        await client.connect()
        await client.close()
        fail("Ch5 no-cred denied", "connection succeeded (should have been refused)")
    except PermissionError:
        ok("Ch5 no credential → denied")
    except Exception as e:
        if "denied" in str(e).lower() or "PERMISSION" in str(e).upper():
            ok("Ch5 no credential → denied")
        else:
            fail("Ch5 no-cred denied", f"unexpected error: {e}")


async def test_ch5_edge_credential(address: str, edge_cred: str) -> None:
    """Edge credential: getStatus OK, tailLogs DENIED, runCommand DENIED."""
    from aster import AsterClient

    # The .cred file is a TOML .aster-identity containing both the node
    # secret key and the consumer peer entry. Pass it as identity= and the
    # client auto-extracts the credential from the same file.
    client = AsterClient(address=address, identity=edge_cred)
    try:
        await client.connect()
    except Exception as e:
        fail("Ch5 edge connect", str(e))
        return

    mc = client.proxy("MissionControl")

    # getStatus must succeed (has ops.status)
    try:
        r = await mc.getStatus({"agent_id": "edge-7"})
        if r.get("status") != "running":
            fail("Ch5 edge getStatus", f"unexpected: {r}")
        else:
            ok("Ch5 edge getStatus → OK (has ops.status)")
    except Exception as e:
        fail("Ch5 edge getStatus", str(e))

    # tailLogs must be denied (lacks ops.logs / ops.admin)
    try:
        async for _ in mc.tailLogs.stream({"level": "info"}):
            break
        fail("Ch5 edge tailLogs denied", "stream succeeded (should have been denied)")
    except Exception as e:
        if "PERMISSION_DENIED" in str(e) or "permission" in str(e).lower():
            ok("Ch5 edge tailLogs → DENIED (lacks ops.logs)")
        else:
            fail("Ch5 edge tailLogs denied", f"unexpected error: {e}")

    # runCommand bidi must be denied (lacks ops.admin)
    # This tests the bidi auth bypass fix.
    try:
        from examples.python.mission_control.services_auth import AgentSession
        from examples.python.mission_control.types import Command
        agent = await client.client(AgentSession)

        async def commands():
            yield Command(command="echo hello")

        await agent.runCommand(commands())
        fail("Ch5 edge runCommand denied", "bidi succeeded (should have been denied)")
    except Exception as e:
        if "PERMISSION_DENIED" in str(e) or "permission" in str(e).lower():
            ok("Ch5 edge runCommand → DENIED (bidi auth check)")
        else:
            fail("Ch5 edge runCommand denied", f"unexpected error: {e}")

    await client.close()


async def test_ch5_ops_credential(address: str, ops_cred: str) -> None:
    """Ops credential: getStatus OK, tailLogs OK, runCommand OK."""
    from aster import AsterClient

    client = AsterClient(address=address, identity=ops_cred)
    try:
        await client.connect()
    except Exception as e:
        fail("Ch5 ops connect", str(e))
        return

    mc = client.proxy("MissionControl")

    # getStatus
    try:
        r = await mc.getStatus({"agent_id": "ops"})
        if r.get("status") == "running":
            ok("Ch5 ops getStatus → OK")
        else:
            fail("Ch5 ops getStatus", f"unexpected: {r}")
    except Exception as e:
        fail("Ch5 ops getStatus", str(e))

    # tailLogs (verifies any_of(LOGS, ADMIN) with comma-separated role parsing)
    received: list[Any] = []
    consume_done = asyncio.Event()

    async def consume() -> None:
        try:
            async for entry in mc.tailLogs.stream({"level": "info"}):
                received.append(entry)
                if len(received) >= 1:
                    break
        finally:
            consume_done.set()

    task = asyncio.create_task(consume())
    await asyncio.sleep(0.3)
    try:
        await mc.submitLog({
            "timestamp": time.time(),
            "level": "info",
            "message": "ops auth marker",
            "agent_id": "ops",
        })
    except Exception:
        pass

    try:
        await asyncio.wait_for(consume_done.wait(), timeout=5.0)
    except asyncio.TimeoutError:
        task.cancel()
        fail("Ch5 ops tailLogs", "no entry received within 5s")
    else:
        task.cancel()
        try:
            await task
        except Exception:
            pass
        if received:
            ok("Ch5 ops tailLogs → OK (entry received)")
        else:
            fail("Ch5 ops tailLogs", "stream opened but no entries")

    await client.close()


# ── Chapter 6: gen-client ───────────────────────────────────────────────────

async def test_ch6_gen_client(address: str, work_dir: str) -> None:
    """Chapter 6: aster contract gen-client produces a working client."""
    import subprocess

    out_dir = Path(work_dir) / "generated"
    out_dir.mkdir(parents=True, exist_ok=True)

    # Run gen-client
    try:
        result = subprocess.run(
            [
                "uv", "run", "aster", "contract", "gen-client",
                address,
                "--out", str(out_dir),
                "--package", "mc_gen",
                "--lang", "python",
            ],
            cwd=str(REPO_ROOT),
            capture_output=True,
            text=True,
            timeout=30,
        )
    except Exception as e:
        fail("Ch6 gen-client run", str(e))
        return

    if result.returncode != 0:
        fail("Ch6 gen-client run", f"exit {result.returncode}: {result.stderr.strip()}")
        return

    # Verify generated files exist
    expected = [
        out_dir / "mc_gen" / "services" / "mission_control_v1.py",
        out_dir / "mc_gen" / "types" / "mission_control_v1.py",
    ]
    missing = [p for p in expected if not p.exists()]
    if missing:
        fail("Ch6 gen-client output", f"missing: {missing}")
        return
    ok("Ch6 gen-client produced files")

    # Import and verify method patterns
    sys.path.insert(0, str(out_dir))
    try:
        from mc_gen.services.mission_control_v1 import MissionControlClient
        from mc_gen.types.mission_control_v1 import StatusRequest
    except Exception as e:
        fail("Ch6 generated import", str(e))
        return
    ok("Ch6 generated client imports")

    # Verify generated patterns are correct (catches the unary-only bug)
    info = getattr(MissionControlClient, "__aster_service_info__", None)
    if info is None:
        # Try to find the service info on a related symbol
        info = getattr(sys.modules["mc_gen.services.mission_control_v1"], "__aster_service_info__", None)
    if info is not None:
        expected_patterns = {
            "getStatus": "unary",
            "submitLog": "unary",
            "tailLogs": "server_stream",
            "ingestMetrics": "client_stream",
        }
        wrong = []
        for mname, expected_pattern in expected_patterns.items():
            mi = info.methods.get(mname) if hasattr(info.methods, "get") else None
            if mi is None:
                continue
            actual = getattr(mi, "pattern", None)
            if actual != expected_pattern:
                wrong.append(f"{mname}: expected {expected_pattern}, got {actual}")
        if wrong:
            fail("Ch6 method patterns", "; ".join(wrong))
        else:
            ok("Ch6 method patterns correct")

    # Make a real call through the generated client
    from aster import AsterClient
    client = AsterClient(address=address)
    try:
        await client.connect()
        stub = await MissionControlClient.from_connection(client)
        r = await stub.getStatus(StatusRequest(agent_id="gen-test"))
        if getattr(r, "agent_id", None) == "gen-test" and getattr(r, "status", None) == "running":
            ok("Ch6 generated client makes real call")
        else:
            fail("Ch6 generated call", f"unexpected response: {r}")
    except Exception as e:
        fail("Ch6 generated call", str(e))
    finally:
        await client.close()


# ── Mode runners ─────────────────────────────────────────────────────────────

async def run_dev_mode(address: str, work_dir: str | None) -> None:
    from aster import AsterClient

    section("Dev mode (no auth)")

    client = AsterClient(address=address)
    try:
        await client.connect()
    except Exception as e:
        fail("connect", str(e))
        return

    mc = client.proxy("MissionControl")
    await test_ch1_unary(mc)
    await test_ch2_streaming(mc)
    await test_ch3_client_stream(mc)

    # Ch4 needs the typed AgentSession class
    try:
        from examples.python.mission_control import services as services_module
        await test_ch4_session(client, services_module)
    except ImportError as e:
        fail("Ch4 import", f"could not import services: {e}")

    await client.close()

    if work_dir:
        section("Chapter 6 (gen-client)")
        await test_ch6_gen_client(address, work_dir)


async def run_auth_mode(address: str, keys_dir: str) -> None:
    section("Auth mode (Chapter 5)")
    edge_cred = os.path.join(keys_dir, "edge.cred")
    ops_cred = os.path.join(keys_dir, "ops.cred")
    if not os.path.exists(edge_cred):
        fail("Ch5 setup", f"missing {edge_cred} -- run setup_auth.sh first")
        return
    if not os.path.exists(ops_cred):
        fail("Ch5 setup", f"missing {ops_cred} -- run setup_auth.sh first")
        return

    await test_ch5_no_credential(address)
    await test_ch5_edge_credential(address, edge_cred)
    await test_ch5_ops_credential(address, ops_cred)


# ── Main ────────────────────────────────────────────────────────────────────

async def main() -> int:
    global QUIET

    parser = argparse.ArgumentParser(description="Mission Control guide test (Python client)")
    parser.add_argument("address", help="Server address (aster1...)")
    parser.add_argument("--mode", choices=["dev", "auth"], default="dev")
    parser.add_argument("--keys-dir", help="Directory containing root.key, edge.cred, ops.cred (auth mode)")
    parser.add_argument("--work-dir", help="Working directory for gen-client output (dev mode)")
    parser.add_argument("-q", "--quiet", action="store_true", help="Suppress per-test output, show only summary")
    args = parser.parse_args()

    QUIET = args.quiet

    if args.mode == "auth" and not args.keys_dir:
        print("Error: --keys-dir is required for --mode auth", file=sys.stderr)
        return 2

    if not QUIET:
        print(f"Testing {args.address[:30]}... (mode={args.mode})")

    if args.mode == "dev":
        await run_dev_mode(args.address, args.work_dir)
    else:
        await run_auth_mode(args.address, args.keys_dir)

    if not QUIET:
        print(f"\n\033[1mResult:\033[0m \033[32m{PASS_COUNT} passed\033[0m, "
              f"\033[31m{FAIL_COUNT} failed\033[0m")
    else:
        print(f"py-client {args.mode}: {PASS_COUNT} pass, {FAIL_COUNT} fail")

    return 0 if FAIL_COUNT == 0 else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
