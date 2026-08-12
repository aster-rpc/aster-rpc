# aster-tunneld — Linux client tunneler

**Status:** Working idea
**Date:** 2026-05-04
**Scope:** Map out what's needed to build an Aster equivalent of OpenZiti's `ziti-tunnel-sdk-c` — a Linux daemon that intercepts DNS for configured hostnames, routes app traffic over Aster tunnels via a TUN device + userspace TCP/UDP stack, and handles split-tunnel routing. Policy / posture / JWT minting are out of scope here; this is mechanism only.

References:
- `https://github.com/openziti/ziti-tunnel-sdk-c`
- `https://netfoundry.io/docs/openziti/reference/tunnelers/linux/linux-tunnel-options`
- Layer 1 transport primitive: `ffi_spec/Aster-tunneling.md`

---

## 1. Architecture at a glance

```
┌─────────────────────────────────────────────────────────────────┐
│ aster-tunneld (Linux daemon, CAP_NET_ADMIN)                      │
│                                                                  │
│  ┌──────────┐  ┌─────────────┐  ┌──────────────────────────┐    │
│  │ TUN dev  │─▶│  smoltcp    │─▶│ Flow → service resolver  │    │
│  │ aster0   │  │  TCP/UDP    │  │ (dst IP → fqdn → conn)   │    │
│  └──────────┘  └─────────────┘  └──────────┬───────────────┘    │
│                                             │                    │
│  ┌────────────────────────┐                 ▼                    │
│  │ DNS server (hickory)   │     ┌────────────────────────────┐   │
│  │ - alloc CGNAT 100.64/x │     │ Aster client (per          │   │
│  │ - register w/          │     │ connector connection):     │   │
│  │   systemd-resolved     │     │   1. Broker.open(service)  │   │
│  └────────────────────────┘     │   2. conn.open_tunnel(t)   │   │
│                                  │   3. splice smoltcp ↔ tun │   │
│  ┌────────────────────────┐     └─────────────┬──────────────┘   │
│  │ rtnetlink: routes      │                   │                  │
│  └────────────────────────┘                   │                  │
└────────────────────────────────────────────────┼─────────────────┘
                                                 ▼
                           connector node (Aster RPC server, runs Broker
                           service + has reachability to backend services)
```

The daemon is a **pure-userspace** packet handler. It does not load kernel modules, does not patch netfilter, does not need root beyond `CAP_NET_ADMIN`. App traffic is captured by adding routes for an intercept range (CGNAT `100.64.0.0/10`) through a TUN device that the daemon owns; the daemon parses raw IP packets in userspace via `smoltcp`, terminates the TCP/UDP flow, and bridges it onto an Aster tunnel.

---

## 2. What core needs (small)

The transport library stays userspace-pure. Two additions, neither of which touches the kernel/DNS/routing concerns:

### 2.1 `aster-tunnel-broker-contract` (new crate)

A standard RPC contract every connector exposes and every tunneler dials. Without this, every deployment reinvents the same surface.

```
service Broker {
  unary open(OpenRequest { service_id: str })
    -> OpenResponse { ticket: bytes };          // wraps ctx.tunnel.authorize

  unary list_services()
    -> ListResponse { services: [{
         id: str,
         fqdns: [str],
         intercept_cidrs: [str],     // optional: pre-claimed CIDRs
         ports: [u16],
         protocol: "tcp" | "udp"     // udp is §11
       }] };
}
```

**Scope rules:**
- No policy fields on the wire types — handler-side decides who can `open`.
- No tickets in `list_services` — that endpoint just publishes which services this connector can broker. Tickets are issued only at `open` time, after the handler validates the request.
- The `intercept_cidrs` field is optional metadata for the daemon's resolver — it can pre-allocate stable IPs for those CIDRs instead of synthesising from the CGNAT pool.

### 2.2 `TunnelDialer` helper (core or thin companion crate)

One opinionated method for the tunneler use case:

```rust
impl TunnelDialer {
    /// Cached connection per connector endpoint. Calls Broker.open,
    /// redeems the ticket, returns raw streams.
    pub async fn dial(&self, service_id: &str)
        -> Result<(CoreSendStream, CoreRecvStream)>;
}
```

Internally:
1. Look up `connector_endpoint_id` for `service_id` (config lookup; later: registry).
2. Acquire / reuse a `CoreConnection` to that connector.
3. RPC: `Broker.open { service_id }` → 32-byte ticket.
4. `conn.open_tunnel(ticket)` → raw streams.
5. Return.

One QUIC connection per connector is reused across many concurrent flows via the multiplexed-streams primitive (`Aster-multiplexed-streams.md` §3). The daemon doesn't need a fresh connection per flow.

**That's it for core.** Kernel-side concerns stay out of the transport library.

---

## 3. Daemon design (`aster-tunneld`)

Linux v1. Single binary. Cross-platform glue (macOS `utun`, Windows `wintun`) deferred but the design doesn't preclude it — only the TUN driver and resolver-registrar are platform-specific.

### 3.1 Crate dependencies

| Concern | Crate |
|---|---|
| TUN device I/O | `tokio-tun` |
| Userspace TCP/UDP stack | `smoltcp` (pure-Rust lwIP analogue; used by Tailscale's userspace stack) |
| Embedded DNS server | `hickory-dns` (formerly trust-dns) |
| Routing table | `rtnetlink` |
| systemd-resolved D-Bus | `zbus` |
| Capability checks | `caps` |
| Aster client | `aster_transport_core` + `aster-tunnel-broker-contract` + `TunnelDialer` |

### 3.2 Components

**TUN driver loop.**
Reads raw IP packets from `aster0`, hands them to smoltcp's device interface. Writes packets back when smoltcp emits them. Single async task; backpressure is whatever smoltcp's internal buffers do.

**smoltcp poll loop.**
Owns the TCP/UDP state machine for every flow. Exposes a `socket → (peer_ip, peer_port, dst_ip, dst_port)` view. On a fresh inbound TCP SYN to an intercept IP, the daemon allocates a smoltcp `tcp::Socket`, completes the handshake locally, then forwards bytes to the Aster side.

**Flow → service resolver.**
Two maps held under one lock:
- `dst_ip → service_id` (populated by the DNS server on alloc).
- `service_id → connector_endpoint_id` (from config; later from registry).

Lookup is sync; tunnel dial is async. Lookup misses (e.g. someone tries to use an unallocated 100.64.x.y) get a TCP RST.

**DNS server.**
Local UDP listener (typically bound to the TUN's gateway IP, e.g. `100.64.0.1:53`). For a query whose name matches a configured suffix or fqdn:
1. Allocate a fresh CGNAT IP from the pool (or reuse a previously-allocated one for the same fqdn — keeps long-lived clients stable).
2. Insert into the resolver's `dst_ip → service_id` map.
3. Add a route: `ip via aster0` (rtnetlink).
4. Return an A/AAAA record TTL=brief (5–30s; lets us reclaim IPs after a quiet period).

For non-matching names: forward upstream or return REFUSED, depending on whether the daemon is the only resolver for the host or scoped via systemd-resolved.

**systemd-resolved integration.**
The right way to coexist with the host's DNS. D-Bus call to `org.freedesktop.resolve1` to register the daemon's DNS server as a link-scoped resolver on `aster0`, with the configured domain suffixes pinned to that link. Result: only `*.internal.dev` (or whatever) hits our server; everything else uses the host's normal resolvers.

For systems without systemd-resolved (Alpine, embedded, NixOS minimal): fallback path that rewrites `/etc/resolv.conf` or refuses to start. Not a v1 priority — make `systemd-resolved` the supported path and document the fallback as best-effort.

**Routing manager.**
Two strategies:
- **Per-IP routes:** add a /32 route for each allocated CGNAT IP. More routes, but only intercepted services have routes — no surprise capture of unrelated 100.64.x.y traffic.
- **Range route:** one /10 route for `100.64.0.0/10`. Simpler. Captures all traffic to that range, which is fine on machines that don't otherwise use CGNAT (most laptops/desktops); breaks on machines behind a CGNAT-using ISP.

Default: per-IP. Range is a config opt-in for known-clean networks.

**Per-flow lifecycle.**
1. App on host: `getaddrinfo("db.internal.dev")` → resolver hits `aster0`'s DNS server → daemon allocates `100.64.0.7`, records `100.64.0.7 ↔ db.internal.dev`, returns the IP, adds the route.
2. App opens `TCP 100.64.0.7:5432`. Kernel routes via `aster0`.
3. TUN driver delivers SYN to daemon. smoltcp synthesises the local endpoint, accepts.
4. Resolver: `100.64.0.7` → `db.internal.dev` → service id → connector endpoint id.
5. `dialer.dial("db.internal.dev")` → 32-byte ticket → `(send, recv)` raw streams.
6. Spawn two byte-pumps: smoltcp socket ↔ Aster `(send, recv)`.
7. On either-side EOF or smoltcp FIN: tear both legs down.

**Capability check / privilege drop.**
At startup: verify `CAP_NET_ADMIN`. Recommended hardening (later, not v1): drop all other capabilities post-init via `caps`; run as a non-root user that has `CAP_NET_ADMIN` granted via systemd unit (`AmbientCapabilities=CAP_NET_ADMIN`).

**Config (TOML).**
```toml
identity_key = "/etc/aster/tunneld.key"

[dns]
listen = "100.64.0.1:53"
domain_suffixes = ["internal.dev"]
allocation_pool = "100.64.0.0/16"
ttl_seconds = 30
upstream_fallback = "system"   # or "block" or "1.1.1.1"

[routing]
strategy = "per_ip"            # or "range"
tun_device = "aster0"

[[services]]
id = "db.prod"
fqdn = "db.internal.dev"
connector_endpoint_id = "abc123…"
ports = [5432]

[[services]]
id = "vnc.workstation-1"
fqdn = "ws-1.internal.dev"
connector_endpoint_id = "def456…"
ports = [5900]
```

### 3.3 Lifecycle

- Startup: load config, create TUN, register DNS with systemd-resolved, start smoltcp + DNS + dialer tasks.
- Shutdown: deregister DNS link, remove routes, close all tunnels, drop TUN. Idempotent — restarts after crash should clean up stale routes from the kernel (track our own additions in memory; on startup, scan `aster0`'s routes and remove any tagged with our protocol number via `rtnetlink`).
- Reload (SIGHUP): re-read config, diff service list, allocate/free as needed. Existing flows on removed services keep running until they close (don't kill in-flight TCP).

---

## 4. Already in place, free for the daemon

- Per-connection tunnel ticket registry (`Aster-tunneling.md`).
- One-shot redeem with replay protection.
- Multi-target preference list per ticket (primary/standby per service).
- `accept_aster_bi`'s `FLAG_TUNNEL` peek handles the connector side without any daemon-specific code on the server.
- Multiplexed streams (`Aster-multiplexed-streams.md`) — one QUIC connection per connector, many concurrent tunnels.

---

## 5. Open issues to flag now

### 5.1 UDP

The daemon's *own* DNS server runs on UDP, but that's internal. *Application* UDP services (WireGuard, game servers, DNS-over-UDP backends) need `TunnelTarget::Udp`, which is §11 in `Aster-tunneling.md` — not in v1 transport. Daemon ships TCP-first; UDP is a follow-up requiring both core support and smoltcp UDP wiring. Most services people want to tunnel are TCP (VNC, SSH, Postgres, HTTP) so deferring UDP doesn't kill v1.

### 5.2 Service discovery: static config vs. registry

Config-file mapping `{ fqdn → connector }` works for small fleets. Anything bigger wants Layer 2 of `Aster-tunneling.md` §11 — replicated policy data model, modelled on OpenZiti's subscribe-to-data-model RPC. The `Broker` contract above is the boundary: swap "config-file resolver" for "subscribe-to-data-model client" later without changing the daemon's flow path.

### 5.3 IP allocation churn

CGNAT pool is finite (`100.64.0.0/16` ≈ 65k IPs). Long-lived daemons that churn through fqdns risk exhaustion. Mitigations:
- Stable allocation: same fqdn always maps to the same IP within a daemon lifetime, until the entry is GC'd.
- TTL-based reclamation: free an IP after no flows have used it for N minutes.
- Optional persistent map: pin allocations across restarts (small sqlite file).

### 5.4 Resolver coexistence on non-systemd-resolved hosts

Alpine, NixOS-minimal, embedded distros. The fallback (`/etc/resolv.conf` rewrite) is fragile and conflicts with NetworkManager / dhclient. Document as best-effort; long-term we may need per-distro packaging notes or recommend systemd-resolved.

### 5.5 Cross-platform glue

Out of v1 scope. macOS uses `utun` (no extra capability needed for user-owned utuns on recent macOS), Windows uses `wintun.dll`. Same daemon shape; only the TUN driver and DNS-registrar layers change. The smoltcp + Aster-RPC core is portable.

---

## 6. What to NOT do

- **Don't put TUN/DNS/netlink in core.** Keeps core embeddable, portable, and small. Daemon is Linux-specific by definition — keeping that in a separate crate isolates the platform code.
- **Don't run smoltcp inside the connector.** Connector just speaks TCP to backends; tunneler does all userspace protocol reassembly client-side. (Connector is Layer 0+1 only — accept tunnel, splice to backend socket. That's already done.)
- **Don't require systemd-resolved as a hard dependency.** Plug the resolver-registrar; most users are on systemd-resolved, but the daemon should compile and run without `zbus`-resolving.
- **Don't reach for eBPF / cgroup-based redirection in v1.** TUN + smoltcp is well-trodden, capability-friendly, and debuggable. eBPF is a perf optimisation worth revisiting after profiling shows TUN is the bottleneck.

---

## 7. Build order suggestion

1. **Broker contract crate** + a reference `Broker` service implementation in tests. Stable target for the daemon to build against.
2. **`TunnelDialer` helper** in core (or companion crate). Unit-testable without TUN.
3. **Daemon skeleton** — config loader, lifecycle, capability check, no TUN yet. Validates config parsing + identity loading.
4. **TUN + smoltcp loop** — read packets, accept TCP, pump bytes through `TunnelDialer`. End-to-end without DNS yet (clients connect by raw IP that the daemon knows about from config).
5. **DNS server + per-IP allocation + routes.**
6. **systemd-resolved registration.**
7. **Operational: metrics, structured logs, SIGHUP reload, `aster-tunneld status`.**

Each step is independently shippable and testable. Step 4 is the natural MVP — proves the architecture end-to-end without DNS magic.

---

## 8. What this enables once shipped

A user runs `aster-tunneld` on their laptop pointed at a connector node inside their employer's VPC. They `ssh ws-1.internal.dev`, `psql db.internal.dev`, browse `wiki.internal.dev` — all without a VPN, without opening any inbound ports on the connector, with per-service authorisation handled by the connector's RPC handler. The connector node is the only thing that touches the corporate network; the user's laptop just speaks Aster QUIC to one connector, and bytes flow.

That's the OpenZiti Edge-Tunneler value proposition, built on Aster's identity + RPC + tunneling primitives instead of Ziti's edge protocol.

---

## 9. Addendum: rayfish comparison (2026-07-06)

[rayfish](https://github.com/rayfish) is a mesh VPN built on iroh — the nearest prior art on the same transport. Reviewed from a local checkout (`~/dev/github/rayfish/rayfish`). It sits at a different layer than this design, and the differences are instructive.

### 9.1 Their mechanism

WireGuard/Tailscale-shaped L3 mesh, not a proxy:

- Root daemon owns a TUN (MTU 1280, IPv6 minimum) and one shared iroh `Endpoint`; one QUIC connection per peer.
- **Each raw IP packet rides one unreliable QUIC datagram** (`send_datagram`). No streams for data. The end-hosts' kernel TCP stacks do loss recovery, so the tunnel never retransmits payload — no TCP-over-TCP meltdown, at the cost of per-packet (not per-flow) processing.
- Per-network ALPN embeds a wire-protocol version (`rayfish/net/<version>/<netkey-prefix>`): incompatible peers fail at the TLS handshake, no in-band version negotiation at all.
- Peer addressing is coordinator-free: stable IPs in `100.64.0.0/10` / `200::/7` derived from each peer's Ed25519 identity.
- Data plane is three tasks: single TUN-read loop (parse → userspace firewall → peer lookup → `send_datagram`), one `read_datagram` reader per peer (anti-spoof: source IP must equal the peer's derived mesh IP), single TUN-writer task.

### 9.2 Their efficiency work (worth stealing)

The code is measurement-driven — Criterion per-packet microbenches keep the *pre-optimization copy paths as fixtures* so the zero-copy delta is regression-guarded, plus an iperf3 e2e harness that scores the direct-vs-tunnel *ratio*, not absolute Mbit/s.

- **Zero-copy handoff both directions**: TX slices packets from a pooled 64 KiB `BytesMut` via `split_to(n).freeze()` (one alloc per ~50 packets; the `Bytes` handed to quinn keeps the chunk alive); RX passes the datagram `Bytes` through by refcount.
- **Transport config tuned to shape**: `send_fairness(false)` (no competing data streams), GSO pinned on explicitly so it can't silently regress, Cubic kept with a documented BBR3 deferral.
- **Drop-newest backpressure**: check `datagram_send_buffer_space()` before sending; drop the *new* packet rather than let quinn evict the oldest queued one. Keeps the single TUN read loop non-blocking — no cross-peer head-of-line blocking.
- RFC 1624 incremental checksum updates for their in-path port NAT instead of full recompute.

### 9.3 Their ceiling

One syscall per ≤1280-byte packet on the TUN side, both directions (plain `read_buf` / `write_all` per packet). No `IFF_VNET_HDR`/GRO/GSO coalescing on the TUN fd — the technique that gave Tailscale its userspace throughput jump by moving ~64 KiB per syscall. Every 1280 bytes pays: TUN syscall → parse → firewall eval → AEAD seal → UDP send (GSO helps only that last hop). Their own bench notes concede single-stream TCP is CPU-bound with "userspace TUN + QUIC datagram encryption" as the bottleneck. Userspace-Tailscale-class; nowhere near kernel WireGuard.

### 9.4 Implications for this design

- **Layer choice validated, in our favour for our use case.** Rayfish's L3/datagram model buys transparency (ICMP, UDP, any protocol, "same LAN" semantics) that we explicitly don't target. Our smoltcp-terminate + splice-over-reliable-stream model should be *more* CPU-efficient for bulk flows: bytes move in large chunks with QUIC doing loss recovery, instead of parse+firewall+seal per ≤1280 B packet; and the app's TCP handshake terminates at the local daemon (zero-RTT connect from the app's perspective).
- **Adopt the ALPN-embedded protocol version** for Aster tunnel ALPNs — transport-enforced version gating, zero in-band cost.
- **Adopt drop-newest via `datagram_send_buffer_space()`** wherever we do datagram relay (aster-expose Stage B / future `TunnelTarget::Udp` §5.1) — same problem shape.
- **Adopt the pooled `split_to/freeze` handoff** on any per-packet path, and their bench discipline (copy-path fixtures pinned in Criterion; ratio-based e2e numbers).
- **The unclaimed lever**: TUN vnet_hdr GRO/GSO offloads. Neither rayfish nor this spec uses them. For us they'd only matter on the TUN↔smoltcp boundary; revisit after profiling (consistent with §6's "eBPF later" stance).
- Their fixed-UDP-listen-port trick (stable manually-forwardable port across daemon restarts) is cheap and worth considering for connector nodes.
