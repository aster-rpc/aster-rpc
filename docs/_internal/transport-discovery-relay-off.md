# Transport Discovery and Relay-Off Mode

Status: **design note**
Date: 2026-06-06

## Problem

Aster's public transport docs currently blur three separate mechanisms:

1. Iroh's default n0 address lookup, which publishes and resolves endpoint
   address records through Pkarr/DNS infrastructure.
2. Aster's `discovery` / `local_discovery` switch, which only adds mDNS local
   network discovery.
3. Explicit address material, such as `aster1...` tickets or `NodeAddr`, which
   bypasses discovery by giving Iroh direct/relay hints up front.

This caused a bad operational assumption: if a caller has only an `EndpointId`
and relay transports are disabled, it may expect wide-area DNS/Pkarr discovery to
find a usable direct path. In the current Aster wiring, that is not a reliable
mode.

## Current Wiring

Aster builds Iroh endpoints through `Endpoint::builder(presets::N0)`:

- `core/src/lib.rs:1005` in `build_endpoint_config`.
- `core/src/lib.rs:1143` for default full-node construction.
- `core/src/lib.rs:1660` for `CoreNetClient::create`.

In the pinned Iroh fork, `presets::N0` installs:

- `PkarrPublisher::n0_dns()` for publishing this endpoint's address record.
- `DnsAddressLookup::n0_dns()` on native targets for resolving another
  endpoint's address record.
- `PkarrResolver::n0_dns()` on browser/WASM targets instead of native DNS.
- Default n0 relay mode.

Aster then applies its own config, including `.relay_mode(...)`, optional
transport clearing, and optional mDNS:

- `CoreEndpointConfig.enable_discovery` defaults to `false`.
- When `enable_discovery` is true, Aster adds `MdnsAddressLookup`.
- Rust facade `AsterConfigBuilder::discovery(true)` is documented and wired as
  local-network mDNS discovery, not global DNS/Pkarr discovery.

So the earlier claim that "Aster wires mDNS only and nothing else" is incomplete:
Aster does not explicitly add Pkarr/DNS in `core`, but the Iroh `N0` preset does.

## Pkarr, DNS, and DHT Terminology

Pkarr is a DHT-based naming system in the broader ecosystem. The current Aster
runtime, however, does not depend on `iroh-mainline-address-lookup` and does not
run a local mainline-DHT address lookup client.

The default path provided by `presets::N0` is the n0 Pkarr/DNS bridge:

- publish by HTTP to `https://dns.iroh.link/pkarr`;
- resolve by DNS under `dns.iroh.link` on native targets;
- resolve by HTTP Pkarr relay on browser targets.

It is fair to describe this as Pkarr-backed discovery. It is misleading to imply
that Aster nodes directly participate in a DHT by default.

## Relay-Off Behavior

The critical detail is the publish filter.

Iroh's default `PkarrPublisher` uses `AddrFilter::relay_only()`. This avoids
publishing local or private IP addresses to public infrastructure. It is the right
privacy default for relay-enabled endpoints.

When relays are disabled:

1. `presets::N0` still installs Pkarr/DNS lookup services.
2. Aster applies `RelayMode::Disabled`, removing relay transports.
3. Aster skips `endpoint.online()` because Iroh's online state waits for a home
   relay and would otherwise pend forever.
4. The endpoint may still discover local/direct addresses internally.
5. The default Pkarr publisher filters published address material to relay
   addresses only, which usually leaves nothing useful to publish.

Result: a peer that knows only the `EndpointId` should not expect WAN
reachability with relays disabled. DNS/Pkarr may be configured, but it may return
no usable address material. mDNS only helps on the same local network.

This applies both to `RelayMode::Disabled` and to configurations that remove
relay transports with `clear_relay_transports`.

## Mode Matrix

| Mode | Bare `EndpointId` over WAN | Same-LAN discovery | Notes |
|---|---:|---:|---|
| Relays default, mDNS off | Expected to work after publish | Not via mDNS | n0 Pkarr/DNS resolves relay address material. |
| Relays default, mDNS on | Expected to work after publish | Yes | Adds local mDNS as another lookup path. |
| Relays disabled, mDNS off | Not reliable | No | DNS/Pkarr exists, but default publish filter is relay-only. |
| Relays disabled, mDNS on | Not reliable | Yes | Local segment can work; WAN still needs explicit direct address material or custom publishing. |
| Full `NodeAddr` / ticket | Depends on contained hints | Depends on contained hints | Discovery is bypassed for the initial dial. |

## Guidance for Callers

For production/WAN connectivity:

- Prefer relay-enabled endpoints unless there is a strong reason to disable
  relays.
- If relays are disabled, exchange full `NodeAddr` / `aster1...` tickets that
  contain usable direct addresses.
- For static public servers, use explicit bind/external address configuration
  and an intentional publish policy before relying on bare `EndpointId`.
- Treat `discovery(true)` as mDNS/local discovery only.

For tests and local development:

- `Node::add_peer`, `add_node_addr`, and `add_ticket_addr` are deterministic and
  avoid timing/external infrastructure assumptions.
- mDNS is useful for same-LAN demos, but should not be used as proof of WAN
  discovery behavior.

## Design Options

### Option A: Documentation-Only Fix

Clarify public docs and Rust guide:

- Replace "DNS, distributed hash table, local network" with "configured address
  lookup services, such as n0 Pkarr/DNS and optional mDNS."
- State that Aster's `discovery` flag means mDNS only.
- State that relay-disabled nodes are not generally reachable by bare
  `EndpointId` over WAN unless direct addresses are explicitly distributed or
  published.

Pros:
- Low risk.
- Matches the current implementation.

Cons:
- Does not help users who intentionally want relay-free public direct
  connectivity.

### Option B: Expose Address Publish Policy

Add a config surface for Iroh `addr_filter`, for example:

```rust
pub enum PublishAddressPolicy {
    RelayOnly,
    DirectOnly,
    All,
}
```

Wire it through:

- `CoreEndpointConfig`.
- Rust `AsterConfigBuilder`.
- Python/TypeScript bindings if needed.
- Full-node and bare-net construction paths.

Suggested defaults:

- Relay-enabled: `RelayOnly`.
- Relay-disabled: still default to `RelayOnly` or `None` for privacy, but require
  an explicit opt-in for `DirectOnly`/`All`.

Pros:
- Enables intentional relay-free deployments with public direct addresses.
- Keeps privacy explicit.

Cons:
- Publishing direct addresses can leak private/LAN topology if misused.
- Needs careful docs, tests, and binding parity.

### Option C: Add Custom Discovery Services

Expose a higher-level Aster discovery abstraction rather than raw Iroh
`AddressLookupBuilder`, e.g.:

- self-hosted Pkarr/DNS endpoint;
- in-memory/static registry;
- Aster registry-backed endpoint resolution.

Pros:
- Gives Aster control over privacy and trust semantics.
- Can integrate with registry/admission design.

Cons:
- Larger API design.
- More infrastructure and conformance work.

## Recommended Path

Do Option A immediately. The docs currently overpromise in exactly the mode that
failed.

Then add a focused version of Option B:

1. Add `publish_address_policy` to `CoreEndpointConfig`.
2. Support `relay_only`, `direct_only`, and `all`.
3. Keep the default privacy-preserving.
4. Document that direct publishing is only for nodes with intentionally public or
   otherwise routable direct addresses.
5. Add tests that demonstrate:
   - relay-enabled bare `EndpointId` works through lookup;
   - relay-disabled bare `EndpointId` does not imply WAN reachability by default;
   - relay-disabled plus explicit direct address material works;
   - relay-disabled plus direct publish policy works in a controlled local
     lookup test.

Option C should wait until the registry and self-hosted discovery story is more
settled.

## Public Docs Corrections Needed

Transport concept page:

- Avoid saying "No DNS lookup" as an absolute statement. Aster users do not use
  DNS names as service addresses, but Iroh may use DNS/Pkarr internally as an
  address lookup mechanism.
- Clarify that a bare `EndpointId` can only be resolved through configured
  address lookup services.
- Clarify that local network discovery is opt-in mDNS.

Rust getting-started guide:

- Change the feature table from "DNS/mDNS resolution" to "mDNS local discovery."
- Clarify the sentence "relay/discovery" near `rpc_connect(&peer_id)`:
  "reachable by id through configured address lookup, mDNS, or a prior
  `add_ticket_addr`."

## Open Questions

1. Should relay-disabled mode automatically switch the publish filter to
   `direct_only` when an explicit public `bind_addr` or external address is
   configured?
2. Should Aster expose external address configuration before exposing a direct
   publish policy?
3. Do we want public docs to mention Pkarr/DNS internals, or keep them in an
   "implementation detail" box?
4. Should Aster's high-level `AsterServer` ever allow relay-disabled operation
   without requiring a ticket/direct-address distribution path?
