# Self-Hosted Relay Infrastructure

Status: **planning**
Date: 2026-04-08

## Why Self-Host?

n0's default relays (`use1-1.relay.n0.iroh-canary.iroh.link`, etc.) are free and work well. But for production Aster deployments:

1. **Access control** — prevent non-Aster users from consuming relay bandwidth
2. **Data sovereignty** — traffic stays on your infrastructure
3. **Reliability** — no dependency on n0's uptime or policy changes
4. **Branding** — `relay.aster.dev` instead of `iroh-canary.iroh.link`
5. **Rate limits** — configure per-client bandwidth to your needs

## Architecture

```
┌────────────────┐     ┌────────────────┐
│  relay-eu      │     │  relay-us      │    ← HTTPS + QUIC relay servers
│  (Frankfurt)   │     │  (Virginia)    │       restrict access by endpoint ID
└───────┬────────┘     └───────┬────────┘
        │                      │
        │    ┌─────────────┐   │
        └────┤ iroh-dns    ├───┘            ← optional: peer discovery by public key
             │ (pkarr)     │                   NOT required if distributing tickets
             └─────────────┘
```

**Do we need our own DNS server?** Probably not initially. Aster distributes connection info via:
- `aster1...` compact tickets (contain relay IP directly)
- `.aster-identity` files (contain endpoint_id, DNS resolves the rest)
- aster.site directory (future: resolves handle → endpoint)

n0's DNS server resolves endpoint_id → relay URL. If we run our own relay, we'd either:
- Keep using n0's DNS (our endpoints publish their relay URL there via pkarr)
- Or run our own DNS server (full independence)

**Recommendation:** Start with self-hosted relay only. Keep n0's DNS for discovery. Switch DNS later if needed.

## Relay Access Control

iroh-relay has four access modes:

### 1. Everyone (default)
```toml
access = "everyone"
```
No restriction. Anyone with an iroh endpoint can connect.

### 2. Allowlist — only specific endpoint IDs
```toml
access.allowlist = [
  "bda1158f1ef9de5eaa822009a22c18bc...",
]
```
Static list of allowed Ed25519 public keys. Good for small deployments but doesn't scale — you'd need to update the config and restart for every new node.

### 3. Denylist — everyone except specific IDs
```toml
access.denylist = [
  "abc123...",
]
```
Block specific bad actors. Not useful for our "restrict to Aster only" goal.

### 4. HTTP Webhook (recommended for Aster)
```toml
access.http.url = "https://auth.aster.dev/relay-access"
access.http.bearer_token = "secret-token"
```
The relay POSTs to your URL with header `X-Iroh-NodeId: <hex endpoint id>`. Your service returns `200` + body `true` to allow, anything else to deny.

**This is the right approach for Aster.** The webhook service can:
- Check if the endpoint_id belongs to an enrolled Aster node
- Query a database of active Aster deployments
- Rate-limit by organization
- Log access for billing/analytics

## Recommended Relay Config

```toml
# /etc/iroh-relay/config.toml

enable_relay = true
http_bind_addr = "[::]:80"
enable_quic_addr_discovery = true
enable_metrics = true
metrics_bind_addr = "[::]:9090"

[tls]
https_bind_addr = "[::]:443"
quic_bind_addr = "[::]:7842"
hostname = "relay-eu.aster.dev"
cert_mode = "LetsEncrypt"
contact = "ops@aster.dev"
cert_dir = "/var/lib/iroh-relay/certs"
prod_tls = true

[limits]
accept_conn_limit = 200.0        # connections/sec
accept_conn_burst = 512
[limits.client.rx]
bytes_per_second = 2097152       # 2 MB/s per client
max_burst_bytes = 4194304        # 4 MB burst

[access]
# Restrict to Aster nodes via webhook
access.http.url = "https://auth.aster.dev/relay-access"
access.http.bearer_token = "${RELAY_AUTH_TOKEN}"
```

## Deployment Options

### Docker (simplest)
```bash
docker run -d --name iroh-relay \
  -v /etc/iroh-relay:/config \
  -p 80:80 -p 443:443 -p 7842:7842/udp -p 3478:3478/udp \
  n0computer/iroh-relay:latest \
  --config /config/config.toml
```

### Systemd (bare metal)
Build from source (`cargo install iroh-relay`) or use the binary from iroh releases. The iroh repo has the relay server at `iroh/iroh-relay/`.

### Cloud providers
- **Fly.io** — good for multi-region, handles TLS termination
- **Hetzner** — cheap, good EU presence
- **AWS/GCP** — standard, multi-region with load balancing

## Recommended Relay Regions

Start with 2, expand as needed:

| Region | Hostname | Location |
|--------|----------|----------|
| EU | `relay-eu.aster.dev` | Frankfurt |
| US East | `relay-us.aster.dev` | Virginia |

Add later:
| US West | `relay-usw.aster.dev` | Oregon |
| APAC | `relay-ap.aster.dev` | Singapore |

## What Needs to Change in Aster

### 1. Fix `RelayMode::Custom` (bug)

`core/src/lib.rs` line ~718: when `relay_mode = "custom"` with `relay_urls`, the code falls through to `RelayMode::Default` instead of building a `RelayMode::Custom(RelayMap)`.

Fix:
```rust
Some("custom") if !config.relay_urls.is_empty() => {
    let map = RelayMap::from_iter(config.relay_urls.iter().map(|url| {
        RelayConfig {
            url: url.parse::<Url>().expect("valid relay URL").into(),
            quic: Some(RelayQuicConfig::default()),
        }
    }));
    Ok(RelayMode::Custom(map))
}
```

### 2. Aster config: `relay_urls` field

Already exists in `AsterConfig` but needs the core fix above to take effect. Usage:

```toml
# aster.toml
[network]
relay_mode = "custom"
relay_urls = [
    "https://relay-eu.aster.dev",
    "https://relay-us.aster.dev",
]
```

Or via environment:
```bash
ASTER_RELAY_MODE=custom
ASTER_RELAY_URLS=https://relay-eu.aster.dev,https://relay-us.aster.dev
```

### 3. Default relay config for Aster

Once we have our own relays, we could add an `AsterRelayMode` that defaults to our relays instead of n0's:

```python
config = AsterConfig(relay_mode="aster")  # uses relay-eu + relay-us.aster.dev
```

This would be implemented as a `RelayMode::Custom` with our relay URLs hardcoded (with env override).

### 4. Webhook auth service

A small HTTP service that checks relay access requests:

```
POST /relay-access
X-Iroh-NodeId: bda1158f1ef9de5e...

Response: 200 "true" or 403 "false"
```

Implementation options:
- **Simple:** Static allowlist file, checked on each request
- **Database:** Query enrolled nodes from the aster.site directory DB
- **Stateless:** Check if the node_id was signed by a known root key (verify an enrollment credential on the fly — the relay auth service has the list of trusted root pubkeys)

The stateless option is elegant: any node with a valid Aster enrollment credential (signed by any registered root key) gets relay access. No database needed. The relay auth service just needs the set of trusted root public keys.

### 5. AsterTicket relay addresses

The compact ticket format already encodes relay as IP:port. When switching to self-hosted relays, tickets will contain our relay IPs instead of n0's. No format changes needed.

### 6. DNS publishing

If we keep using n0's DNS server (recommended initially), our endpoints publish their address (including our custom relay URL) via pkarr to `dns.iroh.link`. Other endpoints can discover them by public key. No change needed.

If we later self-host DNS:
- Run `iroh-dns-server` alongside the relays
- Configure `IROH_DNS_PKARR_RELAY` env var in our endpoints to point to our DNS server
- This requires changes to the `PkarrPublisher` configuration in the endpoint builder

## Migration Checklist

- [ ] Fix `RelayMode::Custom` bug in `core/src/lib.rs`
- [ ] Deploy relay-eu.aster.dev (Docker, LetsEncrypt)
- [ ] Deploy relay-us.aster.dev
- [ ] Build relay auth webhook (stateless credential check)
- [ ] Add `relay_mode = "aster"` shorthand to AsterConfig
- [ ] Test: node with custom relay connects to node on n0 relay (cross-relay)
- [ ] Test: node with custom relay connects to node on same relay
- [ ] Update default relay config in Aster to prefer our relays
- [ ] Monitor relay metrics (connections, bandwidth, latency)
- [ ] Optional: deploy iroh-dns-server for full independence
