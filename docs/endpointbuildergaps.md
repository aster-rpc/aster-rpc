# Endpoint Builder FFI Gap Analysis

> **Sources:** `iroh/iroh/src/endpoint.rs` — `iroh::endpoint::Builder`
>
> This document is a developer reference for the iroh Endpoint builder options that exist in upstream Rust but are **not yet exposed** via the Python FFI (`aster_transport_core` → `bindings/aster_rs`). Use this to decide which options to prioritise for FFI exposure.

---

## Table of Contents

1. [How the Layers Connect](#1-how-the-layers-connect)
2. [Currently Exposed Options](#2-currently-exposed-options)
3. [Missing Options (Detailed Reference)](#3-missing-options-detailed-reference)
4. [Recommendations](#4-recommendations)

---

## 1. How the Layers Connect

```
Python bindings (bindings/aster_rs/src/net.rs)
    └── EndpointConfig (Python dataclass)
            └── CoreEndpointConfig (aster_transport_core/src/lib.rs)
                    └── build_endpoint_config() → iroh::Endpoint::builder
                            └── .bind() → Endpoint
```

**`CoreNode::memory()` and `CoreNode::persistent()`** bypass `EndpointConfig` entirely — they call `Endpoint::bind(presets::N0)` directly. Any new builder options must also be wired through `CoreNode` variants if they're needed at the full-node level.

---

## 2. Currently Exposed Options

| Builder method | FFI field | Notes |
|---|---|---|
| `secret_key` | `secret_key: Vec<u8>` (32 bytes) | ✅ |
| `alpns` | `alpns: Vec<Vec<u8>>` | ✅ |
| `relay_mode` | `relay_mode: str` + `relay_urls: Vec<String>` | ✅ |
| `hooks` | `enable_hooks: bool` + `hook_timeout_ms: u64` | ✅ |
| *(enable_discovery)* | `enable_discovery: bool` | ✅ |
| *(enable_monitoring)* | `enable_monitoring: bool` | ✅ |

`relay_mode` string values: `"default"` | `"staging"` | `"disabled"` | `"custom"` (requires `relay_urls`)

---

## 3. Missing Options (Detailed Reference)

### 3.1 `bind_addr` — Socket Binding Control

**Builder method:** `fn bind_addr<A>(self, addr: A) -> Result<Self, InvalidSocketAddr>`

**What it does:**
Controls which network interface and port the underlying UDP/QUIC socket binds to. By default, iroh binds to `0.0.0.0:0` (all IPv4 interfaces, random port) and `[::]:0` (all IPv6 interfaces, random port).

**Input format:** A string like `"0.0.0.0:0"`, `"127.0.0.1:8080"`, `"192.168.1.100:0"`, `"[::1]:0"`, or `":9999"` (all interfaces, port 9999). The port `0` means "assign a random free port."

**Why it matters:**
- **Fixed server port:** In a server process, you want a predictable port so peers know where to reach you. Set `":12345"`.
- **Local-only binding:** Set `"127.0.0.1:0"` to keep the node unreachable from outside the machine (useful for local dev, single-machine multi-node testing).
- **Specific LAN interface:** On a machine with multiple NICs, bind to the LAN IP to avoid exposing the node on a guest/wifi interface.
- **Port reuse:** On some systems, binding to a fixed port after a crash requires `SO_REUSEPORT` — iroh handles this internally.

**⚠️ Behaviour notes:**
- Calling `bind_addr` with an IPv4 address **replaces** the default `0.0.0.0:0` IPv4 socket. Call `bind_addr` with an IPv6 address to replace the default `[::]:0` IPv6 socket.
- To add **additional** sockets alongside the defaults, use `bind_addr_with_opts` with prefix_len control instead.
- If the port is already in use, binding fails. If you want it to silently skip a failed bind, use `bind_addr_with_opts` with `set_is_required(false)`.

**Use cases for Python/FFI:**
```python
# Server that announces a fixed port
EndpointConfig(alpns=[...], bind_addr=":9000")

# Local-only node (not exposed externally)
EndpointConfig(alpns=[...], bind_addr="127.0.0.1:0")

# Specific interface on a multi-NIC machine
EndpointConfig(alpns=[...], bind_addr="192.168.1.50:0")
```

---

### 3.2 `bind_addr_with_opts` — Advanced Socket Binding

**Builder method:** `fn bind_addr_with_opts<A>(self, addr: A, opts: BindOpts) -> Result<Self, InvalidSocketAddr>`

**What it does:**
Same as `bind_addr`, but with additional routing and failure-control options. This is the full form; `bind_addr` is just `bind_addr_with_opts(..., BindOpts::default())`.

**`BindOpts` fields:**

#### `prefix_len: u8`
- **What:** Sets the subnet prefix length for routing decisions. Controls which socket is used for outbound connections based on destination address.
- **Default:** `0` (equivalent to `/0` — matches everything, becomes the default route).
- **Example:** `set_prefix_len(24)` for IPv4 = `/24` subnet. Outbound connections to IPs in `192.168.1.x` use this socket; others fall back to the default route.
- **Why:** On multi-homed hosts, you may want connections to the LAN to exit via the LAN interface, not the VPN interface. Setting per-interface prefix lengths gives you control.
- **Note:** For most Python use cases the default `0` is fine.

#### `is_required: bool`
- **What:** Controls whether a failed bind is fatal or silently ignored.
- **Default:** `true` (fatal — if the socket can't bind, `bind()` returns an error).
- **When false:** If the bind fails (e.g. port already in use), that socket is skipped and the endpoint uses whatever other sockets succeeded.
- **Use case:** Graceful fallback when you want to share a port with another service or don't care if binding to a specific interface fails.

#### `is_default_route: bool`
- **What:** Marks this socket as the "default route" for outbound connections that don't match any more-specific subnet.
- **Default:** `true` implicitly for prefix_len=0 sockets; `false` for non-zero prefix lengths.
- **Why:** Useful when binding multiple sockets and only one should handle the "catch-all" traffic.
- **⚠️ Multiple default routes are non-deterministic.** Only set `is_default_route=true` on one socket.

**Use case examples:**
```python
# Bind to LAN interface with /24 routing, fail gracefully if port is in use
BindOpts.default().set_prefix_len(24).set_is_required(False)

# Explicit default route on unspecified address
BindOpts.default().set_is_default_route(True)
```

---

### 3.3 `clear_ip_transports` — Disable Direct IP Connections

**Builder method:** `fn clear_ip_transports(self) -> Self`

**What it does:**
Removes all IP-based (UDP/QUIC direct) socket transports from the endpoint. The endpoint will only communicate via relay servers.

**Why it matters:**
- **Firewall-restricted environments:** Some environments block all UDP/QUIC traffic except via specific relay endpoints. Disabling IP transports avoids unnecessary socket binding and connection attempts.
- **Relay-only topology:** For a simple relay-assisted topology with no need for hole-punching or direct connections, this simplifies the connection model.

**What it doesn't do:**
Does not affect relay transports. You still need at least one relay configured to establish connectivity.

**Python use case:**
```python
# Node behind strict firewall — relay only
EndpointConfig(alpns=[...], clear_ip_transports=True)
```

---

### 3.4 `clear_relay_transports` — Disable Relay Connections

**Builder method:** `fn clear_relay_transports(self) -> Self`

**What it does:**
Removes all relay-based transports. The endpoint can only communicate over direct IP connections (no hole-punching or relay fallback).

**Why it matters:**
- **Local network only:** For nodes on the same LAN or VPN where direct connectivity is guaranteed.
- **Zero-trust relay environments:** Where relay infrastructure is not available or not trusted.

**⚠️ Warning:** If you clear both IP and relay transports, the endpoint cannot connect to any peer. Always keep at least one.

**Python use case:**
```python
# Local network cluster — direct QUIC only
EndpointConfig(alpns=[...], clear_relay_transports=True)
```

---

### 3.5 `dns_resolver` — Custom DNS Resolution

**Builder method:** `fn dns_resolver(self, dns_resolver: DnsResolver) -> Self`

**What it does:**
Replaces iroh's default DNS resolver with a custom one. iroh uses DNS to resolve relay hostnames and any configured address-lookup services.

**Input:** An `iroh::dns::DnsResolver` instance. This is configured separately — it's not a simple string. In practice you'd build a resolver with specific upstream DNS servers.

**Why it matters:**
- **Corporate networks with split-horizon DNS:** Resolve internal relay hostnames that external DNS can't see.
- **Custom DNS-over-HTTPS (DoH) resolvers:** Route relay DNS lookups through a specific DoH provider.
- **Testing with controlled DNS:** Fake DNS responses in tests.

**Python use case (advanced):**
This is unlikely to be a high priority for FFI exposure unless you have a specific DoH or corporate DNS requirement. The default resolver (system DNS) is correct for 99% of use cases.

---

### 3.6 `transport_config` — QUIC Transport Tuning

**Builder method:** `fn transport_config(self, transport_config: QuicTransportConfig) -> Self`

**What it does:**
Sets the QUIC transport parameters governing connection behaviour — stream limits, timeouts, congestion control, etc. The default is tuned for general internet use.

**Key sub-options:**

| Parameter | Default | What it does |
|---|---|---|
| `max_concurrent_bidi_streams` | varies | Max simultaneous bidirectional streams per connection |
| `max_concurrent_uni_streams` | varies | Max simultaneous unidirectional streams |
| `initial_rtt` | 333ms | Initial round-trip time estimate |
| `max_idle_timeout` | 60s | Connection lifetime if no traffic |
| `enable_0rtt` | true | Allow zero-round-trip resumption |

**Why it matters:**
- **`max_concurrent_*_streams = 0`:** For pure request-response protocols where the remote should never open streams back to you (stops peers from initiating).
- **`initial_rtt`:** Lower values (e.g. 100ms) for LAN/low-latency networks. Higher values for high-latency/satellite links.
- **`enable_0rtt = false`:** For security-sensitive apps where 0-RTT replay attacks are a concern.

**Python use case (advanced):**
This is the deepest lever in the stack. Most Python use cases should use the defaults. It makes sense to expose if you need specific QUIC semantics for a custom protocol.

---

### 3.7 `proxy_url` — HTTP/S Proxy Support

**Builder method:** `fn proxy_url(self, url: Url) -> Self`

**What it does:**
Routes all HTTP/HTTPS traffic from iroh (relay HTTPS connections, address-lookup services) through an HTTP/SOCKS proxy.

**Input:** A URL like `"http://proxy.example.com:8080"` or `"socks5://localhost:1080"`.

**Why it matters:**
- **Corporate networks:** Environments where all outbound HTTP/S traffic must go through an HTTP proxy.
- **Privacy:** Routing relay connections through a specific proxy.
- **Testing:** Capturing iroh's HTTP traffic in a proxy for debugging.

**`proxy_from_env`:** Also available — reads `HTTP_PROXY`, `HTTPS_PROXY` (or lowercase variants) from the environment automatically.

**Python use case:**
```python
EndpointConfig(alpns=[...], proxy_url="http://corporate-proxy:8080")
```

---

### 3.8 `ca_roots_config` — Custom TLS Certificate Authority

**Builder method:** `fn ca_roots_config(self, ca_roots_config: CaRootsConfig) -> Self`

**What it does:**
Sets custom root CA certificates for verifying TLS certificates presented by **external services** — iroh relays, pkarr servers, DNS-over-HTTPS resolvers. Does **not** affect iroh's own cryptographic authentication (which uses its own key-based auth).

**Input:** A `CaRootsConfig` containing one or more CA certificate files/data. Iroh ships with a set of well-known public CA roots by default.

**Why it matters:**
- **Enterprise PKI:** Environments using a private CA for internal relay infrastructure.
- **Testing with self-signed certs:** Point at your test CA root to accept self-signed relay certificates.

**Python use case (advanced):**
Most Python users will never need this. The default CA roots handle public iroh relay infrastructure correctly.

---

### 3.9 `keylog` — TLS Key Logging for Debugging

**Builder method:** `fn keylog(self, keylog: bool) -> Self`

**What it does:**
Enables logging of TLS pre-master secrets to a file (controlled by the `SSLKEYLOGFILE` environment variable). Used with Wireshark or similar tools to decrypt captured TLS traffic for debugging.

**Why it matters:**
- **Debugging QUIC/TLS issues:** Decrypting live traffic to inspect packet contents.
- **⚠️ Security warning:** Writing TLS keys to a file is a security risk in production — anyone with the keylog file can decrypt all traffic.

**Python use case:**
```python
# Development only — set SSLKEYLOGFILE env var before creating the endpoint
EndpointConfig(alpns=[...], keylog=True)
```

---

### 3.10 `max_tls_tickets` — TLS Session Ticket Cache Size

**Builder method:** `fn max_tls_tickets(self, n: usize) -> Self`

**Default:** `256` (consumes ~150 KiB)

**What it does:**
Controls how many TLS session tickets are cached for 0-RTT connection resumption. Each ticket allows a client to resume a session without a full TLS handshake.

**Why it matters:**
- **High connection churn:** If you're creating many short-lived connections to many different clients, increase this to avoid full handshakes on every reconnect.
- **Memory-constrained environments:** Decrease to reduce memory footprint.

**Python use case:**
Unlikely to need tuning for most Python applications.

---

### 3.11 `portmapper_config` — NAT Port Mapping (UPnP/NAT-PMP)

**Builder method:** `fn portmapper_config(self, config: PortmapperConfig) -> Self`

**What it does:**
Configures the port mapper service that attempts to create port mappings via UPnP or NAT-PMP. This helps nodes behind NAT routers become directly reachable without manual port forwarding.

**Options:** `PortmapperConfig::Enabled` (default) or `PortmapperConfig::Disabled`.

**Why it matters:**
- **`Disabled`:** In environments where UPnP is blocked (corporate networks), or where port mapper probes cause issues.
- **`Enabled`:** For home/office users behind NAT — improves direct connectivity without manual router configuration.

**Python use case:**
```python
# Corporate network — disable port mapper to avoid failed UPnP probes
EndpointConfig(alpns=[...], portmapper_config="disabled")
```

---

### 3.12 `address_lookup` — Custom Peer Discovery Services

**Builder method:** `fn address_lookup<T: AddressLookupBuilder>(self, address_lookup: T) -> Self`

**What it does:**
Adds an additional address-lookup service for peer discovery beyond the default mechanisms. Iroh supports multiple address lookup services simultaneously (they are combined via `ConcurrentAddressLookup`).

**Available lookup services:**
- **`DnsAddressLookup`:** Resolve a DNS name to discover a peer's addresses.
- **`PkarrResolver`:** Use pkarr (Public Key Absent Resource Records) for signed DNS-based discovery.
- **`MemoryLookup`:** In-memory lookup for local/testing scenarios.
- **`ConcurrentAddressLookup`:** Combines multiple lookups.

**Why it matters:**
- **Custom discovery:** If you have a custom peer registry or DNS-based service for your application.
- **pkarr integration:** For fully decentralised discovery without a relay server.

**Python use case:**
This is an advanced feature for custom discovery topologies. The default relay-based discovery covers most use cases.

---

### 3.13 `addr_filter` — Filter Published Addresses

**Builder method:** `fn addr_filter(self, filter: AddrFilter) -> Self`

**What it does:**
Applies a filter to addresses before they are published via address-lookup services. Controls what addressing information is shared with the network.

**Why it matters:**
- **Privacy:** Filter out local/interface addresses you don't want published.
- **Selective sharing:** Only publish relay URLs and specific interfaces.

**Python use case (advanced):**
Most Python applications should let iroh publish all discovered addresses.

---

### 3.14 `user_data_for_address_lookup` — Custom Metadata in Discovery

**Builder method:** `fn user_data_for_address_lookup(self, user_data: UserData) -> Self`

**What it does:**
Attaches arbitrary user-defined metadata (a string) to address-lookup records. When other nodes discover your endpoint, they receive this metadata alongside the addressing info.

**Why it matters:**
- **Application metadata:** e.g. node name, role, version, capabilities. Other peers can inspect this without connecting.
- **Routing hints:** e.g. which region/cluster this node belongs to.

**Python use case:**
```python
EndpointConfig(alpns=[...], user_data="role=backup-server,v=1.0")
```

---

## 4. Recommendations

### Priority 1: `bind_addr` (highest impact)

The most frequently missing option. Every server deployment needs a predictable port. The implementation is minimal:

```rust
// In build_endpoint_config() in aster_transport_core/src/lib.rs:
if let Some(ref bind_addr) = config.bind_addr {
    builder = builder.bind_addr(bind_addr)?;
}
```

Add `bind_addr: Option<String>` to `CoreEndpointConfig`, expose it from `EndpointConfig` in Python, and wire it through.

### Priority 2: `clear_ip_transports` and `clear_relay_transports`

Simple boolean flags — one-liners to add. Useful for specific network topologies.

### Priority 3: `portmapper_config`

A simple string enum (`"enabled"` / `"disabled"`) — low effort, useful for enterprise/CI environments.

### Priority 4: `proxy_url` / `proxy_from_env`

Useful for corporate networks. Worth adding if there are users behind HTTP proxies.

### Lower priority (only if there's a specific need)

- `transport_config` — deep QUIC tuning; most Python apps don't need it
- `dns_resolver` — custom DNS is niche
- `ca_roots_config` — enterprise PKI only
- `address_lookup` — custom discovery; rarely needed
- `keylog` — debugging only
- `max_tls_tickets` — memory tuning

---

*Generated from `iroh/iroh/src/endpoint.rs`, April 2026.*
