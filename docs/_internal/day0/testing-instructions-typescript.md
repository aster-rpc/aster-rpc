
---

# Day 0 Test Plan -- Mission Control Guide (TypeScript)

You are testing the Aster RPC framework by following the Mission Control
TypeScript guide. Your environment has Bun with `@aster-rpc/aster` linked
locally, plus the Python `aster-cli` for client tools. You have no prior
config, no identity files, no credentials.

## Environment

- Bun 1.0+ with `@aster-rpc/aster` and `@aster-rpc/transport` linked
- Python 3.13 with `aster` and `aster-cli` installed (for CLI tools)
- Clean home directory (no `~/.aster/`)
- Two terminal windows available (one for server, one for client)
- The guide is at: `GUIDE.md` (copy of GUIDE_TypeScript.md)

## What to test

Work through each chapter in order. For each chapter:

1. Copy the code from the guide into `.ts` files
2. Run it with `bun run <file>.ts`
3. Report: **PASS** if it works as described, **FAIL** with the exact
   error message and what you expected

## Chapter 1: First Agent Check-In

**Server (Terminal 1):**

Create `control.ts` with the code from Chapter 1, then:

```bash
bun run control.ts
```

Expected: prints an `aster1...` address and shows the startup banner
(with "typescript" in the runtime line). Record the address.

**Client (Terminal 2):**

```bash
aster shell <the aster1... address from above>
```

Then in the shell:

```
cd services/MissionControl
./getStatus agentId="edge-node-7"
```

Also test:

```bash
aster call <address> MissionControl.getStatus '{"agentId": "edge-node-7"}'
```

Expected: returns `{"agentId": "edge-node-7", "status": "running", "uptimeSecs": 3600}`

**Report:**
- [ ] Server starts and prints address
- [ ] Banner shows "typescript" in runtime line
- [ ] Shell connects and shows MissionControl with methods
- [ ] `./getStatus` or `aster call` returns correct response

## Chapter 2: Live Log Streaming

Add `submitLog` and `tailLogs` to the service as shown in the guide.
Restart the server.

**Test:**

```bash
aster call <address> MissionControl.submitLog '{"message": "test entry", "level": "info"}'
```

In the shell:

```
cd services/MissionControl
./tailLogs agentId="" level="info"
```

**Report:**
- [ ] `tailLogs` opens a stream (shows waiting)
- [ ] Submitted log entry appears in the stream
- [ ] Ctrl+C cleanly stops the stream

## Chapter 3: Metric Ingestion

Add `ingestMetrics` to the service. Create `agent.ts` with the proxy
client code from the guide.

```bash
bun run agent.ts
```

Expected: prints `Accepted: 10000` (or similar count).

**Report:**
- [ ] Client streaming works
- [ ] Server receives and counts all metrics
- [ ] IngestResult returned with correct count

## Chapter 4: Agent Sessions

Add `AgentSession` (session-scoped service) as shown. Restart server.

Session-scoped services need a dedicated session subshell -- you can't
just `./register` from `/services/AgentSession` like a shared service,
because each call would tear down and re-create the session. Use the
`session` command from `/services` (or any path under it) to open a
persistent session subshell.

**Test:**

```bash
aster shell <address>
cd services
session AgentSession
# now in the session subshell, prompt becomes:
#   AgentSession~
register agentId="edge-1" capabilities='["gpu"]'
```

Expected: returns an Assignment. The session persists -- subsequent
calls share state with the same `AgentSession` instance.

For bidi streaming:

```
AgentSession~ runCommand command="echo hello"
```

Expected: returns CommandResult with stdout.

To leave the session subshell:

```
AgentSession~ end
```

**Report:**
- [ ] Session-scoped service shows in shell as `(session)`
- [ ] `session AgentSession` opens a subshell with `AgentSession~` prompt
- [ ] `register` returns Assignment
- [ ] `runCommand` bidi stream works
- [ ] `end` returns to the main shell
- [ ] Two separate shell connections get independent sessions
- [ ] Calling `./register` directly from `/services/AgentSession` shows
  a clear error pointing at the `session` command

## Chapter 5: Auth & Capabilities

**Generate root key:**

```bash
aster trust keygen --out-key ~/.aster/root.key
```

**Update control.ts** with auth config as shown in the guide.

**Enroll agents:**

```bash
aster enroll node --role consumer --name "edge-node-7" \
    --capabilities ops.status,ops.ingest \
    --root-key ~/.aster/root.key \
    --out edge-node-7.cred

aster enroll node --role consumer --name "ops-team" \
    --capabilities ops.status,ops.logs,ops.admin,ops.ingest \
    --root-key ~/.aster/root.key \
    --out ops-team.cred
```

**Test access control:**

```bash
# Edge agent -- should be able to call getStatus but not tailLogs
aster shell <address> --rcan edge-node-7.cred
cd services/MissionControl
./getStatus agentId="test"
# Should succeed

./tailLogs agentId="" level="info"
# Should fail with PERMISSION_DENIED
```

```bash
# Ops team -- should have full access
aster shell <address> --rcan ops-team.cred
cd services/AgentSession
session AgentSession
./runCommand
> command="echo admin"
# Should succeed
```

**Report:**
- [ ] `aster trust keygen` generates key
- [ ] `aster enroll node --role consumer` creates credential files
- [ ] Server starts with `allowAllConsumers: false`
- [ ] Edge credential: getStatus works, tailLogs denied
- [ ] Ops credential: all methods work including admin
- [ ] No credential: connection refused

## Chapter 6: Generating Typed Clients

```bash
# --lang is required (no default). Use typescript here.
aster contract gen-client <address> --out ./clients --package mission_control --lang typescript
```

Then:

```bash
# NOTE: TS codegen emits kebab-case filenames -- mission-control-v1.js,
# NOT mission_control_v1.js (snake_case is the Python codegen convention).
bun -e "
import { MissionControlClient } from './clients/mission_control/services/mission-control-v1.js';
console.log('Import OK');
"
```

**Report:**
- [ ] `gen-client` produces files without errors
- [ ] Generated code imports successfully
- [ ] Types have correct fields and defaults
- [ ] (Bonus) Making an actual RPC call via generated client works

## Chapter 7: Cross-Language (Python client to TypeScript server)

With the TypeScript server still running from earlier chapters, test a
**Python client** connecting to it.

**Create `py_client.py`:**

```python
import asyncio
from aster import AsterClient

async def main():
    client = AsterClient(address="<aster1... address from TS server>")
    await client.connect()

    mc = client.proxy("MissionControl")
    result = await mc.getStatus({"agentId": "py-worker-1"})
    print(f"Status from TS server: {result}")

    await client.close()

asyncio.run(main())
```

```bash
python py_client.py
```

Expected: Python client connects to TS server and receives a valid
response. The JSON serialization mode bridges the two languages.

**Report:**
- [ ] Python client connects to TypeScript server
- [ ] RPC call returns correct response
- [ ] Proxy client works cross-language

## General observations

Report any of:
- Confusing error messages
- Missing commands or wrong syntax in the guide
- Import errors or missing dependencies
- Differences between TS and Python behavior for the same operation
- Slow operations (> 5 seconds for something that should be instant)
- Warnings or deprecation notices
- Anything that made you stop and think "this doesn't seem right"

## After testing

Share your results as a checklist. Mark each item PASS/FAIL. For FAILs,
include the exact error output and what step you were on.
