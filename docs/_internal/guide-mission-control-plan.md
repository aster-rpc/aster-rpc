# Mission Control Guide — Plan & Status

## What this is

The Mission Control example is a single worked example that serves
multiple objectives at once. It sits on the docs site as the "next step
after quickstart" and is written in parallel for each supported language
using the docs site's language switcher.

## Objectives

1. **Real-world example** — not a contrived hello world or chat room.
   "Remote agents checking into a control plane" is a scenario our
   audience relates to instantly without us spelling out the problems
   we're solving.

2. **Full Aster experience guide** — covers the core Aster workflow
   naturally: all four RPC patterns, session-scoped services, CLI
   tooling (shell, call, keygen, enroll, publish, contract inspect,
   contract gen), proxy + typed clients, capability-based auth, and
   cross-language interop.

3. **Publishable example** — ships under `examples/mission-control/`
   for every supported language. Real runnable code, not just snippets.

4. **Benchmark harness** — the same example doubles as a performance
   test: throughput, latency, memory across languages. Reproducible
   and comparable.

5. **Cross-language interop proof** — Python control plane + TypeScript
   agent, using published contracts and generated clients. No shared
   repo, no hand-maintained schema.

## Guide structure (Part 1 — locked)

The guide lives at `examples/mission-control/GUIDE.md`. 7 chapters,
under an hour:

| Ch | Title | What it teaches |
|----|-------|-----------------|
| 1 | Your First Agent Check-In | Unary RPC, `@service`, `@wire_type`, `AsterServer`, `aster shell`, `aster call` |
| 2 | Live Log Streaming | Server streaming, `submitLog` + `tailLogs`, services as plain Python classes |
| 3 | Metric Ingestion | Client streaming, proxy client, typed client (sidebar), gateway use case |
| 4 | Agent Sessions & Remote Commands | Session-scoped services, bidi streaming, `AgentSession` with register/heartbeat/runCommand |
| 5 | Auth & Capabilities | `aster trust keygen`, roles, `any_of`/`all_of`, `aster enroll consumer`, credential-based connection |
| 6 | Publishing Your Service | `aster publish`, `aster contract inspect`, `aster contract gen` |
| 7 | Cross-Language — TypeScript Agent | Generated TS client from published contract, `createClient`, proxy fallback |

Plus: appendix with benchmark runner (numbers marked illustrative).

### Architecture (as shown in guide)

- **MissionControl** (shared service) — fleet-wide: status, logs, metrics
- **AgentSession** (session-scoped) — per-agent: register, heartbeat, commands
- Operators connect via `aster shell` / `aster call` or programmatic client
- Agents connect as consumers with scoped credentials
- Relay for NAT traversal (public default, self-hostable via `IROH_RELAY_URL`)

## Part 2+ topics (deferred)

Captured in `docs/_internal/guide-series-plan.md`. Includes:
interceptors, contract identity internals, producer mesh / load
balancing, LocalTransport testing, error handling, advanced
cross-language (TS services), blobs/docs/gossip primitives,
security hardening.

## Key design decisions

- **`runCommand` lives on `AgentSession`, not `MissionControl`** —
  commands execute on the agent, not the control plane. This makes the
  topology honest and gives AgentSession real purpose beyond
  register/heartbeat.

- **No `DEPLOY` role** — removed from Part 1 because it's not
  demonstrated. Auth chapter only uses roles that map to methods
  actually shown.

- **Typed client shown as sidebar, not standalone chapter** — avoids
  breaking narrative momentum. Introduced in Ch 3 as a blockquote.

- **Proxy client positioned for gateways, not just prototyping** —
  "if you're building a dashboard that talks to any Aster service
  without knowing its types at compile time, the proxy is your best
  friend."

- **Opening leads with pain, not features** — the first thing readers
  see is the frustration they already feel, then the 5-line solution.

- **Benchmark numbers marked illustrative** — no performance claims
  until we have a real reproducible harness.

## Current status

- [x] Guide content written and reviewed (GUIDE.md locked)
- [x] Part 2+ topics captured (guide-series-plan.md)
- [x] External review (Gemini, ChatGPT) — feedback incorporated
- [ ] Audit guide naming vs actual codebase (CLI commands, API names)
- [ ] Identify implementation gaps
- [ ] CLI changes pending (user flagged this as next)
- [ ] Build runnable example code for `examples/mission-control/`
- [ ] Haiku walkthrough — have a simple model follow the guide end to
      end as a user, confirm it works, provide feedback
- [ ] Publish guide to docs site
- [ ] Publish example repos per language
