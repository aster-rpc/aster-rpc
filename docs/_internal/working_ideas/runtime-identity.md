# Runtime Identity — Duplicate Instance Detection

**Status:** Working idea — design draft, not built.
**Date:** 2026-05-06
**Related:**
- [ownership-attestations.md](../ownership-attestations.md) — the trust spine; this doc is orthogonal (operational, not security)
- [../aster-trust-architecture.md](../aster-trust-architecture.md) — broader trust architecture

---

## The problem

Aster identities are Ed25519 keypairs. iroh routes connections to whichever endpoint resolves for a given pubkey. **If two processes start with the same keypair, they both claim the same NodeId, and nothing in the transport layer prevents this or detects it.** Connections get routed to whichever instance the discovery layer surfaced first. Each instance has its own local state. Load balancing does not work.

This is **not a security issue** — both instances are operator-controlled clones, not adversaries. It is an operational issue:

- Discovery records flap between addresses (each instance overwrites the other's published address).
- RPC connections randomly land on different instances; in-flight session state diverges.
- Metrics, logs, and traces split across instances under one identity, making debugging miserable.
- Load balancing has no granularity to balance across — the consumer sees one NodeId, the network resolves to whichever instance won the discovery race.

The structurally-correct fix is **one keypair per replica, all attested under a shared root** (see [ownership-attestations.md](../ownership-attestations.md)). But this is easy to get wrong:

- Build a single Docker image with the keypair baked in → scaling the deployment clones the keypair across replicas.
- StatefulSet sharing a ConfigMap or Secret containing the keypair → every pod reads the same key.
- Vendor getting-started example shows generating one keypair, then the operator bumps `replicas: 3` → no docs nudge to issue distinct keypairs per replica.
- Lift-and-shift from single-node testing to multi-node production → the dev environment's keypair gets copied to all production replicas.

The mistake is easy. Detection has to surface it quickly enough that operators catch it before consumer behaviour gets weird.

---

## Why this isn't a security problem (and why detection is enough)

If an attacker had the keypair, they wouldn't need to clone — they'd just impersonate from one location. The attacker's threat model is "I have the privkey; I can sign anything." Adding more instances doesn't help the attacker.

Conversely, if a clone is *accidental*, both instances are the legitimate operator. Detection is enough; no need for cryptographic prevention. The operator's response is "rotate the deployment to per-replica keypairs," not "reject the imposter."

So: surface the duplicate to the operator, let them fix it. No protocol-level enforcement.

---

## The detection primitive: `instance_id`

Each Aster service generates a random **process-scoped** `instance_id` at startup — 16 random bytes, NOT derived from the keypair. It's exposed via the well-known service surface alongside `aster_node_chain()` (per `ownership-attestations.md`):

```python
@dataclass
class NodeInstance:
    instance_id: bytes        # 16 random bytes, generated at process boot
    started_at: int64         # unix seconds at process start
    aster_version: str        # build version, debug info
    bind_addrs: list[str]     # optional, debug-only

# Public service surface query:
def aster_node_instance() -> NodeInstance:
    ...
```

Public, unauthenticated, read-only. Same posture as `aster_node_chain()` — anyone who can reach the service can ask "which instance am I talking to?"

`instance_id` SHOULD change on every process start. Container restart legitimately gets a new one. **The signal isn't "two `instance_id`s observed at different times" — it's "two distinct `instance_id`s alive concurrently."**

This distinction is critical. A client seeing `instance_id` A at noon and B at 1 pm cannot tell:

- the container restarted at 12:30 (legitimate), OR
- two instances were running concurrently the whole time (the duplicate problem).

Detection requires **temporal correlation**: were both `instance_id`s "fresh" at the same point in time?

---

## Detection layers

### Layer 1 — `instance_id` exposed via the public service surface (foundation)

Every Aster service implements `aster_node_instance()`. Required for everything else.

By itself, single-observation tracking does **not** detect duplicates (because of the restart vs concurrent ambiguity above). It's the primitive consumed by the layers below.

### Layer 2 — automatic, gossip-heartbeat detection

Each instance publishes a signed heartbeat every 30s on a per-pubkey gossip topic:

```
topic: aster.instance.<base64url-of-node_pk>
```

```python
@WireType("aster.heartbeat.body.v1")
class HeartbeatBody:
    v: int32                  # = 1
    node_pk: bytes            # 32 bytes, the shared identity
    instance_id: bytes        # 16 bytes, this process's per-boot id
    started_at: int64         # unix seconds at process start
    heartbeat_ts: int64       # unix seconds, this heartbeat
    aster_version: str

@WireType("aster.heartbeat.v1")
class Heartbeat:
    body: bytes               # opaque Fory bytes of HeartbeatBody
    signature: bytes          # 64 bytes, by node_sk
```

Domain separator: `b"aster.heartbeat.v1\0"`. Same envelope discipline as attestations.

**Detection rule:**

```
duplicate detected when:
  count(distinct instance_ids with heartbeat_ts > now() - FRESHNESS_WINDOW) > 1
```

`FRESHNESS_WINDOW` defaults to 60 seconds (twice the heartbeat interval). Adjustable per deployment.

**Properties:**

- **Container restart handled correctly.** Heartbeats from instance A stop at T=30 (process died); heartbeats from instance B start at T=60. At no point are two distinct ids both "fresh." No false alarm.
- **Concurrent duplicates surface.** Instances A and B both heartbeat at overlapping times, both ids stay fresh. Real alarm.
- **Self-subscription**: each instance subscribes to its own topic. Sees other heartbeats from "itself" → its own duplicate alarm fires. The instance can choose to log, alert, or refuse to operate (operator policy).
- **No central infrastructure** — iroh-gossip already does the work.
- **Authenticated** — heartbeats are signed by `node_sk`. Without signatures, any peer could spoof a duplicate alarm.

**Rolling deployment edge case.** During a graceful shutdown, the old pod may still be heartbeating while the new pod has started. Brief concurrent overlap (seconds to a minute) is expected. The detection should suppress alerts shorter than a threshold:

```
fire alert when:
  concurrent_count > 1 AND
  duration_of_concurrent_overlap > GRACE_PERIOD
```

`GRACE_PERIOD` defaults to 90 seconds. Tuned by operator.

### Layer 3 — manual, rapid-probe CLI

```
$ aster diagnose duplicates <node_id> --probes=10 --window=2s
  Probe  1 (t=0.0s)  → instance_id: a3f2...
  Probe  2 (t=0.2s)  → instance_id: 9c1e...  ← DIFFERENT
  Probe  3 (t=0.4s)  → instance_id: a3f2...
  Probe  4 (t=0.6s)  → instance_id: 9c1e...
  ...
DUPLICATE DETECTED: 2 distinct instances alive within 2.0s window.

  instance a3f2...  (started 2026-05-06 14:23:01)  responded 5 times
  instance 9c1e...  (started 2026-05-06 14:25:33)  responded 5 times
```

The CLI does N concurrent connections to the target NodeId via the same discovery + transport path real consumers use; reports `instance_id` divergence within the probe window.

A 2-second probe window forecloses the restart-between-probes false-positive: a process can't die and another start in 2 seconds. Any divergence is real concurrency.

Useful for:

- Operators who suspect a problem and want a quick manual answer.
- CI/CD post-deploy checks (smoke test that scaling produced distinct identities).
- Bug reports ("here's the diagnose output showing two instance_ids").

No infrastructure required beyond what the consumer side already runs.

---

## Behaviour when a duplicate is detected

Default: **WARN, do not refuse.**

- Log a structured warning.
- Emit a metric (`aster_duplicate_instances_detected{node_id=...}`).
- (Layer 2) Surface in gossip-aware monitoring.

Refusing to start would force-fail legitimate operator scenarios where overlapping instances are tolerable (rolling deploys, debugging). The warning is loud enough to surface; the operator decides.

**Optional strict mode** (config flag): `ASTER_STRICT_SINGLE_INSTANCE=true`. When set, instance refuses to operate if it detects another live instance with the same `node_pk` (after the grace period). Useful for deployments that want to fail-fast on misconfiguration.

```
[ERROR] Detected another instance running with my keypair.
  My instance_id:    a3f2...
  Other instance_id: 9c1e... (last heartbeat 12s ago)
  ASTER_STRICT_SINGLE_INSTANCE is set; refusing to serve traffic.
  See https://aster.site/docs/runtime-identity for fix instructions.
```

---

## Persisted `instance_id` (optional)

For stateful workloads where each pod has its own persistent volume, `instance_id` can be persisted to disk and survive container restart (but not pod re-creation):

```
ASTER_INSTANCE_ID_FILE=/var/lib/aster/instance_id
```

If the file exists and is well-formed, read it. Otherwise generate fresh and write.

This gives a more stable signal: a pod's `instance_id` is stable across the pod's lifetime; pod re-creation regenerates. Two distinct stable values seen concurrently = two distinct pods.

**However**, if your deployment is at the point of "each pod has its own persistent volume," you should be using **per-pod keypairs** instead of a shared keypair with per-pod `instance_id`s. The structurally-correct answer dissolves the duplicate-detection problem entirely. Persisted `instance_id` is for narrow cases where shared-keypair-with-stable-volumes is genuinely the deployment shape (rare).

---

## The structurally correct fix (where this doc points operators)

Detection's job is to surface the misconfiguration so the operator can fix it. The fix:

```bash
# For each replica, generate a distinct keypair and attest under the shared root:
for i in 1 2 3; do
  aster trust generate --output node-$i.key
  aster attest node --root deployment-root.key --node node-$i.key \
    --output node-$i.attestation
done
```

Result:

- Each pod has its own `node_id` (cryptographically distinct).
- All pods appear in service-discovery records as separate endpoints under the same handle.
- Consumers see N distinct nodes operated by the same root; load balancing becomes a service-discovery concern (weighted round-robin, latency-based, etc.) at the consumer layer.
- The chain's `ROOT_OWNS_NODE` edge is per-instance — `aster attest node` is run N times, once per replica, all under the same root.
- Duplicate detection becomes irrelevant — no two pods share an identity.

The CLI flow should make this trivially easy. Ideally `aster attest node-batch --replicas=3 --root=...` issues N keypairs + N attestations in one call.

---

## CLI surface

```
aster diagnose duplicates <node_id>
  Connects N times to the target node within a tight window; reports
  any instance_id divergence. Probes-and-window are tunable flags.
  Default: 10 probes, 2-second window.

aster diagnose self
  Run from inside a node. Resolves the node's own NodeId via discovery,
  connects to whatever endpoint resolves, fetches instance_id, compares
  to the running process's instance_id. If different, this node has been
  shadowed by a clone.

aster diagnose heartbeats <node_id>          (Layer 2)
  Subscribes to aster.instance.<node_pk> for the configured FRESHNESS_WINDOW;
  reports all live instance_ids observed.

aster attest node-batch --replicas=N --root=<key>      (structurally-correct fix)
  Generates N keypairs and N attestations under the shared root in one call.
  Outputs a directory of (key, chain) pairs ready for distribution to replicas.
```

---

## Relationship to ownership attestations

This doc is orthogonal to the attestation primitive. Specifically:

- The attestation chain is per-keypair. Two cloned instances have **identical chains** (not multiple chains; the same bytes). The chain verifier sees a valid chain regardless of how many instances are running. Attestation verification cannot detect duplicates.
- `instance_id` is **not signed by the chain or the keypair** in the basic Layer 1 form (it's a runtime identifier, not a trust claim). The Layer 2 heartbeat IS signed (so peers can authenticate the duplicate signal), but `instance_id` itself is just a random uniqueness token.
- Trust derivation is unaffected by duplicates. The shared keypair still validly owns the chain. The operational misalignment is *which physical instance answers*, not *whether the instance is authorised*.

So: same trust path, no changes required. This is purely additive runtime observability.

---

## MVP scope

**Minimum viable:**

- Layer 1: `aster_node_instance()` exposed on every Aster service. Cheap; primitive other layers consume.
- Layer 3: `aster diagnose duplicates` and `aster diagnose self` CLI commands. No infrastructure required.
- `aster attest node-batch` for the structurally-correct fix. Makes per-replica keypairs trivial to generate.

**Deferred:**

- Layer 2 (gossip heartbeat) — adds infrastructure (per-pubkey topic management, signed heartbeat wire format, subscription discipline). Nice to have when scale or auto-detection demand it; not necessary for Day-0.
- Persisted `instance_id` (`ASTER_INSTANCE_ID_FILE`) — narrow use case, defer to when someone asks.
- Strict mode (`ASTER_STRICT_SINGLE_INSTANCE`) — operator-policy flag, easy to add when wanted; not blocking MVP.

**Out of scope entirely:**

- Adversarial duplicates (security): a different problem; the threat model assumes both clones are operator-controlled.
- Active leader election / mutex / split-brain resolution: this is *detection*, not *coordination*. Coordination is application-layer concern using iroh-docs CRDTs or similar.
- Network-partition handling: not addressed by `instance_id`; orthogonal infrastructure problem.

---

## Open questions

1. **Where exactly does `aster_node_instance()` live in the service surface?** Probably co-located with `aster_node_chain()` — both are well-known introspection queries. Naming convention TBD; could be a single `aster_introspect()` returning both, or separate calls.
2. **Heartbeat interval and freshness window defaults.** 30s heartbeat, 60s freshness, 90s grace period are reasonable starting points; production tuning will inform final values.
3. **Should the heartbeat carry the full `AttestationChain`?** Probably not — chain is large (~150–800 bytes per chain) and already available via `aster_node_chain()`. Heartbeat should stay small and frequent. But: if a verifier subscribes to heartbeats without the chain context, it has to fetch the chain separately. Worth confirming the subscription pattern.
4. **Self-detection from inside the node.** A node subscribing to its own `aster.instance.<pk>` topic sees its own heartbeats. Should it filter its own `instance_id` out, or include it (so the topic shows the full live set including self)? Lean: include all; self filters at the consumer if it cares.
5. **Default for `ASTER_STRICT_SINGLE_INSTANCE`.** Off (warn-only) is safer for the rolling-deploy case; on (refuse) is safer for static deployments. Operator policy; default off.
6. **CLI fix automation.** Should `aster diagnose duplicates` offer to *fix* (run `node-batch` and rotate replicas) when it finds duplicates? Probably no — too magic, easy to misuse. Show the duplicate, point at the fix, let the operator drive.
7. **Telemetry / metrics integration.** What standard metrics should every Aster service emit so duplicate detection plugs into Prometheus / OpenTelemetry / etc.? At minimum: `aster_instance_id`, `aster_started_at_seconds`, `aster_duplicate_alerts_total`. Spec-level nice-to-have.

---

## Next concrete steps

1. **`aster_node_instance()` on the service surface.** Add to the well-known introspection methods. Implement in core; expose across all four bindings.
2. **`aster diagnose duplicates` CLI subcommand.** N concurrent connections, report divergence. No new wire formats needed.
3. **`aster diagnose self` CLI subcommand.** Same primitive, used from inside a node to detect "have I been shadowed."
4. **`aster attest node-batch` CLI subcommand.** Generates N keypairs + N attestations under a shared root in one call. Closes the loop on the structurally-correct fix.
5. **Document the operator pattern in the getting-started guide.** Single-replica setup uses one keypair; multi-replica uses `node-batch`. Avoid showing examples that bake a keypair into a Docker image without a clear "this is the wrong way to scale" caveat.
6. **Defer Layer 2 heartbeat infrastructure** until use cases demand auto-detection. The CLI tools cover most needs; gossip subscription is an addition, not a precondition.
