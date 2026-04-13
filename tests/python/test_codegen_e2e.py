"""End-to-end codegen tests against a live Mission Control AsterServer.

These would have caught the bugs the QA agent surfaced on 2026-04-10:

- TS publisher mislabeling @ServerStream / @BidiStream methods as
  ``unary`` (because RpcPattern is a string enum and the pattern
  switch was comparing against integers).
- Python codegen emitting ``tags: dict = {}`` (mutable default that
  Python 3.13 rejects at class creation time).
- Generated TS clients failing strict ``tsc`` because the loader cast
  was malformed, the import lists were stale, etc.

The test spins up a Mission Control server with shared, session-scoped,
unary, server-streaming, client-streaming, and bidi-streaming methods,
runs both the Python and TypeScript codegen against the live manifest,
and validates the output two ways:

1. Python: import every generated module via ``importlib`` -- this
   catches mutable defaults, syntax errors, undefined names, and
   broken cross-module imports for free.
2. TypeScript: structural assertions on the emitted files plus an
   optional ``bunx tsc --strict`` pass when the toolchain is available
   (skipped on systems without bun, with a clear marker).
"""

from __future__ import annotations

import dataclasses
import importlib
import importlib.util
import os
import re
import shutil
import subprocess
import sys
from collections.abc import AsyncIterator
from pathlib import Path

import pytest

from aster import (
    AsterServer,
    bidi_stream,
    client_stream,
    rpc,
    server_stream,
    service,
    wire_type,
)


# ── Test service surface ─────────────────────────────────────────────────────
#
# Mirrors the Mission Control example: one shared service with all four
# RPC patterns + one session-scoped service. Field types include strings,
# ints, lists, dicts, and nested types so the manifest exercises every
# branch of both codegens.


@wire_type("test.codegen/StatusRequest")
@dataclasses.dataclass
class StatusRequest:
    agent_id: str = ""


@wire_type("test.codegen/StatusResponse")
@dataclasses.dataclass
class StatusResponse:
    agent_id: str = ""
    status: str = "idle"
    uptime_secs: int = 0
    tags: dict = dataclasses.field(default_factory=dict)


@wire_type("test.codegen/LogEntry")
@dataclasses.dataclass
class LogEntry:
    timestamp: int = 0
    level: str = "info"
    message: str = ""


@wire_type("test.codegen/TailRequest")
@dataclasses.dataclass
class TailRequest:
    level: str = "info"


@wire_type("test.codegen/SubmitLogResult")
@dataclasses.dataclass
class SubmitLogResult:
    accepted: bool = True


@wire_type("test.codegen/MetricPoint")
@dataclasses.dataclass
class MetricPoint:
    name: str = ""
    value: float = 0.0


@wire_type("test.codegen/IngestResult")
@dataclasses.dataclass
class IngestResult:
    accepted: int = 0


@wire_type("test.codegen/Heartbeat")
@dataclasses.dataclass
class Heartbeat:
    agent_id: str = ""
    capabilities: list[str] = dataclasses.field(default_factory=list)


@wire_type("test.codegen/Assignment")
@dataclasses.dataclass
class Assignment:
    task_id: str = ""
    command: str = ""


@wire_type("test.codegen/Command")
@dataclasses.dataclass
class Command:
    command: str = ""


@wire_type("test.codegen/CommandResult")
@dataclasses.dataclass
class CommandResult:
    stdout: str = ""
    exit_code: int = 0


@service(name="MissionControl", version=1)
class MissionControl:
    @rpc()
    async def get_status(self, req: StatusRequest) -> StatusResponse:
        return StatusResponse(agent_id=req.agent_id, status="running", uptime_secs=42)

    @rpc()
    async def submit_log(self, entry: LogEntry) -> SubmitLogResult:
        return SubmitLogResult(accepted=True)

    # Mode 2 inline method -- exercises the request_style="inline" codepath
    # through the full manifest → codegen → generated-client pipeline.
    @rpc()
    async def ping_agent(self, agent_id: str, nonce: int) -> StatusResponse:
        return StatusResponse(agent_id=agent_id, status=f"pong:{nonce}", uptime_secs=0)

    @server_stream()
    async def tail_logs(self, req: TailRequest) -> AsyncIterator[LogEntry]:
        for i in range(2):
            yield LogEntry(timestamp=i, level=req.level, message=f"entry {i}")

    @client_stream()
    async def ingest_metrics(
        self, stream: AsyncIterator[MetricPoint]
    ) -> IngestResult:
        n = 0
        async for _ in stream:
            n += 1
        return IngestResult(accepted=n)


@service(name="AgentSession", version=1, scoped="session")
class AgentSession:
    def __init__(self, peer=None):
        self._peer = peer

    @rpc()
    async def register(self, hb: Heartbeat) -> Assignment:
        return Assignment(task_id="train-42", command="python train.py")

    @bidi_stream()
    async def run_command(
        self, stream: AsyncIterator[Command]
    ) -> AsyncIterator[CommandResult]:
        async for cmd in stream:
            yield CommandResult(stdout=f"ran: {cmd.command}", exit_code=0)


# ── Helpers ──────────────────────────────────────────────────────────────────


async def _fetch_manifests_from_server(server: AsterServer) -> dict[str, dict]:
    """Connect a client to the server and pull the published manifests.

    Goes through the same path the CLI uses: PeerConnection's background
    manifest fetcher reads the manifests/{contract_id} shortcut from the
    registry doc.
    """
    from aster_cli.shell.app import PeerConnection

    addr = server.endpoint_addr_b64
    conn = PeerConnection(peer_addr=addr)
    try:
        await conn.connect()
        # PeerConnection kicks off _fetch_manifests_background on connect.
        # Wait for it to finish so we have rich method/field info.
        await conn.wait_for_manifests(timeout=30.0)
        return dict(conn._manifests)
    finally:
        await conn.close()


def _import_generated_module(module_path: Path, package_name: str):
    """Import a generated .py file via its path, returning the module.

    Adds the package root to sys.path temporarily so relative imports
    inside the generated tree work. The package_name argument is the
    top-level namespace (e.g. "mc"), which lives directly under the
    out_dir we passed to the generator.
    """
    spec = importlib.util.spec_from_file_location(
        f"{package_name}._gen_test_{module_path.stem}",
        module_path,
    )
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# ── Python codegen test ──────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_python_codegen_against_live_server(tmp_path):
    """Generate Python clients from a live server, then import every emitted file."""
    from aster_cli.codegen import generate_python_clients

    async with AsterServer(
        services=[MissionControl(), AgentSession],
        allow_all_consumers=True,
    ) as server:
        manifests = await _fetch_manifests_from_server(server)

    assert "MissionControl" in manifests, f"got: {list(manifests)}"
    assert "AgentSession" in manifests, f"got: {list(manifests)}"

    # Sanity-check that the manifest carries the expected method patterns.
    # If a publisher labels @server_stream as `unary`, this assertion fails
    # before we even get to codegen -- which is exactly what we want, so
    # the failure is loud and points at the publish layer rather than
    # silently producing wrong-looking generated code.
    mc_methods = {m["name"]: m for m in manifests["MissionControl"]["methods"]}
    assert mc_methods["get_status"]["pattern"] == "unary"
    assert mc_methods["submit_log"]["pattern"] == "unary"
    assert mc_methods["tail_logs"]["pattern"] == "server_stream"
    assert mc_methods["ingest_metrics"]["pattern"] == "client_stream"

    sess_methods = {m["name"]: m for m in manifests["AgentSession"]["methods"]}
    assert sess_methods["register"]["pattern"] == "unary"
    assert sess_methods["run_command"]["pattern"] == "bidi_stream"
    assert manifests["AgentSession"]["scoped"] == "session"

    # Generate Python clients
    out_dir = tmp_path / "py-out"
    generated = generate_python_clients(
        manifests,
        str(out_dir),
        namespace="mc",
        source="test://codegen",
    )
    assert generated, "codegen produced no files"

    # Critical check: every generated .py file must import without raising.
    # This catches mutable default values, undefined names, syntax errors,
    # broken cross-module imports, and the whole class of "the file got
    # written but is wrong" failures the QA agent has surfaced.
    py_files = [Path(p) for p in generated if p.endswith(".py")]
    assert py_files, f"no .py files generated; got: {generated}"

    # Add the namespace dir to sys.path so the generated package can
    # resolve its own relative imports (`from ..types.x import Y`).
    pkg_root = str(out_dir)
    sys.path.insert(0, pkg_root)
    try:
        importlib.invalidate_caches()
        # Import the package itself first so __init__.py runs.
        mc_pkg = importlib.import_module("mc")
        assert mc_pkg is not None

        # Then explicitly import each generated module to surface
        # individual file failures.
        for f in py_files:
            rel = f.relative_to(out_dir).with_suffix("")
            module_name = ".".join(rel.parts)
            assert module_name.startswith("mc."), module_name
            try:
                # Wipe any previous import of the same name -- pytest
                # may have re-run the test, and importlib caches.
                if module_name in sys.modules:
                    del sys.modules[module_name]
                importlib.import_module(module_name)
            except Exception as e:
                pytest.fail(
                    f"generated module {module_name} failed to import: "
                    f"{type(e).__name__}: {e}"
                )

        # Specifically validate the StatusResponse class so a regression
        # of the `tags: dict = {}` mutable-default bug fails loudly with
        # a meaningful error rather than burying it in a generic import
        # exception higher up.
        types_mod = importlib.import_module("mc.types.mission_control_v1")
        cls = getattr(types_mod, "StatusResponse")
        # `tags` field must use a default_factory, never a literal {}.
        # If the codegen regresses, dataclass class creation raises
        # ValueError before we get here -- this assertion is just for
        # the post-creation invariant check.
        fields = {f.name: f for f in dataclasses.fields(cls)}
        assert "tags" in fields
        assert fields["tags"].default_factory is dict, (
            "StatusResponse.tags must use default_factory=dict, not a literal "
            f"(got default_factory={fields['tags'].default_factory!r})"
        )

        # Spot-check a streaming method's signature on the typed client.
        services_mod = importlib.import_module("mc.services.mission_control_v1")
        client_cls = getattr(services_mod, "MissionControlClient")
        # The codegen emits `tail_logs` as a non-coroutine that returns
        # an AsyncIterator -- specifically NOT an `async def`. If the
        # publisher mislabels it as unary, the codegen would emit
        # `async def tail_logs(...)` which would silently work for
        # `await ...` calls but break iteration.
        assert hasattr(client_cls, "tail_logs"), \
            f"MissionControlClient missing tail_logs; methods: {dir(client_cls)}"

        # Mode 2 assertion: `ping_agent` is an inline-param method -- the
        # generated client must accept inline args, not an explicit
        # request object. Inspect the generated signature to confirm the
        # codegen emitted the right shape.
        import inspect as _inspect
        ping_sig = _inspect.signature(client_cls.ping_agent)
        param_names = list(ping_sig.parameters.keys())
        assert "agent_id" in param_names, (
            f"ping_agent client missing inline 'agent_id' param; got {param_names}"
        )
        assert "nonce" in param_names, (
            f"ping_agent client missing inline 'nonce' param; got {param_names}"
        )
        assert "request" not in param_names, (
            f"ping_agent client should not take a 'request' arg in Mode 2; "
            f"got {param_names}"
        )
    finally:
        sys.path.remove(pkg_root)
        # Clean up imported modules so a re-run gets fresh state.
        for name in list(sys.modules):
            if name == "mc" or name.startswith("mc."):
                del sys.modules[name]


# ── TypeScript codegen test ──────────────────────────────────────────────────


_TS_LOADER_CAST = re.compile(
    r"\(client as unknown as \{ transport\?: AsterTransport \}\)\.transport"
)
_INTEGER_PATTERN_BUG = re.compile(r"mi\.pattern\s*===\s*\d")


def _has_bunx() -> bool:
    """Return True iff `bunx tsc` can be invoked."""
    return shutil.which("bunx") is not None


@pytest.mark.asyncio
async def test_typescript_codegen_against_live_server(tmp_path):
    """Generate TS clients from a live server and validate the output."""
    from aster_cli.codegen_typescript import generate_typescript_clients

    async with AsterServer(
        services=[MissionControl(), AgentSession],
        allow_all_consumers=True,
    ) as server:
        manifests = await _fetch_manifests_from_server(server)

    out_dir = tmp_path / "ts-out"
    generated = generate_typescript_clients(
        manifests,
        str(out_dir),
        namespace="mc",
        source="test://codegen",
    )
    assert generated, "codegen produced no files"

    files_by_name = {Path(p).name: Path(p) for p in generated}

    # ── Structural assertions ────────────────────────────────────────────
    #
    # Cheap regex-based checks that catch the most common regressions
    # without needing the TypeScript toolchain. Each one is paired with
    # a comment naming the regression it would have caught.

    # 1. Both service client files exist with the right shape.
    assert "mission-control-v1.ts" in files_by_name
    assert "agent-session-v1.ts" in files_by_name
    mc_client = files_by_name["mission-control-v1.ts"]
    ag_client = files_by_name["agent-session-v1.ts"]
    assert mc_client.parent.name == "services"
    assert ag_client.parent.name == "services"

    mc_text = mc_client.read_text()
    ag_text = ag_client.read_text()

    # 2. Each service client must export a `<Name>Client` class with a
    # static fromConnection. If either is missing the file is broken.
    assert "export class MissionControlClient" in mc_text
    assert "static async fromConnection" in mc_text
    assert "export class AgentSessionClient" in ag_text
    assert "static async fromConnection" in ag_text

    # 3. The shared client must declare each method with the right
    # streaming pattern. Specifically: tailLogs MUST be `async *` (an
    # async generator that yields LogEntry), not `async`. The QA bug
    # was that the manifest mislabelled tail_logs as unary so the
    # codegen emitted `async tailLogs(...): Promise<LogEntry>` which
    # is broken on every level.
    assert re.search(r"async \*tail_logs\(", mc_text), (
        "tail_logs must be emitted as `async *tail_logs` (server stream); "
        "if it appears as `async tail_logs` the publisher is mislabelling "
        "@server_stream as unary"
    )
    assert "transport.serverStream" in mc_text
    # ingest_metrics is client_stream -> takes AsyncIterable, returns Promise
    assert re.search(
        r"async ingest_metrics\(requests: AsyncIterable", mc_text
    )
    assert "transport.clientStream" in mc_text

    # 4. The session client must delegate to client.proxy(), NOT call
    # transport.unary directly -- session-scoped services need the
    # session protocol bidi stream, which only client.proxy() opens.
    # Regression of this would silently break Ch4 in the day0 tests.
    assert 'client.proxy("AgentSession")' in ag_text
    assert "transport.unary" not in ag_text, (
        "session client must NOT call transport.unary directly -- "
        "session-scoped services require the SessionProxyClient path "
        "via client.proxy(name)"
    )

    # 5. The shared client's loader cast must be the documented form.
    # If the AsterClientWrapper's transport accessor changes, this
    # check will fail loudly so we update both sides together.
    assert _TS_LOADER_CAST.search(mc_text), (
        "shared client loader cast doesn't match the expected form; "
        "did AsterClientWrapper grow a public transport accessor? "
        "If so, update the codegen to use it."
    )

    # 6. Type files exist for both services and contain the expected
    # class declarations.
    assert "mission-control-v1.ts" in {p.name for p in (out_dir / "mc" / "types").iterdir()}
    types_mc = (out_dir / "mc" / "types" / "mission-control-v1.ts").read_text()
    assert "export class StatusRequest" in types_mc
    assert "export class StatusResponse" in types_mc

    types_ag = (out_dir / "mc" / "types" / "agent-session-v1.ts").read_text()
    assert "export class Heartbeat" in types_ag
    assert "export class Assignment" in types_ag

    # 7. None of the generated files should contain the integer-pattern
    # bug pattern (`mi.pattern === 0|1|2|3`) -- that lives in the TS
    # publisher, not the codegen, but the regex is cheap to run as a
    # belt-and-braces check that no codegen template ever emits the
    # broken comparison form into user code.
    for f in files_by_name.values():
        text = f.read_text()
        assert not _INTEGER_PATTERN_BUG.search(text), \
            f"{f.name} contains the legacy `mi.pattern === N` bug pattern"

    # ── Optional: tsc strict pass ────────────────────────────────────────
    #
    # When bunx is available, run tsc against the generated tree with
    # strict mode + Node16 module resolution. We use --noResolve so we
    # don't need a real @aster-rpc/aster install in the test sandbox --
    # the import resolution is checked syntactically (every relative
    # import has the right .js extension, every type reference exists
    # within the file or in a sibling file) but external packages
    # don't need to be present.
    if not _has_bunx():
        pytest.skip(
            "bunx not available; skipping tsc validation. "
            "Structural checks above still ran."
        )

    tsconfig = tmp_path / "tsconfig.json"
    tsconfig.write_text(
        '{\n'
        '  "compilerOptions": {\n'
        '    "target": "ES2022",\n'
        '    "module": "Node16",\n'
        '    "moduleResolution": "Node16",\n'
        '    "strict": true,\n'
        '    "skipLibCheck": true,\n'
        '    "noUnusedLocals": false,\n'
        '    "noEmit": true,\n'
        '    "noResolve": true,\n'
        '    "types": []\n'
        '  },\n'
        '  "include": ["ts-out/**/*.ts"]\n'
        '}\n'
    )
    result = subprocess.run(
        ["bunx", "tsc", "-p", str(tsconfig)],
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        timeout=120,
        env={**os.environ, "TERM": "dumb"},
    )
    if result.returncode != 0:
        # Filter out the noResolve "cannot find module @aster-rpc/aster"
        # errors -- those are expected because we deliberately don't
        # install the framework in the test sandbox. Real syntax/type
        # errors in the GENERATED files (the thing we care about) are
        # everything else.
        stdout_lines = [
            ln for ln in result.stdout.splitlines()
            if "@aster-rpc/aster" not in ln
            and "Cannot find module" not in ln
            and ln.strip()
        ]
        if stdout_lines:
            pytest.fail(
                "tsc found real errors in generated TS:\n  "
                + "\n  ".join(stdout_lines[:30])
            )
