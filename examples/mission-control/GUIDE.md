# Mission Control — Build a P2P Ops Platform with Aster

> You'll go from zero to a working peer-to-peer operations platform in 30 minutes.
> No servers to provision. No cloud accounts. No DNS. Just your code and a
> cryptographic identity.

---

## What we're building

**Mission Control** — a lightweight control plane for managing remote agents.
An agent could be a CI runner, an IoT sensor, an AI worker, or a service
running on your colleague's laptop across the world.

By the end of this guide you'll have:
- A control plane that agents connect to over encrypted QUIC
- Live log streaming from remote agents
- Metric ingestion at thousands of points per second
- An interactive remote shell — bidirectional, real-time
- Per-agent sessions with heartbeat and capability tracking
- Role-based access: operators deploy, agents report, viewers watch
- Cross-language interop: Python agents talking to TypeScript control planes

Everything runs peer-to-peer. The only infrastructure is a relay server
(which can be self-hosted) for NAT traversal. Once peers discover each other,
traffic flows directly.

---

## Chapter 1: Your First Agent Check-In (5 min)

**Goal:** Define a service, start it, call it from the CLI.

```python
# control.py
from dataclasses import dataclass
from aster import AsterServer, service, rpc, wire_type

@wire_type("mission/StatusRequest")
@dataclass
class StatusRequest:
    agent_id: str = ""

@wire_type("mission/StatusResponse")
@dataclass
class StatusResponse:
    agent_id: str = ""
    status: str = "idle"
    uptime_secs: int = 0

@service(name="MissionControl", version=1)
class MissionControl:
    @rpc()
    async def getStatus(self, req: StatusRequest) -> StatusResponse:
        return StatusResponse(
            agent_id=req.agent_id,
            status="running",
            uptime_secs=3600,
        )

async def main():
    async with AsterServer(services=[MissionControl()]) as srv:
        print(srv.ticket)       # compact aster1... address
        await srv.serve()

if __name__ == "__main__":
    import asyncio
    asyncio.run(main())
```

```bash
# Terminal 1: start the control plane
python control.py

# Terminal 2: connect and inspect
aster shell aster1Qm...
> cd services/MissionControl
> ./getStatus agent_id="edge-node-7"
```

**What just happened:**
- `@service` + `@rpc` defined a typed RPC contract
- `@wire_type` made the types serializable across languages
- `AsterServer` created an encrypted QUIC endpoint, published the contract
  to a content-addressed registry, and started listening
- `aster shell` connected, discovered the service, and invoked it — with
  tab completion and typed responses

No YAML. No protobuf compilation. No port numbers. One file.

---

## Chapter 2: Live Log Streaming (5 min)

**Goal:** Add a server-streaming method — the agent pushes logs, the
control plane streams them to operators.

```python
@wire_type("mission/LogEntry")
@dataclass
class LogEntry:
    timestamp: float = 0.0
    level: str = "info"
    message: str = ""
    agent_id: str = ""

@wire_type("mission/TailRequest")
@dataclass
class TailRequest:
    agent_id: str = ""
    level: str = "info"    # minimum level filter

@service(name="MissionControl", version=1)
class MissionControl:
    # ... getStatus from Chapter 1 ...

    @server_stream()
    async def tailLogs(self, req: TailRequest):
        """Stream log entries as they arrive."""
        while True:
            entry = await self._log_queue.get()
            if req.agent_id and entry.agent_id != req.agent_id:
                continue
            if _level_rank(entry.level) < _level_rank(req.level):
                continue
            yield entry
```

```bash
# In the shell:
> ./tailLogs agent_id="edge-node-7" level="warn"
#0 {"timestamp": 1712567890.1, "level": "warn", "message": "disk 92% full", ...}
#1 {"timestamp": 1712567891.3, "level": "error", "message": "health check failed", ...}
# Ctrl+C to stop
```

**What just happened:**
- `@server_stream` turns an async generator into a streaming RPC
- The client receives items as they're yielded — no polling, no websockets
- Under the hood: a single QUIC stream, with Aster framing, flowing until
  either side closes it

---

## Chapter 3: Metric Ingestion (5 min)

**Goal:** Agents push thousands of metric datapoints per second using
client streaming.

```python
@wire_type("mission/MetricPoint")
@dataclass
class MetricPoint:
    name: str = ""
    value: float = 0.0
    timestamp: float = 0.0
    tags: dict = field(default_factory=dict)

@wire_type("mission/IngestResult")
@dataclass
class IngestResult:
    accepted: int = 0
    dropped: int = 0

@service(name="MissionControl", version=1)
class MissionControl:
    # ... previous methods ...

    @client_stream()
    async def ingestMetrics(self, stream) -> IngestResult:
        """Receive a stream of metric points from an agent."""
        accepted = 0
        async for point in stream:
            self._store_metric(point)
            accepted += 1
        return IngestResult(accepted=accepted)
```

On the agent side, we'll start with a **proxy client** — quick to set up,
no types needed on the consumer side:

```python
# agent.py — proxy client (good for prototyping)
from aster import AsterClient

async with AsterClient(endpoint_addr="aster1Qm...") as client:
    mc = client.proxy("MissionControl")
    
    # Stream 10,000 metrics — the proxy accepts dicts
    async def metrics():
        for i in range(10_000):
            yield {"name": "cpu.usage", "value": random(), "timestamp": time()}
    
    result = await mc.ingestMetrics(metrics())
    print(f"Accepted: {result['accepted']}")
```

The proxy client discovers methods from the contract and sends dicts over
the wire. Great for scripting and prototyping. Later (Chapter 7) we'll
switch to a **typed client** for production safety.

**What just happened:**
- Client streaming sends many messages, gets one response at the end
- The producer processes items as they arrive — no buffering the entire batch
- The proxy client requires no type imports — it reads the contract from
  the producer and builds method stubs dynamically
- This is how you'd build telemetry ingestion, log shipping, or bulk data upload

---

## Chapter 4: Remote Shell (5 min)

**Goal:** Bidirectional streaming — send commands, receive output in real time.

```python
@wire_type("mission/ShellInput")
@dataclass
class ShellInput:
    command: str = ""

@wire_type("mission/ShellOutput")
@dataclass
class ShellOutput:
    stdout: str = ""
    stderr: str = ""
    exit_code: int = -1    # -1 means still running

@service(name="MissionControl", version=1)
class MissionControl:
    # ... previous methods ...

    @bidi_stream()
    async def remoteShell(self, commands):
        """Interactive command execution — send commands, receive output."""
        async for cmd in commands:
            proc = await asyncio.create_subprocess_shell(
                cmd.command,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate()
            yield ShellOutput(
                stdout=stdout.decode(),
                stderr=stderr.decode(),
                exit_code=proc.returncode,
            )
```

```bash
# In the shell:
> ./remoteShell
bidi> command="df -h"
← {"stdout": "Filesystem  Size  Used ...", "exit_code": 0}
bidi> command="uptime"
← {"stdout": " 14:32  up 3 days ...", "exit_code": 0}
> end
```

**What just happened:**
- Bidi streaming: both sides send and receive concurrently on one stream
- Each command flows to the producer, output flows back — all multiplexed
- This pattern works for interactive sessions, collaborative editing,
  game state sync, or any real-time bidirectional protocol

---

## Chapter 5: Agent Sessions (5 min)

**Goal:** Per-connection state — track which agents are online, their
capabilities, and heartbeat status.

```python
@wire_type("mission/Heartbeat")
@dataclass
class Heartbeat:
    agent_id: str = ""
    capabilities: list = field(default_factory=list)   # ["gpu", "arm64", ...]
    load_avg: float = 0.0

@wire_type("mission/Assignment")
@dataclass
class Assignment:
    task_id: str = ""
    command: str = ""

@service(name="AgentSession", version=1, scoped="session")
class AgentSession:
    """Session-scoped: one instance per connected agent."""

    def __init__(self):
        self._agent_id = ""
        self._capabilities = []

    @rpc()
    async def register(self, hb: Heartbeat) -> Assignment:
        """Agent announces itself and gets an assignment."""
        self._agent_id = hb.agent_id
        self._capabilities = hb.capabilities
        # Assign work based on capabilities
        if "gpu" in hb.capabilities:
            return Assignment(task_id="train-42", command="python train.py")
        return Assignment(task_id="idle", command="sleep 60")

    @rpc()
    async def heartbeat(self, hb: Heartbeat) -> Assignment:
        """Periodic check-in — update load, maybe get new work."""
        self._capabilities = hb.capabilities
        return Assignment(task_id="continue", command="")
```

**What just happened:**
- `scoped="session"` creates a fresh `AgentSession` instance per connection
- State like `self._agent_id` is private to that agent's session
- When the agent disconnects, the session is cleaned up automatically
- Compare with `MissionControl` (shared) — one instance, all clients see
  the same state

---

## Chapter 6: Auth & Capabilities (5 min)

**Goal:** Not every caller should be able to deploy or open a remote shell.
Define roles, compose requirements, and issue scoped credentials.

First, define your roles as an enum — this keeps capability strings
consistent and discoverable:

```python
from enum import Enum
from aster import any_of, all_of

class Role(str, Enum):
    """Capabilities that can be granted to consumers."""
    STATUS = "ops.status"      # read service status
    LOGS   = "ops.logs"        # tail live logs
    DEPLOY = "ops.deploy"      # trigger deployments
    ADMIN  = "ops.admin"       # remote shell, config changes
    INGEST = "ops.ingest"      # push metrics (agents)
```

Now apply requirements to methods. Simple cases take a single role;
complex cases compose with `any_of` / `all_of`:

```python
@service(name="MissionControl", version=1)
class MissionControl:

    @rpc(requires=Role.STATUS)
    async def getStatus(self, req: StatusRequest) -> StatusResponse: ...

    @rpc(requires=all_of(Role.DEPLOY, Role.STATUS))
    async def deploy(self, req: DeployRequest) -> DeployResponse:
        """Deploy requires BOTH deploy permission AND status access
        (because deploy reads current state before acting)."""
        ...

    @server_stream(requires=any_of(Role.LOGS, Role.ADMIN))
    async def tailLogs(self, req: TailRequest):
        """Log access for log viewers OR admins — either role works."""
        ...

    @client_stream(requires=Role.INGEST)
    async def ingestMetrics(self, stream) -> IngestResult:
        """Agents push metrics — scoped to the ingest role."""
        ...

    @bidi_stream(requires=Role.ADMIN)
    async def remoteShell(self, commands):
        """Remote shell is admin-only."""
        ...
```

Running with auth enabled:

```python
config = AsterConfig(
    root_pubkey_file="~/.aster/root.key",
    allow_all_consumers=False,   # require credentials
)
async with AsterServer(services=[MissionControl()], config=config) as srv:
    await srv.serve()
```

Enrolling an agent with specific roles:

```bash
# Operator (has root key) issues a credential:
aster enroll consumer --name "edge-node-7" \
    --capabilities ops.status,ops.logs,ops.ingest \
    --output edge-node-7.cred

# Give edge-node-7.cred to the agent. It can now:
#   ✓ getStatus     (has ops.status)
#   ✓ tailLogs      (has ops.logs — satisfies any_of)
#   ✓ ingestMetrics (has ops.ingest)
#   ✗ deploy        (missing ops.deploy — fails all_of)
#   ✗ remoteShell   (missing ops.admin)
```

**What just happened:**
- Roles are just strings — the enum keeps them organized and typo-free
- `requires=Role.ADMIN` — caller must have this capability
- `all_of(A, B)` — caller must have BOTH capabilities
- `any_of(A, B)` — caller must have at LEAST ONE
- The root key holder issues credentials scoped to specific roles
- Aster checks at the method level — no auth middleware to write
- Composition lets you express real authorization policies:
  "deploy needs both deploy AND status" is a single line

---

## Chapter 7: Typed Client (5 min)

**Goal:** Graduate from the proxy client to a typed client for production.

In Chapters 2-4 we used `client.proxy("MissionControl")` — dicts in,
dicts out. That's great for prototyping, but for production you want
type safety, IDE autocomplete, and compile-time checks.

```python
# typed_agent.py — import the same types the producer uses
from types import StatusRequest, StatusResponse, MetricPoint, IngestResult
from aster import AsterClient, create_client

async with AsterClient(endpoint_addr="aster1Qm...") as client:
    # Typed client — methods have full type signatures
    mc = create_client(MissionControl, client.transport("MissionControl"))
    
    # IDE knows: getStatus(StatusRequest) -> StatusResponse
    status = await mc.getStatus(StatusRequest(agent_id="edge-7"))
    print(status.uptime_secs)   # autocomplete works
    
    # IDE knows: ingestMetrics(AsyncIterable[MetricPoint]) -> IngestResult
    result = await mc.ingestMetrics(metric_stream())
    print(result.accepted)      # typed, not result['accepted']
```

**What just happened:**
- `create_client(ServiceClass, transport)` builds a typed proxy
- Same wire protocol, same contract — just with type information
- Your IDE catches `mc.getStatu()` (typo) at edit time, not runtime
- The proxy client and typed client are interchangeable — same wire format

**When to use which:**
- **Proxy client** — scripts, CLI tools, exploratory work, cross-language
  consumers that don't share types
- **Typed client** — production services, same-language consumers, anything
  where you want compile-time safety


---

## Chapter 8: Cross-Language — TypeScript Agent (5 min)

**Goal:** Write an agent in TypeScript that connects to the Python
control plane.

```typescript
// agent.ts
import { AsterClient, Service, Rpc, WireType } from '@aster-rpc/aster';

@WireType("mission/Heartbeat")
class Heartbeat {
  agentId = "";
  capabilities: string[] = [];
  loadAvg = 0;
}

@WireType("mission/MetricPoint")
class MetricPoint {
  name = "";
  value = 0;
  timestamp = 0;
  tags: Record<string, string> = {};
}

const client = new AsterClient({ endpoint: "aster1Qm..." });
await client.connect();

// Same contract, different language — proxy client
const session = client.proxy("AgentSession");
const assignment = await session.register(
  new Heartbeat({ agentId: "ts-worker-1", capabilities: ["gpu", "arm64"] })
);
console.log(`Assigned: ${assignment.taskId}`);

// Stream metrics from TypeScript to Python
const mc = client.proxy("MissionControl");
await mc.ingestMetrics(async function*() {
  for (let i = 0; i < 1000; i++) {
    yield new MetricPoint({ name: "gpu.temp", value: 72 + Math.random() * 10 });
  }
}());
```

**What just happened:**
- Same `@wire_type` tags → same wire format → full interop
- TypeScript agent talks to Python control plane with zero glue code
- The contract hash is identical regardless of implementation language

---

## Beyond RPC: What Else Can You Do?

Aster is built on [iroh](https://iroh.computer), which gives you more than
just RPC:

**Content-addressed blobs** — distribute build artifacts, model weights,
or config bundles to agents. The hash IS the address — fetch once, verify
forever.

```python
# Upload a deployment artifact
blob_hash = await blobs.add_bytes(artifact_data)
# Any agent with the hash can fetch it — verified, deduplicated
```

**Collaborative documents** — shared state that syncs across peers using
CRDTs. Use it for distributed config, feature flags, or service registries
that survive partitions.

**Gossip pub/sub** — broadcast messages to all producers in a mesh.
Used internally for producer coordination, but available for your own
real-time fanout needs.

**Port forwarding** — tunnel TCP or UDP through the encrypted QUIC
connection. Expose a debug port, forward a database connection, or
bridge legacy services into the mesh.

**File transfer** — send files between peers using the blob store.
Content-addressed, resumable, and verified.

---

## Appendix: Running the Benchmarks

```bash
cd examples/mission-control
python bench/benchmark.py

# Output:
# ┌─────────────────────────────────┐
# │ Mission Control Benchmark       │
# ├──────────────┬──────────────────┤
# │ Unary        │ 12,400 req/s     │
# │ Server stream│ 48,000 msg/s     │
# │ Client stream│ 52,000 msg/s     │
# │ Bidi stream  │ 31,000 msg/s     │
# │ Latency p50  │ 0.08 ms          │
# │ Latency p99  │ 0.34 ms          │
# └──────────────┴──────────────────┘
```

---

## What's Next?

- **Publish your service** to [aster.site](https://aster.site) and get a
  short, shareable address: `yourname/MissionControl`
- **Add more agents** — they discover each other through the mesh
- **Deploy to production** — the same code runs locally, on a server,
  or at the edge. No infrastructure changes.

The full source for this example is in `examples/mission-control/`.
