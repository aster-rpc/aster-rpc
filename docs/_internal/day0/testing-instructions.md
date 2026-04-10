
---

# Day 0 Test Plan -- Mission Control Guide

You are testing the Aster RPC framework by following the Mission Control
guide. Your environment is a clean Python 3.13 install with `aster-rpc`
and `aster-cli` pre-installed. You have no prior config, no identity
files, no credentials.

## Environment

- Python 3.13 with `aster` and `aster-cli` installed
- Clean home directory (no `~/.aster/`)
- Two terminal windows available (one for server, one for client)
- The guide is at: `GUIDE.md` (in your working directory)

## What to test

Work through each chapter in order. For each chapter:

1. Copy the code from the guide into files
2. Run it exactly as shown
3. Report: **PASS** if it works as described, **FAIL** with the exact
   error message and what you expected

## Chapter 1: First Agent Check-In

**Server (Terminal 1):**

Create `control.py` with the code from Chapter 1, then:

```bash
python control.py
```

Expected: prints an `aster1...` address and shows the startup banner.
Record the address.

**Client (Terminal 2):**

```bash
aster shell <the aster1... address from above>
```

Then in the shell:

```
cd services/MissionControl
./getStatus agent_id="edge-node-7"
```

Expected: returns `{"agent_id": "edge-node-7", "status": "running", "uptime_secs": 3600}`

**Report:**
- [ ] Server starts and prints address
- [ ] Shell connects and shows MissionControl with methods
- [ ] `./getStatus` returns correct response

## Chapter 2: Live Log Streaming

Add `submitLog` and `tailLogs` to the service as shown in the guide.
Restart the server.

**Test:**

```
cd services/MissionControl
./tailLogs agent_id="" level="info"
```

In a third terminal or via a script, submit a log entry. The `tailLogs`
stream should print the entry.

**Report:**
- [ ] `tailLogs` opens a stream (shows waiting)
- [ ] Submitted log entry appears in the stream
- [ ] Ctrl+C cleanly stops the stream

## Chapter 3: Metric Ingestion

Add `ingestMetrics` to the service. Create `agent.py` with the proxy
client code from the guide.

```bash
python agent.py
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
register agent_id="edge-1" capabilities='["gpu"]'
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

**Update control.py** with auth config as shown in the guide.

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
./getStatus agent_id="test"
# Should succeed

./tailLogs agent_id="" level="info"
# Should fail with PERMISSION_DENIED
```

```bash
# Ops team -- should have full access
aster shell <address> --rcan ops-team.cred
cd services/AgentSession
./runCommand
> command="echo admin"
# Should succeed
```

**Report:**
- [ ] `aster trust keygen` generates key
- [ ] `aster enroll node --role consumer` creates credential files
- [ ] Server starts with `allow_all_consumers=False`
- [ ] Edge credential: getStatus works, tailLogs denied
- [ ] Ops credential: all methods work including admin
- [ ] No credential: connection refused (or open methods only)

## Chapter 6: Generating Typed Clients

```bash
aster contract gen-client <address> --out ./clients --package mission_control
```

Then:

```python
python -c "
from mission_control.services.mission_control_v1 import MissionControlClient
from mission_control.types.mission_control_v1 import StatusRequest
print('Import OK')
print(StatusRequest(agent_id='test'))
"
```

**Report:**
- [ ] `gen-client` produces files without errors
- [ ] Generated code imports successfully
- [ ] Types have correct fields and defaults
- [ ] (Bonus) Making an actual RPC call via generated client works

## Chapter 7: Cross-Language

Skip if TypeScript environment is not available. Otherwise:

```bash
# From a node/bun environment with @aster-rpc/aster installed
bun run agent.ts
```

**Report:**
- [ ] TypeScript proxy client connects
- [ ] RPC calls work cross-language
- [ ] OR: note that TS binding needs update (known)

## General observations

Report any of:
- Confusing error messages
- Missing commands or wrong syntax in the guide
- Import errors or missing dependencies
- Slow operations (> 5 seconds for something that should be instant)
- Warnings or deprecation notices
- Anything that made you stop and think "this doesn't seem right"

## After testing

Share your results as a checklist. Mark each item PASS/FAIL. For FAILs,
include the exact error output and what step you were on.


