# Aster for Python: getting started

The Python binding (`pip install aster-rpc`, `import aster`) wraps the Rust
`aster` crate. Configuration, the node, and node identity are the Rust ones;
this page covers the Python surface shipped in the 0.3 preview.

## 1. Start a node

```python
import asyncio
from aster import AsterConfig, Node, blobs_client

async def main():
    config = AsterConfig(storage_path="/var/lib/my-app")  # omit for in-memory
    async with await Node.start(config) as node:          # shut down on exit
        print("node id:", node.id)
        blobs = blobs_client(node)
        ...

asyncio.run(main())
```

`Node.start(config=None, *, alpns=None, gate=None)` is a coroutine that returns a running
`Node`. The node is an async context manager, so `async with await
Node.start(...)` closes it on exit (the same shape as `asyncio.start_server`).
Without `async with`, call `await node.shutdown()` yourself; it flushes
persistent state before closing.

| `Node` member | Returns |
|---|---|
| `id` (property) | `NodeId`, which compares and hashes equal to its 64-char hex string |
| `node_id()` | the id as a hex `str` |
| `node_addr_info()` | `NodeAddr` (id + relay + direct addresses) |
| `secret_key()` / `export_secret_key()` | `SecretKey` / 32 raw bytes |
| `accept_aster()` | `(alpn: bytes, IrohConnection)` for custom ALPNs passed via `alpns` / `config.alpns` |
| `admission(*, outbound=False)` | Gate 0 requests as an async iterator (needs `enable_hooks=True`, no `gate`); see section 5 |
| `gate` (property), `has_hooks()` | the attached `GatePolicy` (or `None`); whether admission hooks are on |
| `add_node_addr(other)` | teach this node another node's address (tests, local setups) |
| `shutdown()` / `close()` | clean shutdown |

The protocol clients take the node: `blobs_client(node)`, `docs_client(node)`,
`gossip_client(node)`, `net_client(node)`.

## 2. Configure: `AsterConfig`

`AsterConfig` is a frozen dataclass mirroring the Rust `aster::AsterConfig`.
Rust loads and checks every value. Settings resolve in this order, later
sources overriding earlier ones:

1. built-in defaults
2. `aster.toml`, read by `AsterConfig.from_file(path)`
3. `ASTER_*` environment variables, read by `AsterConfig.from_env()` (and by
   `from_file`, which applies them on top of the file)
4. code: keyword arguments, or `config.replace(**changes)` for a modified copy

```python
config = AsterConfig.from_file("aster.toml").replace(bind_addr="0.0.0.0:9000")
```

The keyword names are the Rust field names, which are also the TOML keys. The
complete builder / env / TOML table is in
[aster-rust-getstarted.md section 2.2](./aster-rust-getstarted.md#22-configuration-reference).
Two spellings differ in Python:

- `gc_interval_ms: int | None` stands in for the builder's `gc_interval(Duration)`.
  `None` or `0` turns GC off.
- `relay_mode` takes `"default"`, `"staging"`, `"disabled"` or `"custom"`.
  Setting `relay_urls=[...]` selects `"custom"`, and so does a bare URL passed
  as `relay_mode`.

Two settings are on by default, and you can turn either one off:

- `blob_zstd_level: int | None` (default `3`; `ASTER_BLOB_ZSTD_LEVEL`,
  `[storage] blob_zstd_level`) is the zstd level for blob transport. `0`,
  `None` or `"off"` turns compression off, so the node serves and fetches
  blobs only on the plain ALPN. Levels above 19 are clamped to 19. When
  compression is off, the resolved value is `None`.
- `reuse_last_port: bool` (default `True`; `ASTER_REUSE_LAST_PORT`,
  `[network] reuse_last_port`): a persistent node (`storage_path`) re-binds
  the UDP ports it last used, which are recorded in
  `<storage_path>/bind-ports.json`. With `False`, the node binds a fresh port
  on every start and records nothing.

```python
config = AsterConfig(storage_path="/var/lib/my-app", blob_zstd_level=0, reuse_last_port=False)
```

The low-level `EndpointConfig` takes the same two keyword arguments.
`blob_zstd_level` is an int there, and `0` turns compression off. A bare
endpoint serves no blobs and records no ports, so on an endpoint these
settings are only passed through to its config.

Cluster network settings are the Rust `AsterConfig` fields of the same names
(env `ASTER_<NAME>`, TOML `[network]`); list values also take one
comma-separated string, the env spelling:

- `bind_addr_v6` (e.g. `"[::]:7842"`) pins the IPv6 UDP socket as `bind_addr`
  pins the IPv4 one, so a default-deny NetworkPolicy can allow one port per
  family.
- `advertise_addrs` adds `IP:port` addresses the node cannot discover itself
  (a NodePort, a load balancer) to its published addresses. Peers still check
  the node key in the handshake.
- `relay_ca_certs` / `relay_ca_only` trust a private CA for relay TLS (and for
  custom discovery's pkarr relays), beside or instead of the WebPKI roots.
- `discovery` is `"default"` (n0 at `iroh.link`), `"none"`, `"local"` (mDNS
  only) or `"custom"`. Setting `discovery_dns_origin` or
  `discovery_pkarr_relays` selects `"custom"`; combining either one with
  `"none"` or `"local"` is a `ValueError`. Relays are separate: an air-gapped
  node also sets `relay_mode`.

`metrics_peer_labels` / `metrics_peer_label_cap` (`[metrics] peer_labels` /
`peer_label_cap`, `ASTER_METRICS_PEER_LABELS` / `_CAP`) label
`aster.peer.bytes` per peer for the first `cap` peers (default 64) instead of
by locality level. The ops listener that `AsterServer` starts from `[health]`
applies them.

```python
config = AsterConfig(
    bind_addr="0.0.0.0:7842", bind_addr_v6="[::]:7842",
    advertise_addrs=["203.0.113.10:31842"],
    discovery_dns_origin="dns.example.internal",   # discovery == "custom"
)
```

Invalid values raise when the config is built: `ValueError` for a bad value,
`TypeError` for a wrong type or an unknown field, and `FileNotFoundError` when
`from_file` cannot find its file. `config.print_config()` prints the resolved
settings with the source of each one and masks secrets.

## 3. Node identity

A node's identity is its secret key. On disk, that key is a **secret key
file**: 64 hex characters, written with mode 0600 on Unix. The Rust SDK and every
binding use this same format.

```python
from aster import AsterConfig, SecretKey

config = AsterConfig(secret_key_file="/var/lib/my-app/node.key")
```

- When the file exists, the node loads its key from it. When it does not,
  `Node.start` creates it, using `secret_key` if one is set and a fresh key
  otherwise.
- The start fails with `ValueError` if the file is malformed, or if
  `secret_key` is also set to a different key.
- `SecretKey` provides `generate()`, `from_hex()`, `from_bytes()`,
  `read_file()`, `read_file_if_exists()`, `write_file()` (which never
  overwrites an existing file), `load_or_generate()`, `public_key()` and
  `node_id()`. Its `repr` never shows the key.
- `aster keygen producer --out PATH` writes a key file in this format and
  prints the node id it gives.

A persistent node started directly with `Node.start` gets a fresh identity on
every start unless you set `secret_key` or `secret_key_file`. `AsterServer` and
`AsterClient` handle this for you: when `storage_path` is set and no key is
configured, they use `<storage_path>/node.key`.

The `.aster-identity` TOML file, selected by `identity_file` or found
automatically in the working directory, holds peers and published-service
tokens, not the node key file. If it has a `[node] secret_key`, that key is
used when the config does not set one. Enrollment credentials are separate
chain files (`[trust] enrollment_credential`, section 4).

## 4. `AsterServer` / `AsterClient`

Both take `config=AsterConfig(...)`, or read `AsterConfig.from_env()` when no
config is given, and start their node with `Node.start`. Persistent storage,
relay settings, transport tuning and the identity rules above therefore apply
to both.

Admission is configuration. With Enrollment configured (`[trust]
root_pubkey` and a credential, plus `[enrollment] audience` and
`fleet_namespace`; the schema, TOML keys and `ASTER_ENROLLMENT_*` variables
are those of the Rust `AsterConfig`, `aster.EnrollmentConfig` in Python),
Enrollment owns Gate 0: `AsterServer` admits only nodes holding a credential
from the root, maps each peer's verified claims to roles (`[enrollment.roles]`,
default the `aster.role` claim) that `requires=` checks on every call, closes
a revoked peer's connections, and renews its own credential. `AsterClient`
presents its credential over the reciprocal enrollment handshake, syncs the
fleet namespace the server offers, and renews. Any missing or malformed
setting fails `start()` / `connect()` with `ValueError`; nothing falls back
to an open node. `server.enrollment` / `client.enrollment` expose the
installed `aster.Enrollment`. Credentials come from the `aster` CLI (`aster
enroll node`); see `examples/python/mission_control/README.md`.

Without enrollment the server is open. `AsterServer(..., gate=GatePolicy(...))`
supplies a custom Gate 0 instead (section 5); `server.gate.admit(peer,
attributes=...)` then records the attributes `requires=` reads, and
`server.gate.revoke(peer_id)` shuts the peer out of new connections.

### How RPC is served

The Python process runs only your handlers. Everything before a handler runs
in Rust (`aster::rpc::Server` with dynamic dispatch):

- **Registration.** At start, each `@service` class is lowered to a
  `DynamicService` (`aster._lowering.lower(cls)`): name, version, scope,
  methods and patterns, type hashes from the type-graph walker, `requires=`,
  `idempotent=` and `timeout=`. Rust derives the contract from it, so
  `aster._lowering.contract_id(cls)` equals `contract_id_from_service(cls)`.
  The contract id commits to `requires=`, which Rust enforces.
- **Pre-dispatch.** Rust decodes the header and looks up the service and
  method. It checks the serialization mode and applies the session rules
  (per-connection sessions, graveyard, `max_sessions_per_connection`). It
  then runs Gate 3 (`requires=`, checked against the caller's enrollment
  roles or the attributes your gate recorded) and arms the deadline. A refused call never
  reaches Python.
- **Deadlines.** The effective deadline is the minimum of the caller's
  deadline, the method's `timeout=` and the server's handler cap (300 s,
  `AsterServer(handler_timeout=...)`, `<= 0` for none). Rust sends
  `DEADLINE_EXCEEDED` and cancels the handler task. No server-side
  `DeadlineInterceptor` is needed; the client half of `DeadlineInterceptor`
  is unchanged.
- **Errors.** An `RpcError` raised by a handler reaches the caller as-is
  (code, message, details). A request payload that does not decode fails with
  `INVALID_ARGUMENT`. Any other exception fails with `INTERNAL` and its
  message. Unknown services fail with `NOT_FOUND`, and unknown methods with
  `UNIMPLEMENTED`.
- **Sessions.** A `scoped="session"` class is instantiated as
  `cls(peer=...)` on a session's first call. Its `close()` runs when the
  connection closes.
- **Interceptors** run in Python as before: request, handler, response, and
  for streams also error.
- **Handler context.** A handler that declares a `ctx: CallContext` parameter
  gets the call's context; any code in the handler's task can read it with
  `CallContext.current()`. It mirrors Rust's `aster::rpc::CallContext`:
  `peer` (the transport's caller id) and `principal` (the authenticator's
  resolved caller, else `peer`), `provenance` (`"iroh_authenticated"` or
  `"other"`), `attributes` (what Gate 3 checked), `metadata`, `service`,
  `method`, `call_id`, `session_id`, `deadline` / `remaining_seconds` (the
  effective deadline above), `is_cancelled()` and `await ctx.cancelled()`.
  A peer cancel (or a drain's deadline) cancels the handler's task, so
  `cancelled()` serves early exits raced against other work, and tasks the
  handler spawned with the context see it resolve.
- **Baseline services.** The node also serves `aster.ops.NodeInfo`, from
  Rust; its `aster.services` / `aster.registry_namespace` attributes tell an
  `AsterClient` which services the server offers.

Per-call options (`aster.CallOptions`, Rust `CallOptions`) set the deadline
(`timeout`, seconds; sub-second values are enforced on the client, the header
rounds up to whole seconds), metadata and serialization mode on every pattern:

```python
svc = await client.client(MissionControl)
fast = svc.with_options(CallOptions(timeout=0.5, metadata={"tenant": "t1"}))
await fast.getStatus(req)                                   # unary
async for entry in fast.tailLogs(req): ...                  # server stream
await svc.ingestMetrics(points(), options=CallOptions(timeout=5))
channel = fast.runCommand()                                 # bidi
mc = (await client.proxy("MissionControl")).with_options(CallOptions(timeout=2))
```

A call's explicit `timeout=` / `metadata=` win over its `options=`, which win
over the client's `with_options`. Generated clients (`aster contract
gen-client`) take the same `metadata=` and `options=` keywords.

**Draining.** `report = await server.drain(grace_period)` is Rust's
`AsterServer::drain`: it sets the ops listener not ready, refuses new
connections (closed with `DRAIN_CLOSE_CODE`, 503) and new calls (`UNAVAILABLE`
with the `aster.draining` detail; calls on an existing session still run),
closes each connection once idle, and after `grace_period` seconds closes the
rest, which cancels what still runs. It returns a `DrainReport`
(`calls_in_flight`, `calls_completed`, `calls_cancelled`, `calls_refused`,
`connections_closed`, `connections_refused`, `deadline_reached`, `elapsed`).
A client sees `RpcError.is_draining` and should reconnect to another replica.
Follow the drain with `close()`; `server.is_draining` reports whether one has
started.

Clients (`create_client`, `AsterClient.client`, the proxies) call through the
pooled Rust `RpcConnection` (`aster._aster.RpcClient`). A unary call is one
native crossing. Streamed responses are read ahead in Rust and handed over in
batches. Session ids come from the connection's shared counter, so Python and
Rust clients on one connection never collide.

### Calling on behalf of a user (gateway identity)

The Rust guide's §4.7.1 describes the model; in Python it is the same
configuration and the same checks, all in Rust. An edge that signs users in
itself calls a service **as the user** with one JWT per call: a token it
mints, or an identity provider's token forwarded unchanged. The service
verifies it before Gate 3, so `requires` checks the token's roles.

- **The service accepts it:** `AsterConfig(identity_accept_on_behalf_of=True)`
  (`[identity] accept_on_behalf_of`, `ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF`);
  `AsterServer(accept_on_behalf_of=...)` overrides it. Off, a call carrying
  the entry is refused with `PERMISSION_DENIED`.
- **The edge may forward:** its credential grants `aster.gateway`
  (`aster.identity.GATEWAY_ROLE`).
- **The edge may issue the roles:** its credential carries
  `aster.identity.issuable_roles` (comma-separated).

```python
from aster import CallOptions
from aster.identity import AMR_WEBAUTHN

# On the edge (an enrolled AsterClient), after checking alice's passkey:
token = client.mint_user_token("alice", ["data.read"], [AMR_WEBAUTHN], ttl=120)
data = (await client.client(Data)).with_options(CallOptions(on_behalf_of=token))
reply = await data.read(req)
```

`aster.bearer_token(header)` takes the token out of an `Authorization:
Bearer` header for forwarding a provider's token unchanged. In the handler,
`ctx.principal` is `iss|sub`, `ctx.actor` the edge (also `ctx.peer`),
`ctx.auth_methods` the token's `amr`, `ctx.attributes["aster.role"]` the
token's roles, and `ctx.on_behalf_of` the whole verified identity
(`ForwardedIdentity`: `issuer`, `subject`, `jti`, `expires_at`). For methods
that need a stronger sign-in, call `ctx.require_auth_method(AMR_WEBAUTHN)`
(raises `PermissionDeniedError`).

Providers are `identity_trusted_issuers=[TrustedIssuer(iss=..., audiences=(...,),
roles_claim="groups", role_map={...})]` (`[[identity.trusted_issuers]]`;
needs Enrollment). Their keys come from the fleet namespace, written by the
node running `identity_publisher=IdentityPublisherConfig(...)`
(`[identity.publisher]`), whose credential grants
`aster.identity-publisher`. On an `AsterClient` that runs the publisher, every
token a typed client sends is noticed, so a new key id is published at once;
`client.enrollment.notice_token(jwt)`, `notice_kid(iss, kid)` and
`publisher_status()` are the explicit forms (`LookupError` without a
publisher).

**NodeConfig.** Every `AsterServer` serves `aster.ops.NodeConfig` behind
`aster.config-admin` (`remote_config_enabled=False` turns it off,
`remote_config_lock=("identity",)` locks the `[identity]` override). An admin:

```python
from aster.identity import IdentityOverride

admin = await client.node_config()            # NodeConfigClient
view = await admin.get()                      # view.setting("identity.accept_on_behalf_of").source
ack = await admin.set(name="core-1", tags=["eu"], attributes={"tier": 3},
                      identity=IdentityOverride(accept_on_behalf_of=False))
await admin.ack(ack.generation, timeout=5)
await admin.clear()
```

Handlers read the properties with `ctx.node_properties` / `ctx.node_attribute("tier")`
(and the server with `server.node_properties()`); they describe the node and
never authorise. `AsterServer(audit_outbox=...)` records every `set` / `clear`.
`setting_sources` on `AsterConfig` carries where each overridable setting came
from, so `get` reports `file` / `env` / `code`.

## 5. Gate 0: connection-level admission

Gate 0 runs in Rust. With Enrollment configured it is Enrollment's
(section 4). Otherwise you may attach a `GatePolicy` (the `aster` crate's
`GatePolicy` and `PeerAdmissionStore`). Its admission ALPNs
(`ALPN_PRODUCER_ADMISSION`, `ALPN_DELEGATED_ADMISSION`) are always open. In `TrustMode.PROTECTED` every other
ALPN (RPC, blobs, docs, gossip, your own) needs an unexpired admission. In
`TrustMode.OPEN_DEV` everything is open.

```python
from aster import AsterConfig, GatePolicy, Node, TrustMode

gate = GatePolicy(TrustMode.PROTECTED)    # or GatePolicy.open_dev()
gate.set_reject(403, b"peer not admitted")  # QUIC close code + reason
node = await Node.start(AsterConfig(), gate=gate)

gate.admit(peer_id, attributes={"aster.role": "reader"}, ttl=3600)
gate.is_admitted(peer_id)       # True
gate.attributes(peer_id)        # {"aster.role": "reader"}
gate.revoke(peer_id)            # new connections are rejected again
```

With a gate attached, every inbound connection is decided in Rust and nothing
reaches Python. Rejected peers see the gate's close code and reason. The gate
also decides this node's own dials (including docs sync and gossip): a dial to
a peer it would not admit fails before connecting. Setters
(`trust_mode`, `admission_alpns`, `set_reject`) and admissions apply to the
next connection. `gate.store` is the live `PeerAdmissionStore` (`admit`,
`revoke`, `get` returning a `PeerAdmission` record, `attributes`,
`prune_expired`, `len`, `in`); a store can be shared between gates with
`GatePolicy(store=...)`. `admit` takes `attributes`, `expires_at` (epoch
seconds) or `ttl` (seconds), `source`, `owner_id`, `proof_hash` and
`policy_epoch`. Peer ids are `NodeId`s or 64-hex strings.

To decide in Python instead, start the node with `enable_hooks=True` and no
gate, then iterate its admission requests:

```python
node = await Node.start(AsterConfig(enable_hooks=True))
async for req in node.admission():      # HandshakeRequest per inbound peer
    if allowed(req.peer, req.alpn):
        req.accept()
    else:
        req.reject(4031, b"not on the list")
```

`node.admission(outbound=True)` also yields a `ConnectRequest` for each of
this node's own dials (otherwise they are allowed). A request left unanswered
falls back to `hook_failure_mode` after `hook_timeout_ms`: `"fail_closed"`
(the default) rejects it, `"fail_open"` (explicit opt-in) accepts it. The iterator ends
when the node closes.

Or hand the decisions to an `AdmissionHandler` served over the upcall channel
(section 9) instead of iterating:

```python
from aster import AdmissionHandler

class Allow(AdmissionHandler):
    async def handshake(self, peer, alpn, is_alive):
        return allowed(peer, alpn) or (4031, b"not on the list")  # True / False / (code, reason)
    # async def connect(self, peer, alpn) -> bool   (outbound; default: allow)

node = await Node.start(AsterConfig(enable_hooks=True))
node.serve_admission(Allow())
```

A handler that raises or misses the `admission_handshake` timeout gets that
kind's failure mode (fail-closed: `403 "hook unavailable"`).

## 6. Blobs, docs and the network layer

These wrap the Rust SDK's `Blobs`, `Docs`/`Doc` and `Connection`. Every hash
or node-id argument takes a typed `Hash` / `NodeId` or a hex string; namespace
arguments take 32 bytes or 64 hex characters.

Blobs (`blobs_client(node)`):

| Call | Returns |
|---|---|
| `read_range(hash, offset, length)` | bytes |
| `download_hash(hash, provider, format=BlobFormat.RAW)` | bytes |
| `download_hash_to_store(hash, provider, format=...)` | `None` (the blob is stored, not copied out) |
| `download_hash_from(hash, providers, format=..., strategy=FetchStrategy.WHOLE)` | `(bytes, FetchReport)` |
| `download_hash_to_store_from(hash, providers, format=..., strategy=...)` | `FetchReport` |
| `download_collection_from(hash, providers, strategy=...)` | `([(name, bytes)], FetchReport)` |
| `tag_set(name, hash, format=...)`, `tag_delete(name)`, `tag_delete_prefix(prefix)` | `None` / count removed |
| `gc_run_once()` | `None`, after one GC pass over every untagged blob |

`providers` is an ordered list: each is tried in turn, and a later one fetches
only what is still missing. `FetchStrategy.SPLIT_CHILDREN` fetches each child
of a HashSeq as its own request and needs `BlobFormat.HASH_SEQ`. A
`FetchReport` has `providers` (`ProviderOutcome`: `node_id`, `attempts`,
`failures`, `completed_requests`, `in_flight`, `plain_fallbacks`, and the bytes
it carried: `bytes_transferred`, plus `wire_bytes`, the compressed bytes over
zstd, `None` when every attempt was plain), `attempted()`, `failed()`,
`served()`, `in_flight()`, `plain_only()` (providers without zstd: cache them
as plain-only rather than ranking them down) and `bytes_transferred`. `format`
also accepts `"raw"` / `"hash_seq"`.

`blobs.transfer_stats()` returns the node's `BlobTransferStats` since start
(monotonic, both directions): payload, zstd payload, wire and plain bytes for
fetching and serving, `plain_fallbacks`, `plain_serves`, `fetch_transfers` /
`serve_transfers` (`BlobTransferCounts`: `plain_ok`, `plain_failed`,
`zstd_ok`, `zstd_failed`) and the zstd decode errors. Per peer, a monitored
node's `node.topology().peer(id)` carries `quic_traffic` (`QuicTraffic`: QUIC
bytes sent and received for `blobs`, `blobs_zstd`, `docs`, `gossip`, `rpc` and
`other`, plus `total()`) and `blob_transfer` (`PeerBlobStats`).

Docs (`docs_client(node)` and its `DocHandle`s):

- `DocsClient`: `default_author()` (equals the node id), `open(namespace)`
  (`None` if not local), `open_or_import_read_namespace(namespace)`,
  `open_or_import_write_namespace(secret)`, `authority_report(namespace)`,
  `retire_local_write_authority(namespace)`.
- `DocHandle`: `set_bytes_durable(author, key, value)` (commits value blob metadata before
  publishing the document record),
  `entry_exists(author, key)` (true for a deletion tombstone too),
  `entry_content_status(hash)` (`BlobStatusResult`),
  `read_entry_contents_if_complete(hashes)`, `authority_report()`,
  `shutdown_for_role_swap(budget_seconds)`, `share(ShareMode.READ)`,
  `set_download_policy(DownloadPolicy.nothing_except([b"prefix/"]))`,
  `get_download_policy()`.
- `AuthorityReport` has `namespace_id`, `namespace_hex`, `access`
  (`LocalAccess.WRITE` / `READ`, as stored) and `fingerprint`
  (`AuthorityFingerprint`, comparable with `AuthorityFingerprint.of(namespace,
  access)`). `RetireOutcome` has `retired`, `access` and `handles` (open
  references that made it refuse; release them with
  `shutdown_for_role_swap`).
- Events are async iterators: `async for ev in doc.events(): ...` (or
  `await doc.subscribe()`); `ev.kind` says which fields are set.

The durable write orders blob persistence before document publication: after a
hard process kill, a surviving record cannot name missing locally-authored
content. It does not guarantee persistence of document metadata or survival of
sudden machine power loss.

Network: `IrohConnection.paths()` lists every open `PathInfo` (`kind`
`"direct"` / `"relay"` / `"other"`, `addr`, `relay_url`, `is_selected`,
`rtt_micros`) and `selected_path()` returns the one carrying data.
`IrohSendStream.set_priority(n)` / `priority()` order send streams on one
connection (higher first, default 0).

## 7. Sealed grants, leases and topology

These wrap the Rust SDK's `crypto`, `grants`, `lease` and `topology`
modules; every wire-defining rule runs in Rust. Value types are
frozen dataclasses (vary one with `dataclasses.replace`); durations are
seconds. Key arguments take a `SecretKey` / `PublicKey` / `NodeId`, 32 bytes,
or 64 hex characters.

**HPKE keys from identities** -- `hpke_x25519_public_from_node_id(peer_id)`
gives the recipient key for `hpke_seal`; the peer opens with
`hpke_x25519_secret_from_identity(node.secret_key())` and `hpke_open`.
Nothing extra to publish; rotating the key means rotating the node identity.

**Sealed grants** -- a root writes one record per recipient into a replicated
policy doc, sealed to the recipient's identity key and bound to a
`GrantContext` (app, granter, recipient, resource, path, role):

```python
from aster import (GrantContext, GrantRecord, NamespaceCapability, PolicyDoc,
                   docs_client, grant_key, reconcile_namespace_grant,
                   seal_namespace_grant)

ctx = GrantContext(app="myapp/grant/v1", granter=root.id, recipient=member_id,
                   resource=bytes.fromhex(ns_id),
                   path=grant_key("/myapp/v1", member_id), role="write")
record = GrantRecord.active("write", 1, seal_namespace_grant(ctx, capability))
await doc.set_bytes(root_author, ctx.path.encode(), record.encode())

# on the member
policy = await PolicyDoc.import_read_only(docs_client(node), ns_id, root_author)
outcome = reconcile_namespace_grant(
    node.secret_key(), ctx, await policy.fetch_grant("/myapp/v1", node.id))
match outcome.kind:
    case "active": use(outcome.value)          # a NamespaceCapability
    case "revoked" | "unopenable": drop()       # outcome.drops_capability
    case "absent": pass                         # not revocation: keep waiting
```

`PolicyDoc` reads only the pinned author's records (`get`, `list_prefix`,
`fetch_grant`, `subscribe` + `is_policy_write`) -- HPKE Base has no sender
authenticity, so this is what makes grants trustworthy. Revoke by writing
`GrantRecord.revoked(role, reason)`, never by deleting the key. Also:
`seal_grant` / `open_grant` (raw payloads), `seal_grant_to_key` /
`open_grant_with_key` (explicit X25519 keys), `grant_aad`, `check_grant_role`,
`reconcile_grant`. Grant failures raise `ValueError`.

**Fenced leases** -- a single owner per resource. The grant is a CAS plus a
TTL; correctness comes from the fence every protected action carries:

```python
from aster import LeaseOptions, MemorySerializer

ser = MemorySerializer()
outcome = await ser.acquire("tree/42", node.id, LeaseOptions(ttl=30))
if outcome.granted:
    async with outcome.lease as lease:          # released on exit
        fence = lease.fence()                   # take at send time; None = stop
        if fence:
            ser.mutate_fenced(fence, node.id, write)   # FenceRejectedError if stale
else:
    backoff(outcome.reason, outcome.snapshot.remaining)
```

The lease renews in Rust; `lease.status` is `"held"` / `"released"` /
`"fenced"` / `"lost"`, and `async for s in lease.status_changes()` ends after
the first terminal status. `stop_renewing()` hands back the fence for one
final operation without releasing. A designated grantor serves leases over
RPC: `server = grantor.serve_lease_grantor(MemorySerializer())` on a node
started with `alpns=[b"aster/1"]` (it owns that node's RPC accept loop), and
clients use `ser = await node.lease_grantor(grantor_id)` -- every grant is
bound to the caller's authenticated identity, and `revoke` needs the
`operator` role unless `open_revoke=True`. `lease_transition(row, expired, op,
resource)` and `check_fence(row, fence, writer)` are the pure rules for a
resource that is its own serialization point.

**Topology** -- `node.topology()` on a node started with
`enable_monitoring=True`:

```python
topo = node.topology()
topo.peers(), topo.peer(peer_id)                # v1: PeerView (level, rtt_us, ...)
await topo.enable_shared(secret, bootstrap=[root_id])
snap = await topo.snapshot()                    # v2: one consistent view
snap.clusters, snap.cluster_of(peer_id), snap.separated(a, b)
```

`separated` answers `"unknown"` / `"connected"` / `"separated"`; `unknown` is
never evidence of distance. `enable_shared` also takes `position_refresh`
(seconds) and `timing=TopologyTiming(...)`.

**Python-implemented lease pieces** (served over upcalls, section 9):

```python
from aster import FencedResource, FenceRejectedError, LeaseAuthorizer, LeaseSerializer

class RowStore(LeaseSerializer):                 # your own serialization point
    async def transition(self, resource, op):     # one CAS, e.g. lease_transition(row, ...)
        ...
    async def snapshot(self, resource):
        ...

outcome = await RowStore().acquire("tree/42", node.id, LeaseOptions(ttl=30))  # Rust engine, Python CAS
server = grantor.serve_lease_grantor(RowStore())  # or back a designated grantor

class Admins(LeaseAuthorizer):
    async def authorize(self, op):                 # False -> denied / unauthorized
        return op.actor in admins
MemorySerializer().set_authorizer(Admins())      # a plain (op) -> bool works too

class Register(FencedResource):
    async def mutate(self, fence, writer, op):    # check_fence(...) inside your lock
        ...                                        # return output bytes, or raise FenceRejectedError
```

A serializer or fenced-resource failure is never guessed into an outcome:
the Rust caller gets an error (`UpcallError`; for a fenced mutation with no
answer `FenceOutcomeUnknownError` -- the write may or may not have happened).
A failed authorizer applies the `lease_authorize` failure mode (fail-closed).

**Topology `admitted` filter.** `enable_shared(secret, admitted=...)` takes a
predicate `(NodeId) -> bool` (sync or `async def`), an object with
`admitted_batch(node_ids)`, or an `AdmittedFilter(predicate, ttl=30,
failure_retry=1, max_batch=256)`. Rust consults it synchronously on its read
path, so it never waits for Python: answers come from a cache filled by
batched upcalls. A fresh entry is a hit; a stale one (older than `ttl`) is
served and refreshed in the background; an unknown id gets the
`topology_admitted` failure-mode answer (fail-closed: filtered) until its
batch is answered -- a new producer typically counts from the next view
(~1 s). `flt.set(id, False)` / `flt.invalidate(id)` push a revocation
immediately (otherwise it lands after `ttl`); `await flt.prefetch(ids)`
warms the cache; a failed batch caches the fallback for `failure_retry` and
never overwrites a real answer.

## 8. Coordination client

`aster.coordination` wraps the Rust coordination client (it is not
reimplemented): durable fenced registers, exact ACLs and linearizable reads
against a coordination group. Journaling, leader redirects, retries and reply
validation all run in Rust. The voters themselves stay Rust-only; see
[the coordination walkthrough](aster-coordination-getstarted.md) for how to
host a group.

```python
from aster.coordination import (CoordinationClient, CoordinationRights,
                                ResourceKey, fence_of)

client = await CoordinationClient.connect(
    node, group_id,
    seed_tickets=member_tickets,             # Open tickets, routing hints only
    journal_dir="/var/lib/app/coordination", # persist with the node identity
)
async with client:                           # close() releases the journal lock
    # On startup, before any mutation: redeliver what a previous run resolved.
    pending = await client.unacknowledged() or await client.resume()
    if pending:
        await handle(pending.reply)
        await pending.acknowledge()

    head = ResourceKey("app.head", b"w-1")   # or ("app.head", "w-1")
    resolved = await client.acquire(head, ttl=30)
    await resolved.acknowledge()             # it was fsynced before it returned
    fence = fence_of(resolved.snapshot)
    cas = await client.compare_and_set(fence, resolved.snapshot.value_revision, b"v1")
    if cas.rejected:                         # a typed rejection is still a result
        log(cas.reason)                      # e.g. "value_revision_mismatch"
    await cas.acknowledge()
```

**The delivery contract.** Every mutation (`acquire`, `renew`,
`compare_and_set`, `release`, `release_with_value`, `revoke`, `set_grant`,
`set_admin`, `change_membership`, `cancel_membership`) returns a
`ResolvedMutation` that was fsynced before the awaitable completed. Until you
`acknowledge()` it, no new mutation is allowed -- across calls and across
restarts (`CoordinationAwaitingAckError`) -- and `unacknowledged()` /
`resume()` hand it back. `resolved.reply` is the exact `MutationReply`;
`outcome`, `reason`, `rejected`, `sequence` and `snapshot` are shortcuts, and
`rejection()` returns a `CoordinationRejectedError`. One process owns a
journal at a time (`CoordinationJournalLockedError`).

**Leases.** `acquire_lease(resource, LeaseOptions(...))` does one committed
acquire; on a grant Rust renews in the background and `lease.fence()` returns
`None` once the holder must stop. Lease mutations are journaled and
acknowledged for you:

```python
outcome = await client.acquire_lease(head, LeaseOptions(ttl=30))
if outcome.granted:
    async with outcome.lease as lease:       # released on exit
        reply = await lease.compare_and_set(expected_revision, b"next")
        await lease.release_with_value(reply.snapshot.value_revision, b"final")
else:
    backoff(outcome.reason, outcome.snapshot)
```

**Reads and administration.** `client.reads()` (or
`await CoordinationReadClient.connect(node, group_id, seed_tickets=...)`, which
has no journal) gives `read_linearizable`, `inspect_acl`, `inspect_admins`,
`membership_status`, `group_status`, `probe_client`, `recovery_export()` /
`recovery_export_to_file(path)`, and the audit log as an async iterator:

```python
async for row in client.reads().audit_stream(after=cursor, page_size=500):
    ...                                      # ascending AuditRecord rows
```

Rights are exact bits (`CoordinationRights.READ | ...`, `HOLDER` for a
register holder) granted per node with `set_grant(resource, node_id, rights,
expected_acl_revision)`.

**Errors** all derive from `CoordinationError` (`.kind`):
`CoordinationAwaitingAckError`, `CoordinationUnresolvedError` (call
`resume()`), `CoordinationJournalFatalError`, `CoordinationJournalLockedError`,
`CoordinationInvalidArgumentError` (also a `ValueError`), `CoordinationRpcError`
(`.code`, e.g. 7 for a permission denial), `CoordinationUnavailableError` (safe
to retry), `CoordinationClosedError`, and `CoordinationRejectedError` (raised by
the lease helpers on a typed rejection; `.reason`, `.reply`).

**Placement.** `placement_plan(PlacementInput(...), await
node.topology().snapshot())` is the pure, deterministic voter planner; submit a
`kind == "plan"` result with
`await client.change_membership(new_operation_id(), rec.plan)`.

**Placement manager.** `PlacementManager(client, topology=..., policy=...,
provisioner=...)` runs the section 7.7.3 reconcile loop in Rust beside an
administrator client; each source may be Python (section 9):

```python
from aster.coordination import (
    PlacementManager, PlacementPolicySource, PlacementProvisioner, TopologySource,
)

class Seeds(PlacementProvisioner):
    async def provision(self, request):          # ProvisionRequest -> MemberSeed
        return await start_candidate(request.node_id)

manager = PlacementManager(
    client,
    topology=node.topology(),        # or a TopologySource: async def snapshot() -> TopologyView
    policy=my_policy,                # a PlacementPolicy, or a PlacementPolicySource: def policy()
    provisioner=Seeds(),
)
report = await manager.reconcile()   # PlacementReport
```

A provider failure stops that pass with `UpcallError` (nothing is submitted).

## 9. Python extension points (upcalls)

Rust calls into Python for the pieces above (lease serializer, authorizer,
fenced resource, topology filter, placement providers, Gate 0 handler) and
for expose (below). It works the same way for all of them: Rust emits a typed
event, an asyncio pump task on your event loop decodes it in Rust into the
usual `aster` dataclasses, runs your method (`async def` or plain) as its own
task and answers by id. You never handle bytes. Bind these objects from async
code, and keep that loop running while Rust may ask.

```python
from aster import UpcallPolicy, UpcallRuntime
from aster.upcalls import set_runtime

rt = UpcallRuntime(default_policy=UpcallPolicy(timeout=2.0))
rt.set_policy("lease_authorize", UpcallPolicy(timeout=0.5, failure_mode="fail_open"))
set_runtime(rt)          # this loop's default; or set `upcall_runtime = rt` on one object
...
await rt.aclose()        # pending upcalls fail; decisions fall back
```

Each kind has a timeout (default 5 s). The yes/no kinds (`lease_authorize`,
`topology_admitted`, `expose_authorize`, `expose_route_policy`,
`admission_connect`, `admission_handshake`) also have a failure mode applied
when there is no usable answer (exception, timeout, full queue, wrong type):
`"fail_closed"` (default) or `"fail_open"`. Other kinds surface the failure
to the Rust caller as `UpcallError` (`.kind`: `timeout`, `overloaded`,
`closed`, `foreign`, `unimplemented`, `decode`). When Rust stops waiting, your
task is cancelled (`asyncio.CancelledError`) and a late answer is dropped.

**Expose.** `node.expose()` publishes local services to peers:

```python
from aster import EdgeRouter, ExposeAuthorizer, HttpHandler, HttpResponse, RoutePolicy

class Team(ExposeAuthorizer):
    async def authorize(self, service_id, peer, metadata):   # once per (connection, service)
        return peer in team

class Hello(HttpHandler):
    async def handle(self, request):                       # HttpRequest
        name = await request.read_body()                     # bytes, inline or streamed
        return HttpResponse(200, [("content-type", "text/plain")], b"hi " + name)

expose = node.expose()
expose.expose_socket("db", "127.0.0.1:5432", authorize=Team())   # raw L4 splice
expose.expose_http("app", Hello(), authorize=Team())             # Python handler
await expose.serve([b"aster-expose/app"])                       # or expose.handle(conn)

# consumer side, on a connection to that ALPN
send, recv = await open_relay(conn, "db", b"token")
resp = await relay_http(conn, "app", "POST", "/", body=b"you")   # HttpResponse
```

Each HTTP request is one `expose_http` upcall. `request.body` is `bytes` when
the request declared a body of at most `inline_request_max` (64 KiB), else an
`HttpBodyStream` (`await read()`, `async for`), pulled from the wire only as
you read. Return an `HttpResponse` (or `bytes` / `str` for a 200) whose body
is `bytes` / `str` (sent with the head) or an (async) iterable of chunks
(streamed after the head with backpressure, then finished with `trailers`).
The kind's timeout bounds the time to the response head only -- raise it for
long-polling handlers. A handler that raises answers `500` (the message is
logged, never sent), a timeout `504`, no handler `501`; `max_request_body` /
`max_response_body` cap bodies (`413` / `502`), and `max_chunk` (default
1 MiB, must be > 0) sets the largest body chunk: reads return at most that
many bytes and writes of bigger chunks are split. If the consumer leaves
before the head, the handler task is cancelled. Closing the `UpcallRuntime`
cancels in-flight handlers and streamed responses too (your iterator sees
`CancelledError` or `aclose()`); `aclose()` waits at most `close_grace`
seconds (default 2), then abandons an iterator that ignores cancellation.

An edge accepts route registrations with `expose.serve_routes(EdgeRouter(),
policy)`, where a `RoutePolicy`'s `async def grant(peer, metadata, specs)`
returns the granted `RouteSpec`s (or indexes; `True` = all). With no usable
answer, fail-closed grants nothing. Origins ask with `reg = await
request_route(conn, [RouteSpec("a.example", 443, service_id="app")], b"token")`
and hold `reg` while the routes should stay live.

## 10. Observability: logs, metrics, traces, health

The same pipeline serves all six languages; see the
[observability guide](aster-observability-getstarted.md) for the shared
configuration, metric names and the other bindings.

The whole pipeline lives in Rust (`aster-otel`); Python starts it and reads
from it. Configuration is parsed once, in Rust: `observability.init(...)`
arguments, then `OTEL_*` / `ASTER_*` environment variables, then the
`[observability]` TOML table, then defaults.

```python
from aster import metrics, observability, tracing, HealthServer

observability.init(service_name="orders")        # optional; before the first server
orders = metrics.counter("orders.placed", "Orders placed")
orders.add(1, {"region": "eu"})
with tracing.span("db.query", {"db.system": "postgresql"}):
    ...
```

- **Rust logs in `logging`.** The first `AsterServer` / `AsterClient` start opens the
  log bridge (exporters start only when `OTEL_*` asks). Rust events arrive on
  loggers named after their target (`aster::rpc::dispatch` -> `aster.rpc.dispatch`),
  with `trace_id`, `span_id`, `rpc_service`, `rpc_method` attributes. Rust filters
  by the `aster` logger's level: call `aster.logging.sync_level()` after
  `dictConfig` / `basicConfig`. `ASTER_LOG=warn,aster=debug` pins the Rust filter.
  `aster.logging.LoggingFilter`, `structlog_processor` and `loguru_patcher` add the
  current trace ids to your own records.
- **Metrics.** The built-in RPC, admission, transport and health metrics are
  recorded in Rust; `aster.metrics` adds counters, up-down counters, histograms
  and gauges on the same pipeline. There is no `MetricsInterceptor`.
- **Traces.** `aster.tracing.span` / `traced`; inside a handler the current trace
  is the call's server span, and outbound calls continue it (`traceparent` in the
  call metadata). An inbound trace continues even with no exporter configured
  (or traces off): `tracing.current_trace_id()` and outbound calls still see it.
- **Health.** `AsterConfig(health_port=8080)` (or `ASTER_HEALTH_PORT`, `[health]`)
  makes `AsterServer` serve `/healthz`, `/readyz` and Prometheus `/metrics`
  (`server.health`). Standalone: `await HealthServer.start(node, host, port)`;
  `health.check("db").set(True)` adds a readiness check (created failing).
  Binds `127.0.0.1` by default; pod probes need `health_host="0.0.0.0"`.
- **Fork.** Initialise in each worker, not in a pre-fork master: after `fork()`
  the child's telemetry is disabled with one warning.

## Request-stream retry policy

`RetryInterceptor` can retry idempotent unary calls. Python client-stream calls
accept caller-owned `AsyncIterator` request sources and make one attempt, even
when their RPC is declared idempotent. A source may already be partly consumed
when a call fails; reusing it would send a different body. Server-stream and bidi
calls also do not automatically retry. Interceptor request/response/error hooks,
deadlines and circuit-breaker policies still apply to these calls. To retry a
stream at the application level, create a fresh source and account for the
service's idempotence and partial effects explicitly.
