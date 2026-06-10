# Aster Orchestrator — Flat Workload Orchestration On The Mesh

**Status:** Working idea / product thesis
**Date:** 2026-06-10
**Related:**
- [../trust-directory.md](../trust-directory.md) — the directory (apiserver-equivalent) and delegation model
- [aster-tunneld-linux.md](aster-tunneld-linux.md) — the network plane (DNS intercept, TUN, broker contract)
- portal-sync repo: `docs/substrate-thesis.md` (the substrate), `docs/roaming-workspace.md` (the lease primitive), `docs/design/control-plane-auth-rpc.md` (the proven control loop)

---

## Thesis

Kubernetes is a database with controllers around it — and both halves are the
operational burden: the database (etcd + apiserver) must be provisioned,
upgraded, and babysat, and the hard subsystems (networking, storage, identity,
registry) are not in the orchestrator at all but in a bolt-on ecosystem (CNI,
CSI, OIDC config, Helm sprawl) where most of the complexity actually lives.
Nomad proved a simpler core was possible and then died of "batteries sold
separately" — its value was gated on adopting Consul and Vault as separate
conscious choices.

The Aster/portal substrate inverts both failure modes:

> **A flat, P2P workload orchestrator where the replicated directory is the
> apiserver, the QUIC mesh is the cluster network, the CAS is the registry and
> the volume store, and there is no control plane to operate at all.**

Every machine that enrolls — a VPS, an office server, a homelab box behind
CGNAT — is "in the cluster," because NAT traversal is the substrate, not an
add-on.

## The kernel already exists: portal-syncd is an orchestrator

This is not "we could build an orchestrator on our stack." portal-syncd
**already is one, for a single workload type** (sync sessions), in production
through portal-sync Phase 5:

| Orchestrator concern | Already proven in portal-syncd |
| --- | --- |
| Desired state | Operator-authored records in the replicated directory (`SyncSessionDesired`) |
| Control loop | Watchers → `SyncSupervisor` reconcile → workload execution with health states |
| Status / observability | Debounced status to node-owned namespaces; console/CLI reads by node id |
| Interactive ops | Aster RPC plane (`start/stop/flush/status`), role-gated |
| Placement exclusivity | Single-owner occupancy per (node, resource) with eviction (P5T-007) |
| AuthZ / enrollment | Trust directory: admissions, roles, designations; OIDC enroller for workload identity |
| Revocation | Tombstones, re-validated during reconcile — running work stops on revoke |

The orchestrator product is: **generalize the workload type from "sync
session" to "OCI container / service unit."** The control loop, status plane,
RPC plane, and authority model carry over unchanged.

And every hard subsystem k8s outsources is a native primitive here:

| K8s bolt-on | Native primitive |
| --- | --- |
| etcd + apiserver | Trust directory — replicated, watchable, no quorum to operate, no upgrade treadmill |
| RBAC + OIDC wiring | Directory roles/designations + the workload-identity enroller |
| CNI / Ingress / mesh sidecars | aster-tunneld + broker contract — QUIC, NAT-traversing, per-service authorization at `open` |
| CSI / PV / stateful operators | Portal Trees (replicated volumes) + roaming lease (fenced migration) + snapshots (history) |
| Registry + image distribution | The CAS — OCI layers are content-addressed already; the multi-provider downloader is P2P image distribution (what spegel/dragonfly bolt onto k8s) |
| Rollout machinery | Epochs — a release is a ChangeSet; rollback is a restore |
| kubectl + dashboard | The console-as-peer (portal `product-vision-ux.md`) |

## Four design stances (where complexity will try to creep back)

1. **No scheduler.** V1 placement is **constraint matching + replica count**
   ("run 3 of this on nodes labeled `gpu`; never two on one node"), claimed
   via directory records with a deterministic tiebreak. No bin-packing, no
   preemption, no autoscaler. Fleets under ~50 nodes — the entire wedge
   market — need "keep this running on machines matching X," not a scheduler.
   This is a product stance, the same discipline as portal's "no fat per-node
   GUI."
2. **The node OS is the kubelet.** Do not re-implement runtime supervision —
   that is where k8s's controller-years of edge cases live (crash loops,
   backoff, image GC, disk pressure). The node agent writes **podman Quadlet
   units and lets systemd supervise**: restarts, cgroups, journald. The agent
   does reconcile-and-report, not process babysitting. Because systemd
   supervises a VMM process exactly like a container, this generalizes across
   an isolation spectrum (container ↔ microVM) with one loop — see *Workload
   runtime spectrum*.
3. **CP for leases, AP for everything else.** The directory is
   eventually-consistent LWW — correct for desired state, status, and roles;
   *wrong* for "at most one instance" (singleton jobs, primary databases).
   The shared primitive is **[aster-leases.md](aster-leases.md)**: advisory
   grant in the directory + a monotonic fencing token enforced at the resource,
   with the resource's own CAS as the per-resource serialization point (the
   volume is a portal Tree, so `portal-store` serializes). K8s puts *everything*
   in CP and pays etcd's price for all of it; here consensus cost is paid only
   at the specific resources that would corrupt, and even there the resource
   serializes itself — never a standing lock service.
4. **Batteries invisible.** The Nomad trap is a mirror: "an Aster-based
   orchestrator" must never require the customer to know Aster exists. One
   binary, identity + network + storage + registry inside it. The stack being
   ours is an implementation advantage, never a sales proposition.

## Workload runtime spectrum

Isolation is a **property of the workload record**, not an architecture choice.
The agent maps the property to a backend; **systemd supervises all of them
identically** (the same `Restart=`, cgroup accounting, journald capture), so
the reconcile-and-report loop is unchanged across the spectrum.

| Isolation class | Backend | Status |
| --- | --- | --- |
| `container` | podman Quadlet `.container` → systemd | **v1** |
| `microvm` | **Kata** runtime (OCI-compatible; same Quadlet unit, `--runtime kata`) over Firecracker / Cloud Hypervisor → systemd | **v1** |
| `vm` (full VM) | systemd-vmspawn / QEMU-KVM unit | **future, subject to need — see below** |

Two classes ship in v1: `container` for trusted/dense workloads, `microvm` for
isolation. The win is that `microvm` is the *same record shape and the same
unit*, just a runtime-class field — Kata makes a microVM look like a container
to the OCI runtime, so there is no second code path.

**The microVM tier is shared with exec, not new.** `aster-exec`'s sandbox
already specified a microVM backend as its untrusted-code isolation tier
([aster-exec.md](aster-exec.md) — "running genuinely untrusted code needs a
microVM backend behind the same profile schema"). Exec jobs and orchestrator
workloads are the *same thing at different lifetimes* — run-once vs
long-running — on *one* isolation spectrum. Building `microvm` once pays for
both: isolation-as-a-dial makes multi-tenant / untrusted-code a toggle, not a
re-architecture.

**Full VMs are deliberately deferred — and may stay deferred.** Full-VM
hosting is well-served by Proxmox/Incus already, and it carries the genuinely
hard parts (tap-device-to-mesh bridging, guest-aware health, the temptation of
live migration) without obvious differentiation over incumbents. The substrate
*could* host VMs — the disk image is exactly the big-file/CDC/snapshot/
roaming-lease case portal already serves, so cold crash-consistent failover
would come nearly free — but that is an expansion gated on real demand (edge /
legacy consolidation), not a v1 goal. If it lands, it is the same record with a
`vm` isolation field and a QEMU/vmspawn backend; nothing above changes.

## Differentiators (rank-ordered)

1. **Stateful workloads.** Every lightweight orchestrator punts on state
   ("use a managed database"). This one uniquely has: replicated volumes
   (Trees), exclusive-writer migration with fencing (roaming lease),
   crash-consistent activation floors, and point-in-time history in the
   customer's bucket (snapshots). *"Your Postgres moves between nodes with
   its volume, fenced, and yesterday is in your bucket."* Nobody in the
   lightweight tier can say that sentence.
2. **NAT-spanning clusters with zero network config.** K8s assumes a flat
   pod network; multi-site is Submariner-class pain. Here the cluster is
   whatever enrolls, across sites, clouds, and CGNAT. Edge / multi-site /
   hybrid is where k8s structurally cannot follow.
3. **No control plane to operate.** Not "HA control plane" — none. The
   CLI/console is a peer of the mesh; if it is offline, workloads do not
   notice.
4. **Client-aware load balancing for free.** Service consumers resolve
   endpoints from directory records and rank by liveness, connection type
   (direct > relay), and RTT — the `ContentDiscovery` ranking pattern pointed
   at service endpoints instead of blob holders. tunneld's DNS intercept
   makes it transparent to applications.

## Architecture sketch

```text
operator / console (a peer)
      │  writes Workload records (image, constraints, replicas, volumes,
      │  service exposure, secrets as sealed records)
      ▼
trust directory  ── watched by every node agent
      │
node agent (per node, one binary)
      ├─ claim: constraint match + occupancy/lease rules → claim record
      ├─ fetch: image layers from CAS (multi-provider downloader)
      ├─ run:   write Quadlet unit (container | microvm via Kata) → systemd supervises
      ├─ wire:  register service endpoint with local tunneld broker
      ├─ state: mount Trees as volumes; singleton state under a fenced lease
      └─ report: status → node-owned namespace (console/CLI read live)

rollout = epoch over Workload records (old-or-new, never half)
rollback = restore the previous epoch
```

## Scaling posture

The honest answer to "does it scale?" — projected, not yet measured (validated
mesh testing is 2–5 nodes; treat the numbers below as a reasoned bet a soak
test must confirm, not a fact).

**No scaling wall below a few hundred nodes**, because the structure removes the
three things that usually break orchestrators:

1. **No central control plane to bottleneck.** k8s tops out on etcd
   write-throughput and apiserver watch fan-out under churn — not on compute.
   Here authority is a *local* directory lookup, so the per-decision authz
   round-trip that hammers the apiserver does not exist. This is the big one.
2. **Content distribution scales *with* node count.** The multi-provider
   downloader + have-map swarms images BitTorrent-style — more nodes are more
   seeders. The centralized-registry bottleneck (what spegel/dragonfly bolt
   onto k8s) inverts into an advantage.
3. **Leases are O(1) in fleet size.** The CP quorum for singletons is a fixed
   3–5 nodes whether the fleet is 5 or 500; consensus cost is per-lease, never
   per-node. Gossip is bounded-degree epidemic, not full-mesh, so liveness is
   squarely within its design range.

**Two soft spots that need modest design (not redesign):**

- **Observability fan-in.** N node-owned status namespaces make the console an
  N-way aggregator, and fleet-wide views (unhealthy-node rollup, *rollout
  progress across the fleet*) need an aggregation layer that is deferred today
  (metrics is this-node-view only). Felt first, because progressive rollouts
  (canary, "wait for N healthy") depend on reading fleet status back. Likely
  shape: a designated-aggregator role or hierarchical rollup.
- **Claim churn under correlated failure.** Steady state is fine (most directory
  changes are irrelevant to most nodes). The stress case is a *correlated*
  event — a node dies and its workloads reschedule, or a rollout touches many
  records — where many nodes wake and race to claim. Occupancy/tiebreak
  converges (LWW + deterministic claim, the P5T-007 seed), but the convergence
  window wants backoff/jitter validation at fleet scale.

**The stated ceiling: the directory is unsharded.** Every node sees every
record. At a few hundred nodes × a few thousand records this is small and fine;
past ~1000 nodes or ~10k workloads it needs partitioning (per-zone namespaces,
or nodes subscribing only to relevant slices). 200 nodes is a deliberate
comfortable zone *below* that threshold, not an accident — naming the ceiling
keeps it a design choice rather than a surprise.

The real gap at 200 nodes is **validation, not architecture**: the deferred
soak/scale test (portal L1 / LPS-4) is what converts this posture into a fact.

## Prior art (survey required before any public claim)

As of knowledge cutoff (Jan 2026): **Uncloud** (P2P Docker orchestration over
a WireGuard mesh, no control plane) is the closest existing thing; also Kamal
(37signals), Skate, Docker Swarm's remnants, and k3s/k0s as "smaller k8s." On
the container+VM-unification axis (relevant even though full VMs are deferred):
**Incus/LXD** (one tool, system containers + VMs, but database-clustered, not
mesh), **Harvester** (SUSE HCI on k8s+KubeVirt+Longhorn — the close vision, but
heavy), and **Fly.io** (Firecracker microVMs over a mesh — the philosophical
cousin, but proprietary and not self-hostable). None have the
flat-mesh + no-control-plane + NAT-spanning + isolation-spectrum +
mesh-native-replicated-storage combination — that is the moat — but this needs
an OI-012-style prior-art pass (and a fresh search; the space is moving) before
"nobody does this" appears in any positioning.

## Sequencing

This is a **third product** and the substrate thesis's third leg — files
(portal-sync), git (the worktree mesh), **workloads (this)** — all built from
the same five primitives: directory + CAS + epochs + leases + tunnels.

portal-sync ships first; the orchestrator is the substrate's second act, not
a fork of current attention. The cheap moves **now** are design-level
(each costs a paragraph today and saves a rewrite later):

- Keep the **lease primitive generic** when roaming-workspace lands — it is
  [aster-leases.md](aster-leases.md) (grant/enforce split + resource-CAS
  fencing), not a file-sync feature.
- Keep tunneld's **service registry as directory records** (its doc §5.2
  already points at "subscribe-to-data-model" — that model is the trust
  directory).
- Keep portal's **status-record schema workload-agnostic** where free.
- Keep the **occupancy/claim pattern** (P5T-007) documented as a generic
  single-owner primitive, not a tree-specific rule.

## The one-line version

> Kubernetes is a database with controllers around it. We have a
> better-shaped database (replicated, NAT-traversing, nothing to operate),
> the controllers are proven in portal-syncd, and the three subsystems k8s
> outsources to its ecosystem — network, storage, identity — are native
> primitives. The cluster is wherever the mesh reaches.
