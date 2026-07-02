# Connectivity spike: does a Fly.io edge get a *direct* Aster connection home?

Settles the open question in `docs/_internal/working_ideas/aster-expose-portal-webrtc.md` §11:
Cloudflare Containers can't do outbound UDP, so iroh there is **relay-only**. Fly.io
is UDP-native — this spike measures whether a Fly container hole-punches a **Direct**
path to a home-NAT node, and how much faster direct is than relay.

Two nodes (both the `aster` crate, raw custom-ALPN echo — no RPC/Fory):

| Node | Where | Role |
|------|-------|------|
| `probe_local` (`aster/examples/probe_local.rs`) | this Mac, behind home NAT | dial target; echoes 4 KB frames |
| `probe_edge` (`aster/examples/probe_edge.rs`)   | Fly.io container | dials home, logs **Direct/Relay + RTT** per ping |

`probe_edge` reports, per ping, the selected QUIC path (`PathRemote::Direct` vs
`Relay`) and both app-level and QUIC RTT, plus a summary (time-to-direct, relay
vs direct RTT percentiles).

## 1. Start the home node (leave it running)

```bash
cargo run -p aster --example probe_local
```
Copy the `PROBE_TICKET=…` token it prints (one line: `<node_id>@<relay_url>@<addrs>`).
Keep it running — the address is tied to this process.

> Not the idiomatic `aster1` ticket: that compact format can't carry a
> DNS-hostname relay (the default relays are hostnames), and Aster only has mDNS
> discovery — so a remote edge needs the relay URL spelled out to bootstrap the
> hole-punch. Hence this relay-carrying token.

## 2. (Optional) local baseline

In another terminal, confirm the harness goes Direct over LAN:
```bash
PROBE_TICKET='<paste>' PROBE_PINGS=8 cargo run -p aster --example probe_edge
```
Expect `reached direct : yes, after ~Nms` and sub-ms RTT.

## 3. Deploy the edge to Fly (the real test)

Run from the **repo root** (the build context must be the repo — the example
depends on the workspace):

```bash
fly launch --no-deploy --config spikes/connectivity/fly.toml
fly secrets set PROBE_TICKET=<paste the token from step 1> \
    --config spikes/connectivity/fly.toml
fly deploy --config spikes/connectivity/fly.toml .
fly logs   --config spikes/connectivity/fly.toml
```

Set `primary_region` in `fly.toml` to the Fly region nearest the home node for
best-case direct RTT (it doesn't affect *whether* direct works — that's about UDP
+ NAT — only the latency number).

## 4. Read the result

In `fly logs`, look for the path class and the summary:
- **`reached direct : yes, after … ms`** + `direct app_rtt` populated → Fly
  hole-punches home. CF (relay-only) would never print this. ✅ Use Fly.
- **`reached direct : NO — relay only`** → even Fly stays relayed (both ends hard
  NAT, or Fly egress NAT is symmetric). Compare the `relay app_rtt` to decide if
  relayed signaling is acceptable anyway (it's a one-shot offer, §1/§8 of the doc).

Tear down: `fly apps destroy <app>`.

## Notes
- `probe_edge` runs forever (`PROBE_PINGS=0`) printing a rolling summary every 20
  pings, so `fly logs` stays live. Set `PROBE_PINGS=N` to stop after N.
- No inbound ports — it's a pure outbound prober, so no `[[services]]`.
- The Dockerfile uses **cargo-chef**: the iroh/noq-fork dependency compile is a
  cached layer. The first build is cold (~15 min); after that, **source-only
  edits to the prober rebuild in ~1–2 min** (deps reused), and dep changes re-cook.
- Re-pointing at a new home node needs **no rebuild** — only the secret changes:
  `fly secrets set PROBE_TICKET=<token> --config spikes/connectivity/fly.toml && fly apps restart aster-probe-edge`.
