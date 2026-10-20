# Getting started with the `aster` Rust crate

`aster` is the first-class Rust API for the Aster peer-to-peer stack: a node with
content-addressed **blobs**, CRDT **docs**, **gossip**, connection **admission**
(Gate 0) + root-signed **enrollment** credentials, and — behind the `rpc` feature — a
**Rust-native RPC framework** (services, clients, streaming, capability checks,
interceptors, and cross-binding contract publication).

> Aster 0.3 is a preview release. RPC interoperability is exercised by the
> Rust, Python, TypeScript, Go, Java and .NET matrix in `tests/interop/`.
> See “Current limits” for restrictions that affect application design.

---

## 1. Add the dependency

Released Rust crates are distributed from Aster's public Forgejo Cargo registry.
Configure the registry in `.cargo/config.toml` and select one released Aster
version:

```toml
# .cargo/config.toml
[registries.aster]
index = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
```

```toml
# Cargo.toml
[dependencies]
aster = { version = "0.3", registry = "aster", features = ["rpc"] }
```

Commit `Cargo.lock` in applications so the resolved patch is reproducible.
Consumers do not copy Aster's fork pins. See
[Consuming the Aster Rust SDK](rust-sdk-consumer-guide.md)
for registry setup, native Iroh/Fory/Salvo interop and version policy.

To define **typed RPC payloads** you also need the Apache Fory derive in scope
(the `#[derive(ForyStruct)]` macro expands to `::fory_core::` paths, so both
crates must be direct dependencies):

```toml
fory-core   = "=1.7.6"
fory-derive = "=1.7.6"
```

---

## 2. Start a node

```rust
use aster::{AsterConfig, Node, RelayMode};

# async fn run() -> aster::Result<()> {
let node = Node::start(
    AsterConfig::builder()
        .persistent("/var/lib/my-app")   // omit for an in-memory node
        .relay(RelayMode::Default)
        .build(),
)
.await?;

println!("node id: {}", node.id());

// Identity is restart-stable only if you persist and restore the secret key —
// the on-disk store does NOT keep the endpoint key.
let secret = node.export_secret_key()?;

node.shutdown().await; // flushes the store, then tears down
# Ok(())
# }
```

`Node` is cheap to **clone** (no `Arc<Node>` needed to share it). Note that
`shutdown(self)` closes the *shared* underlying node — if you hand out clones,
only the owner should shut it down.

Blobs / docs / gossip handles hang off the node (feature-gated):

```rust
let hash = node.blobs().add_bytes(b"hello".to_vec()).await?;
let doc  = node.docs().create().await?;
let topic = node.gossip().subscribe(topic_id).await?;
```

> Running an **RPC producer**? Prefer [`AsterServer`](#43-serve-it) — it starts
> the node, registers the ALPNs, manages identity/persistence, and serves your
> services from one builder. Drop to `Node` directly only for non-RPC use or
> custom wiring.

### 2.1 Identities and keypairs

An Aster identity is a **32-byte Ed25519 secret key**; its public key *is* the
`NodeId` peers see. If you don't supply one, `Node::start` generates a fresh key
each boot (a new identity every restart). To control identity yourself — a
restart-stable node, or an offline **root/vendor** key that signs enrollment
credentials —
mint a key with `SecretKey::generate()`:

```rust
use aster::{AsterConfig, Node, SecretKey};

# fn demo() -> aster::Result<()> {
// Fresh random identity (OS CSPRNG). Persist `to_bytes()` to keep it — losing
// the secret loses the identity; the public key is what others verify against.
let secret = SecretKey::generate();
let public = secret.public_key();        // the NodeId form of this identity
println!("identity: {}", public.to_hex());

// Restore later from the 32 bytes you stored.
let restored = SecretKey::from_bytes(secret.to_bytes());

// Pin a node to it (otherwise the node makes its own key).
let cfg = AsterConfig::builder().secret_key(restored).build();
let _node_fut = Node::start(cfg);
# Ok(())
# }
```

`SecretKey::generate()` is the same construction as the C FFI
(`aster_secret_key_generate`) and the Python binding (`ed25519_generate_keypair`)
— 32 CSPRNG bytes — so a key minted in any binding is usable from the others.

**On disk, a node key is a secret key file**: the 32 bytes as 64 hex characters
(plus a trailing newline). This is the one identity-file format shared by the
Rust SDK and every binding. `SecretKey::{to_hex, from_hex}` give the text form;
`SecretKey::read_file`, `read_file_if_exists`, `write_file` (refuses to
overwrite, mode `0600` on Unix, creates parent dirs) and `load_or_generate`
handle the file. Most code just points the config at it and lets `Node::start`
load-or-create it:

```rust
# use aster::{AsterConfig, Node};
# async fn run() -> aster::Result<()> {
let node = Node::start(
    AsterConfig::builder()
        .secret_key_file("/var/lib/my-app/node.key") // or ASTER_SECRET_KEY_FILE
        .persistent("/var/lib/my-app")
        .build(),
)
.await?;
# node.shutdown().await;
# Ok(())
# }
```

If the file exists its key is used (and an explicit `secret_key` that disagrees
is an error); if not, the explicit `secret_key` — or a fresh key — is written
there first. `AsterServer`'s `.identity(path)` is the same file (§4.3). Inline
keys (`ASTER_SECRET_KEY`, `[network] secret_key`, `.aster-identity`'s
`[node] secret_key`) accept the same 64 hex characters, or base64 for older
files. The `.aster-identity` TOML (peers, root key, enrollment material, read by
`AsterConfig::load_identity`) is a credential file, not a node-key file.

The same secret key can also **bind a TLS certificate** to this identity: when
serving over HTTP/WebTransport, the node key signs the cert so peers can verify
it (and reject a MITM) without trusting a CA — see [Binding the cert to your
Aster identity](./aster-http-getstarted.md#binding-the-cert-to-your-aster-identity).

---

### 2.2 Configuration reference

`AsterConfig` resolves defaults < `aster.toml` (`AsterConfig::from_file`) <
`ASTER_*` env (`from_env`) < builder calls (`AsterConfig::builder().load_file(p)?
.…`). Every endpoint/transport knob the bindings expose has a builder method:

| Builder | Env | TOML | Notes |
|---|---|---|---|
| `relay(RelayMode)` | `ASTER_RELAY_MODE` | `[network] relay_mode` | `default` / `staging` / `disabled` / `custom` (a bare URL means custom) |
| `relay_urls([..])` | `ASTER_RELAY_URLS` (comma-separated) | `[network] relay_urls = [..]` | selects `RelayMode::Custom`; with a mode set, it must be `custom` |
| `secret_key(k)` | `ASTER_SECRET_KEY` | `[network] secret_key` | 64 hex (or base64) |
| `secret_key_file(p)` | `ASTER_SECRET_KEY_FILE` | `[network] secret_key_file` | load-or-create at start (§2.1) |
| `persistent(dir)` | `ASTER_STORAGE_PATH` | `[storage] path` | in-memory when unset |
| `gc_interval(d)` | `ASTER_GC_INTERVAL_MS` | `[storage] gc_interval_ms` | `0` / unset = no GC |
| `blob_zstd_level(n)` | `ASTER_BLOB_ZSTD_LEVEL` | `[storage] blob_zstd_level` | zstd blob transport, on by default at `3`: served beside plain and preferred when fetching, with plain fallback; `0` / `off` = plain only; `5` suits slow or relayed links (`Blobs::with_transport_compression` overrides per client) |
| `bind_addr(a)` | `ASTER_BIND_ADDR` | `[network] bind_addr` | e.g. `0.0.0.0:9000`; pins the socket of that address family only |
| `bind_addr_v6(a)` | `ASTER_BIND_ADDR_V6` | `[network] bind_addr_v6` | e.g. `[::]:9000`: pins the IPv6 socket; `bind_addr` must then be IPv4 or unset; a host without IPv6 fails to start with it set |
| `advertise_addr(a)` (repeatable) | `ASTER_ADVERTISE_ADDRS` (comma-separated) | `[network] advertise_addrs = [..]` | extra `IP:port` direct addresses to publish (NodePort, load balancer); hints only, see below |
| `relay_ca_cert(p)` (repeatable) | `ASTER_RELAY_CA_CERTS` (comma-separated paths) | `[network] relay_ca_certs = [..]` | PEM CA bundles trusted for relay TLS, in addition to the WebPKI roots |
| `relay_ca_only(b)` | `ASTER_RELAY_CA_ONLY` | `[network] relay_ca_only` | trust only `relay_ca_certs` for relay TLS |
| `discovery_mode(DiscoveryMode)` | `ASTER_DISCOVERY` | `[network] discovery` | `default` (n0 pkarr + DNS at `iroh.link`) / `none` / `local` (mDNS only) / `custom` |
| `discovery_dns_origin(o)` / `discovery_pkarr_relay(u)` | `ASTER_DISCOVERY_DNS_ORIGIN` / `ASTER_DISCOVERY_PKARR_RELAYS` (comma-separated) | `[network] discovery_dns_origin` / `discovery_pkarr_relays = [..]` | your own iroh-dns-server; either selects `custom` |
| `reuse_last_port(b)` | `ASTER_REUSE_LAST_PORT` | `[network] reuse_last_port` | default `true`: a persistent node re-binds its last ports when free (see below) |
| `online_timeout_ms(ms)` | `ASTER_ONLINE_TIMEOUT_MS` | `[network] online_timeout_ms` | default `30000`: how long start-up waits for a home relay; past it start-up fails with `RelayOnlineTimeout` naming the relay URL(s) instead of hanging. Direct-only nodes (`relay_mode = "disabled"`) never wait. Rust only so far |
| `alpn(bytes)` | — | — | extra inbound ALPNs, merged with `Node::start_with_alpns` |
| `discovery(b)` | `ASTER_LOCAL_DISCOVERY` | `[network] local_discovery` | mDNS |
| `monitoring(b)` | `ASTER_ENABLE_MONITORING` | `[network] enable_monitoring` | |
| `hooks(b)` | `ASTER_ENABLE_HOOKS` | `[network] enable_hooks` | needed for `take_admission` |
| `hook_timeout_ms(ms)` | `ASTER_HOOK_TIMEOUT_MS` | `[network] hook_timeout_ms` | default 5000 |
| `hook_failure_mode(m)` | `ASTER_HOOK_FAILURE_MODE` | `[network] hook_failure_mode` | `fail_closed` (default: a stalled or missing admission loop rejects) / `fail_open` (explicit opt-in: accepts) |
| `clear_ip_transports(b)` | `ASTER_CLEAR_IP_TRANSPORTS` | `[network] clear_ip_transports` | relay-only |
| `clear_relay_transports(b)` | `ASTER_CLEAR_RELAY_TRANSPORTS` | `[network] clear_relay_transports` | direct-only |
| `portmapper(b)` | `ASTER_PORTMAPPER_CONFIG` | `[network] portmapper_config` | `enabled` / `disabled` |
| `proxy_url(u)` | `ASTER_PROXY_URL` | `[network] proxy_url` | HTTP/SOCKS |
| `proxy_from_env(b)` | `ASTER_PROXY_FROM_ENV` | `[network] proxy_from_env` | `HTTP(S)_PROXY` |
| `transport_<knob>(v)` | `ASTER_TRANSPORT_<KNOB>` | `[network] transport_<knob>` | `max_concurrent_bidi_streams`, `max_concurrent_uni_streams`, `stream_receive_window`, `receive_window`, `send_window`, `max_idle_timeout_ms`, `keep_alive_interval_ms`, `initial_mtu`, `datagram_receive_buffer_size`, `datagram_send_buffer_size`, `send_fairness`, `enable_segmentation_offload` |
| `identity_file(p)` | `ASTER_IDENTITY_FILE` | `[identity] file` | `.aster-identity` credential file |
| `accept_on_behalf_of(b)` | `ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF` | `[identity] accept_on_behalf_of` | default `false`: accept calls a gateway makes for a user (§4.7.1); off, such calls are refused |
| `trusted_issuer(TrustedIssuer)` / `trusted_issuers([..])` | `ASTER_IDENTITY_TRUSTED_ISSUERS` (JSON array) | `[[identity.trusted_issuers]]` | identity providers whose forwarded tokens are accepted (§4.7.1.1); each needs `audiences`; needs `[enrollment]` |
| `edge_token_max_lifetime_secs(n)` | `ASTER_IDENTITY_EDGE_TOKEN_MAX_LIFETIME_SECS` | `[identity] edge_token_max_lifetime_secs` | default and maximum `300` |
| `clock_leeway_secs(n)` | `ASTER_IDENTITY_CLOCK_LEEWAY_SECS` | `[identity] clock_leeway_secs` | default `60`, at most `300`; applies to `iat`/`nbf`, never `exp` |
| `max_key_set_age_secs(n)` | `ASTER_IDENTITY_MAX_KEY_SET_AGE_SECS` | `[identity] max_key_set_age_secs` | default `172800` (48 h), `60..=2592000`: an issuer key set older than this verifies nothing (§4.7.1.1) |
| `identity_publisher(IdentityPublisherConfig)` | `ASTER_IDENTITY_PUBLISHER` (JSON object) | `[identity.publisher]` | publish identity providers' keys from this node (§4.7.1.2); feature `identity-publisher`, needs `[enrollment]` |
| `remote_config(b)` | `ASTER_REMOTE_CONFIG_ENABLED` | `[remote_config] enabled` | default `true`: serve `aster.ops.NodeConfig` (§4.7.2) |
| `remote_config_lock([..])` | `ASTER_REMOTE_CONFIG_LOCK` (comma-separated) | `[remote_config] lock = [..]` | sections remote overrides may not touch, e.g. `["identity"]`; an unknown name fails the start |
| `health_port(p)` | `ASTER_HEALTH_PORT` | `[health] port` | health/metrics HTTP port; `0` (default) = disabled unless `health(true)` |
| `health_host(h)` | `ASTER_HEALTH_HOST` | `[health] host` | default `127.0.0.1`; `0.0.0.0` for pod probes |
| `health(b)` | `ASTER_HEALTH_ENABLED` | `[health] enabled` | force on (port `0` → ephemeral) or off; unset = on iff port ≠ 0 |
| `root_pubkey(k)` | `ASTER_ROOT_PUBKEY` / `ASTER_ROOT_PUBKEY_FILE` | `[trust] root_pubkey` / `root_pubkey_file` | the deployment root Enrollment trusts (and coordination pins) |
| `root_rotation(c, seq, statements)` | `ASTER_NEXT_ROOT_COMMITMENT`, `ASTER_ROOT_SEQ`, `ASTER_ROOT_ROTATIONS` | `[trust] next_root_commitment`, `root_seq`, `root_rotations` | the rotation pin, copied from `aster trust show` |
| `enrollment_credential_file(p)` / `enrollment_credential_chain(c)` | `ASTER_ENROLLMENT_CREDENTIAL` / `ASTER_ENROLLMENT_CREDENTIAL_CHAIN` | `[trust] enrollment_credential` / `enrollment_credential_chain` | this node's credential: the file `aster enroll node --out` writes, or the chain inline |
| `enrollment(EnrollmentConfig)` | `ASTER_ENROLLMENT_*` | `[enrollment]` | see §3.0 |

**Port reuse on restart.** A persistent node (`persistent(dir)`) whose
`bind_addr` leaves the port to the OS (unset, or port `0`) records the UDP
ports it bound in `<dir>/bind-ports.json`, and on the next start re-binds
them when they are still free. Peers' cached direct addresses and the NAT
mapping then stay valid, so they reconnect sooner after a restart: Docs sync
recovers in about a second instead of tens of seconds, and a node behind NAT
with relays off becomes reachable again instead of never. Each address family
falls back to a fresh port on its own (logged at `info`) when its port is
taken, and an unreadable record is ignored with a warning, so the record never
fails a start. An explicit non-zero port in `bind_addr` is always used as
given and never recorded. In-memory nodes are unaffected. Turn it off with
`reuse_last_port(false)` / `ASTER_REUSE_LAST_PORT=false` /
`[network] reuse_last_port = false`. Two processes must not share one storage directory; if they do, the second never
takes the first one's port and waits on the store lock without touching the
record.

**Cluster and air-gapped settings.** Advertised addresses are published with
the addresses the node discovers (in its address and every discovery record),
and peers try them like any other. They are hints, never identity: a dial
verifies the node's key in the TLS handshake, so an address that is wrong,
stale or reused by another pod cannot reach a different node
(`core/tests/stale_address_hint.rs`). `relay_ca_certs` secures the TCP/TLS hop
to a relay only (and, with `custom` discovery, to pkarr relays); peer-to-peer
traffic is end-to-end encrypted and authenticated by node keys whatever the
relay presents. A missing or unreadable CA file, or `relay_ca_only` without
files, fails the start. With discovery `none` or `custom` nothing is sent to
n0's DNS servers; relays are separate, so also set `relay_urls` or
`RelayMode::Disabled`. With discovery `none`, dial peers by full address (node
ID plus direct address or relay URL). See
[running inside Kubernetes](relay-self-hosted.md#running-inside-kubernetes).
In the C ABI these settings are `aster_network_config_t` (ABI 7.3,
`aster_config_set_network`); in Go, `AsterConfig.BindAddrV6`, `AdvertiseAddrs`,
`RelayCACerts`, `RelayCAOnly`, `Discovery`, `DiscoveryDNSOrigin`,
`DiscoveryPkarrRelays`. Python, TypeScript, Java and .NET do not expose these
settings yet; they follow in the next binding mirror pass.

Both `reuse_last_port` and `blob_zstd_level` reach every binding, with the
same env and TOML names and the same defaults:

| Binding | Blob compression | Port reuse |
|---|---|---|
| C ABI (6.0) | `aster_config_t.blob_zstd_level`: `0` = default, `-1` = off, 1..19 | `aster_config_t.reuse_last_port`: `aster_reuse_last_port_t` (`DEFAULT` / `ENABLED` / `DISABLED`) |
| Go | `AsterConfig.BlobZstdLevel *int` (nil = 3, `0` = off) | `AsterConfig.ReuseLastPort *bool` (nil = on) |
| Python | `AsterConfig(blob_zstd_level=0)` | `AsterConfig(reuse_last_port=False)` |
| TypeScript | `blobZstdLevel: 0` | `reuseLastPort: false` |
| Java / Kotlin | `AsterConfig.builder().blobZstdLevel(0)` | `.reuseLastPort(false)` |
| .NET | `AsterConfig.BlobZstdLevel = 0` | `AsterConfig.ReuseLastPort = false` |

The `[health]` keys start the `aster::ops` HTTP listener (feature `health`,
which implies `otel`) automatically under `AsterServer`; with a bare `Node`,
call `aster::ops::HealthServer::start(&node, cfg.health_config().unwrap())`.
Routes: `GET /healthz` (liveness), `GET /readyz` (readiness: the server is
ready, `HealthState::set_ready`, and every named check passes;
`health.check("db").set(true)`; the JSON lists them under `checks`) and
`GET /metrics` (Prometheus text, `text/plain; version=0.0.4`).

`/metrics` is the OpenTelemetry Prometheus pull reader: the built-in
`aster.*` instruments (RPC calls, durations, streams, connections, Gate 0
decisions, upcalls), the `aster.transport.*` iroh counters, `aster.health.*`,
`process.uptime`, and any custom instruments. The listener joins the
`aster_otel` pipeline if one is running, or starts one from `OTEL_*` /
`ASTER_*`, and adds the pull reader whatever `OTEL_METRICS_EXPORTER` says.
The instruments record wherever calls happen, so a hand-built `Server` or a
bare `Node` needs no extra wiring. `aster::ops::route` serves the same routes
from another HTTP stack, and `aster::ops::metrics_text()` returns the scrape
body.

With feature `otel`, `aster::rpc::current_trace_context()` returns the trace
context of the running code (the current span's, else the inbound `traceparent`
of the call a Rust-native handler is serving, even with no exporter), and
`aster::rpc::with_trace_context(ctx, fut)` runs a future so that its client
calls carry `ctx`. Logs, metrics and traces for all six languages are covered
in the [observability guide](aster-observability-getstarted.md).

The ALPNs are exported as `aster::RPC_ALPN`, `aster::enrollment::ENROLLMENT_ALPN`
and `aster::alpns::{RPC, PRODUCER_ADMISSION, DELEGATED_ADMISSION, BLOBS, DOCS,
GOSSIP}`.

## 3. Admission (Gate 0) and enrollment credentials

### 3.0 Enrollment through `AsterServer` and `AsterClient` (recommended)

Configure enrollment and `requires` just works: `AsterServer` admits only
nodes holding a credential from your root, turns the roles in each peer's
credential into the attributes Gate 3 checks, and closes a peer's connections
when the root revokes it. There is no admission code to write. Needs the
`enrollment` feature (with `rpc` and `docs`).

The operator produces every file with the CLI:

```bash
aster key root generate                                   # the profile's root
aster enroll namespace create --out-secret fleet.secret   # prints FLEET
aster key node generate --out server.key                  # prints SERVER
aster key node generate --out agent.key                   # prints AGENT
aster enroll node --subject $SERVER --audience my-fleet --out server.cred
echo '{"aster.role":{"type":"string","value":"ops.status,ops.ingest"}}' > agent-roles.json
aster enroll node --subject $AGENT --audience my-fleet \
  --attributes agent-roles.json --out agent.cred
```

and points the configuration at them (`aster.toml`, or the `ASTER_*`
variables in the comments):

```toml
[trust]
root_pubkey = "<aster key root show>"        # ASTER_ROOT_PUBKEY
enrollment_credential = "server.cred"   # ASTER_ENROLLMENT_CREDENTIAL

[network]
secret_key_file = "server.key"               # ASTER_SECRET_KEY_FILE

[enrollment]
audience = "my-fleet"                        # ASTER_ENROLLMENT_AUDIENCE
fleet_namespace = "<FLEET>"                  # ASTER_ENROLLMENT_FLEET_NAMESPACE
state_path = "/var/lib/my-app/enrollment.json"  # ASTER_ENROLLMENT_STATE_PATH
```

```rust
use aster::rpc::{AsterClient, AsterServer};

# async fn run() -> aster::Result<()> {
let srv = AsterServer::builder()
    .service(EchoServer::new(EchoImpl))     // methods carry #[rpc(requires = …)]
    .config(aster::AsterConfig::from_file("aster.toml")?)
    .start()
    .await?;

// The agent: its own key and credential, the same root/audience/fleet.
let client = AsterClient::builder()
    .config(aster::AsterConfig::from_env()?)
    .address(srv.address()?)
    .connect()
    .await?;
let echo = EchoClient::new(client.rpc());
# Ok(())
# }
```

- **Who is admitted.** Any node with a valid credential from the root for this
  audience, minus every revocation the root has published. Narrow it with
  `[enrollment] policy`, e.g. `policy = { all_of = [{ scope = "ops" },
  "tolerate_stale_policy"] }`, or in code with
  `AsterServerBuilder::enrollment_policy`.
- **Roles.** By default the comma-separated `aster.role` claim attribute is
  the peer's role set; `[enrollment.roles]` can also map scopes and tags to
  roles (`scopes_as_roles = true`, `[enrollment.roles.scopes] "fleet.ops" =
  ["ops.status"]`), or use `AsterServerBuilder::enrollment_roles`. Other claim
  attributes reach handlers through `call.attributes()`.
- **Policy and revocation.** With no `replicas`, the server holds the fleet
  namespace itself, and the root publishes into it:
  `aster enroll revoke --subject $AGENT --fleet-only --fleet-namespace $FLEET
  --fleet-secret-file fleet.secret --audience my-fleet --peer $SERVER`. The
  revoked agent's next call is refused and its connections close. With
  `replicas = ["aster1…"]` the server instead enrolls with those policy
  replicas and synchronizes from them at start.
- **Issuing and renewing.** `[enrollment.issuer] delegation_file =
  "issuer.chain"` (from `aster enroll delegate`) makes the server a delegated
  issuer: `srv.issuer().unwrap().issue_for(..)` enrolls new nodes and the
  server serves their renewals. A client renews automatically from the server
  that signed its credential, or from `[enrollment] renew_from`.
  A client renews when `max(lifetime / 3, min(10 min, lifetime / 2))` of the
  credential remains, and retries with backoff until it expires. An hour-long
  credential renews with 20 minutes left, a 20-to-30-minute one with the
  10-minute floor, and anything shorter at half its lifetime. The floor keeps
  a stalled renewal from costing live connections: a server whose session
  still rests on the old chain at expiry revokes it and closes the peer's
  connections. Credentials under 30 minutes are discouraged (the issuer logs
  a warning once at configuration): their renewals run almost back to back,
  and the margin shrinks to seconds. Prefer an hour or more.
- **Fail closed.** Setting a credential, audience, fleet namespace or issuer
  turns enrollment on; anything missing or malformed stops `start()` /
  `connect()` with an error that names the setting. Nothing falls back to an
  open node.

`srv.enrollment()` and `client.enrollment()` return the installed
`aster::enrollment::Enrollment` for anything further (checking a peer,
following revocations, applying a root rotation). The full schema is in
`docs/_internal/working_ideas/aster-enrollment.md` ("AsterServer
integration"); the Rust Mission Control example's auth chapter is a complete
walk-through.

The rest of this section is the low-level path, for hosts that run their own
node and accept loop.

Use Aster's `Gate0` rather than a separate static allowlist. Built-in
docs/blobs/gossip ALPNs are always registered; add your admission ALPN(s)
explicitly and enable hooks.

This needs the `enrollment` feature. For a managed fleet (reciprocal handshake,
replicated root policy, revocation) use `aster::enrollment::Enrollment::install`
instead; see `docs/_internal/aster-enrollment-portal-porting-guide.md`. The
manual loop below shows the moving parts.

```rust
use aster::{alpns, AsterConfig, Gate0, Node, PublicKey, RelayMode};
use aster::enrollment::{verify_connection, JwtEnrollmentVerifier};

# async fn run(root_pub: PublicKey, now: u64) -> aster::Result<()> {
let verifier = std::sync::Arc::new(
    JwtEnrollmentVerifier::new(root_pub.to_bytes(), "my-fleet").expect("valid root"),
);
let cfg = AsterConfig::builder().relay(RelayMode::Default).hooks(true).build();
let node = Node::start_with_alpns(cfg, vec![alpns::PRODUCER_ADMISSION.to_vec()]).await?;
let gate = Gate0::new();

// 1) Hook loop: admission ALPNs are always open (to present a credential);
//    every other ALPN requires an already-admitted peer.
let mut admission = node.take_admission().expect("hooks enabled");
let gate_hook = gate.clone();
tokio::spawn(async move {
    while let Some(req) = admission.next_handshake().await {
        if gate_hook.should_allow(&req.peer, &req.alpn) {
            req.accept();
        } else {
            req.reject(403, b"not admitted".to_vec()); // don't leak detail on the wire; log locally
        }
    }
});

// 2) Accept loop on the admission ALPN: read the peer's enrollment credential
//    and verify it was issued by the root for exactly this authenticated peer.
let srv = node.clone();
tokio::spawn(async move {
    while let Ok((alpn, conn)) = srv.accept().await {
        if alpn != alpns::PRODUCER_ADMISSION { continue; }
        let (send, recv) = conn.accept_bi().await?;
        let raw = recv.read_to_end(16 * 1024).await?;        // bound the request
        let token = String::from_utf8(raw).unwrap_or_default();
        // The credential's subject must be the peer the QUIC handshake authenticated.
        let ok = verify_connection(&verifier, &token, &conn, now).is_ok();
        if ok { gate.admit(conn.peer()); }
        send.write_all(if ok { b"ok".to_vec() } else { b"no".to_vec() }).await?;
        send.finish().await?;
        conn.closed().await; // IMPORTANT: keep the connection alive until the peer has read
    }
    aster::Result::Ok(())
});
# Ok(())
# }
```

> **Gotcha:** a responder must `conn.closed().await` after `finish()` —
> dropping the connection immediately truncates the peer's read ("closed by
> peer: 0"). `Connection::closed()` exists for exactly this.

Issuing a credential (root side; `aster enroll node` does the same from the CLI):

```rust
use aster::SecretKey;
use aster::enrollment::{issue_credential, EnrollmentIssueRequest};

# fn issue(root_secret: &SecretKey, node_public: [u8; 32], now: u64) {
// The root key is your trust anchor: generate it once, keep it offline,
// persist `to_bytes()`. The node generates its own key; only its public key
// reaches the root. Every credential expires (at most 30 days).
let request = EnrollmentIssueRequest::new(node_public, "my-fleet", now + 7 * 24 * 3600);
let credential = issue_credential(root_secret, &request, now).expect("issued");
let chain: String = credential.into_chain();  // hand this to the node
let trusted_root = root_secret.public_key();  // what verifiers pin
# }
```

A credential is a delegation chain (`aster-chain-v1~...`) of compact Ed25519
JWTs. `node.export_secret_key()?.public_key()` equals `node.id()`: the
enrollment subject *is* the transport identity, so a stolen credential is
useless on any other node. An online issuer can sign credentials for an
offline root under a root-signed delegation (`issue_delegated_credential`,
`aster enroll delegate`); see `docs/_internal/ownership-attestations.md` and
`docs/_internal/working_ideas/aster-enrollment.md`.

### 3.1 Allowlist specific node IDs

When the set of callers is known ahead of time (e.g. only the **root node** from
your `AsterConfig`, or a fixed list of operators), skip enrollment credentials and
gate at Gate 0 directly on the peer's node ID. The hook runs **after** the QUIC/
TLS handshake, so `req.peer` is the peer's TLS-certificate ed25519 key — it is
cryptographically authenticated and cannot be spoofed. Rejecting here closes the
connection before *any* protocol (RPC, blobs, docs, gossip) can be used.

```rust
use std::collections::HashSet;
use aster::{AsterConfig, AsterServer, NodeId, PublicKey, RelayMode};

# async fn serve(root_pubkey: PublicKey, also_allow: Vec<NodeId>) -> aster::Result<()> {
// Node IDs allowed to reach this node at all. A node ID *is* an ed25519 public
// key, so the root key from config goes straight in (compare as hex).
let mut allowed: HashSet<String> = HashSet::new();
allowed.insert(root_pubkey.to_hex());
for id in &also_allow {
    allowed.insert(id.as_str().to_string());
}

let config = AsterConfig::builder()
    .relay(RelayMode::Default)
    .hooks(true) // enable the admission hook; fails closed by default (see below)
    .build();

let srv = AsterServer::builder()
    .service(EchoServer::new(EchoImpl))
    .config(config)
    .start()
    .await?;

// Gate-0 loop: fires per inbound connection, before any stream is dispatched.
let mut admission = srv.take_admission().expect("hooks enabled");
tokio::spawn(async move {
    while let Some(req) = admission.next_handshake().await {
        if allowed.contains(req.peer.as_str()) {
            req.accept();
        } else {
            req.reject(403, b"not allowed".to_vec()); // closes the connection
        }
    }
});

srv.run().await;
# Ok(())
# }
```

For a single allowed identity (e.g. *only* the root node), use a one-element set.

> **Admission fails closed by default.** If your admission loop stalls past
> `hook_timeout_ms` (default 5 s) or is dropped, the connection is *rejected*
> (`403 "hook unavailable"`): the default `HookFailureMode::FailClosed` means a
> stalled or missing loop denies rather than admits. Setting
> `.hook_failure_mode(HookFailureMode::FailOpen)` (`fail_open`) opts into the
> opposite: an unanswered decision *accepts* the connection, which suits only
> observability hooks that must never block a peer. Either way, make sure the
> loop actually runs (`take_admission` returns the handle once) — without a
> live loop draining `next_handshake`, every decision hits the fallback. The
> node's own outbound connections never reach `next_handshake`: Gate 0 admits
> peers connecting *to* this node, so its own dials are accepted at the
> handshake (gate them with `next_connect` instead).

**Per-method instead of per-connection?** If you'd rather admit a broad set but
restrict only some methods to specific identities, use Gate 3 (§4.6): map each
allowed node ID to a role at startup (`attrs.set_role(id.as_str(), "root")`) and
tag those methods `#[rpc(requires = require_role("root"))]`. Gate 3 only checks
methods that declare `requires` — an untagged method stays open — so for a
whole-service lockdown the Gate-0 allowlist above is the surer choice.

---

## 4. RPC

> To serve these same services over **HTTPS / HTTP3 / WebTransport** (browsers,
> JSON, TLS, sessions, static files), see
> [Serving Aster over HTTP](./aster-http-getstarted.md).

Enable the `rpc` feature. The public surface lives under `aster::rpc::*`, with
`#[aster::service]` and `#[derive(aster::AsterType)]` also re-exported at the
crate root.

### 4.1 Define payload types

Payload types derive **`ForyStruct`** (wire codec) and **`AsterType`** (contract
identity). The `#[aster(wire = "namespace/Name")]` tag pins the type's
cross-binding identity.

```rust
use fory_derive::ForyStruct;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "echo/EchoRequest")]
struct EchoRequest {
    message: String,
    count: i32,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "echo/EchoResponse")]
struct EchoResponse { reply: String }
```

Field-type mapping: `String`→string, `bool`, `i8/i16/i32/i64`, `u16/u32/u64`,
`f32/f64`, `Vec<u8>`→binary (needs `#[fory(bytes)]`, see below),
`Vec<T>`/`HashSet<T>`→list/set, `HashMap<K,V>`→map, `Option<T>`→nullable, and a
nested `AsterType` struct → a typed reference.

**Byte fields.** A byte buffer is published as `binary`, the type Python
`bytes` and TypeScript `Uint8Array` use. Fory's Rust derive only writes a
`Vec<u8>` as binary when the field carries `#[fory(bytes)]`; without it, the
field goes on the wire as a list of `u8` that other bindings cannot read as
bytes. `AsterType` therefore refuses to compile an unannotated `Vec<u8>`, so
the contract and the wire cannot disagree:

```rust
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "files/Chunk")]
struct Chunk {
    #[fory(bytes)]
    data: Vec<u8>,                  // binary
    #[fory(bytes)]
    checksum: Option<Vec<u8>>,      // nullable binary (`Option` is transparent)
    digest: [u8; 32],               // binary; Fory writes `[u8; N]` as binary already
    #[fory(list(element(bytes)))]
    parts: Vec<Vec<u8>>,            // list<binary>
    #[fory(map(value(bytes)))]
    blobs: HashMap<String, Vec<u8>>, // map<string, binary>
    #[aster(uint8_list)]
    levels: Vec<u8>,                // list<uint8>: an explicit opt-out
}
```

- `#[aster(uint8_list)]` is the opt-out for a `Vec<u8>` (or `VecDeque<u8>`)
  that really is a list of small integers. The field stays a Fory list and is
  published as `list<uint8>`, not `binary`.
- A `[u8; N]` field decodes only a payload of exactly N bytes. If a peer may
  send any length, use `#[fory(bytes)] Vec<u8>` and check the length yourself.
- `VecDeque<u8>` and `#[fory(array)] Vec<u8>` cannot be binary, so they are
  rejected unless the field is marked `#[aster(uint8_list)]`. Fory 1.7 has no
  `bytes::Bytes` support, so `Bytes` cannot be a field type.
- Spell byte arrays and containers out in the field; don't use a type alias.
  Both derives classify these fields by how they are written, so
  `hash: Hash32` with `type Hash32 = [u8; 32]` is not recognised as
  binary. `AsterType` fails the build ("`[u8; 32]` is not an Aster wire
  type"), and Fory alone fails at runtime ("carrier serializers selected by a
  field must use canonical carrier syntax"). Write `hash: [u8; 32]`; the
  alias can still name the type everywhere else. The same holds for aliases of
  `Vec<_>`, sets and maps. An alias of your own struct is fine.

Nested payload types — a struct field of another payload type, `Vec<Segment>`,
`Option<Foo>`, `HashMap<String, Bar>`, and so on at any depth — are registered
automatically: the macro names each method's top-level request/response type and
the derive walks every field's element/value/key type into the Fory runtime.
Every nested type just needs to derive `#[derive(ForyStruct, aster::AsterType)]`
like any other payload (it must anyway to serialize).

You can lower a scalar default into the contract with
`#[aster(default = <expr>)]` (supported for `String` / `bool` / `i32` / `i64` /
`f64`); a changed default changes the `contract_id`. Matching cross-language
contracts must declare the same default.

### 4.2 Define a service

A trait annotated with `#[aster::service]` generates a server adapter, a client
stub, and a cross-binding `ServiceContract`. Methods are `async fn(&self, …) ->
aster::Result<…>`; the streaming kind is declared per method.

```rust
use aster::rpc::{require_role, RequestStream, ResponseSink};

#[aster::service(name = "Echo", version = 1)]
trait Echo {
    // unary
    async fn echo(&self, req: EchoRequest) -> aster::Result<EchoResponse>;

    // server-stream: one request, many responses
    #[rpc(server_stream)]
    async fn echo_n(&self, req: EchoRequest, out: ResponseSink<EchoResponse>) -> aster::Result<()>;

    // client-stream: many requests, one response
    #[rpc(client_stream)]
    async fn collect(&self, reqs: RequestStream<EchoRequest>) -> aster::Result<EchoResponse>;

    // bidi: many requests, many responses
    #[rpc(bidi_stream)]
    async fn chat(&self, reqs: RequestStream<EchoRequest>, out: ResponseSink<EchoResponse>) -> aster::Result<()>;

    // per-method capability requirement (Gate 3); also #[rpc(idempotent)]
    #[rpc(requires = require_role("operator"))]
    async fn admin(&self, req: EchoRequest) -> aster::Result<EchoResponse>;
}
```

Implement it with `#[aster::rpc::async_trait]`:

```rust
struct EchoImpl;

#[aster::rpc::async_trait]
impl Echo for EchoImpl {
    async fn echo(&self, req: EchoRequest) -> aster::Result<EchoResponse> {
        Ok(EchoResponse { reply: format!("echo: {}", req.message) })
    }

    async fn echo_n(&self, req: EchoRequest, out: ResponseSink<EchoResponse>) -> aster::Result<()> {
        for i in 0..req.count {
            out.send(&EchoResponse { reply: format!("{}#{i}", req.message) })?;
        }
        Ok(()) // Ok → OK trailer; Err(aster::Error::Rpc{..}) → that status
    }

    async fn collect(&self, mut reqs: RequestStream<EchoRequest>) -> aster::Result<EchoResponse> {
        let mut all = String::new();
        while let Some(r) = reqs.recv().await? { all.push_str(&r.message); }
        Ok(EchoResponse { reply: all })
    }

    async fn chat(&self, mut reqs: RequestStream<EchoRequest>, out: ResponseSink<EchoResponse>) -> aster::Result<()> {
        while let Some(r) = reqs.recv().await? {
            out.send(&EchoResponse { reply: r.message })?;
        }
        Ok(())
    }

    async fn admin(&self, req: EchoRequest) -> aster::Result<EchoResponse> {
        Ok(EchoResponse { reply: format!("admin: {}", req.message) })
    }
}
```

### 4.3 Serve it

Use **`AsterServer`** — the declarative, recommended way to run a producer, and
the Rust peer of the Python/TS `AsterServer`. One builder resolves config, starts
the node (RPC + the built-in blobs/docs/gossip ALPNs registered for you), and
serves your services; it also exposes the protocol clients and a stable identity
in one place.

```rust
use aster::rpc::AsterServer;

# async fn serve() -> aster::Result<()> {
let srv = AsterServer::builder()
    .service(EchoServer::new(EchoImpl))   // repeat .service(…) per service
    .identity(".aster-identity")          // restart-stable endpoint key (load-or-create)
    .persistent("/var/lib/my-app")        // durable blobs / docs / gossip
    .relay(aster::RelayMode::Default)
    // .attributes(attrs)                 // shared Gate-3 store (see §4.6)
    // .hooks(true)                       // enable Gate-0 admission (.take_admission())
    // .alpn(aster::alpns::PRODUCER_ADMISSION.to_vec())  // extra inbound ALPN (see §3)
    .start()
    .await?;

println!("address: {}", srv.address()?); // aster1… ticket — hand this to clients
let _hash = srv.blobs().add_bytes(b"hi".to_vec()).await?; // blobs/docs/gossip on the same node

srv.run().await;       // serve until the node closes
// or: srv.shutdown().await;  // graceful close, flushes the store
# Ok(())
# }
```

`EchoServer` is generated from the `Echo` trait. Add one `.service(…)` per
service (Rust can't hold heterogeneous instances in one `[…]` list the way Python
does). The server keeps serving as long as you hold `srv` and call
`run()` — or end it with `shutdown` for a clean,
store-flushing close (Rust has no `async with`).

`.identity(path)` persists only the endpoint **secret key** (64 hex chars,
load-or-create) — enough for a restart-stable `NodeId`, which is what an
enrollment credential is issued for. Credentials come from the configuration
(§3.0).

#### Low-level: building a `Server` by hand

Reach for this only when you need to own the node yourself — e.g. a custom accept
loop on a non-RPC ALPN alongside RPC, or to start the node before services exist.
You register the RPC ALPN (`aster::rpc::RPC_ALPN`, `b"aster/1"`) and wire the
`Server` directly; `AsterServer` does exactly this for you.

```rust
use aster::rpc::{AttributeStore, Server, RPC_ALPN};

# async fn serve(cfg: aster::AsterConfig) -> aster::Result<()> {
let node = Node::start_with_alpns(cfg, vec![RPC_ALPN.to_vec()]).await?;

let attrs = AttributeStore::new(); // populate per-peer roles for Gate-3 checks
let _server = Server::new(&node)
    .register(EchoServer::new(EchoImpl)) // EchoServer is generated from `Echo`
    .attributes(attrs.clone())
    .serve();                            // spawns the accept+dispatch loop
# Ok(())
# }
```

### 4.4 Call it

`AsterClient` is the high-level consumer, the peer of the Python/TS
`AsterClient`: it resolves the config (`ASTER_ENDPOINT_ADDR` supplies the
address when `.address(..)` is not called), starts a node, enrolls when
`[enrollment]` is configured, and opens the RPC connection:

```rust
use aster::rpc::AsterClient;

# async fn call(address: &str) -> aster::Result<()> {
let client = AsterClient::builder()
    .config(aster::AsterConfig::from_env()?)
    .address(address)                       // the aster1… string from srv.address()
    .connect()
    .await?;
let echo = EchoClient::new(client.rpc());   // EchoClient is generated from `Echo`
let resp = echo.echo(EchoRequest { message: "hi".into(), count: 0 }).await?;
client.close().await;
# Ok(())
# }
```

Low-level: with your own `Node`, register the ticket's address material, then
open an RPC connection by id:

```rust
use aster::rpc::RpcConnection;
use aster::Ticket;

# async fn call(node: &Node, address: &str) -> aster::Result<()> {
let ticket = Ticket::from_base58(address)?; // the aster1… string from srv.address()
let peer = node.add_ticket_addr(&ticket)?;  // teach this node how to reach it

let conn: RpcConnection = node.rpc_connect(&peer).await?;
let client = EchoClient::new(conn);         // EchoClient is generated from `Echo`

let resp = client.echo(EchoRequest { message: "hi".into(), count: 0 }).await?;
assert_eq!(resp.reply, "echo: hi");
# Ok(())
# }
```

If the peer is already reachable by id (relay/discovery, or a prior
`add_ticket_addr`), skip the ticket and call `node.rpc_connect(&peer_id)`
directly.

### 4.5 Streaming, client side

```rust
# async fn stream(client: &EchoClient) -> aster::Result<()> {
// server-stream → a typed MessageStream you read
let mut s = client.echo_n(EchoRequest { message: "x".into(), count: 3 }).await?;
while let Some(item) = s.recv().await { println!("{}", item?.reply); }
// or: let all = s.collect().await?;

// client-stream / bidi take the requests up front (eager) for now
let one = client.collect(vec![
    EchoRequest { message: "a".into(), count: 0 },
    EchoRequest { message: "b".into(), count: 0 },
]).await?;

let mut chat = client.chat(vec![EchoRequest { message: "hi".into(), count: 0 }]).await?;
let replies = chat.collect().await?;
# Ok(())
# }
```

#### Per-call deadline and metadata

`with_options` returns a copy of the client (cheap: no codec rebuild) whose
calls carry a `CallOptions`: a deadline, request metadata, a session id. It
applies to every method in all four patterns; the plain client sends none.
An edge that forwards each request's budget and identity builds one per
request:

```rust
use std::time::Duration;
use aster::rpc::CallOptions;

# async fn forward(client: &EchoClient, budget: Duration, user: &str) -> aster::Result<()> {
let call = client.with_options(
    CallOptions::new()
        .deadline(budget)                   // client-enforced; DEADLINE_EXCEEDED when it passes
        .metadata("x-user", user),          // the handler reads ctx.metadata_value("x-user")
);
let resp = call.echo(EchoRequest { message: "hi".into(), count: 0 }).await?;
let mut s = call.echo_n(EchoRequest { message: "x".into(), count: 3 }).await?;
# Ok(())
# }
```

The handler sees both through `CallContext` (§4.7): `metadata()` and
`deadline()` / `remaining()`. A unary deadline bounds each attempt and
overrides the connection's `with_deadline` (§4.8); a streaming deadline bounds
the whole call from opening the stream. Unary interceptors see the deadline
and metadata in `ClientCallContext`. For a session-scoped service, start from
`session.options()` (`conn.open_session()?`) to stamp the session id.

The client enforces a deadline exactly, to the millisecond. The copy sent to
the server is whole seconds, rounded up, so a server's `deadline()` can be up
to a second later than the caller's. The server never cuts a call short. It
may, though, keep working on a call the caller has already abandoned for up
to a second longer.

### 4.6 Per-call authorization (Gate 3)

`#[rpc(requires = …)]` is checked **before** the handler runs, against the
caller's attributes in the server's `AttributeStore`. A missing capability
fails with `PERMISSION_DENIED`. With enrollment configured (§3.0) the store
reads each peer's roles from its live, verified enrollment session, so there
is nothing to populate. The credential sets the maximum; runtime settings can
only restrict: `srv.attributes().restrict_roles(peer, ["ops.status"])` narrows
an enrolled peer's credential roles from its next call (a role the credential
lacks is never granted, and the restriction ends with the enrollment session
but survives renewals), and `clear_restriction(peer)` restores them.
`set` / `set_role` refuse `aster.role` for an enrolled peer
(`RoleChangeError::CredentialOwnsRoles`); other keys you set still apply, and
the credential's attributes win on a clash.

Without enrollment, give `AsterServer` the store with `.attributes(store)` (or read it back via
`srv.attributes()`) and keep a clone to populate from admission:

```rust
use aster::rpc::{require_any_of, require_role, AttributeStore};

let attrs = AttributeStore::new();
// let srv = AsterServer::builder().service(…).attributes(attrs.clone()).start().await?;

// In your admission accept loop, once a peer's chain verifies and you know its role:
attrs
    .set_role(conn.peer().as_str(), "operator")
    .expect("refused only for a peer with a live enrollment session");
// attrs.remove(peer)  // revoke

// Requirements are constructed with require_role / require_any_of / require_all_of.
let _ = require_any_of(["operator", "admin"]);
```

Gate 0 (connection admission) and Gate 3 (per-call capability) are independent
layers; Enrollment drives both when configured. Gate 2 (session `authorize` →
rcan) is not yet in the Rust crate.

### 4.7 The call context in handlers

Add a `ctx: &CallContext` parameter to any `#[aster::service]` method — unary,
streaming or session-scoped — to read who is calling and how long they will
wait. The macro detects it by type and passes the call's context; it is not a
wire field, so adding or removing it never changes the contract id, and the
generated client does not take it.

```rust
use aster::rpc::{async_trait, AsterServer, AuthContext, AuthOutcome, Authenticator,
                 CallContext, ResponseSink, RpcStatus, StatusCode};

#[aster::service(name = "Reports", version = 1)]
trait Reports {
    #[rpc(server_stream)]
    async fn export(&self, req: ExportRequest, out: ResponseSink<Row>, ctx: &CallContext)
        -> aster::Result<()>;
}

struct ReportsImpl;

#[async_trait]
impl Reports for ReportsImpl {
    async fn export(&self, req: ExportRequest, out: ResponseSink<Row>, ctx: &CallContext)
        -> aster::Result<()> {
        let user = ctx.principal();                 // the Authenticator's principal
        let budget = ctx.remaining();               // Some(time left) when a deadline applies
        for row in load_rows(user, &req, budget).await? {
            tokio::select! {
                _ = ctx.cancelled() => return Ok(()), // the caller went away
                sent = out.send(&row) => sent?,
            }
        }
        Ok(())
    }
}

struct BearerAuth;
#[async_trait]
impl Authenticator for BearerAuth {
    async fn authenticate(&self, ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        let token = ctx.metadata.get("authorization")
            .ok_or_else(|| RpcStatus::error(StatusCode::Unauthenticated, "no token"))?;
        Ok(AuthOutcome { principal: Some(verify(token)?), ..Default::default() })
    }
}

# async fn serve() -> aster::Result<()> {
let srv = AsterServer::builder()
    .service(ReportsServer::new(ReportsImpl))
    .authenticator(BearerAuth)
    .start()
    .await?;
# Ok(())
# }
```

`CallContext` (one `Arc`, cheap to clone) carries:

| Accessor | Value |
|---|---|
| `peer()` | the transport's caller id (Iroh: endpoint id hex); `provenance()` says how far to trust it |
| `principal()` | the `Authenticator`'s `AuthOutcome.principal`, else `peer()` |
| `attributes()` / `attribute(k)` | the merged set Gate 3 checked (store + authenticator) |
| `node_properties()` / `node_attribute(k)` | **this node's** NodeConfig name, tags and typed attributes (§4.7.2); never authorise |
| `service()`, `method()`, `call_id()` | what was called |
| `session_id()` | the session, for a session-scoped service |
| `metadata()` / `metadata_value(k)` | request metadata (HTTP: the request headers) |
| `deadline()` / `remaining()` | the effective deadline: min(caller's, method timeout, server handler timeout) |
| `is_cancelled()` / `cancelled().await` | peer cancel, stream failure or deadline passed |

Take `ctx: CallContext` (owned) instead when the handler moves it into a task.
The dispatcher drops a handler's future when its call is cancelled, so inside
the handler `cancelled()` serves `select!`-style early exits; a spawned task
holding a clone sees it resolve too.

Code deeper in the call reads the same context without a parameter:
`CallContext::current()` (panics outside a handler) or
`CallContext::try_current()`. It is a tokio task-local set around every
handler: a task the handler `tokio::spawn`s does **not** inherit it — pass a
clone instead. Hand-written `ServiceDispatch` impls get it from
`call.context()`.

### 4.7.1 Calling on behalf of a user (gateway identity)

An edge (a gateway, an HTTP front end) that signs users in itself — session
cookie, API key, passkey — calls services over iroh **as the user**: it mints
a short-lived JWT signed with its own node key and sends it in the
`aster-on-behalf-of` call-metadata entry. The service verifies it before
Gate 3, so `requires` checks the **token's** roles (the edge's own roles never
apply to the call) and the handler sees the user.

Three things make it work, all configuration:

- **The service accepts it:** `[identity] accept_on_behalf_of = true`
  (`AsterConfig::builder().accept_on_behalf_of(true)`). Off (the default), a
  call carrying the entry is refused with `PERMISSION_DENIED`, never run as
  the edge.
- **The edge may forward:** its enrollment credential grants the role
  `aster.gateway` (`aster.role` attribute, or your `[enrollment.roles]`
  mapping). Without it, `PERMISSION_DENIED`.
- **The edge may issue these roles:** its credential carries the issuer
  delegation, the string attribute `aster.identity.issuable_roles`
  (comma-separated). A token claiming a role outside it is refused with
  `PERMISSION_DENIED`. A delegated issuer can sign that attribute only with
  `--attribute-key aster.identity.issuable_roles` in its delegation.

```sh
cat > edge-attributes.json <<'EOF'
{"aster.role": {"type": "string", "value": "aster.gateway"},
 "aster.identity.issuable_roles": {"type": "string", "value": "data.read,data.write"}}
EOF
aster enroll node --subject <edge-node-id> --audience my-fleet \
  --attributes edge-attributes.json --out edge.credential
```

On the edge (an `AsterClient` enrolled with that credential):

```rust
use aster::rpc::{CallOptions, AMR_WEBAUTHN};
use std::time::Duration;

let issuer = client.user_token_issuer()?;
// After the edge has checked alice's passkey assertion:
let token = issuer.mint_user_token("alice", ["data.read"], [AMR_WEBAUTHN], Duration::from_secs(120))?;
let reply = DataClient::new(client.rpc())
    .with_options(CallOptions::new().on_behalf_of(token))
    .read(req)
    .await?;
```

`mint_user_token` refuses roles outside the delegation and lifetimes above 5
minutes, as the service would. In the handler:

| Accessor | Value for a forwarded call |
|---|---|
| `principal()` | `iss|sub`: the edge's node id, `|`, the user (`<edge-id>|alice`) |
| `actor()` | `Some(edge node id)`; `None` for a call made by the peer as itself |
| `peer()` | the edge (the transport peer) |
| `auth_methods()` | the token's `amr` (`["webauthn"]`); require passkey sign-in by checking it |
| `attributes()` | `aster.role` = the token's roles only |
| `on_behalf_of()` | the whole verified identity: issuer, subject, `jti`, expiry |
| `token_claims()` | every claim of the verified token as a read-only `serde_json::Map` (a provider token's custom claims such as `tenant` included); `None` for a call without a verified forwarded token |

`token_claims()` holds the verified token's claims, as signed, and nothing
else: a token that fails verification (issuer, signature, audience, lifetime)
refuses the call, so no handler ever sees its claims. Treat them as read-only.
Do not read identity from unverified headers or metadata: use `principal()`,
`attributes()` and `token_claims()`, which come from the token the service
verified.

The service checks: `iss` is the transport peer (another node's token, or one
replayed by another gateway, is `UNAUTHENTICATED`), the Ed25519 signature,
`exp` (never extended) and `iat`/`nbf` with `[identity] clock_leeway_secs`
(default 60 s), and `exp - iat` ≤ `[identity] edge_token_max_lifetime_secs`
(default and maximum 5 min). A call without the entry is authorised as the
peer, exactly as before. Accepted calls record `principal`, `actor`, `obo_iss`
and `obo_jti` on their `rpc` span and log under `aster::rpc::identity`
(debug).

#### 4.7.1.1 Forwarding an identity provider's token

A user signed in with OIDC (or a machine client with OAuth client
credentials) already holds a JWT from the identity provider. The edge
forwards it **unchanged** with `CallOptions::on_behalf_of(token)`; it cannot
forge these tokens, only replay them until they expire. The service chooses
the verifier from the token's `iss`: a node id is an edge-minted token, any
other issuer must be trusted:

```toml
[identity]
accept_on_behalf_of = true

[[identity.trusted_issuers]]
iss = "https://accounts.example.com"
audiences = ["odata-core"]       # required: start-up fails without it
roles_claim = "groups"           # an array, or a space-separated string such as "scope"
role_map = { "data-readers" = "data.read", "data-admins" = "data.admin" }
max_token_lifetime_secs = 3600   # default
```

The service checks the signature (RS256, PS256 or ES256; `none`, `HS*` and an
`alg` that is not the key's are refused), `iss`, that `aud` holds a configured
audience, `exp`/`nbf`/`iat` with the leeway, and `exp - iat` ≤
`max_token_lifetime_secs`. Roles are the `roles_claim` values that
`role_map` maps; **unmapped values are dropped**, and a map may not grant
`aster.*` roles. `principal()` is `iss|sub`, `auth_methods()` the token's
`amr` (empty when it has none). Every failure is `UNAUTHENTICATED`.

**Keys never come from a fetch.** The service reads the issuer's keys from the
fleet namespace Enrollment already uses, under
`aster/identity/v1/issuers/<base64url(iss)>/jwks`, so air-gapped nodes verify
too. A token whose `kid` is not there is refused. A record counts only when
its docs author (the node that wrote it) holds a credential granting
`aster.identity-publisher`; the record carries that credential, so the
publisher needs no session with each service. Anyone holding the fleet
namespace's write secret can write entries there, so a service reads every
entry and counts nothing before its author is shown to be a publisher: other
authors change nothing, however much they write. A publisher has one record
per issuer; of its records for issuers the service does not trust, at most
`max(trusted issuers, 1) × 4` are kept, so one misbehaving publisher cannot
displace another's keys. The newest record per issuer
wins, so publishing a new key set rotates keys without a restart, and a key
dropped from it stops verifying once it syncs. A set older than
`[identity] max_key_set_age_secs` (default 172800, 48 hours; 60..=2592000,
overridable through `aster.ops.NodeConfig`) verifies nothing, so a publisher
that stops refreshing cannot keep a removed key alive. When the root rotates,
the service reads keys from the new root's fleet namespace; nothing written
under the old root counts.

`AsterServer::issuer_keys()` shows what a service holds (issuer, publisher,
`kid`s, fetch time). The publisher below writes these records; the low-level
writer is `aster::rpc::issuer_keys::publish_issuer_keys`.

#### 4.7.1.2 Publishing issuer keys (`[identity.publisher]`)

One node publishes each issuer's keys, normally the gateway, which already
talks HTTPS to the identity provider. Build `aster` with the
`identity-publisher` feature and configure it:

```toml
[identity.publisher]
issuers = ["https://accounts.example.com"]
jwks_url = { "https://accounts.example.com" = "https://accounts.example.com/.well-known/jwks.json" }
refresh_secs = 3600          # default; 60..=86400, with ±10 % jitter
min_refresh_secs = 60        # default; unknown-kid refreshes at most this often
ca_certs = ["/etc/aster/idp-ca.pem"]   # optional extra roots, as for relay_ca_certs
ca_only = false              # true: trust only ca_certs, not the WebPKI roots
```

(Env: `ASTER_IDENTITY_PUBLISHER`, a JSON object of the same fields; builder:
`AsterConfigBuilder::identity_publisher(IdentityPublisherConfig::new(...))`.)
`AsterClient` and `AsterServer` both run it; it needs `[enrollment]`, and
start-up fails on an invalid section.

- **Fetching.** HTTPS only, with TLS validated against the WebPKI roots plus
  `ca_certs` (or only `ca_certs`). No redirects, no proxy, at most 1 MiB, 5 s
  to connect and 15 s per fetch, status 200 only. Keys of an unsupported type
  or algorithm (anything but RSA RS256/PS256 and EC P-256 ES256), private
  keys and non-signature keys are dropped with a warning; a set with no
  usable key is a failure.
- **Refreshing.** Every `refresh_secs`, and at once when a token names a
  `kid` the publisher has not published for its issuer (the usual sign of a
  rotation): `AsterClient` checks every call it makes with
  `on_behalf_of` (unary calls, through its interceptor), and `AsterServer`
  every provider token it refuses for an unknown `kid`. Other edges call
  `publisher.notice_kid(iss, kid)` or `notice_token(jwt)` on the handle from
  `identity_publisher()`. A failure is retried with backoff (5 s doubling to
  `refresh_secs`) and **never replaces the last good record**.
- **Writing.** Every successful refresh writes the record again with a new
  fetch time, then synchronizes the fleet namespace with the server the
  client is connected to (or a server's `[enrollment] replicas`).
- **Status.** `identity_publisher().status()` gives each issuer's published
  `kid`s, fetch time, counts and the last error.

**Interplay with `max_key_set_age_secs`.** A record lives for
`max_key_set_age_secs` after its fetch. `refresh_secs` must be at most half
of the publisher's own `max_key_set_age_secs` (start-up refuses more), so a
failed refresh is retried for at least as long again before services stop
trusting the keys. With the defaults (3600 s and 48 h) the provider can be
unreachable for about 47 hours before its tokens stop verifying. Keep the
services' limit no lower than the publisher's.

**Admin steps.**

1. **Grant the role.** Issue the publisher's credential with
   `aster.identity-publisher` in its roles (for example
   `aster.role = "aster.gateway,aster.identity-publisher"`). Whoever holds
   this role can forge any user of the issuers it publishes for, so grant it
   to one or two nodes only.
2. **Seal the write grant.** Only the root can write the fleet namespace.
   The root seals its write capability to the publisher's node key, HPKE
   addressed as for observer grants, into the fleet namespace at
   `aster/identity-grants/v1/publishers/grants/<node id>`:

   ```sh
   aster enroll identity-publisher --seal-to <publisher node id> \
       --fleet-namespace <id> --fleet-secret-file fleet.secret \
       --audience <fleet> --peer <policy replica>
   ```

   The publisher opens the grant before every write, so it can be issued
   before or after the publisher starts. Withdraw it with
   `--unseal <node id>`; an honest publisher stops writing. A leaked
   capability can only be revoked by rotating the root (and so the fleet
   namespace); revoking the publisher's credential makes services ignore
   its records at once.
3. **Configure** `[identity.publisher]` on that node and
   `[[identity.trusted_issuers]]` on the services.

**Root rotation.** After a rotation, services and the publisher use the new
root's fleet namespace. The new root must seal a new write grant there, and
the publisher needs a credential from the new root; until then nothing is
published, and services hold no keys for the issuer.

#### 4.7.1.3 Calling Aster services on behalf of users (an edge, end to end)

The runnable example is `examples/rust/gateway-identity`: a browser signs in
at a Salvo edge, and the edge calls a `Notes` service over iroh as that user.

```sh
cargo run -p gateway-identity     # then open http://localhost:8080
cargo test -p gateway-identity    # the same flows, no browser
```

It starts the whole fleet in one process (a root, a stand-in OpenID provider
serving its JWK set over HTTPS, the service, the edge). In a deployment each
part is its own node, configured as below.

**The service** (`AsterServer`, from `AsterConfig`):

```toml
[enrollment]                         # as for every enrolled node
audience = "my-fleet"
fleet_namespace = "<id>"

[identity]
accept_on_behalf_of = true

[[identity.trusted_issuers]]         # the OIDC provider
iss = "https://accounts.example.com"
audiences = ["notes-edge"]           # the edge's client id (or a token-exchange audience)
roles_claim = "groups"
role_map = { "readers" = "notes.read", "writers" = "notes.write" }
```

Its methods use `requires` as for any caller, and read the user from the
`CallContext`. A method that needs a passkey sign-in checks `amr` with
`require_auth_method`; `requires` cannot express it, because the contract's
capability requirements are roles only:

```rust
#[aster::service(name = "Notes", version = 1)]
pub trait Notes {
    async fn whoami(&self, ctx: &CallContext) -> aster::Result<Caller>;

    #[rpc(requires = require_role("notes.write"))]
    async fn publish(&self, ctx: &CallContext) -> aster::Result<Caller>;
}

#[aster::rpc::async_trait]
impl Notes for NotesImpl {
    async fn publish(&self, ctx: &CallContext) -> aster::Result<Caller> {
        ctx.require_auth_method(AMR_WEBAUTHN)?;   // PERMISSION_DENIED otherwise
        // ctx.principal() = "iss|sub", ctx.actor() = Some(edge id),
        // ctx.auth_methods() = ["webauthn"]
        Ok(caller(ctx))
    }
    // whoami …
}
```

**The edge** (`AsterClient`) needs one credential and, if it also publishes
the provider's keys, `[identity.publisher]`:

```sh
cat > edge-attributes.json <<'EOF'
{"aster.role": {"type": "string", "value": "aster.gateway,aster.identity-publisher"},
 "aster.identity.issuable_roles": {"type": "string", "value": "notes.read,notes.write"}}
EOF
aster enroll node --subject <edge-node-id> --audience my-fleet \
  --attributes edge-attributes.json --out edge.credential
aster enroll identity-publisher --seal-to <edge-node-id> ...   # §4.7.1.2
```

- `aster.gateway` lets it forward identities at all;
- `aster.identity.issuable_roles` bounds what it may mint (a minted token
  with another role is refused);
- `aster.identity-publisher` and the sealed grant let it write the
  provider's keys. Leave both out when another node publishes.

Per HTTP request the edge attaches exactly one JWT. Build the generated
client once; `with_options` gives a cheap per-request copy:

```rust
use aster::rpc::{bearer_token, CallOptions, AMR_WEBAUTHN};

let notes = NotesClient::new(edge.rpc());           // once
let issuer = edge.user_token_issuer()?;              // once

// Sign-in path 1, OIDC: forward the provider's token unchanged.
let jwt = bearer_token(&authorization_header).ok_or(Unauthorized)?.to_string();

// Sign-in path 2, passkey: the edge verified the WebAuthn assertion and holds
// a session for the user; mint a token for this request only.
let jwt = issuer.mint_user_token(&user, ["notes.read", "notes.write"],
                                 [AMR_WEBAUTHN], Duration::from_secs(60))?;

let reply = notes
    .with_options(CallOptions::new().deadline(Duration::from_secs(15)).on_behalf_of(jwt))
    .publish()
    .await?;
```

Map `UNAUTHENTICATED` to HTTP 401 and `PERMISSION_DENIED` to 403. The
example's `webauthn` module is a minimal ES256 relying party over `aws-lc-rs`
(registration with attestation `none`, assertion with the RP ID hash, the
user-present and optionally user-verified flags, `clientDataJSON` type,
challenge and origin, the signature over `authenticatorData ‖
SHA-256(clientDataJSON)`, and the signature counter); a production edge may
use a full WebAuthn library instead. Keep challenges single-use and short
lived, and minted tokens short (the edge mints per request).

**Changing trusted issuers without a restart.** `[identity]` is the first
section `aster.ops.NodeConfig` can override (§4.7.2). An admin holding
`aster.config-admin` adds or replaces issuers; `set` answers once the new set
is live, and a provider whose keys are already in the fleet namespace is
trusted at once:

```rust
let api = NodeConfigClient::new(admin.rpc());
api.set(NodeConfigSetRequest::new().identity(IdentityOverride {
    trusted_issuers: Some(vec![
        TrustedIssuer::new("https://accounts.example.com", ["notes-edge"])
            .roles_claim("groups")
            .map_role("readers", "notes.read"),
    ]),
    ..Default::default()
})).await?;
```

Lock the section with `[remote_config] lock = ["identity"]` on nodes whose
issuers only the local file may set.

### 4.7.2 Remote overrides (`aster.ops.NodeConfig`)

Every `AsterServer` serves `aster.ops.NodeConfig`, which lets an admin set a
record on the node:

- **properties:** a name and tags;
- **typed attributes:** bool, int, float or string;
- **overrides** of the sections that declare themselves overridable.
  `[identity]` (`accept_on_behalf_of`, `trusted_issuers`,
  `edge_token_max_lifetime_secs`, `clock_leeway_secs`,
  `max_key_set_age_secs`) is the only one today. `trusted_issuers` replaces the whole list, and the resulting
  section is validated as a whole (an issuer without audiences is
  `INVALID_ARGUMENT`).

Gate 3 requires the role `aster.config-admin` on every method. `set` and
`clear` also require that the role comes from the caller's own Enrollment
credential, over Iroh, and not on behalf of a user. A node without Enrollment
therefore refuses every change.

```rust
use aster::rpc::node_config::{IdentityOverride, NodeConfigSetRequest};
use aster::rpc::NodeConfigClient;

# async fn f(admin: aster::rpc::AsterClient) -> aster::Result<()> {
let config = NodeConfigClient::new(admin.rpc());
let ack = config
    .set(
        NodeConfigSetRequest::new()
            .name("eu-gateway-1")
            .tag("gateway")
            .attribute("region", "eu-west")
            .identity(IdentityOverride {
                accept_on_behalf_of: Some(true),
                ..Default::default()
            }),
    )
    .await?;
assert!(ack.complete); // the override is in force on the node
let view = config.get().await?; // effective values, each with its source
# Ok(())
# }
```

- **Acknowledgement means applied.** `set` and `clear` answer only once the
  change is live, for example once the next call sees the new
  `accept_on_behalf_of`. `ack(generation)` answers the same question later,
  from the node's store and the settings in force. A request is validated as
  a whole before anything is stored, and applied without a restart.
- **Precedence:** defaults, then file / env / code, then the override. `get`
  reports each setting's source: `default`, `file`, `env`, `code` or
  `override`.
- **Attributes never authorise.** Keys starting `aster.` are refused, and
  `requires` never reads them. Handlers read them with
  `ctx.node_properties()` / `ctx.node_attribute("region")`; other code reads
  `AsterServer::node_config()`.
- **Hardening.** Only listed sections and fields can be overridden. Anything
  else, such as the bind port or `hook_failure_mode`, is refused with
  `PERMISSION_DENIED`. The local file can lock a section
  (`[remote_config] lock = ["identity"]`, refused with `FAILED_PRECONDITION`)
  or turn the service off (`[remote_config] enabled = false`). Either way, a
  stored override the file no longer allows is ignored.
- **Storage and reset.** The record is kept in a namespace of the node's own
  docs store, so it survives a restart when the node is persistent. Only an
  admin's `set` or `clear` changes it. `clear` reverts to the file
  configuration, and there is no local reset.
- **Audit.** Each change records the caller, the time and a generation
  number. It is logged under `aster::audit` and, with
  `AsterServerBuilder::audit_outbox(outbox)`, committed to the audit outbox
  before it is stored. A change the outbox refuses is not made
  (`UNAVAILABLE`).

### 4.8 Client interceptors (deadline / retry / circuit breaker)

Attach policies to a connection with builder methods; they wrap **unary** calls.
A connection with no policies is a no-op pass-through.

```rust
use std::time::Duration;
use aster::rpc::{CircuitBreaker, RetryPolicy};

let conn = node.rpc_connect(peer).await?
    .with_deadline(Duration::from_secs(5))               // per-attempt timeout → DEADLINE_EXCEEDED
    .with_retry(RetryPolicy::new(3))                     // retry retryable codes w/ backoff (idempotent only!)
    .with_circuit_breaker(CircuitBreaker::new(5, Duration::from_secs(30)));

let client = EchoClient::new(conn);
```

Custom interceptors implement `aster::rpc::Interceptor`
(`on_request`/`on_response`/`on_error`) and attach via `.with_interceptor(…)`.

### 4.9 Publish & discover a contract (manifests)

A producer publishes its contract collection (`contract.bin` + per-type defs +
`manifest.json`) into a registry doc; a consumer resolves it and verifies
`blake3(contract.bin) == contract_id`.

```rust
use aster::rpc::{fetch_and_verify_contract, publish_contract, AsterType};
use std::time::{SystemTime, UNIX_EPOCH};

# async fn publish(node: &Node) -> aster::Result<()> {
let doc = node.docs().create().await?;
let author = node.docs().default_author().await?;
let type_defs = vec![EchoRequest::aster_type_def(), EchoResponse::aster_type_def()];
let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;

let published = publish_contract(&doc, &node.blobs(), &author,
    &EchoClient::contract(), &type_defs, now_ms).await?;
println!("published contract_id = {}", published.contract_id);

// Consumer (after the registry doc has synced to it):
let fetched = fetch_and_verify_contract(&doc, &node.blobs(), &producer_id, "Echo", 1).await?;
assert!(fetched.verified);
# Ok(())
# }
```

#### Publication by default

`AsterServer::builder()...start()` publishes the contract of every service
added with `service` / `register_session` into a registry doc it creates, as
the Python, TypeScript, Java and .NET servers do. Read the result from
`srv.published_manifests()` and `srv.manifest_registry()` (share that doc
read-only so consumers can discover the services).

Publication never fails `start`. A node that cannot publish (built without
`docs` and `blobs`, or a service with no contract such as a hand-written
`ServiceDispatch`) logs at info level, serves normally and lists the reasons
in `srv.manifest_failures()`. To publish nothing, opt out:

```rust
# use aster::rpc::{AsterServer, ManifestPublication};
# async fn run() -> aster::Result<()> {
let srv = AsterServer::builder()
    .manifest_publication(ManifestPublication::Disabled)
    .start()
    .await?;
# Ok(())
# }
```

A server built with `deferred_surface()` chooses the policy per `RpcSurface`
(`BestEffort` by default; `Strict` fails the activation instead).

### 4.10 Draining a server, and sizing a router

`AsterServer::drain(grace)` takes a replica out of rotation for a rolling
restart or a scale-in, then shuts it down:

```rust
# async fn stop(srv: aster::rpc::AsterServer) {
let report = srv.drain(std::time::Duration::from_secs(30)).await;
tracing::info!(?report, "drained");
# }
```

1. The health listener, if enabled, answers `/readyz` with 503 first.
2. New connections are closed right after the handshake with QUIC application
   close code 503 (`aster::rpc::DRAIN_CLOSE_CODE`); new calls on existing
   connections answer `UNAVAILABLE` with the detail `aster.draining = true`.
   Calls on a session that already exists still run.
3. Each connection is closed with the drain code once it has had no call in
   flight and no live session for four round trips (100 ms to 2 s), so its
   last response is acknowledged before the close.
4. Calls of every pattern run until `grace`; then every remaining connection is
   closed, which cancels the handlers still running.

The `DrainReport` counts calls in flight, completed, cancelled at the deadline
and refused, and connections closed and refused. On a client,
`Error::is_draining()` is true for both a refused call and a call whose
connection the drain closed (`RpcConnection::is_drained()` tells the
connection apart), and `RetryPolicy` does not retry it: reconnect to another
replica. The server publishes no endpoint leases itself; if your application
publishes them, renew yours with the `draining` health status before draining.
A lower-level `Server` drains through `ServerHandle::drain(grace)`, which leaves
the node up.

**Sizing.** On loopback (macOS, Apple M3 Max) an idle `aster/1` connection
cost the server about **100-115 KiB of RSS** and about **1.5 % of one core per
1,000 connections** for keep-alives, from 500 to 2,000 connections
(`docs/_internal/investigations/2026-10-16-idle-connection-cost.md`; rerun
`aster/examples/idle_connections.rs` on your target). Plan on ~130 MiB per
1,000 idle peers before handler state, and measure on real paths: loss,
relays and multipath add state that loopback does not.

These `transport_*` knobs (§2.2) bound what a connection can cost under load;
they do not shrink an idle one, because windows are flow-control credit, not
preallocated buffers:

| Knob | Default | Bounds |
|---|---|---|
| `transport_stream_receive_window` | 1.25 MB | data buffered per incoming stream |
| `transport_receive_window` | unlimited | data buffered per connection across streams |
| `transport_send_window` | 10 MB | unacknowledged data queued per connection |
| `transport_max_concurrent_bidi_streams` / `_uni_streams` | 100 / 100 | concurrent streams a peer may open |
| `transport_keep_alive_interval_ms` | 5,000 | idle CPU and packets (keep it below the 30 s idle timeout and NAT timeouts) |

For a router with mostly idle peers, set a connection `receive_window` (for
example 256 KiB) and a smaller `send_window` so a slow or stalled peer cannot
make the server buffer megabytes, and cap the streams a peer may open.

---

## 5. Logging

`aster` logs through the [`tracing`](https://docs.rs/tracing) facade and
installs no subscriber: nothing is printed until your application installs
one. With none installed, each log site costs a level check and allocates
nothing. The usual choice is `tracing-subscriber` with an `EnvFilter`:

```toml
# Cargo.toml of your application
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

```rust,ignore
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Warnings from everything, routine server events from aster.
                .unwrap_or_else(|_| "warn,aster=info".into()),
        )
        .init();
    // ... start the runtime, the node, the server ...
}
```

Then `RUST_LOG=aster=debug` shows every call, `RUST_LOG=aster::rpc::dispatch=debug`
only the per-call events, and `RUST_LOG=warn,aster::admission=info` the Gate 0
refusals. The targets (also listed in `aster::log_targets`):

| Target | Level | Events |
|---|---|---|
| `aster::rpc::server` | info | `AsterServer` started / shutting down |
| | debug | serve loop start/end; connection closed (with the number of sessions reaped); connection on an ALPN nobody handles |
| `aster::rpc::dispatch` | debug | an `rpc` span around every native handler (`service`, `method`, `call_id`, `peer`), so your handler's own events carry the call; `rpc completed` (`status`, `elapsed_us`) for every call; `rpc refused` (status and message) for calls stopped before a handler; deadline expiry |
| | warn | a corrupt request frame: a frame that does not inflate, or a client-stream / bidi request that does not decode. The call fails with `INVALID_ARGUMENT` |
| `aster::rpc::session` | debug | session opened, sessions reaped with their connection, HTTP session ended |
| `aster::rpc::foreign` | debug / trace | binding-implemented services: call cancelled (peer, deadline, invalid request) or abandoned (debug); call delivered (trace) |
| `aster::admission` | info | Gate 0: inbound peer rejected (`peer`, `alpn`, `code`, `reason`) |
| | debug | Gate 0: inbound peer accepted; outbound connect allowed / blocked |
| `aster::ops` | info / debug | health listener started / stopped; accept and connection errors |

The transport underneath logs under `aster_transport_core` (reactor, framing)
and `iroh` / `iroh_*` (QUIC, relay, discovery); `RUST_LOG=warn,aster=debug`
keeps those quiet while showing everything from `aster`.

Python, TypeScript, Go, Java and .NET forward Rust events to their language
loggers through the shared observability pipeline. See the
[observability guide](aster-observability-getstarted.md) for initialization,
exporters and log-level synchronization.

---

## 6. Cargo features

| Feature | Default | What it enables |
|---------|:-------:|-----------------|
| `blobs` / `docs` / `gossip` | ✅ | the corresponding `Node` accessors |
| `enrollment` | — | Enrollment: credentials, root policy, the handshake, and (with `rpc` + `docs`) `[enrollment]` on `AsterServer` / `AsterClient` |
| `rpc` | — | the RPC framework (pulls Fory + the proc-macros) |
| `discovery` / `hooks` | — | Local discovery and admission hooks |
| `otel` / `health` | — | Observability integration and the health/Prometheus listener |

Admission (`Node::take_admission`) additionally requires the node to be started
with `AsterConfigBuilder::hooks(true)`.

---

## 7. Current limits

- **Gate 2** auth (session `authorize` → rcan) is not yet in the Rust crate.
  Enrollment (§3.0) covers admission and per-call roles.
- **Generated client request-stream methods** take a `Vec<Req>`. For incremental
  request sending, use the lower-level `RpcConnection::client_stream_open` or
  `bidi_open` APIs.
- **Interceptors** apply to unary calls; streaming calls are not yet intercepted.
- `Admission` exposes separate `next_handshake()` (inbound) and `next_connect()`
  (outbound) streams; with hooks enabled, unhandled outbound connect hooks wait
  for the hook timeout before a default accept. `next_handshake()` yields only
  inbound connections; this node's own are accepted at the handshake.

---

## 8. See also

- [aster-baseline-services-getstarted.md](aster-baseline-services-getstarted.md) —
  the default-on `aster.ops.NodeInfo` service, the `aster/Value` typed
  name/value primitive, the reserved `aster.*` namespace, and defining your own
  union payloads with `#[derive(ForyUnion, AsterType)]`.
- [aster-sealed-grants-getstarted.md](aster-sealed-grants-getstarted.md) —
  `aster::grants`: hand a secret (e.g. a docs-namespace capability) to one node
  through a publicly-replicated policy doc.
- [aster-expose-getstarted.md](aster-expose-getstarted.md) /
  [aster-http-getstarted.md](aster-http-getstarted.md) — additional transports
  on the same node.
