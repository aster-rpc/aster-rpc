# C ABI rename map (ABI 2.x -> 3.0) and 3.x / 4.x / 5.x / 6.x / 7.x changelog (current: 7.8)

## New in 7.8 (peer events; additive)

Rust `AsterServer::peer_events` / `ServerHandle::peer_events` (semantics in
`aster::rpc::peers`): which peers are connected to a dispatch server, and when
that changes. A peer is connected from its first admitted `aster/1`
connection (past Gate 0 / enrollment, not refused by a drain) until its last
one ends, so several connections from one peer are one connect and one
disconnect, and a refused peer never appears. No existing layout changes.

| Item | Role |
|---|---|
| `aster_server_peer_events(server, *out_receiver, *out_snapshot_json)` | The peers connected now (JSON array of `{peer, alpn, connections, identity: {endpoint_id, principal, roles}}`; `principal` is the endpoint id of a peer with a live enrollment session, else `null`) and a receiver handle, taken under one lock so no peer falls between them. |
| `aster_peer_events_poll(rt, receiver, timeout_ms, *out_json)` | Blocks up to `timeout_ms` for one event as JSON: `connected` (`peer`, `alpn`, `identity`), `disconnected` (`peer`, `reason`: `closed`, `timed_out`, `revoked`, `drained`, `error`) or `lagged` (`missed`). `TIMEOUT` when none; `NOT_FOUND` once released; `ALREADY_CLOSED` when the server ended. `UNSUPPORTED` on a tokio runtime thread. |
| `aster_peer_events_release(receiver)` | Releases the receiver and wakes a pending poll. |

The queue holds 1024 events. A receiver that falls behind gets one `lagged`
event with the number missed; its state is stale, so the consumer calls
`aster_server_peer_events` again and rebuilds from the new snapshot. Go
requires 7.8 (`ABIMinorRequired = 8`).

## New in 7.7 (gateway identity on the config-driven path; additive)

Slices 1-4 and 6 of `docs/_internal/web_gateway/gateway-identity.md` (edge
tokens, provider tokens with replicated issuer keys, the issuer-key
publisher, the edge helpers, `aster.ops.NodeConfig`) reach the C ABI the way
they reach `AsterServer` / `AsterClient`: from the node config. The facade's
`binding_support::identity` runs the same validation and wiring; bindings
re-implement none of it. No existing layout changes.

Configuration:

| Item | Role |
|---|---|
| `aster_identity_config_t` (96 bytes), `aster_config_set_identity`, `aster_config_identity` | `[identity]`, `[identity.publisher]` and `[remote_config]` beside `aster_config_t`, in its `storage`, as 7.3 / 7.4 do. Scalars: `accept_on_behalf_of` (0/1), `remote_config_disabled` (0/1; 0 = NodeConfig served), and `edge_token_max_lifetime_secs`, `clock_leeway_secs`, `max_key_set_age_secs`, each applied only with its `ASTER_IDENTITY_SET_*` bit (clear = the default; leeway 0 is a valid value). Lists as JSON, the shape their environment variables take: `trusted_issuers_json` = `ASTER_IDENTITY_TRUSTED_ISSUERS` (array of `{iss, audiences, roles_claim, role_map, max_token_lifetime_secs}`), `publisher_json` = `ASTER_IDENTITY_PUBLISHER` (`{issuers, jwks_url, refresh_secs, min_refresh_secs, ca_certs, ca_only}`); `remote_config_lock` is an `aster_bytes_list_t`. `sources[ASTER_IDENTITY_SETTING_*]` carries `aster_config_source_t` (`UNSET` 0, `DEFAULT`, `FILE`, `ENV`, `CODE`) so NodeConfig `get` reports where each setting came from; filled configs set it, callers may leave 0. |
| `aster_config_from_env` / `_from_toml` | Also read `ASTER_IDENTITY_*`, `ASTER_REMOTE_CONFIG_*`, `[identity]`, `[[identity.trusted_issuers]]`, `[identity.publisher]`, `[remote_config]`. |
| `aster_node_start` / `aster_node_start_enrolled` | Keep the settings with the node. `[identity.publisher]` without Enrollment is `INVALID_ARGUMENT` (as `AsterClient` / `AsterServer`); a client-side enrollment validates it at start. |
| `aster_server_builder_new` (behaviour change) | Validates `[identity]` as a whole, the `[remote_config]` locks, trusted issuers needing Enrollment and the publisher (`INVALID_ARGUMENT` naming the setting). `ASTER_SERVER_OPTION_ACCEPT_ON_BEHALF_OF`, when set, overrides the config. |
| `aster_server_start` (behaviour change) | As `AsterServer`: applies the settings; with Enrollment installs the issuer-key cache and, after attaching policy, the fleet-namespace watcher and the publisher (delivering to `[enrollment] replicas`); serves `aster.ops.NodeConfig` behind `aster.config-admin` unless `[remote_config] enabled = false`, applying a stored record before serving. Opening NodeConfig blocks briefly (on a tokio thread it waits on a helper thread). |
| `aster_server_set_audit_outbox(rt, builder, outbox)` | Durable audit of every NodeConfig `set` / `clear` (`AsterServerBuilder::audit_outbox`). `UNSUPPORTED` with NodeConfig off. |

Handlers (dispatch):

| Item | Role |
|---|---|
| `ASTER_DISPATCH_FLAG_ON_BEHALF_OF` (`0x2000`) | A `CALL` a gateway forwarded on behalf of a user (`HAS_PRINCIPAL` is set too; `peer_ptr` is `iss|sub`, `aster_dispatch_identity`'s `actor` / `peer` the gateway). |
| `aster_dispatch_on_behalf_of(rt, server, call, *ptr, *len, *buffer)` | The forwarded identity as a k/v list: `iss`, `sub`, `jti`, `exp` (Unix s, decimal), then one `amr` per sign-in method (`CallContext::auth_methods`). Empty for other calls; `NOT_FOUND` once the call is over. This is the 7.6 "`amr` has no slot yet": a new accessor rather than a slot, since `aster_dispatch_identity_t` has no room for a list. Roles are the call's attributes. |
| `aster_server_node_properties(server, *out_json)`, `aster_server_node_config_generation(server)` | NodeConfig properties as handlers see them (`CallContext::node_properties`): `{generation, name, tags, attributes: {k: {type, value}}}`; the generation is cheap, so a binding caches the JSON per generation. Attributes never authorise. |

Edges (clients):

| Item | Role |
|---|---|
| `aster_call_options_t.metadata` entry `aster-on-behalf-of` | The per-call token (`CallOptions::on_behalf_of`): Rust's option is exactly this metadata entry, so the existing k/v list carries it and `aster_call_options_t` does not grow. Bindings expose a call option that replaces any earlier entry. |
| `aster_enrollment_mint_user_token(rt, e, json, *out)` | `AsterClient::user_token_issuer` + `Issuer::mint_user_token`: `{"sub", "roles", "amr", "ttl_secs"}` → the compact JWT (owned). Bounded by the delegation in the credential the enrollment holds now; refused with `INVALID_ARGUMENT`. |
| `aster_enrollment_publisher_notice(rt, e, iss, kid)`, `_notice_token(rt, e, jwt)`, `_publisher_status(rt, e, *out_json)` | `PublisherHandle::notice_kid` / `notice_token` / `status` for the node's `[identity.publisher]`: a client's starts on its first `aster_enrollment_enroll` and delivers to every server it enrolled with (each later server joins the running publisher as a delivery target; a publisher that will not start is the enroll result's `publisher_error`, not a failed enrollment); a server's from `aster_server_start`. `NOT_FOUND` without a publisher. Rust's client notices forwarded tokens itself (unary interceptor); bindings call `_notice_token` for each forwarded provider token. |
| `aster_node_config_call(rt, connection, method, json, ud, *op)`, `aster_node_config_method_t` (`GET` 0, `SET` 1, `CLEAR` 2, `ACK` 3), `ASTER_EVENT_NODE_CONFIG_RESULT` = 129 | The `aster.ops.NodeConfig` admin client (`NodeConfigClient`) over an `aster/1` connection, with JSON requests (`SET`: `{expected_generation, name, tags, attributes: {k: {type, value}}, sections: {"identity": {...}}}`; `CLEAR`: `{expected_generation}`; `ACK`: `{generation, timeout_ms}`; each takes `deadline_ms`). The event's payload is `{"code": 0, "result": view-or-ack}` or `{"code": <RPC status>, "message"}`. Functions rather than the generic RPC path: the wire types are Fory structs with a union (`aster/NodeConfigView`, `Value`), which bindings would otherwise have to mirror type by type. |

The facade also gains `aster::binding_support::identity` (`plan`,
`IdentityPlan::apply`, `PendingIdentity::attach`, `ServerIdentity`,
`client_publisher_plan`). The C ABI builds `aster` with
`identity-publisher`. Go requires 7.7 (`ABIMinorRequired = 7`).

## New in 7.6 (gateway identity; additive)

The C ABI side of gateway identity, slice 1
(`docs/_internal/web_gateway/gateway-identity.md`): an edge holding
`aster.gateway` calls a service on behalf of a user with an edge-minted JWT in
the `aster-on-behalf-of` metadata entry. Layouts do not change.

| Item | Role |
|---|---|
| `ASTER_SERVER_OPTION_ACCEPT_ON_BEHALF_OF` (= 6) | `aster_server_set_option` 0/1, default 0. Off, a call carrying `aster-on-behalf-of` is refused with `PERMISSION_DENIED`; on, the entry is verified before Gate 3 and the call runs as the user. The Rust `[identity] accept_on_behalf_of` key; mirroring it through `aster_config_t` is a later slice. |
| `ASTER_DISPATCH_IDENTITY_ACTOR` (= 2), `actor_ptr` / `actor_len` | Filled for a forwarded call: `actor` is the gateway's node id (the same bytes as `peer`), `principal` is the user (`iss|sub`), and the `CALL` event has `ASTER_DISPATCH_FLAG_HAS_PRINCIPAL`. Clear: `actor_ptr` is NULL / 0, as in 7.5. `aster_dispatch_attributes` carries the token's roles only. |

A 7.5 reader that ignores `actor` still sees the user as `principal` and the
gateway as `peer`. The token's `amr` (`auth_methods()` in Rust) has no C ABI
slot yet.

## New in 7.5 (handler call identity; additive)

The C ABI side of `aster::rpc::CallContext::peer()` / `principal()`, so a
binding's handler context can report the transport peer and the
`Authenticator`'s principal apart. `aster_dispatch_event_t` does not change:
`peer_ptr` is still the principal-or-peer, so 7.4 readers keep working.

| Item | Role |
|---|---|
| `ASTER_DISPATCH_FLAG_HAS_PRINCIPAL` (`0x1000`) | On a `CALL` event: an `Authenticator` resolved a principal other than the transport peer id. `peer_ptr` is that principal. Clear: `peer_ptr` is the transport peer and the principal, and the binding need not call `aster_dispatch_identity`. |
| `aster_dispatch_identity(rt, server, call_id, out)` / `aster_dispatch_identity_t` (64 bytes) | A live call's `peer` (transport id: 64-hex node id over iroh, the remote socket address over HTTP / WebTransport), `principal` (`AuthOutcome.principal`, else `peer`) and `flags` (`ASTER_DISPATCH_IDENTITY_PRINCIPAL` = 1). The caller sets `out->struct_size`; the strings live in one lease, `out->buffer` (`aster_dispatch_buffer_release`). `NOT_FOUND` once the call is over. |
| `actor_ptr` / `actor_len`, `ASTER_DISPATCH_IDENTITY_ACTOR` (= 2) | Reserved for a gateway's on-behalf-of `actor()` (`docs/_internal/web_gateway/gateway-identity.md`): NULL / 0 and never set in 7.5. A later minor fills them without changing the struct. |

The rest of the handler context was already in the ABI: `call_id` /
`wire_call_id`, `session_id` / `session_key`, `deadline_unix_ms`,
`service_idx` / `method_idx`, `ASTER_DISPATCH_FLAG_IROH_AUTHENTICATED`
(provenance), `aster_dispatch_metadata`, `aster_dispatch_attributes` (the
merged set Gate 3 checked, authenticator attributes included), and
cancellation through the `CANCELLED` event (reason `PEER` / `DEADLINE` /
`INVALID_REQUEST`) and `aster_dispatch_is_cancelled`.

Go requires 7.5 (`ABIMinorRequired = 5`): `CallInfo.PeerID` is now the
transport peer and `CallInfo.Principal` (also on `SessionInfo`) the
principal. Rust: `aster::binding_support::dispatch::ForeignCall::transport_peer_id`
and `CallRef::transport_peer`.

## New in 7.4 (transfer metrics; additive)

The transfer metrics Rust gained in 0.3 (Portal M1-M4), for the bindings
that reach Rust through this ABI. Nothing existing changes shape: fetch
report v1 stays the default and the topology JSON only gains keys.

| Item | Role |
|---|---|
| `aster_blobs_transfer_stats(rt, node, out)` / `aster_blob_transfer_stats_t` (168 bytes) | Node-wide blob counters (`aster::Blobs::transfer_stats`): fetched / served payload, zstd payload, zstd wire and plain bytes, plain fallbacks and serves, transfers by encoding and result, zstd decode errors. Synchronous; the caller sets `out->struct_size`. |
| `ASTER_FETCH_REPORT_V2` (fetch `flags` bit 2) | `aster_blobs_download_hash_from` and the custody download write fetch report v2 (`ASTER_FETCH_REPORT_VERSION_2` = 2): the v1 header, then 72-byte records (`ASTER_FETCH_PROVIDER_RECORD_V2_SIZE`) adding `in_flight`, `plain_fallbacks`, record flags (`ASTER_FETCH_PROVIDER_HAS_WIRE_BYTES`), `bytes_transferred` and `wire_bytes` per provider (`aster::ProviderOutcome`). Layout in `ffi/src/data.rs`. |
| `aster_topology_peers` / `aster_topology_peer` JSON | Each peer gains `quic_traffic` (object keyed `blobs`, `blobs-zstd`, `docs`, `gossip`, `rpc`, `other`, each `{quic_bytes_sent, quic_bytes_received}`) and `blob_transfer` (`fetched_payload_bytes`, `fetched_zstd_payload_bytes`, `fetched_wire_bytes`, `fetched_plain_bytes`, `served_payload_bytes`, `served_zstd_payload_bytes`, `served_wire_bytes`, `served_plain_bytes`, `plain_fallbacks`) (`aster::topology::PeerView`). |
| `aster_metrics_config_t` (16 bytes), `aster_config_set_metrics`, `aster_config_metrics` | `[metrics] peer_labels` / `peer_label_cap` beside `aster_config_t`, in its `storage`, as 7.3 does for the network settings. `peer_label_cap` 0 = `ASTER_METRICS_PEER_LABEL_CAP_DEFAULT` (64). `aster_config_from_env` / `_from_toml` read `ASTER_METRICS_PEER_LABELS` / `ASTER_METRICS_PEER_LABEL_CAP` and `[metrics]`; the node's ops listener applies them, with or without an explicit `aster_health_config_t`. |

Go requires 7.4 (`ABIMinorRequired = 4`; `Blobs.TransferStats`,
`FetchReport` v2 fields, `TopologyPeerView.QuicTraffic` / `BlobTransfer`,
`AsterConfig.MetricsPeerLabels` / `MetricsPeerLabelCap`).

## New in 7.3 (cluster network settings; additive)

Node settings for running inside a cluster or an air-gapped network (Kasm
A6). `aster_config_t` has no spare room and checks `struct_size` exactly, so
the settings live beside it, in the config's library-owned `storage`;
`aster_config_t`'s layout does not change.

| Item | Role |
|---|---|
| `aster_network_config_t` (96 bytes) | `struct_size`, `discovery` (`aster_discovery_t`), `bind_addr_v6`, `advertise_addrs` (list), `relay_ca_certs` (list of PEM paths), `relay_ca_only` (0/1), `reserved` (0), `discovery_dns_origin`, `discovery_pkarr_relays` (list). |
| `aster_discovery_t` | `DEFAULT` = 0 (n0 pkarr/DNS at `iroh.link`; with servers set, selects `CUSTOM`), `NONE` = 1, `LOCAL` = 2 (mDNS only), `CUSTOM` = 3. |
| `aster_config_set_network(config, network)` | Validates and copies the settings into `config->storage` (allocating it for a caller-built config); NULL clears. A config given settings must be released with `aster_config_release`. |
| `aster_config_network(config, out)` | Reads them back as views valid until release or the next set; the caller sets `out->struct_size`. A config without settings reads as zeroed (`DEFAULT`). |
| `aster_config_from_env` / `_from_toml` | Also read `ASTER_BIND_ADDR_V6`, `ASTER_ADVERTISE_ADDRS`, `ASTER_RELAY_CA_CERTS`, `ASTER_RELAY_CA_ONLY`, `ASTER_DISCOVERY`, `ASTER_DISCOVERY_DNS_ORIGIN`, `ASTER_DISCOVERY_PKARR_RELAYS` and the matching `[network]` keys into `storage`. |

`aster_node_start` and `aster_node_start_enrolled` apply the settings. Go
requires 7.3 (`ABIMinorRequired = 3`; `AsterConfig.BindAddrV6`,
`AdvertiseAddrs`, `RelayCACerts`, `RelayCAOnly`, `Discovery`,
`DiscoveryDNSOrigin`, `DiscoveryPkarrRelays`). Rust: the same fields on
`aster::AsterConfig`, `aster::DiscoveryMode`.

## New in 7.2 (server drain; additive)

A dispatch server can be taken out of rotation gracefully instead of closed.

| Item | Role |
|---|---|
| `aster_server_drain(rt, server, grace_ms, *report)` | Blocks: sets an attached ops listener not ready; refuses new connections (closed after the handshake with `ASTER_DRAIN_CLOSE_CODE`) and new calls (`UNAVAILABLE`, detail `aster.draining` = `true`; calls on existing sessions still run); closes each connection once idle; at `grace_ms` closes the rest (bindings see `CANCELLED{PEER}`) and waits up to 2 s. A concurrent call with an earlier deadline cuts it short; later calls return the same report. `UNSUPPORTED` on a tokio thread. |
| `aster_drain_report_t` (64 bytes) | `calls_in_flight`, `calls_completed`, `calls_cancelled`, `calls_refused`, `connections_closed`, `connections_refused`, `elapsed_ms` (u64 each), `deadline_reached`, `reserved` (u32). |
| `ASTER_DRAIN_CLOSE_CODE` = 503 | QUIC application close code meaning "draining, reconnect elsewhere". |

No existing layout changes and no new event. Go requires 7.2
(`ABIMinorRequired = 2`, `AsterServer.Drain`). Rust: `AsterServer::drain`,
`ServerHandle::drain`, `aster::rpc::drain`.

## New in 7.1 (audit outbox acknowledgement log; additive)

The durable audit outbox now records each event the receiver acknowledged, in
the same storage transaction that deletes it, and the C ABI reads that log.

| Item | Role |
|---|---|
| `aster_audit_outbox_acknowledgements(rt, outbox, json, ud, *op)` | JSON v1 `{version, after, limit, wait_ms}`; completes with the page below. `after` is an exclusive cursor, `limit` 1–1000, `wait_ms` waits (at most `ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS`) for the next acknowledgement when none follows `after`. |
| `ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS` = 128 | `handle` = the outbox; payload `{"version":1,"acknowledgements":[{cursor, event_id, sequence, batch_id, acknowledged_at_ms}]}`. |
| Open config `acknowledgement_log_max_entries`, `acknowledgement_log_max_age_ms` | Optional JSON fields; defaults 100,000 entries and 7 days. |

No struct layout changes. Go requires 7.1 (`ABIMinorRequired = 1`).
See `ffi_spec/Aster-audit-outbox-spec.md`.

## 7.0 (breaking: `aster_hook_failure_mode_t` values swap so zero fails closed)

| Constant | 6.x | 7.0 |
|---|---|---|
| `ASTER_HOOK_FAILURE_FAIL_CLOSED` | 1 | **0** |
| `ASTER_HOOK_FAILURE_FAIL_OPEN` | 0 | **1** |

**Why.** Admission fails closed by default, but in 6.x the value 0 meant
fail-open, so a caller that zero-filled `aster_config_t` instead of calling
`aster_config_init` (and Go's zero-value `AsterConfig{}` / `EndpointConfig{}`)
let a stalled or missing admission loop admit everyone. In 7.0 the zero value
fails closed, for `aster_config_t.hook_failure_mode` and for the `failure_mode`
argument of `aster_upcall_set_policy` / `aster_upcall_set_default_policy`.

**Why a major release.** No layout changes, but the same number now means the
opposite mode: a 6.x caller passing its `FAIL_CLOSED` (1) would get fail-open.
A minor bump would let that caller through a "same major, minor >= required"
check; 7.0 refuses it at the handshake instead (`aster_runtime_new(6, ...)` and
`aster_abi_accept(6)` return `ASTER_STATUS_ABI_MISMATCH`, so the config fill
functions stay locked). Covered by `ffi/tests/abi_handshake.rs` and
`test_abi_version` / `test_enum_values` / `test_config` in
`ffi/tests/abi_contract_test.c` (a zero-filled struct is fail-closed).

Bindings move by updating their expected major: Go (`ABIVersionMajor` 7 from
the header, `#error` for another major, `HookFailClosed` / `FailureModeClosed`
are now 0 so zero values fail closed), Java (`IrohLibrary.EXPECTED_ABI_MAJOR`
7, `FailureMode.FAIL_CLOSED.code` 0) and .NET (`Native.AbiVersionMajor` 7,
`HookFailureMode.FailClosed` = 0) require 7.0. Python and TypeScript do not use
the C ABI.

## 6.0 (breaking: `aster_config_t` grew; blob compression level and port reuse)

**Why a major release.** `aster_config_t` grew from 280 to 288 bytes, and the
library writes the whole struct in `aster_config_init`, `aster_config_from_env`
and `aster_config_from_toml`. A caller built against a 5.x header allocates 280
bytes, so a 6.0 library filling it would write 8 bytes past the caller's struct.
A minor bump cannot prevent that: a 5.x caller would pass a "same major, minor
>= required" check. 6.0 makes the refusal clean instead:

- `aster_runtime_new(5, ...)` returns `ASTER_STATUS_ABI_MISMATCH` (as for any
  other major).
- New `aster_abi_accept(uint32_t expected_abi_major)`: the ABI handshake without
  a runtime. `OK` when `expected_abi_major` is 6, else `ABI_MISMATCH` with the
  last error set. A successful `aster_runtime_new(6, ...)` counts as well.
- `aster_config_init` / `_from_env` / `_from_toml` return `ASTER_STATUS_ABI_MISMATCH`
  **without writing `*out`** until the process has passed the handshake, so a
  5.x caller (which never calls `aster_abi_accept(6)` and whose
  `aster_runtime_new(5)` is refused) never has its struct written.
- Functions that read an `aster_config_t` (`aster_node_start`,
  `aster_node_start_enrolled`, `aster_config_release`) read `struct_size` on its
  own first and refuse a 280-byte struct (`INVALID_ARGUMENT`) before reading
  past it.

Covered by `ffi/tests/abi_handshake.rs` (its own process, simulating a 5.x
caller with a guarded 280-byte buffer) and the first block of
`test_abi_version` in `ffi/tests/abi_contract_test.c`.

**What changed in the struct.** Two `AsterConfig` settings that had no public
slot now have one, so a binding that builds or copies the struct can set them,
and in particular turn them off. Before, `aster_config_from_env` /
`aster_config_from_toml` carried `reuse_last_port` in the opaque `storage`
(and `blob_zstd_level` not at all), so a binding that copied the fields into its
own struct and rebuilt it lost both, leaving them at their defaults (on). The
new fields are appended after `storage`, so every existing offset is unchanged:

| Offset | Field | Encoding |
|---|---|---|
| 280 | `int32_t blob_zstd_level` | `ASTER_BLOB_ZSTD_LEVEL_DEFAULT` (0) = the default level (3); `ASTER_BLOB_ZSTD_LEVEL_OFF` (-1) = off: the node serves only the plain blobs ALPN and fetches default to plain; 1..=19 = that level, larger values clamped to `ASTER_BLOB_ZSTD_LEVEL_MAX` (19); other negative values are `INVALID_ARGUMENT`. Rust `AsterConfig::blob_zstd_level`, env `ASTER_BLOB_ZSTD_LEVEL`, TOML `[storage] blob_zstd_level`. |
| 284 | `uint32_t reuse_last_port` | `aster_reuse_last_port_t`: `ASTER_REUSE_LAST_PORT_DEFAULT` (0) = on, `_ENABLED` (1), `_DISABLED` (2); other values are `INVALID_ARGUMENT`. Rust `AsterConfig::reuse_last_port`, env `ASTER_REUSE_LAST_PORT`, TOML `[network] reuse_last_port`. |

Zero means "not set" for both, like `portmapper` and `relay_mode`, so a zeroed
tail keeps the defaults. The fill functions write explicit values
(`blob_zstd_level` = 3 or -1, `reuse_last_port` = ENABLED or DISABLED);
`storage` no longer carries `reuse_last_port`. `struct_size` remains an exact
match.

Nothing else changed: a binding moves from 5.1 to 6.0 by updating its expected
major, mirroring the 288-byte layout and calling `aster_abi_accept(6)` (or
creating a runtime) before filling a config. Go (`ABIVersionMajor` 6 from the
header, `ABIMinorRequired` 0, plus a compile-time `#error` for another
major's header), Java (`IrohLibrary.EXPECTED_ABI_MAJOR` 6,
`REQUIRED_ABI_MINOR` 0) and .NET (`Native.AbiVersionMajor` 6,
`RequiredAbiMinor` 0, `Native.EnsureAbi`) require 6.0. Python and TypeScript
do not use the C ABI.

## New in 5.1 (Enrollment from configuration, lease grantor, role restriction; additive)

Enrollment reaches the C ABI the way it reaches the Rust `AsterServer` /
`AsterClient`: from the resolved configuration (`[trust]` + `[enrollment]`,
design `docs/_internal/working_ideas/aster-enrollment.md` §15, C ABI mirror
§15.10). The C ABI calls the facade's `binding_support::enrollment::plan` and
`Plan::install` and its server and client sequences; bindings re-implement
none of it. Root operations (namespace create/bind, promote, revoke,
reauthorize, trust rotate, delegate) stay in the Rust CLI. Configuration and
results are UTF-8 JSON; synchronous outputs are owned spans freed with
`aster_string_release`; async operations complete with the new
`ASTER_EVENT_ENROLLMENT_RESULT` = 127 (JSON payload) or `ASTER_EVENT_ERROR`,
whose `flags` carry an `aster_enrollment_error_t` (`NOT_ADMITTED` 1, `DENIED`
2, `CLOCK_SKEW` 3, `UNAVAILABLE` 4) for admission errors. The event status
follows the kind: `DENIED` is `PERMISSION_DENIED`, `NOT_ADMITTED` is
`UNAUTHENTICATED`, and `CLOCK_SKEW` / `UNAVAILABLE` are the new
`ASTER_STATUS_UNAVAILABLE` = 19 (retry later).

| Function | Role |
|---|---|
| `aster_config_enrollment_from_env(*out_json)`, `aster_config_enrollment_from_toml(path, *out_json)` | Resolve `[trust]` / `[enrollment]` (and `ASTER_ROOT_*`, `ASTER_ENROLLMENT_*`) through `AsterConfig::from_env` / `from_file` into the trust JSON: `root_pubkey`, `root_pubkey_file`, `next_root_commitment`, `root_seq`, `root_rotations`, `enrollment_credential_file`, `enrollment_credential_chain`, `enrollment {enabled, audience, fleet_namespace, state_path, replicas, policy, require_synchronized_clock, activation_leeway_secs, limits {…, sync_timeout_ms}, roles {attribute, scopes_as_roles, scopes, tags, include_attributes}, issuer_delegation_file, issuer_lifetime_secs, renew_from, renew, synchronize}`. |
| `aster_node_start_enrolled(rt, config, trust_json, side, ud, *op)` | `side` = `aster_enrollment_side_t` (`SERVER` 0, `CLIENT` 1). Validates the plan before starting (fail closed: `INVALID_ARGUMENT` naming the setting); starts the node with fail-closed hooks and `aster.enrollment/1` and installs Enrollment. `ASTER_EVENT_NODE_CREATED`: `handle` = node, `related` = `aster_enrollment_t` (0 when the trust JSON does not enable enrollment: exactly `aster_node_start(config, gate 0)`). The node takes no gate; closing it releases the enrollment. A client node answers inbound enrollment handshakes itself. |
| `aster_server_builder_new` + `aster_server_start` (behaviour change) | On a server-side enrolled node the server routes `aster.enrollment/1`, serves `aster.enrollment.IssuerV1` when an issuer delegation is configured, feeds Gate 3 each peer's roles per call (`[enrollment.roles]`), and after serving attaches policy (blocking up to `sync_timeout_ms` with replicas; `UNSUPPORTED` on a tokio thread) and starts renewal. A client-side enrollment is `INVALID_ARGUMENT` at `builder_new`. |
| `aster_enrollment_enroll(rt, e, server_json, ud, *op)` | Client: handshake with one server, sync the fleet namespace it offers, start renewal (from `renew_from`, or from this server when it signed the credential). Result `{readiness, renewing}`. Call before the first `aster/1` connection to that server. Either side's admission decision keeps its kind in `flags`: a server that denies this node is `PERMISSION_DENIED` + `DENIED`; a server that cannot evaluate policy now, or a skewed clock, is `UNAVAILABLE` + `UNAVAILABLE` / `CLOCK_SKEW`. A server that cannot be reached or does not finish the handshake in time is `UNAVAILABLE` with `flags` 0. |
| `aster_enrollment_status(rt, e, *out_json)` | `side`, trusted root, fleet namespace, audience, own credential, rotation state, `renewing`, `issuer`. |
| `aster_enrollment_peer_claims(rt, e, peer, *out_json)` | Verified evidence of a peer's session (audit; the server maps roles itself). |
| `aster_enrollment_check_peer(rt, e, peer, ud, *op)` | Re-evaluate a peer before a sensitive operation; `DENIED` revokes. |
| `aster_enrollment_next_revocation(rt, e, ud, *op)` | The next revocation (buffered from install) or `{lagged}`. |
| `aster_enrollment_issuer_info(rt, e, *out_json)`, `aster_enrollment_issue(rt, e, request_json, *out_json)` | The server's configured issuer; `NOT_FOUND` without one. A refusal returns `PERMISSION_DENIED` with `{code, message, retryable}`. |
| `aster_server_set_node_attribute(server, name, name_len, value, value_len)` | Set (empty value: remove) a text attribute of the served `aster.ops.NodeInfo` identity; bindings announce `aster.registry_namespace` there after publishing contracts (the retired consumer-admission response used to carry it). `UNSUPPORTED` with the baseline off. |
| `aster_enrollment_free(rt, e)` | Release early (pending revocation waits end `ALREADY_CLOSED`). |
| `aster_server_add_lease_grantor(builder, serializer, requires_json, revoke_requires_json)` | Serve `aster.lease.Grantor` over a lease serializer handle (foreign or memory). Requirements are `CapabilityRequirement` JSON; empty `revoke_requires_json` = the `operator` role, `null` = none. |
| `aster_lease_serializer_grantor_new(rt, node, connection, *out)` | A serializer backed by a remote grantor over an `aster/1` connection (`GrantorSerializer`); acquire with holder = the node's id. |
| `aster_server_restrict_peer_roles(server, peer, peer_len, roles, roles_len)` | Narrow an enrolled peer's roles to `roles` (comma-separated UTF-8; empty = none) ∩ its credential roles, from its next call. The credential sets the maximum: a role it lacks is never granted. Replaces an earlier restriction; ends with the peer's enrollment session (revocation, eviction, expiry), survives renewals. `NOT_FOUND` for an unknown server or a peer without a live enrollment session. |
| `aster_server_clear_peer_restriction(server, peer, peer_len)` | Remove the restriction; the credential roles apply from the next call. `OK` whether or not one was set. |
| `aster_server_set_peer_attributes` (behaviour change) | For a peer with a live enrollment session, a k/v list carrying `aster.role` is refused with `INVALID_ARGUMENT` (use `aster_server_restrict_peer_roles`); a stored `aster.role` never applies while a peer has a session. Other keys apply as before (the credential's attributes win on a clash). Peers without a session (no enrollment, custom gate) are unchanged. |

Behaviour change: `aster_gate_new`'s default admission ALPNs are
`aster.producer_admission` and `aster.admission`; the retired
`aster.consumer_admission` is no longer open by default.
`aster_node_set_gate` returns `UNSUPPORTED` on an enrolled node. No struct
layout changes. Go requires 5.1 (`ABIMinorRequired = 1`).

An earlier, unreleased draft of 5.1 had a low-level surface; it never shipped
in a tag (v0.3.900 is 4.16) and was replaced before release:

| Removed (draft 5.1) | Replacement |
|---|---|
| `aster_enrollment_install(rt, node, config_json, *out)` and its install JSON | `aster_node_start_enrolled` with the trust JSON |
| `aster_enrollment_connect`, `aster_enrollment_synchronize` | server: `aster_server_start` (replicas); client: `aster_enrollment_enroll` |
| `aster_enrollment_start_renewal`, `_renewal_wait`, `_renewal_free`, `aster_enrollment_renewal_t` | automatic renewal (`renew_from`, or the signing server); `status.renewing` |
| `aster_enrollment_issuer_new`, `_issuer_free`, `aster_enrollment_issuer_t` | `[enrollment.issuer] delegation_file`; `_issuer_info` / `_issue` take the enrollment |
| `aster_server_set_enrollment` | automatic on a server-side enrolled node |
| `aster_enrollment_verify_credential`, `aster_enrollment_replace_credential` | none (renewal replaces credentials; Rust keeps both) |
| mapping `aster_enrollment_peer_claims` into `aster_server_set_peer_attributes` | the server's per-call role source (`[enrollment.roles]`) |

## 5.0 (removals)

ABI 5.0.0 removes the Fory ownership-attestation surface added in 3.4. Aster's
credentials are now JWT delegation chains (`docs/_internal/ownership-attestations.md`),
which have no C ABI yet; a delegation ABI will be added in its own release once a
non-Rust consumer needs one. Everything else in 4.17 is unchanged, so a binding
moves from 4.17 to 5.0 by updating its expected major and dropping these symbols.

| Kind | Removed | Replacement |
|---|---|---|
| function | `aster_attest_root_node` | none (delegation chains, Rust only) |
| function | `aster_attestation_chain_to_text`, `aster_attestation_chain_from_text` | none |
| function | `aster_attestation_verify_chain` | none |
| function | `aster_verified_chain_role_of`, `aster_verified_chain_release` | none |
| struct | `aster_attest_options_t`, `aster_verified_chain_t` | none |
| enum | `aster_attestation_role_t` (`ASTER_ATTESTATION_ROLE_*`) | none |

`aster_secret_key_public_key` stays: key derivation now lives in
`aster_transport_core::keys`. `ASTER_STATUS_VERIFICATION_FAILED` stays for HPKE
and sealed-grant checks.


## New in 4.16 (local/shared topology views and RPC policy; additive)

`aster_topology_has_monitoring`, `_peers` and `_peer` expose local passive metrics.
Peer JSON preserves nullable u64 RTT/jitter, full u64 counters, ladder levels 0–4,
and local-only remote socket addresses; baseline RPC deliberately omits addresses.
No-monitoring views are empty, and unknown peers return JSON null.

`aster_topology_snapshot` captures one immutable shared view and emits
`ASTER_EVENT_TOPOLOGY_SNAPSHOT` = 120 with an owned handle. Free it with
`aster_topology_snapshot_free`; query `_clusters`, `_cluster_of`, `_separated`
on the captured handle. Unknown (0), Connected (1), Separated (2) come from native
coverage rules; missing measurements never imply separation. Returned JSON spans
are independently owned and use `aster_string_release`. Snapshot ownership is
runtime-scoped; canceled undispatched snapshot creation releases its owner.

`aster_topology_enable_shared_with_options` adds strict JSON native options for
position refresh, min hold, liveness/edge TTL, clock skew, separation coverage and
witness count. Omitted values retain defaults; explicit zero is preserved except
position refresh zero means 60s. Every value is at most u32; options cap is 16 KiB
and unknown fields fail. Canceling a join wait does not roll back installed work;
the node owns its publisher until shutdown or replacement.

`aster_server_set_topology` atomically selects explicit baseline RPC opt-in and
an optional native capability requirement (role/any_of/all_of). The new API
requires monitoring when enabled; default is disabled. This does not open an RPC
for local reads or duplicate authorization rules in host code. Legacy builder
options remain compatible. Shared observations are local evidence, not policy proof.

## New in 4.15 (coordination authority provisioning; additive)

Nine stateless `aster_coordination_*` functions expose native canonical genesis
and join document encoding, genesis/join signing preimages, genesis hashing,
canonical candidate tickets, signed-envelope assembly, genesis verification and
join verification for start. Canonical JSON DTO inputs use hex32 identifiers and
nonces; native code fixes version 1 and derives the candidate ticket. Caller
ordering is preserved. Encoding and envelope assembly do not establish authority.

`aster_provisioning_result_t` contains owned `data` and `error` byte spans;
release both with `aster_string_release` on every path. Success data is raw Fory,
preimage/hash bytes, UTF-8 ticket, or verified-summary JSON depending on the call.
Errors contain JSON `{code,detail}` with stable code and diagnostic detail:
invalid input maps to `INVALID_ARGUMENT`, encoding to `INTERNAL`, verification
to `VERIFICATION_FAILED`. These synchronous operations require no runtime.

Verification pins an independently configured authority public key and domain;
join-for-start also pins the candidate, but extracts group/genesis rather than
checking independent expected values. Returned summaries are copied metadata,
not native sealed authority handles. External signing, policy selection, list
sorting, group creation and membership commitment remain caller responsibilities.

## New in 4.14 (locally complete batch Docs reads; additive)

`aster_doc_read_entry_contents_if_complete(runtime, doc, hashes, user_data,
 out_operation)` reads ordered content hashes in one local operation. Inputs
are copied and all hashes validated as hex64 before submission. Empty input
is valid; order and duplicates are preserved. No fetch, tags or retention.

`ASTER_EVENT_DOC_CONTENTS_BATCH` = 119 returns `u32LE count`, then each slot:
`u8 presence`; 0 has no following bytes, 1 has `u64LE length` and exactly that
many bytes. Count equals submitted hashes; empty input encodes four zero bytes.
Present empty data uses presence 1 and length zero, distinct from missing/partial
presence 0. Decoders reject unknown markers, mismatched counts, oversized lengths,
truncation and trailing bytes. Native buffers use normal completion ownership.

Missing or partial content at the initial batch check returns an absent slot.
Complete inline values are copied in the store transaction; large complete
values use validated export. This is not a global snapshot: concurrent GC/delete
may fail large export and thus the entire batch. I/O/integrity errors are never
converted to absence. Use custody for stable host-side verification. Standard
read-only operation cancellation applies; no custody semantics are introduced.

## New in 4.13 (temporary blob custody and verified retained imports; additive)

- `aster_blobs_custody_new` / `aster_blob_custody_free` own a temporary scope
  protecting multiple `(hash, RAW|HASH_SEQ)` roots, including hashes not local
  yet. Free seals new work; synchronously accepted operations keep every scope
  guard until their work and cleanup finish.
- `aster_blob_custody_protect`, `_add_bytes`, `_add_path`, `_retain`, `_read` and
  `_download_hash_from` span acquisition, fetching and host verification.
  Imports create no automatic persistent tag; retain commits the exact named
  root. HashSeq protection includes its graph without imposing Collection
  schema, and scoped reads return raw root bytes.
- `aster_blobs_add_bytes_verified` / `aster_blobs_add_path_verified` copy input,
  compare its store-computed hash and commit the exact named tag only on a match.
  Mismatch returns `VERIFICATION_FAILED`, preserves the old named root and
  creates no anonymous persistent tag. Existing store durability is unchanged.
- `ASTER_EVENT_BLOB_CUSTODY_DONE` = 118 is the empty protect/retain completion;
  imports emit `BLOB_ADDED`, read emits `BLOB_READ`, fetch emits `BLOB_FETCHED`
  with the existing report layout. All operations use cooperative cancellation:
  collect their single definitive terminal event; pre-commit cancellation waits
  for cleanup, while a commit already started returns actual success/error.
  Scoped fetch cancellation may wait for downloader/network drain.

## New in 4.12 (public HTTP/WebTransport and certificate helpers; additive)

- `aster_server_http_start_public`: explicit anonymous serving, no upcall table;
  authentication limits must be zero. Session-scoped services are rejected;
  method gates remain active and identity attributes are empty.
- `aster_tls_generate_self_signed`, `aster_tls_generate_webtransport`,
  `aster_tls_verify_binding`, and `aster_generated_cert_t`: shared TLS generation,
  certificate DER SHA-256 pins and optional signed node-key authorization.
  PEM outputs are separately released with `aster_string_release`.
- Authenticated start remains unchanged. Node-binding verification checks TLS
  key authorization, not certificate expiry, hostname or PKI trust. No ACME or
  FIPS capability is implied.

## New in 4.11 (authenticated HTTP/WebTransport; additive)

`aster_server_http_start` attaches a PEM HTTPS listener to an existing dispatch
server and requires an upcall table. `aster_http_listener_tcp_addr` and
`aster_http_listener_quic_addr` return bound addresses; enabling WebTransport
uses the TCP-selected port for QUIC, including port zero.
`aster_http_listener_stop` drains within a grace period, then cancels remaining
calls and joins listener cleanup. QUIC protocol/socket drain may extend beyond
the application grace period. Start/stop and server destruction run off Tokio
runtime threads.
Listeners belong to their runtime and server; cross-runtime attachment is
rejected. Runtime close retires server and upcall-table owners even when callers
have not explicitly freed them. There is no implicit public mode or ACME
configuration in this API.

`ASTER_UPCALL_KIND_RPC_AUTHENTICATE` (80) uses UCv1 C9/CA. Authentication requires
an explicit nonempty principal and per-call attributes, or explicit rejection.
Pending requests and payload sizes are bounded. Missing callbacks, malformed
answers, overload, shutdown and timeout fail closed regardless of any fail-open
policy. The routed call deadline also applies while authentication is pending.
The payload codecs have shared conformance vectors across all five language
bindings; listener wrappers are introduced in Go first.

## New in 4.10 (Docs receiver lifecycle; additive)

`aster_doc_event_receiver_free(runtime, receiver)` releases a live Docs event
subscription independently from its Doc handle. It removes the registry owner
and wakes pending receives with `ASTER_EVENT_DOC_EVENT` / `NOT_FOUND` (EOF).
An in-flight receive racing close may complete successfully. Repeated release returns `NOT_FOUND`. Stream EOF/errors release the receiver
automatically; runtime shutdown wakes blocked reads and drops their owners.

`aster_doc_event_recv` serializes reads and now interrupts blocked reads when
`aster_operation_cancel` wins. An event consumed just before cancellation wins
is retained for the next receive. Cancellation and completion produce exactly
one terminal event: bindings must await that terminal after cancelling, returning
an already committed successful event when completion won the race. They must
release every returned payload buffer.

The wire layout is unchanged: `event.flags` carries the subtype, query-shaped
entry metadata has no inline value, and atomic join returns the document in
`event.handle` and the 64-bit receiver in `event.related`. The generated header
now states these contracts explicitly.

## New in 4.9 (durable audit outbox; additive)

`aster_audit_outbox_{open,clone,submit,status,flush,close,free,reconfigure,quarantine,export,replay}`
wrap the shared Rust outbox with runtime-owned references and the existing async
completion queue. New events 114–117 carry opened handles, durable submission IDs,
JSON status and original evidence exports. Typed error codes distinguish admission,
storage/uncertain commits, lifecycle, delivery stalls and explicit dispositions.

Requests/results use bounded JSON v1; see
[`Aster-audit-outbox-spec.md`](../ffi_spec/Aster-audit-outbox-spec.md). No existing
layout changes. Ordinary diagnostic logging retains its existing behavior.

ABI 3.0.0 renames the entire C ABI of `aster_transport_ffi` to the single
`aster_` / `ASTER_` prefix. No backward compatibility: a binding must switch
every symbol, pass `ASTER_ABI_VERSION_MAJOR` (3) to `aster_runtime_new`, and
should check `aster_abi_version() >> 16 == 3` at load.

**ABI 4.0** (see "4.0 (removals)" below) removed the reactor, the framed
`aster_call_*` client, the 3.2-deprecated node factories and bare endpoints.
The 3.x sections and the rename table still name those symbols as a record of
3.x; they no longer exist.

Header: `ffi/aster_ffi.h` (was `ffi/iroh_ffi.h`). Library file name unchanged
(`libaster_transport_ffi.{so,dylib}`, `aster_transport_ffi.dll`).

## Signature / semantic changes (beyond the rename)

| Old | New | Change |
|---|---|---|
| `int32_t iroh_runtime_new(const iroh_runtime_config_t*, iroh_runtime_t*)` | `int32_t aster_runtime_new(uint32_t expected_abi_major, const aster_runtime_config_t*, aster_runtime_t*)` | New first argument. Mismatch returns `ASTER_STATUS_ABI_MISMATCH` (12) and sets the last-error message; nothing is created. A non-NULL config must have `struct_size == sizeof(aster_runtime_config_t)` (16), else `ASTER_STATUS_INVALID_ARGUMENT`. |
| `uint32_t iroh_abi_version_major/minor/patch(void)` | `uint32_t aster_abi_version(void)` | One packed value `(major << 16) \| (minor << 8) \| patch`. Macro `ASTER_ABI_VERSION` is the header's packed value. |
| `const char* iroh_status_name(iroh_status_t)` | `const char* aster_status_name(int32_t)` | Raw status int (no UB on unknown values); unknown -> `"ASTER_STATUS_UNKNOWN"`. |
| `int32_t iroh_hook_before_connect_respond(rt, inv, iroh_hook_decision_t)` | `int32_t aster_hook_before_connect_respond(rt, inv, uint32_t decision)` | Same width; values other than ALLOW/DENY -> `ASTER_STATUS_INVALID_ARGUMENT`. |
| `iroh_endpoint_config_t.relay_mode`: unknown value silently = default | `aster_relay_mode_t`; unknown -> `ASTER_STATUS_INVALID_ARGUMENT` | `ASTER_RELAY_MODE_STAGING` = 3 now named. |
| `iroh_doc_share` / `iroh_doc_share_with_addr` `mode`: 0 = read, anything else = write | `aster_doc_share_mode_t`: 0 read, 1 write, other -> `ASTER_STATUS_INVALID_ARGUMENT` | |
| `ASTER_EVENT_KIND_CALL/CONNECTION_CLOSED/RESPONSE_WRITABLE/RESPONSE_CLOSED` (reactor `event_kind`, uint8) | `ASTER_REACTOR_EVENT_CALL/CONNECTION_CLOSED/RESPONSE_WRITABLE/RESPONSE_CLOSED` | Values unchanged (0..3). Renamed so they cannot be confused with the completion-queue `ASTER_EVENT_*` values of `aster_event_kind_t`. |

## New in 4.7 (independent event and operation bounds)

`aster_runtime_config_t` keeps its 16-byte layout and gives the former reserved
word at offset 12 the additive meaning `max_inflight_operations` (zero = 4096).
This preserves binary layout and zero-initialized legacy behavior, but the
field rename is source-incompatible for C callers that used the old
`.reserved` designated initializer; they must rename that initializer.
`event_queue_capacity` now bounds only events waiting to be polled. Running
operations and terminal events waiting for queue capacity are independently
bounded by `max_inflight_operations`; saturation returns the new pre-start
`ASTER_STATUS_OPERATION_LIMIT` (18). Wrappers surface that overload immediately.

## New in 4.8 (bounded foreign dispatch staging)

`ASTER_SERVER_OPTION_MAX_FOREIGN_INFLIGHT` (5) sets the finite per-kind limit
for live foreign calls, sessions, RPC connections and transferred non-RPC
connections (default 4096 each; total at most four times the configured
value). The dispatch ring and its FIFO staging queue retain event ownership
until polling. A saturated call pool rejects a new RPC call with
`RESOURCE_EXHAUSTED` before session creation. `ring_capacity` continues to set
only the SPSC ring size.

## New in 4.6 (observability and health; one removal)

ABI 4.6.0 exposes the Rust-owned OpenTelemetry pipeline (`aster-otel`) and
the ops listener (`aster::ops`) to Java, .NET and Go. Design:
`docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §7. Sources:
`ffi/src/observability.rs`, `ffi/src/health.rs`, `ffi/src/server.rs`. Tests:
`ffi/tests/observability.rs`, `ffi/tests/health.rs`,
`ffi/tests/abi_contract_test.c` (`test_observability`, `test_health`), the
`ffi/src/layout.rs` assertions, and the Miri / sanitizer jobs for the pure
parts (`observability::tests`: the log arena, the codecs). Everything below
except the health and dispatch functions is process-global (no `runtime`).
k/v lists use the metadata encoding; attribute values are strings (Q13).

**Removed (pre-release, decision Q12; no deprecation step):**
`aster_node_metrics_prometheus(rt, node, out)`. Use `aster_metrics_prometheus(out)`:
the transport counters are now `aster_transport_bytes_total` /
`aster_transport_connections` in the OTel scrape, next to every other
instrument. It needs the Prometheus pull reader (a running
`aster_health_start` listener, or the `prometheus` metrics exporter).

**New status:** `ASTER_STATUS_ALREADY_EXISTS = 17` (`aster_status_name`
knows it): `aster_observability_init` with a configuration that differs from
the running pipeline's (the message names the settings), or a second
concurrent `aster_log_poll`.

**Behaviour on misuse:** `aster_upcall_poll` called on a tokio runtime thread
returns `ASTER_STATUS_UNSUPPORTED` (it blocks).

**Poll signatures (4.6):** `aster_log_poll` and `aster_upcall_poll` return an
`int32_t` status with the count in `uint32_t *out_count` (always written; 0 on
any non-OK status; 0 with `OK` on timeout). Bindings read the status code and
never parse the last-error message.

**Built-ins on the C ABI paths:** with a pipeline running, `aster_server_*`
dispatch records the `aster.rpc.server.*` instruments and the server span,
and `aster_rpc_unary` / `aster_rpc_unary_cancellable` / `aster_rpc_open`
record `aster.rpc.client.*` and a client span. A `traceparent` the binding
writes into `aster_call_options_t.metadata` (from
`aster_trace_context_format`) parents the client span, and the outgoing
value is replaced by the client span's (design §5.4).

### Pipeline (§7.1)

| Item | Notes |
|---|---|
| `aster_observability_config_t` (128 bytes) | `struct_size`, `flags` (`ASTER_OBSERVABILITY_FROM_ENV` 1, `_TRACES` 2, `_METRICS` 4, `_OTLP_LOGS` 8, `_LOG_BRIDGE` 16), `service_name`, `otlp_endpoint`, `otlp_headers` (k/v; secret, never logged), `resource_attrs` (k/v), `log_filter`, `extra_toml` (`[observability]` table; the header line is optional), `trace_sample_ratio` (< 0 = env/default), `protocol` (`aster_otel_protocol_t`), `log_queue_capacity`, `binding` (`aster_binding_kind_t`). A signal flag set = export it over OTLP; clear = env decides with `FROM_ENV`, else off. Unknown flag bits are `INVALID_ARGUMENT`. |
| `aster_observability_init(cfg*)` | NULL = env only. Start or join (refcount + 1). Different config: `ALREADY_EXISTS`. `LOG_BRIDGE` is additive (opens the bridge on a running pipeline). Runs off any tokio thread internally, so it may be called from one. |
| `aster_observability_flush(timeout_ms)` | `TIMEOUT` past the limit; no pipeline: `OK`. |
| `aster_observability_shutdown(timeout_ms)` | Releases one init reference; the last reference anywhere (a health listener holds one too) shuts down: providers flush and stop, the log bridge closes, live spans end, instrument and attribute set handles are freed. `ALREADY_CLOSED` when the C ABI holds no reference. |

### Logs (§7.2)

| Item | Notes |
|---|---|
| `aster_log_record_t` (96 bytes, no `struct_size`) | `level` (`aster_log_level_t` 1..5), `flags` (`ASTER_LOG_FLAG_HAS_TRACE` 1, `_IN_CALL` 2, `_SYNTHETIC` 4, `_AUDIT` 8), `timestamp_unix_ns`, `trace_id[16]`, `span_id[8]`, `call_id`, `target`, `message`, `fields` (k/v: event fields, `rpc.service`, `rpc.method`, `aster.peer`, `thread.name`). |
| `aster_log_poll(out*, record_size, max, timeout_ms, uint32_t *out_count)` → status | `record_size` must be `sizeof(aster_log_record_t)` (else `INVALID_ARGUMENT`; so are NULL `out` / `out_count` and `max` 0). One poller per process (a concurrent second poll: `ALREADY_EXISTS`). Refused on a tokio thread (`UNSUPPORTED`). Marks the calling thread as a log drain (no feedback). Audit records first, then FIFO; a synthetic drop report leads a batch at most once a second. Pointers are valid until the next poll or `aster_log_close` (one arena per batch). `NOT_FOUND` once the bridge is closed or not open; `OK` with `*out_count` 0 on timeout. |
| `aster_log_close()` | Closes the bridge, wakes a waiting poll; a later init with `LOG_BRIDGE` reopens. |
| `aster_log_dropped(u64* out)` | General records dropped (queue full) since process start. |
| `aster_log_set_filter(directives, len)` | `EnvFilter` reload; `INVALID_ARGUMENT` on a parse error, `NOT_FOUND` without a bridge. `aster::audit=info` stays on unless named. |
| `aster_log_emit_t` (72 bytes), `aster_log_emit(rec*)` | Path B: an application record into the Rust OTel logs SDK (`level` 0 = INFO, `logger`, `message`, `fields`, optional `context`). `OK` and not exported without an OTLP logs exporter. |

### Metrics (§7.3)

| Item | Notes |
|---|---|
| `aster_instrument_desc_t` (96 bytes), `aster_instrument_kind_t` | `COUNTER_U64` 1, `UP_DOWN_I64` 2, `HISTOGRAM_F64` 3, `GAUGE_F64` 4. `name` (OTel rules), `description`, `unit`, `auto_attrs` (key list; kept, merging lands with the per-call extractor cache in O6+), `cardinality_limit` (kept; the SDK default 2000 applies in 4.6), `boundaries` / `boundaries_len` (histogram bucket upper bounds as `const double *`, finite and strictly increasing; none = the RPC seconds buckets 0.005..10 for unit `s`, else the SDK default; `INVALID_ARGUMENT` on a non-histogram or bad values). |
| `aster_instrument_new(desc*, *out)` / `aster_instrument_free(h)` | Scope `aster.app`. Records into whichever pipeline runs at record time (a handle declared before init works after it). A name already declared with another kind: `INVALID_ARGUMENT`. |
| `aster_attrset_new(kv, len, *out)` / `aster_attrset_free(h)` | Interned attribute set: build once, record many times. |
| `aster_metric_add_u64` / `_add_i64` / `aster_metric_record_f64(h, v, set, kv, kv_len, call_id)` | `set` 0 = none; `kv` appended. Wrong kind for the value type: `INVALID_ARGUMENT`. No I/O, no runtime call; a short SDK lock. `call_id` reserved for `auto_attrs`. |
| `aster_metrics_prometheus(aster_bytes_t* out)` | Prometheus text 0.0.4 of the whole pipeline (what `/metrics` serves); `NOT_FOUND` without a pull reader. Free with `aster_string_release`. |

### Spans and trace context (§7.4)

| Item | Notes |
|---|---|
| `aster_span_context_t` (32 bytes) | `trace_id[16]`, `span_id[8]`, `trace_flags`, `is_remote`. All-zero = none. |
| `ASTER_DISPATCH_FLAG_HAS_TRACE` (0x400), `aster_dispatch_trace_context(rt, server, call_id, *out)` | The call's server span context while the call is live (`NOT_FOUND` after); all-zero with `OK` when its span is not exported. |
| `ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE` (0x800), `aster_dispatch_inbound_trace(rt, server, call_id, *out_ptr, *out_len, *out_buffer)` | The call's inbound W3C `baggage` and `tracestate` (hidden from `aster_dispatch_metadata`, Q7) as a k/v list with keys `baggage` / `tracestate` (raw header values, a key only when sent), whether or not the span is exported. `baggage` over 8192 bytes or 64 members was dropped. Lease released with `aster_dispatch_buffer_release`. `NOT_FOUND` once the call is over. |
| `aster_span_start(name, kind, parent*, kv, kv_len, *out)` | `aster_span_kind_t` 0..4; `parent` NULL = root. At most 65 536 live spans (`QUEUE_FULL`). A no-op span (all-zero context) without a pipeline. |
| `aster_span_set_attributes`, `aster_span_add_event`, `aster_span_set_status(s, code 0/1/2, msg)`, `aster_span_record_error(s, type, msg)`, `aster_span_context(s, *out)`, `aster_span_end(s)` | `end` frees the handle; shutdown ends spans still open. |
| `aster_span_record_t` (152 bytes), `aster_span_record(rec*)` | A finished span with the binding's own ids and times (.NET `Activity` bridge). `events`: `u32 count`, then per event `u32 len` + name, `u64 unix_nanos`, `u32 len` + k/v list. Not exported when its sampled flag is clear. `INVALID_ARGUMENT` for all-zero ids, a parent from another trace, end < start. |
| `aster_trace_context_format(ctx*, buf, cap, *out_len)` / `aster_trace_context_parse(s, len, *out)` | W3C `traceparent` (55 bytes, version 00, lowercase); `BUFFER_TOO_SMALL` with `*out_len` = 55; parse marks the result remote. |

### Health (§7.5)

| Item | Notes |
|---|---|
| `aster_health_config_t` (24 bytes), `aster_health_t` | `struct_size`, `port` (0 = ephemeral), `host` (empty = `127.0.0.1`). |
| `aster_health_start(rt, node, cfg*, *out)` | `cfg` NULL = the node's `AsterConfig` (`ASTER_HEALTH_*`, `[health]`; `INVALID_ARGUMENT` when disabled there). Joins or starts the pipeline with the Prometheus pull reader. Blocks until bound; refused on a tokio thread. |
| `aster_server_attach_health(builder, h)` | Before `aster_server_start`: a successful start sets ready (and the service count); `aster_server_destroy` sets not ready first. |
| `aster_health_set_ready(h, bool)`, `aster_health_set_healthy(h, bool)`, `aster_health_check_set(h, name, bool)` | Named checks are created failing; `/readyz` needs all of them. Once a log bridge is open, `/readyz` also carries the built-in `audit` check (fails for 30 s after a dropped audit record). |
| `aster_health_addr(h, aster_bytes_t* out)` | `"ip:port"`; free with `aster_string_release`. |
| `aster_health_stop(rt, h)` | Port free on return; releases the listener's pipeline reference. |

## New in 4.5 (additive)

ABI 4.5.0 makes unary calls cancellable **on the pooled fast path**, so a
binding can cancel every unary call by default at no cost. Before 4.5,
`aster_rpc_unary` had no handle to cancel: Go and .NET ran a unary call with a
cancellable context/token through `aster_rpc_open` (a dedicated stream per
call) and Java made that opt-in (`CallOptions.cancellable`) because it was
slower. Nothing in 4.4 changed shape. Source: `ffi/src/rpc.rs`. Tests:
`ffi/tests/dispatch_server.rs` (`unary_cancel_token_*`),
`ffi/tests/abi_contract_test.c` (`test_cancel_tokens`). Benchmark:
`benchmarks/dispatch/ffi_unary_bench.rs` (example `ffi_unary_bench`).

| Function | Notes |
|---|---|
| `aster_cancel_token_new(rt, *out_token)` | A token (`aster_cancel_token_t`, `u64`, never reused) for **one** call. Never blocks; `runtime` is not consulted. |
| `aster_rpc_unary_cancellable(rt, conn, opts*, token, req, len, ...)` | `aster_rpc_unary` plus `token` (0 = exactly `aster_rpc_unary`). Same pooled single-write path, same out-parameters and results. Consumes the token on every return path (argument errors too) except when it refuses the token: unknown, or already taken by another call, is `INVALID_ARGUMENT` and leaves it as it was. |
| `aster_cancel_token_cancel(rt, token)` | Any thread, never blocks, idempotent. Before the call starts: the call returns `CANCELLED` without sending. In flight: the call is abandoned like a dropped Rust future and returns `OK` with `status_code` `CANCELLED` ("call cancelled"). `OK` while the token is live; `NOT_FOUND` once the call has returned (nothing left to cancel). |
| `aster_cancel_token_release(rt, token)` | Frees a token that was never passed to a call. `NOT_FOUND` if gone; `INVALID_ARGUMENT` while a call holds it (releasing it would make that call uncancellable; the call frees it). A release that wins the race with a starting call makes that call refuse the token. |

Tokens are process-global (one table for all runtimes); the token functions ignore `runtime`. `aster_rpc_unary` and `aster_rpc_unary_cancellable` called from a tokio runtime thread return `UNSUPPORTED` (nothing sent; a token is consumed) instead of panicking across `extern "C"`.

**Design.** The call `block_on`s `select!{ biased; token.cancelled() => CANCELLED, unary_with(..) => outcome }`
on the runtime. Losing the race drops the `unary_with` future *inside* the
runtime, so `PooledCall`'s drop does what it does for a dropped Rust future:
it discards the pooled stream and, from a spawned task, writes `FLAG_CANCEL`
(skipped if the request write was cut off part-way) then resets the send side
and stops the receive side. The server's reactor turns that into
`Cancelled{Peer}` for the handler (foreign handlers: an
`ASTER_DISPATCH_EVENT_CANCELLED` with reason `PEER`). The pool opens a fresh
stream for the next call; the connection is untouched. A call still waiting
for a pooled stream (all 8 SHARED streams busy) is dropped from the acquire
queue the same way. There is no completion event: as for `aster_rpc_unary`,
the outcome is the out-parameters, so "exactly one outcome" is structural.

**Races.** *Cancel before start:* the token's flag is set first, so a cancel
that lands before (or while) the call takes the token sticks; the call checks
the flag before touching the wire. *Cancel vs. completion:* the `select!` polls
the cancel first; whichever is observed first wins, the other is dropped, and
the caller gets exactly one outcome (the response or `CANCELLED`). *Lost
wakeup:* the waiter registers on the `Notify` before reading the flag, and
`cancel` sets the flag before `notify_waiters`. *Cancel after return:* the call
removes its token from the table when it returns, so a late cancel finds
nothing (`NOT_FOUND`); ids come from the shared handle counter and are never
reused, so a late cancel can never hit another call. *Use-after-free:* the
table holds `Arc`s and the call holds its own clone; nothing points into
caller memory after the call returns. *Token reuse:* a `taken` flag gives a
token to one call; a second concurrent call with it is refused without
freeing it under the first, and so is a release while a call holds it.

**Cost.** One extra crossing (`aster_cancel_token_new`, an insert into one of
64 mutex-sharded maps) plus a lookup and a remove in the call, and a `Notify`
in the `select!`.

**Claim, and what supports it:** *no regression detectable at c=1 (run-to-run
noise is >= 1.5%); off the critical path at c >= 16.* Only the c=1 rows are
evidence about the token's cost: at c >= 16 every pooled mode is bound by the
SHARED pool (8 streams), so callers queue for a stream and a few hundred ns
per call cannot show in throughput. Per-call CPU time (C ABI) and allocations
(Go) are the c-independent check.

**Measurements** (2026-10-04, M3 Max, loopback, release; machine shared with
other agents, load average 15-40; closed loop, one connection; median and
min..max over interleaved runs of 5 s).

C ABI (`ffi_unary_bench`, in-process Rust server; 5 runs; CPU = process
user+system time per call, client and server together):

| path | c=1 rps (min..max) | c=1 CPU us/call | c=16 rps (min..max) | c=100 rps (min..max) | c=100 p99 ms |
|---|---:|---:|---:|---:|---:|
| `aster_rpc_unary` | 17,333 (16,535..17,688) | 65.3 (63.9..65.5) | 34,512 (34,162..36,252) | 34,037 (33,967..34,457) | 3.33 |
| `aster_rpc_unary_cancellable` | 17,413 (17,131..17,910) | 64.2 (63.6..65.5) | 34,311 (32,833..37,426) | 34,112 (33,906..34,403) | 3.31 |
| `aster_rpc_open` + send + recv (pre-4.5 Go/.NET) | 13,569 (13,233..14,319) | 107.7 (105.2..110.2) | 22,676 (22,513..24,087) | 26,652 (26,374..26,797) | 8.74 |

An earlier 3-run session also measured the decomposition (median rps at
c=1/16/100): Rust `RpcConnection::unary_with` without the C ABI
17,583 / 34,028 / 33,901; Rust `server_stream_with` + read 13,085 / 23,810 /
26,831; the C ABI open path with 200 ms recv slices (pre-4.5 Java)
13,211 / 22,596 / 26,061.

**Where the old gap came from:** the dedicated stream per call, not the C ABI.
The Rust-native stream-per-call row (`server_stream_with`) is as slow as the
C ABI `open` path, and the C ABI fast path is as fast as Rust's own
`unary_with`. A fresh QUIC bi-stream per call costs ~22% of throughput at c=1
and 1.65x the CPU per call (the c >= 16 open-path numbers are not comparable:
that path is not pool-bound); the extra crossings, handle registry, boxed
reads and 200 ms polling cost at most ~2%. (Which part of the per-stream work
dominates was not profiled.) Java's earlier "several times slower" was an
ad-hoc measurement on a loaded machine; measured properly it was 18-34%.

Java (`tests/interop/java/InteropEcho.java bench` against the Rust
`interop_echo` server, fresh server per run, 3 runs, the variants interleaved;
"before" = the `bindings-catchup` Java classes, "after" = this change; same
library). rps median (min..max):

| Java client | c=1 | c=16 (pool-bound) | c=100 (pool-bound) |
|---|---:|---:|---:|
| before: default (`aster_rpc_unary`, not cancellable) | 14,022 (13,967..14,178) | 31,798 (31,413..31,836) | 31,759 (31,291..32,238) |
| before: `CallOptions.cancellable(true)` (`aster_rpc_open`) | 10,676 (10,654..10,915) | 20,882 (20,811..21,076) | 26,088 (25,846..26,352) |
| after: default (cancel token, always cancellable) | 14,236 (13,789..14,464) | 31,729 (31,713..32,030) | 31,743 (31,505..31,816) |

Go (`bindings/go/rpc_unary_bench_test.go`, `ASTER_GO_UNARY_BENCH=1`; Go server
and client in one process, one cancellable ctx per worker; 3 runs). rps
median (min..max); allocations per call (client + server, one process):
`Background` 36.9 allocs / 2,889 B, token 37.4 / 2,648 B, open 36.0 / 2,385 B.

| Go client | c=1 | c=16 (pool-bound) | c=100 (pool-bound) |
|---|---:|---:|---:|
| `context.Background()` (`aster_rpc_unary`) | 10,869 (10,857..11,669) | 27,719 (25,633..27,786) | stall-distorted (*): 18,905 (18,767..22,144) |
| cancellable ctx, after (cancel token) | 10,987 (10,948..11,611) | 27,301 (23,807..27,323) | 28,666 (27,823..28,939) |
| cancellable ctx, before (`aster_rpc_open`) | 8,991 (8,737..9,458) | 19,525 (19,202..19,890) | 25,090 (24,801..25,330) |

(*) Every `Background` run at c=100 in that session hit the Go-process stall
(~1.7 s, see the plan's progress log): those numbers say nothing about the
path. The remaining Go-side cost is `context.AfterFunc` (how any cancellable
ctx is hooked; the old open path used it too): 0-6% at c=16 across sessions,
5-9% with all workers on ONE shared ctx (its mutex); an A/B with the
AfterFunc removed put the token itself at zero, and with `GOGC=off` the gap
vanished.

## New in 4.4 (additive)

ABI 4.4.0 (tranche F7) adds the **coordination client** (the `aster`
client wrapped, never ported: journal, routing, inbound validation and the
delivery contract stay in Rust) and the **placement manager** over it, whose
providers are the existing upcall kinds 17 / 32 / 33 (now issued by the C
ABI). Nothing in 4.3 changed shape. Sources: `ffi/src/coordination.rs`,
`ffi/src/coordination_codec.rs`. Tests: `ffi/tests/coordination.rs` (feature
`coordination-fixture`: an in-process voter group; journaled mutations and
redelivery across reopen, journal lock, reads, ACL denial, leases, close with
handles and operations outstanding, placement with hand-answered providers
incl. error / unimplemented / malformed / timeout) and
`ffi/tests/abi_contract_test.c` (`test_coordination`). Design:
`docs/_internal/bindings/coordination-abi-design.md`.

Requests and results are UCv1 messages `D0`-`DE` (tags exported as
`ASTER_COORDINATION_TAG_*`; op bytes as `aster_coordination_mutation_op_t` /
`aster_coordination_read_op_t`).

| Function | Notes |
|---|---|
| `aster_coordination_client_options_init(*out)` | `aster_coordination_client_options_t` (48 bytes): `flags` (`ASTER_COORDINATION_CLIENT_READ_ONLY`), `group_id` (32 bytes), `seed_tickets` (Open tickets, routing hints), `journal_dir`. |
| `aster_coordination_client_open(rt, node, opts*, ud, *op)` | `ASTER_EVENT_COORDINATION_CLIENT_OPENED` (110), `handle` = client. A second durable open over one journal: `ERROR`, `error_code` `JOURNAL_LOCKED`. |
| `aster_coordination_client_close` / `_free` / `_ids(rt, c, group[32], local[32])` / `_id(rt, c, ud, *op)` | Close makes resolved / lease / manager handles inert (`ALREADY_CLOSED`); in-flight ops complete. `_id` → `BYTES_RESULT` (16 bytes). |
| `aster_coordination_mutate(rt, c, D0, ud, *op)` | `ASTER_EVENT_COORDINATION_RESOLVED` (111): `handle` = `aster_coordination_resolved_t`, payload `D2`. Malformed request: `INVALID_ARGUMENT` at once. |
| `aster_coordination_unacknowledged` / `_resume(rt, c, ud, *op)` | `RESOLVED`, `handle` 0 = nothing. |
| `aster_coordination_resolved_acknowledge(rt, r, ud, *op)` / `_free` | Acknowledge consumes the handle (`UNIT_RESULT`); free leaves the result journaled. |
| `aster_coordination_read(rt, c, D1, ud, *op)` | `ASTER_EVENT_COORDINATION_RESULT` (112), payload `D3`-`DA` by op. Either client. |
| `aster_coordination_retire_other_client(rt, c, id[16], ud, *op)` / `_recovery_export_to_file(rt, c, path, ud, *op)` | `RESULT`: `DC` / `DB`. |
| `aster_coordination_lease_acquire(rt, c, ns, key, ttl, renew, margin, ud, *op)` | `ASTER_EVENT_COORDINATION_LEASE_ACQUIRED` (113): granted `handle` + `DD`; denied `handle` 0 + UCv1 `B2`. |
| `aster_coordination_lease_fence(rt, l, buf, cap, *len)` / `_status` | `DD` (`BUFFER_TOO_SMALL` sets `*len`); `aster_lease_status_t`. Never block. |
| `aster_coordination_lease_compare_and_set` / `_release` / `_release_with_value` / `_free` | `RESULT` `D2` / `UNIT_RESULT` / `RESULT` `D2`; release consumes the handle and waits for an in-flight CAS. |
| `aster_coordination_placement_manager_new(rt, c, upcalls, topology_node, *m)` / `_reconcile(rt, m, ud, *op)` / `_free` | Policy kind 32, provisioner kind 33 on `upcalls`; topology = `topology_node`'s view or (0) kind 17. `RESULT` `DE`. Provider failures fail the pass (no fail-open). |

**New values:** `aster_event_kind_t` 110-113; `aster_coordination_error_t`
(the `error_code` of a coordination `ASTER_EVENT_ERROR`: 1 `AWAITING_ACK`, 2
`UNRESOLVED`, 3 `JOURNAL_LOCKED`, 4 `JOURNAL_FATAL`, 5 `RPC` (`flags` = the RPC
code), 6 `UNAVAILABLE`, 7 `CLOSED`, 8 `REJECTED` (reserved), 9 `INVALID`, 10
`OTHER`). No new `aster_status_t`; coordination admission uses the reserved
auth outcomes: RPC 7 (ACL refusal) → `PERMISSION_DENIED` (15), RPC 16 →
`UNAUTHENTICATED` (14); RPC 14 → `CONNECTION_REFUSED`, 12 → `UNSUPPORTED`.
Error events may now carry a non-zero `flags`. Codec vectors:
`conformance/vectors/coordination.json` (`ffi/tests/coordination_vectors.rs`).

## New in 4.3 (additive)

ABI 4.3.0 (tranche F5, last part) adds the **streaming consumer relay**:
relay HTTP to a peer's exposed service with a streamed request body and a
streamed response, full duplex. Nothing in 4.2 changed shape; a 4.2 binding
keeps working. Sources: `ffi/src/expose.rs` (core:
`aster::binding_support::relay`). Tests: `ffi/tests/expose.rs` (`relay_stream_*`:
round trip, stable failure codes, full-duplex lifetime contract, timeouts and
runtime-thread rule, a readiness-driven poll loop, argument validation) and
`ffi/tests/abi_contract_test.c` (`test_relay_stream`, a C-only round trip).
Design: `docs/_internal/bindings/upcall-design.md` §6.4.

| Function | Notes |
|---|---|
| `aster_http_relay_options_init(*out)` | `aster_http_relay_options_t` (24 bytes): `max_chunk` (0 = 1 MiB), `upload_capacity` (0 = 8). |
| `aster_expose_relay_stream(rt, conn, c4, opts*, *upload, *response)` | `C4` with body mode `Stream`; returns `aster_http_upload_t` + `aster_http_response_t` at once, the exchange already running. `aster_status_t`. |
| `aster_http_upload_write(rt, up, buf, len, timeout_ms)` / `_try_write(.., *queued)` / `_finish(rt, up)` / `_abort(rt, up, msg, len)` | `aster_http_body_status_t`, as the 4.2 body calls. A write takes at most `max_chunk`; a timed-out write queued nothing; `finish` / `abort` wake a parked write (`FINISHED`). |
| `aster_http_response_wait(rt, resp, timeout_ms, buf, cap, *len)` | The head as UCv1 `C5` (body mode `Stream`), sticky. `BUFFER_TOO_SMALL` (17) sets `*len`. |
| `aster_http_response_read(rt, resp, buf, cap, timeout_ms, *len, *eof)` | Waits for the head first if needed. `timeout_ms` 0 never blocks (any thread; `TIMEOUT` = nothing ready). |
| `aster_http_upload_ready` / `aster_http_response_ready(rt, h, user_data, *op)` | One-shot `ASTER_EVENT_HTTP_RELAY_READY` (109): `error_code` = the next non-blocking call's result, `status` = its `aster_status_t`. |
| `aster_http_response_close(rt, resp)` / `aster_http_upload_free` / `aster_http_response_free` | `close`/`free` the response before its end abandons the exchange (an unfinished upload is reset); freeing an unfinished upload aborts it. |
| `aster_http_body_status_to_status(code)` | Body/relay code → `aster_status_t` (`DENIED` → `PERMISSION_DENIED`, `UNKNOWN_SERVICE` → `NOT_FOUND`, `RESET`/`READ` → `STREAM_RESET`, `CONNECTION_CLOSED`/`FINISHED` → `ALREADY_CLOSED`, `OVERLOADED` → `CONNECTION_REFUSED`, ...). |

**New values:** `aster_http_body_status_t` 10 `UNKNOWN_SERVICE`, 11 `DENIED`,
12 `OVERLOADED`, 13 `RESET`, 14 `BAD_HEAD`, 15 `CONNECTION_CLOSED`, 16
`UPLOAD_FAILED`, 17 `BUFFER_TOO_SMALL` (relay calls only); `aster_status_t` 14
`UNAUTHENTICATED`, 15 `PERMISSION_DENIED`, 16 `CREDENTIAL_EXPIRED` (reserved
for the R4 auth model; only a denied relay reports `PERMISSION_DENIED` today);
`aster_event_kind_t` 109 `ASTER_EVENT_HTTP_RELAY_READY`.

**Semantic changes (changelog):** an `ASTER_EVENT_ERROR` of
`aster_expose_relay_request` now has `status` from the mapping above and the
relay code in `error_code` (was `INTERNAL` / `-1`). On the wire, a serving
node now resets a refused http-relay stream with `0x41530004` (unknown
service), `0x41530005` (denied) or `0x41530006` (stream cap) instead of
dropping it silently. Shared core, so it reaches Python / TS too:
`RelayResponse::wait` now reports a lost / closed connection as
`aster::Error::Connection` (was `Transport`), so Python's streaming relay
raises `ConnectionError` (was `IrohError`) for it; refusal messages now
describe the refusal ("service 'x' is not exposed by the peer", "the peer
denied access to service 'x'", "... is at its concurrent-stream limit")
instead of a bare end-of-stream error.

## New in 4.2 (additive)

ABI 4.2.0 (tranche F5, expose part) puts the R9 expose surface on the C ABI:
publish a local service to Aster peers (a loopback socket, or an HTTP handler
the binding answers by upcall) and relay HTTP to a peer's exposed service.
Upcall kinds 48 / 49 / 50 are now issued by the C ABI. Nothing in 4.1 changed
shape; a 4.1 binding keeps working. Sources: `ffi/src/expose.rs`; tests:
`ffi/tests/expose.rs` (a polling "foreign" thread decodes `C0` / `C4` / `C7`
and encodes `C5` / `C6` / `C8` / `CF` by hand, checked against
`conformance/vectors/upcalls.json`; real QUIC between two nodes: inline and
streamed request/response with backpressure, authorize deny and failure mode,
route policy grant / deny / malformed / fail-open, 500 / 501 / 502 / 504,
`ChunkTooLarge` / `TooLarge` / 413 / 502 limits, consumer cancel before and
after the head, socket targets, argument validation, `BlockingOnRuntime`) and
`ffi/tests/abi_contract_test.c`. Design: `docs/_internal/bindings/upcall-design.md`
§4 and §6.

**Expose handle** (`aster_expose_t`; needs the `expose` feature of `aster`,
now enabled by this crate):

| Function | Notes |
|---|---|
| `aster_expose_new(rt, node, *out)` / `aster_expose_free(rt, expose)` | A registry plus its reactor. The node is only validated; the registry binds to connections you feed it. Free drops the handle; connections already fed keep their services (`unexpose` first to stop them). |
| `aster_expose_unexpose(rt, expose, service_id)` | Idempotent. |
| `aster_expose_serve(rt, node, expose, alpns, ud, *op)` | Accept loop (`Node::serve_expose`): feeds connections whose ALPN is in `alpns` (empty = all) until the node closes, then completes with `ASTER_EVENT_ERROR`. Do not combine with `aster_accept` / `aster_node_accept_aster` on the node. |
| `aster_expose_handle_connection(rt, expose, conn)` | The composable primitive for a binding's own accept loop (`aster_accept`). The connection handle stays the binding's. |
| `aster_expose_socket(rt, expose, service_id, "ip:port", authorizer_upcalls)` | L4 splice to a loopback server. `authorizer_upcalls` != 0: kind 48 (`C0` -> `CF`) per `(connection, service)`, failure mode applies. 0 admits **every** peer on the node's ALPN (as `aster_lease_serializer_memory_new(.., 0, ..)` authorizes everyone). |
| `aster_expose_http_foreign(rt, expose, upcalls, bodies, service_id, opts*)` | Kind 48 admission + kind 49 per request (`C4` -> `C5`), bodies by `stream_id`. `opts` NULL = defaults. Failures become `500` (`ERROR`), `501` (`UNIMPLEMENTED`), `502` (bad `C5`, inline body over the cap), `503`, `504` (timeout of the kind-49 policy; head only). |
| `aster_expose_route_policy_foreign(rt, expose, upcalls)` | `serve_routes` with kind 50 (`C7` -> `C8`); granted routes land in an internal `EdgeRouter` (the C ABI has no public edge listener). |

**Body table: separate `aster_http_bodies_t`, not folded into
`aster_upcalls_t`.** `HttpBodies` is its own Rust object (one per runtime,
shared by every foreign service, independent of any upcall table); handler
threads call it while the poll thread owns the queue; and keeping it out
leaves the whole 4.1 upcall surface untouched. `aster_http_bodies_new(rt, *out)`,
`aster_http_bodies_free`, `aster_http_bodies_open_count(rt, bodies, *u64)`
(leak checks).

`aster_http_foreign_options_t` (48 B): `struct_size` u32 @0 (must equal
`sizeof`), `flags` u32 @4 (0), `inline_request_max` u64 @8, `max_request_body`
u64 @16, `max_response_body` u64 @24, `max_chunk` u64 @32, `response_buffer`
u32 @40, `reserved` u32 @44 (0). Every field 0 = the Rust default (64 KiB / no
cap / no cap / 1 MiB / 8), so "never inline" is not expressible (use 1 to
inline only empty bodies); `aster_http_foreign_options_init(*out)` fills it.

**Body I/O** (`aster_http_body_*`, each takes `rt` first like every other C
call, which the sketch omitted). Returns `aster_http_body_status_t`: `0` OK,
`1..9` = `BodyError::code()` (stable): `UNKNOWN_STREAM` 1, `CANCELLED` 2,
`TOO_LARGE` 3, `CHUNK_TOO_LARGE` 4, `FINISHED` 5, `TIMEOUT` 6, `READ` 7,
`INVALID_TRAILERS` 8, `BLOCKING_ON_RUNTIME` 9. **These do not use
`aster_status_t`** (its 1 and 2 would collide with body codes 1 and 2), so
argument errors are `INVALID_ARGUMENT` -1 (NULL pointer with a length, zero
`cap`) and `UNKNOWN_HANDLE` -2 (unknown runtime or bodies table), detail in
`aster_last_error_message`.

| Function | Notes |
|---|---|
| `aster_http_body_read(rt, bodies, stream_id, buf, cap, timeout_ms, *out_len, *out_eof)` | `read_blocking`; at most `min(cap, max_chunk)` bytes. `*out_eof` = 1 (and `*out_len` 0) ends the body; short reads are normal. `timeout_ms` 0 polls once. A timeout consumes nothing. |
| `aster_http_body_write(rt, bodies, stream_id, ptr, len, timeout_ms)` | `write_blocking`; waits while the buffer is full. A timeout queues nothing. |
| `aster_http_body_try_write(rt, bodies, stream_id, ptr, len, *out_queued)` | `try_write`; `*out_queued` 0 = full, nothing queued. |
| `aster_http_body_finish(rt, bodies, stream_id, trailers, len)` | `trailers` = optional UCv1 `C6` (NULL / 0 = none); a malformed one is `INVALID_TRAILERS` and leaves the body open. |
| `aster_http_body_abort(rt, bodies, stream_id, msg, len)` | Logged, not sent; the relay resets the stream. |

`read` and `write` block and must not run on a tokio runtime thread (a
binding's own handler / poll threads are fine): the Rust `BlockingOnRuntime`
guard returns `BLOCKING_ON_RUNTIME` (9). `try_write`, `finish` and `abort`
never block.

**Consumer side:**

| Function | Notes |
|---|---|
| `aster_expose_relay_request(rt, conn, c4, ud, *op)` | Buffered `relay_request`. The request is a UCv1 `C4` with an `Inline` body (`service_id` names the peer's service; `peer_id` and `stream_id` are ignored); completes with `ASTER_EVENT_BYTES_RESULT`, payload = `C5` (inline body, release with `aster_buffer_release`), for any HTTP status. Transport failure, refusal (authorize deny) or a response that failed mid-body = `ASTER_EVENT_ERROR`. |
| `aster_expose_request_route(rt, conn, aster_route_spec_t *specs, n, metadata, ud, *op)` / `aster_route_registration_free(rt, reg)` | Needed to drive a remote edge's route policy: `BYTES_RESULT`, payload = `C8` (indexes of the granted specs), `handle` = `aster_route_registration_t`, which keeps the routes live. `aster_route_spec_t` (40 B): `host` @0, `service_id` @16, `port` u32 @32, `protocol` u32 @36 (`aster_route_protocol_t`). |

**Choices and gaps (smallest reasonable reading of the sketch):**

- Streaming `relay_request_streaming` is **not** in the C ABI: it needs a
  sender handle for the upload and a frame-reader handle for the response
  (two new handle kinds, frame/trailer events); the buffered call covers the
  need and this fits no existing stream-handle pattern cleanly. Follow-up.
- The raw L4 consumer of an `aster_expose_socket` service (`open_http_relay`
  speaking a protocol raw) is not in the C ABI either; a socket target is
  reachable from other Aster consumers. The tests prove the splice by the
  target's accept.
- The request-side `C4` for `aster_expose_relay_request` is reused instead of
  a new struct, so a binding needs no extra codec; `version` selects the
  `http` version, HEAD / trailers behave as in design §6.
- `Cancelled` from a body call is what a call parked in `write`
  (buffer full) gets when the consumer goes away (the mid-stream test floods
  the buffer to hit that); a call made after the
  relay has already torn the exchange down finds no handle and returns
  `UNKNOWN_STREAM`. Both mean "stop"; the design text promises `Cancelled`
  for "the next write", which the Rust implementation does not always give.
- An `abort` (and a `max_response_body` crossing) reaches a remote consumer
  as a clean truncated finish until the aster-expose abort fix lands
  (design §6: the consumer sees a failed response). The two tests that
  pin the documented behaviour are `#[ignore = "needs expose-abort-fix"]` in
  `ffi/tests/expose.rs`.
- `aster_expose_free` / `aster_http_bodies_free` do not tear down services or
  open exchanges that already hold their own references.

**New enums / constants:** `aster_http_body_status_t`, `aster_route_protocol_t`;
`ASTER_ABI_VERSION_MINOR` = 2. The `aster_upcall_kind_t` comments for 48-50 no
longer say "not issued by the C ABI".

**Bindings:** nothing wired in Java, Go or .NET (they build against the new
header; .NET `RequiredAbiMinor` stays 0, Go compares the library with the
header's `ASTER_ABI_VERSION`). Java / .NET / Go decode `C4` and encode `C5`
against `upcalls.json`, as for the 4.1 kinds.

## New in 4.1 (additive)

ABI 4.1.0 (tranche F5, upcall part) carries Rust's questions to foreign code:
the R6 upcall mechanism (`aster::binding_support::upcall`, design
`docs/_internal/bindings/upcall-design.md` §4) behind a poll/respond pair,
plus the adapters that turn it into lease, fenced-resource and topology
extension points. Nothing in 4.0 changed shape; a 4.0 binding keeps working.
Sources: `ffi/src/upcall.rs`; tests: `ffi/tests/upcalls.rs` (a polling
"foreign" thread answers through the C functions: foreign serializer with
background renewals, release/revoke/fenced watch, foreign authorizer and its
fail-open/fail-closed fallback, timeout cancel + late respond, ERROR /
UNIMPLEMENTED statuses, fenced mutate incl. outcome-unknown, topology
admission filter batching/caching, `enable_shared`, close → shutdown cancels,
queue capacity, argument validation), `ffi/tests/abi_contract_test.c`.

**Upcall table.** One `aster_upcalls_t` per binding runtime wraps an
`Upcalls` table and a bounded `UpcallQueue`. Payloads are UCv1
(`conformance/vectors/upcalls.json`); the binding decodes requests and
encodes responses itself. No function pointers.

| Function | Notes |
|---|---|
| `aster_upcalls_new(rt, capacity, *out)` | `capacity` 0 = `ASTER_UPCALL_DEFAULT_CAPACITY` (1024) queued upcalls; beyond it upcalls are refused (`Overloaded`: decisions take the failure mode, other kinds error). `CANCEL` is always queued. |
| `aster_upcall_poll(rt, u, aster_upcall_event_t *out, max, timeout_ms, uint32_t *out_count) -> int32_t` | Batched drain; `timeout_ms` 0 = non-blocking. Single consumer; blocks the caller. Status: `NOT_FOUND` (unknown runtime or upcalls handle), `INVALID_ARGUMENT` (NULL `out` / `out_count`, `max` 0), `UNSUPPORTED` (tokio thread). Timeout is `OK` with `*out_count` 0. |
| `aster_upcall_respond(rt, u, invocation_id, status, payload, len)` | `aster_upcall_status_t` (`OK` = UCv1 response, `ERROR` = UTF-8 message, `UNIMPLEMENTED`). Any thread, any order, never blocks. `NOT_FOUND` = not pending (answered, cancelled, timed out) -- ignore. |
| `aster_upcall_set_policy(rt, u, kind, timeout_ms, failure_mode)` / `aster_upcall_set_default_policy(rt, u, timeout_ms, failure_mode)` | `failure_mode` = `aster_hook_failure_mode_t`; applies to decision kinds. Default 5 s, fail closed. |
| `aster_upcall_buffer_release(rt, u, buffer)` | Frees an event's payload (0 = no-op). |
| `aster_upcalls_close(rt, u)` / `aster_upcalls_free(rt, u)` | Pending upcalls fail, each gets `CANCEL` (`SHUTDOWN`); queued events stay drainable until free. |

`aster_upcall_event_t` (40 B): `invocation_id` u64 @0, `kind` u16 @8
(`aster_upcall_kind_t`), `cancel_reason` u8 @10 (`aster_upcall_cancel_reason_t`,
`CANCEL` only), `_p0` @11, `payload_len` u32 @12, `deadline_unix_ms` u64 @16,
`payload_ptr` @24, `buffer` u64 @32. Differs from the design sketch in one
place: a `CANCEL` event carries its reason in `cancel_reason` and no payload
(the Rust event's one reason byte), so it needs no buffer release.

**Leases** (`aster_lease_*`):

| Function | Upcalls | Completion |
|---|---|---|
| `aster_lease_serializer_foreign_new(rt, u, *out)` | 1 `LEASE_TRANSITION`, 2 `LEASE_SNAPSHOT` | -- |
| `aster_lease_serializer_memory_new(rt, authorizer_u, *out)` | 3 `LEASE_AUTHORIZE` when `authorizer_u` != 0 (async `AuthorizedSerializer`, no blocking authorizer) | -- |
| `aster_lease_acquire(rt, ser, resource, holder, ttl_ms, renew_interval_ms, holder_margin_ms, ud, *op)` | via the serializer | `ASTER_EVENT_LEASE_ACQUIRED` (107): payload = UCv1 `TransitionOutcome` (`B2`); `handle` = `aster_lease_t` when granted, 0 when denied |
| `aster_lease_fence(rt, lease, *epoch)` | -- | sync; `ALREADY_CLOSED` once the holder must stop acting |
| `aster_lease_status(rt, lease, *status)` / `aster_lease_watch(rt, lease, ud, *op)` | -- | `aster_lease_status_t`; watch completes with `ASTER_EVENT_LEASE_STATUS` (108), `flags` = status, when the lease leaves `HELD` (the "fence-lost event") |
| `aster_lease_release(rt, lease, ud, *op)` / `aster_lease_free(rt, lease)` | RELEASE / none | `UNIT_RESULT`; free stops renewing without releasing |
| `aster_lease_snapshot(rt, ser, resource, ud, *op)` / `aster_lease_revoke(rt, ser, resource, authority, ud, *op)` | via the serializer | `BYTES_RESULT`: `B4` / `B2` |
| `aster_lease_serializer_free(rt, ser)` | -- | leases keep their own reference |

Timing: `ttl_ms` 0 = defaults (30 s / 10 s / 1 s); with a ttl, `renew` 0 =
ttl/3 and `margin` 0 = ttl/10 capped at 1 s; invalid combinations are
`INVALID_ARGUMENT` up front.

**Fenced resource.** `aster_fenced_resource_foreign_new(rt, u, *out)` (kind 4)
and `aster_fenced_mutate(rt, res, fence_resource, epoch, fence_holder, writer,
op, ud, *op)` → `BYTES_RESULT` with UCv1 `FencedMutateResponse` (`B7`: applied
output or fence rejection); any other failure is `ASTER_EVENT_ERROR` and means
**outcome unknown**. `aster_fenced_resource_free`.

**Topology.** `aster_topology_admitted_new(rt, u, ttl_ms, failure_retry_ms,
max_batch, *out)` (kind 16, cached + batched, never blocks a read);
`_check`, `_set`, `_invalidate` (empty id = clear), `_prefetch` (`UNIT_RESULT`),
`_stats` (`aster_topology_admitted_stats_t`, 32 B), `_free`.
`aster_topology_enable_shared(rt, node, secret[32], admitted (0 = none),
bootstrap, position_refresh_ms, ud, *op)` joins the topology v2 swarm with the
filter installed (`UNIT_RESULT`; needs `enable_monitoring`).

**Not exposed (gaps):** placement providers (kinds 17/32/33 -- wired in 4.4
with the coordination client, see "New in 4.4"), expose authorize / HTTP handler / route policy
(48/49/50 -- wired in 4.2, see "New in 4.2"), Gate 0 over upcalls (64/65 -- the 3.2 hook iterators keep serving
admission), and a read side for the shared topology view (clusters /
serialized snapshot -- needs `SharedView` parts from core). No typed C decode
helpers for UCv1: bindings decode against the vectors (Java does,
`site.aster.upcall.Ucv1`).

**New enums / constants:** `aster_upcall_kind_t`, `aster_upcall_status_t`,
`aster_upcall_cancel_reason_t`, `aster_lease_status_t`,
`ASTER_UPCALL_DEFAULT_CAPACITY`, `ASTER_EVENT_LEASE_ACQUIRED` (107),
`ASTER_EVENT_LEASE_STATUS` (108).

**Bindings:** Java wraps all of it (`site.aster.upcall`, `site.aster.lease`;
`IrohLibrary` unchanged at major 4). Go and .NET only build against the new
header (no wrappers yet; .NET `RequiredAbiMinor` stays 0, Go compares the
library with `ASTER_ABI_VERSION` from the header).

## 4.0 (removals)

ABI 4.0.0 removes what 3.2 and 3.5 deprecated, now that Java, .NET and Go are
on `aster_node_start` and the 3.5 dispatch server / payload client. Nothing
was added or reshaped: a 3.5 binding that used none of the functions below
only has to pass `ASTER_ABI_VERSION_MAJOR` (4) to `aster_runtime_new` and
check `aster_abi_version() >> 16 == 4`. Every remaining struct layout is
unchanged.

**Removed:**

| Kind | Name | Use instead |
|---|---|---|
| functions | `aster_reactor_{create,destroy,poll,submit,submit_frame,submit_trailer,recv_frame,check_cancelled,buffer_release,reserve_response_capacity,cancel_response_reservation,accept_non_rpc}` | `aster_server_*` / `aster_dispatch_*` (3.5); non-RPC ALPNs arrive as `ASTER_DISPATCH_EVENT_NON_RPC` |
| types / constants | `aster_reactor_t`, `aster_reactor_call_t` (112 B), `ASTER_REACTOR_EVENT_{CALL,CONNECTION_CLOSED,RESPONSE_WRITABLE,RESPONSE_CLOSED}`, `ASTER_RECV_FRAME_{OK,END_OF_STREAM,TIMEOUT}` | `aster_dispatch_event_t`, `aster_dispatch_event_kind_t` |
| functions | `aster_call_acquire`, `aster_call_acquire_streaming`, `aster_call_send_frame`, `aster_call_recv_frame`, `aster_call_unary` | `aster_rpc_unary`, `aster_rpc_open` + `aster_call_{send_payload,send_payloads,recv_payload,cancel}` (3.5) |
| constants | `ASTER_CALL_RECV_{OK,END_OF_STREAM,TIMEOUT}`, `ASTER_CALL_ERR_{POOL_FULL,QUIC_LIMIT_REACHED,PEER_STREAM_LIMIT_TOO_LOW,STREAM_OPEN_FAILED,POOL_CLOSED}` | payload-client statuses / AEv1 trailer codes |
| functions | `aster_node_memory`, `aster_node_memory_with_alpns`, `aster_node_persistent` | `aster_node_start` with an `aster_config_t` (`alpns`, `storage_path`) |
| functions | `aster_endpoint_create`, `aster_endpoint_close`, `aster_endpoint_free`, `aster_endpoint_id`, `aster_endpoint_export_secret_key`, `aster_endpoint_addr_info` | a node: `aster_node_start`, `aster_node_close`, `aster_node_free`, `aster_node_id`, `aster_node_export_secret_key`, `aster_node_addr_info` |
| struct | `aster_endpoint_config_t` (152 B) | `aster_config_t` (relay mode/URLs, secret key, ALPNs, hooks, bind address, transports, portmapper, proxy, storage) |

There are no bare endpoints any more: a node started with only custom ALPNs is
the replacement.

**Kept:** `aster_call_release` / `aster_call_discard` (free an `aster_rpc_open`
call; an unfinished one is cancelled -- the two are now identical) and
`aster_call_buffer_release` (leases from `aster_rpc_unary` /
`aster_call_recv_payload`). `aster_endpoint_remote_info`,
`aster_endpoint_remote_info_list`, `aster_endpoint_transport_metrics`,
`aster_add_node_addr` and `aster_connect` keep their names and take node handles
(`aster_endpoint_t` stays as a typedef for them). `ASTER_EVENT_ENDPOINT_CREATED`
(3) / `ASTER_EVENT_ENDPOINT_CREATE_FAILED` (4) stay in `aster_event_kind_t` as
reserved values and are never emitted.

**Behaviour change:** `aster_accept(rt, node, user_data, out_op)` on a node now
takes the next connection from the node's aster-ALPN accept queue (the queue
`aster_node_accept_aster` reads; connections on the node's configured ALPNs
that passed Gate 0) and emits `ASTER_EVENT_CONNECTION_ACCEPTED` (`handle` =
connection, `related` = node, no payload). Before 4.0 it called the endpoint's
accept directly and raced the node's router. Unknown handle -> `NOT_FOUND`.

**Bindings:** Java (`IrohLibrary.EXPECTED_ABI_MAJOR`), .NET
(`Native.AbiVersionMajor`) and Go (`C.ASTER_ABI_VERSION_MAJOR` from the header)
expect 4. Java and Go now wait in `aster_dispatch_recv` /
`aster_call_recv_payload` with finite timeouts and check their own cancellation
between slices instead of `ASTER_TIMEOUT_INFINITE`.

Sources: `ffi/src/call.rs` (what is left of the call family), `ffi/src/lib.rs`
(`aster_accept`); `ffi/src/reactor.rs` and its tests (`reactor_test.rs`,
`reactor_cancel_lifecycle.rs`, `cancel_state_cleanup.rs`) were deleted. Tests:
`ffi/tests/node_start.rs` (`accept_on_node_reads_the_alpn_queue`),
`ffi/tests/abi_contract_test.c`.

## New in 3.5 (additive)

ABI 3.5.0 (tranche F4, additive part) serves bindings' RPC services through
`aster::rpc::Server` (design `docs/_internal/bindings/dynamic-dispatch-design.md`
§4.4, as built) and adds a payload-level client over the pooled
`RpcConnection`. Nothing in 3.0–3.4 changed shape; a 3.4 binding keeps working
(at the time Go and .NET still used `aster_reactor_*` and the framed
`aster_call_*`; both were removed in 4.0). Sources: `ffi/src/server.rs`, `ffi/src/rpc.rs`; tests:
`ffi/tests/dispatch_server.rs` (all four patterns, sessions, peer cancel,
deadlines, `QUEUE_FULL`/reserve, abandon, Gate 3 + attribute bridge, frame
limits, connection close, non-RPC ALPN, argument validation -- every call
through the C functions), unit tests in `server.rs`,
`ffi/tests/abi_contract_test.c`.

**Server.** A binding registers each service as its JSON registration form
(`DynamicService`, design §1.5); Rust decodes headers, routes, runs Gate 3
(`requires`, against an attribute store the binding feeds), keeps sessions per
connection (§7.5 graveyard and cap), enforces deadlines (`min(header, method
default_timeout, handler cap)`) and per-method frame limits (closing the
"no frame-limit resolver" gap of `aster_reactor_create`), serves the baseline
`aster.ops.NodeInfo` (and optionally `aster.net.Topology`), and delivers each
call that passed as one `CALL` event. Calls are answered by `call_id` with
payloads and AEv1 `RpcStatus` bytes -- Rust frames, compresses and encodes.
Response operations never block: `QUEUE_FULL` returns the exact
`required_len`; `aster_dispatch_reserve` it and retry on
`RESPONSE_WRITABLE` (the retry cannot be full again). `call_id`s are allocated
by the server's call table (from 1); a detached call stays there until a
terminal operation, abandon, peer cancel or its deadline.

**k/v list** (metadata, attributes, `aster_call_options_t.metadata`): `u32 LE
count`, then per entry `u32 LE key_len`, key, `u32 LE value_len`, value
(UTF-8); empty = no entries.

| Kind | Name | Notes |
|---|---|---|
| type | `aster_server_builder_t`, `aster_server_t` | `uint64_t` handles |
| enum | `aster_dispatch_event_kind_t` | `CALL`=0, `CANCELLED`=1, `RESPONSE_WRITABLE`=2, `RESPONSE_CLOSED`=3, `SESSION_CLOSED`=4, `CONNECTION_CLOSED`=5, `NON_RPC`=6 |
| enum | `aster_cancel_reason_t` | `PEER`=0 (no trailer), `DEADLINE`=1 (Rust sent `DEADLINE_EXCEEDED`), `INVALID_REQUEST`=2 (Rust sent `INVALID_ARGUMENT`) |
| enum | `aster_method_pattern_t` | `UNARY`=0, `SERVER_STREAM`=1, `CLIENT_STREAM`=2, `BIDI_STREAM`=3 |
| enum | `aster_server_option_t` | `BUILTIN_NODE_INFO`=1 (0/1, default 1), `BUILTIN_TOPOLOGY`=2 (0/1, default 0), `MAX_SESSIONS_PER_CONNECTION`=3 (default 1024), `HANDLER_TIMEOUT_MS`=4 (default 300000; 0 = no cap), `MAX_FOREIGN_INFLIGHT`=5 (per-kind foreign entity bound, default 4096; values below 1 are invalid) |
| macro | `ASTER_DISPATCH_FLAG_FIRST_IS_END` (1), `_NEW_SESSION` (2), `_IROH_AUTHENTICATED` (4), `_HAS_METADATA` (0x100), `_HAS_ATTRIBUTES` (0x200) | `aster_dispatch_event_t.flags` |
| macro | `ASTER_TIMEOUT_INFINITE` (`0xffffffff`) | `timeout_ms` of `aster_dispatch_recv` / `aster_call_recv_payload` |
| struct | `aster_dispatch_event_t` (104 B) | `kind:u8, pattern:u8, reason:u8, _p0, flags:u32, call_id, service_idx:u32, method_idx:u32, session_key, connection_id, connection_handle, deadline_unix_ms:i64, wire_call_id:i32, session_id:u32, request_ptr, request_len:u32, _p2, peer_ptr, peer_len:u32, _p3, buffer`. `request` + `peer` live in one lease, `buffer`. `CALL`: `connection_handle` is borrowed (same for every call of the connection; freed by the server on close). `NON_RPC`: `request` = the ALPN, `connection_handle` is **owned** (free with `aster_connection_free`) |
| struct | `aster_call_options_t` (72 B) | `struct_size, version:i32, service, method, metadata (k/v list), session_id:u32, deadline_ms:u32, serialization_mode:i32 (0 XLANG, 1 NATIVE, 2 ROW), wire_call_id:i32`. The deadline is enforced client-side and sent as whole seconds, rounded up |
| function | `aster_server_builder_new(rt, node, out)` | over a node from `aster_node_start` (`aster/1` among its ALPNs); Gate 0 stays the node's |
| function | `aster_server_add_service_json(builder, json, len, uint32_t* out_idx)` | parsed + validated (`DynamicService::from_json`; unknown keys refused); duplicate `(name, version)` → `INVALID_ARGUMENT`; `out_idx` = `service_idx` |
| function | `aster_server_set_option(builder, key, value)` | unknown key / bad value → `INVALID_ARGUMENT` |
| function | `aster_server_start(rt, builder, ring_capacity, out)` | consumes the builder; ring rounded up to a power of two (0 = 256); a collision with a baseline service → `INVALID_ARGUMENT` |
| function | `aster_server_builder_free(builder)` | for a builder never started |
| function | `aster_server_poll(rt, server, events, max, timeout_ms)` | returns the count; `timeout_ms` 0 = no wait; single consumer; parks on a condvar (no spinning) |
| function | `aster_dispatch_respond(rt, server, call_id, payload, len, status_aev1, status_len, uint64_t* out_required_len)` | terminal data + trailer in one write (unary / client-stream); NULL status = OK; `QUEUE_FULL` / `NOT_FOUND` (call over) / `ALREADY_CLOSED` (connection gone) / `INVALID_ARGUMENT` (malformed status, too large) |
| function | `aster_dispatch_send(…, call_id, payload, len, out_required_len)` | one data frame; payload non-empty |
| function | `aster_dispatch_finish(…, call_id, status_aev1, len, out_required_len)` | trailer; NULL = OK |
| function | `aster_dispatch_reserve(…, call_id, required_len)` / `aster_dispatch_cancel_reservation(…, call_id)` | one reservation per call (a second → `INVALID_ARGUMENT`) |
| function | `aster_dispatch_recv(…, call_id, timeout_ms, out_ptr, out_len, uint64_t* out_buffer)` | next request payload, decompressed: `OK` + buffer ≠ 0 = payload, `OK` + buffer 0 = end of stream, `TIMEOUT`, `NOT_FOUND` = call over. Blocks |
| function | `aster_dispatch_is_cancelled(…, call_id)` | 0 live; 1 cancelled, finished or unknown |
| function | `aster_dispatch_metadata` / `aster_dispatch_attributes(…, call_id, out_ptr, out_len, out_buffer)` | k/v lists (attributes = what Gate 3 saw, sorted) |
| function | `aster_dispatch_abandon(…, call_id)` | `INTERNAL "handler dropped call"` unless already ended (`NOT_FOUND`) |
| function | `aster_dispatch_buffer_release(rt, server, buffer)` | event / recv / metadata leases; 0 = no-op |
| function | `aster_server_set_peer_attributes(server, peer, len, kv, kv_len)` | attribute bridge (design §2.6): what Gate 3 sees for a peer; empty clears |
| function | `aster_server_live_calls(server)` | diagnostics |
| function | `aster_server_destroy(rt, server)` | stops serving; queued events' calls are abandoned; does not close the node |
| function | `aster_dynamic_service_contract_id(json, len, aster_bytes_t* out)` | `contract_id` of a JSON registration form (free with `aster_string_release`) |
| function | `aster_rpc_unary(rt, conn, const aster_call_options_t*, req, len, out_resp_ptr, out_resp_len, out_resp_buffer, int32_t* out_status_code, out_status_ptr, out_status_len, out_status_buffer)` | pooled; one write of header + request. Returns `OK` once the call is over: code 0 + response, or the failure's code + AEv1 status (deadline → `DEADLINE_EXCEEDED`, lost connection / pool timeout → `UNAVAILABLE`). Buffers released with `aster_call_buffer_release` |
| function | `aster_rpc_open(rt, conn, uint8_t pattern, opts, aster_call_t* out)` | server-, client- or bidi-stream; server-stream sends header + request together on the first send |
| function | `aster_call_send_payload(rt, call, payload, len, uint8_t end_stream)` | empty + end = finish; `ALREADY_CLOSED` when the call is over (the trailer says why) |
| function | `aster_call_send_payloads(rt, call, const aster_bytes_t* payloads, count, end_stream)` | client-stream / bidi: several payloads framed into as few writes as possible (64 KiB batches) |
| function | `aster_call_recv_payload(rt, call, timeout_ms, out_ptr, out_len, out_buffer, uint8_t* out_is_trailer)` | payload, or the trailer = AEv1 `RpcStatus` (code 0 on success; client-side failures included); `TIMEOUT` resumes the same read; after the trailer `ALREADY_CLOSED` |
| function | `aster_call_cancel(rt, call)` | `FLAG_CANCEL` + reset; a blocked recv returns `CANCELLED` |
| function | `aster_connection_next_session_id(rt, conn, uint32_t* out)` | from the connection's shared core counter (Rust and binding clients never collide) |

Behaviour change (no signature change): `aster_call_release` / `aster_call_discard`
also free `aster_rpc_open` calls (an unfinished one is cancelled).

**Deprecated in 3.5, removed in 4.0:**
`aster_reactor_{create,destroy,poll,submit,submit_frame,submit_trailer,recv_frame,check_cancelled,buffer_release,reserve_response_capacity,cancel_response_reservation,accept_non_rpc}`,
`aster_reactor_call_t`, and the framed `aster_call_{acquire,acquire_streaming,send_frame,recv_frame,unary}`.

## New in 3.4 (additive)

**Attestation rows below were removed in 5.0** (see "5.0 (removals)").

ABI 3.4.0 (rest of tranche F2) exports the pure functions: typed ids, HPKE,
ownership attestations and sealed grants. Nothing in 3.0–3.3 changed shape; a
3.3 binding keeps working. Sources: `ffi/src/pure.rs`, `ffi/src/grants.rs`;
tests: unit tests in `pure.rs`, `ffi/tests/grant_vectors.rs` (reproduces
`conformance/vectors/grants.json` through the C functions),
`ffi/tests/abi_contract_test.c`.

All functions are synchronous and need no runtime. Raw keys / ids are 32-byte
buffers (`const uint8_t*` in, `uint8_t*` out). Variable inputs are
`aster_bytes_t` by value (`{NULL,0}` = empty); variable outputs are
library-owned `aster_bytes_t` freed with `aster_string_release` (`{NULL,0}`
when empty or on failure). A namespace capability is `(access, key)`:
`ASTER_LOCAL_ACCESS_WRITE` + namespace secret, or `ASTER_LOCAL_ACCESS_READ` +
namespace id.

| Kind | Name | Notes |
|---|---|---|
| enum-value | `ASTER_STATUS_VERIFICATION_FAILED` = 13 | well-formed input failed a security check: envelope does not open, chain does not verify, role ≠ capability. Malformed input stays `INVALID_ARGUMENT` |
| enum | `aster_id_kind_t` | `NODE_ID`=0, `PUBLIC_KEY`=1, `SECRET_KEY`=2, `HASH`=3, `NAMESPACE_ID`=4, `NAMESPACE_SECRET`=5, `AUTHOR_ID`=6 |
| enum | `aster_attestation_role_t` | `NONE`=0, `NODE`=1, `ANCHOR`=2, `INTERMEDIATE`=3 |
| enum | `aster_grant_state_t` | `ACTIVE`=0, `REVOKED`=1 |
| enum | `aster_grant_outcome_kind_t` | `ACTIVE`=0, `REVOKED`=1, `UNOPENABLE`=2, `ABSENT`=3 |
| macro | `ASTER_KEY_SIZE` (32), `ASTER_ID_HEX_LEN` (64) | |
| struct | `aster_attest_options_t` (32 B) | `struct_size, reserved, epoch:i64, not_before:i64, not_after:i64` |
| struct | `aster_verified_chain_t` (96 B) | `struct_size, depth, node[32], anchor[32], intermediates (count × 32 bytes), intermediates_count, storage` |
| struct | `aster_grant_context_t` (104 B) | `struct_size, reserved, app, granter, recipient, resource, path, role` (granter / recipient = node id hex, bound verbatim) |
| struct | `aster_grant_record_t` (72 B) | `struct_size, state, context_version:i32, reserved, role, envelope, reason, storage` |
| struct | `aster_grant_outcome_t` (72 B) | `struct_size, kind, access, drops_capability, payload, reason, role, storage` |
| function | `aster_id_parse(kind, text, out[32])` | exactly 64 hex chars, either case; Ed25519 kinds (`NODE_ID`, `PUBLIC_KEY`, `AUTHOR_ID`) must be a curve point |
| function | `aster_id_format(kind, id[32], out[65])` | lowercase hex + NUL; does not validate |
| function | `aster_id_validate(kind, id[32])` | OK / `INVALID_ARGUMENT` |
| function | `aster_secret_key_public_key(secret[32], out[32])` | Ed25519 public key = node id |
| function | `aster_namespace_secret_id(secret[32], out[32])` | `aster::NamespaceSecret::id` |
| function | `aster_hpke_generate_keypair(out_private[32], out_public[32])`, `aster_hpke_public_key_from_private` | X25519 |
| function | `aster_hpke_seal(public[32], aad, plaintext, out)` / `aster_hpke_open(private[32], aad, envelope, out)` | envelope = Fory `aster.hpke.Envelope`; open failure → `VERIFICATION_FAILED` |
| function | `aster_hpke_x25519_secret_from_identity`, `aster_hpke_x25519_public_from_identity`, `aster_hpke_x25519_public_from_node_id(node_id hex, out)` | libsodium-compatible Ed25519→X25519; bad public key → `INVALID_ARGUMENT` |
| function | `aster_attest_root_node(root_secret, node_secret, options or NULL, out)` | chain wire bytes |
| function | `aster_attestation_chain_to_text` / `_from_text` | `aster.attestation.chain.v1:<base64url>` |
| function | `aster_attestation_verify_chain(chain, anchors (count × 32), count, expected_node, aster_verified_chain_t*)` | any failed check → `VERIFICATION_FAILED`; release with `aster_verified_chain_release` |
| function | `aster_verified_chain_role_of(chain, id, uint32_t* role)` | node wins over anchor if equal |
| function | `aster_grant_context_aad(ctx, out)` | domain `aster/sealed-grant/v1` + 6 fields, each u32-BE length-prefixed |
| function | `aster_grant_key(prefix, recipient, out)` | `<prefix minus trailing '/'>/grants/<recipient>` |
| function | `aster_grant_check_role(role, can_write)` | OK / `VERIFICATION_FAILED` |
| function | `aster_grant_seal(ctx, plaintext, out)`, `aster_grant_seal_to_key(public[32], ctx, plaintext, out)` | identity-derived or explicit X25519 key |
| function | `aster_grant_open(identity_secret, ctx, envelope, out)`, `aster_grant_open_with_key(x25519_private, ctx, envelope, out)` | any mismatch → `VERIFICATION_FAILED` |
| function | `aster_namespace_capability_encode(access, key, out)` / `_decode(bytes, uint32_t* access, key_out[32])` | Fory capability bytes |
| function | `aster_namespace_grant_seal(ctx, access, key, out)` / `aster_namespace_grant_open(identity_secret, ctx, envelope, uint32_t* access, key_out[32])` | role rule checked on both sides |
| function | `aster_grant_record_encode(const aster_grant_record_t*, out)` / `_decode(bytes, aster_grant_record_t*)` / `_release` | read `context_version` before choosing the context |
| function | `aster_grant_reconcile(identity_secret, ctx, const aster_bytes_t* record or NULL, aster_grant_outcome_t*)`, `aster_namespace_grant_reconcile(...)` | every outcome returns OK; NULL record = ABSENT; namespace ACTIVE = `access` + 32-byte key in `payload`; release with `aster_grant_outcome_release` (zeroes the payload) |

## New in 3.3 (additive)

ABI 3.3.0 (tranche F3) brings the data plane to parity with the Rust SDK's
`aster::Blobs`, `aster::Docs`/`Doc` and `aster::Connection`/`SendStream`.
Nothing in 3.0–3.2 changed shape; a 3.2 binding keeps working. Source:
`ffi/src/data.rs`; tests: `ffi/src/data.rs` (encoding, path lowering,
fingerprint), `ffi/tests/data_plane.rs` (two loopback nodes),
`ffi/tests/abi_contract_test.c`.

Async functions take `(…, user_data, aster_operation_t* out_operation)`, finish
on the completion queue, and return `ASTER_STATUS_INVALID_ARGUMENT` (nothing
started) for malformed ids, unknown enum values / flag bits, or a NULL
`out_operation`; unknown handles return `ASTER_STATUS_NOT_FOUND`. Ids are 64-hex
strings unless noted. Failures arrive as `ASTER_EVENT_ERROR` with the message
as payload.

| Kind | Name | Notes |
|---|---|---|
| enum | `aster_blob_format_t` | `RAW`=0, `HASH_SEQ`=1 |
| enum | `aster_fetch_strategy_t` | `WHOLE`=0, `SPLIT_CHILDREN`=1 (needs `HASH_SEQ`; provider order applies per child) |
| enum | `aster_blob_status_t` | `NOT_FOUND`=0, `PARTIAL`=1, `COMPLETE`=2 (same codes as `aster_blobs_status`) |
| enum | `aster_local_access_t` | `WRITE`=1, `READ`=2 (= the fingerprint's access byte) |
| enum | `aster_retire_outcome_t` | `RETIRED`=0, `BUSY`=1 |
| enum-value | `ASTER_PATH_KIND_OTHER` = 3 | custom/future transport path |
| enum-value | `ASTER_EVENT_BLOB_FETCHED`=100, `_DOC_AUTHORITY_REPORT`=101, `_DOC_RETIRE_OUTCOME`=102, `_DOC_OPENED`=103, `_DOC_ENTRY_EXISTS`=104, `_DOC_CONTENT_STATUS`=105, `_STREAM_PRIORITY`=106 | |
| macro | `ASTER_FETCH_TO_STORE` (1) | `flags` bit: keep the blob in the store, return no bytes |
| macro | `ASTER_FETCH_REPORT_VERSION` (1), `ASTER_FETCH_REPORT_HEADER_SIZE` (16), `ASTER_FETCH_PROVIDER_RECORD_SIZE` (44), `ASTER_AUTHORITY_FINGERPRINT_SIZE` (32) | |
| struct | `aster_path_info_t` (72 B) | `struct_size, path_kind, is_selected, peer_port:u16, _pad:u16, peer_host, relay_url, other, rtt_micros` (`UINT64_MAX` = unmeasured); strings live in the list's storage |
| struct | `aster_path_list_t` (32 B) | `struct_size, _pad, items, len, storage` |
| function | `aster_blobs_download_hash_from(rt, node, hash, aster_bytes_list_t providers, format, strategy, flags, …)` | `aster::Blobs::download_hash[_to_store]_from`; ordered failover; → `ASTER_EVENT_BLOB_FETCHED` (`flags` = provider count), payload = fetch report v1 + blob bytes (none with `TO_STORE`) |
| function | `aster_blobs_download_hash(rt, node, hash, provider, format, flags, …)` | single provider; → `ASTER_EVENT_BLOB_DOWNLOADED` with bytes (empty with `TO_STORE`) |
| function | `aster_blobs_read_range(rt, node, hash, offset, len, …)` | → `ASTER_EVENT_BLOB_READ` |
| function | `aster_tags_delete_prefix(rt, node, prefix, …)` | → `ASTER_EVENT_TAG_DELETED`, payload u64 count, `flags` = count (saturated); empty prefix = all tags |
| function | `aster_blobs_gc_run_once(rt, node, …)` | node-wide GC pass → `ASTER_EVENT_UNIT_RESULT` |
| function | `aster_docs_authority_report(rt, node, namespace_id, …)` | → `ASTER_EVENT_DOC_AUTHORITY_REPORT`: `flags` = access, payload = namespace (32) + fingerprint (32); absent namespace = `ASTER_EVENT_ERROR` |
| function | `aster_doc_authority_report(rt, node, doc, …)` | same, namespace taken from the open doc; `event.handle` = doc |
| function | `aster_doc_authority_fingerprint(namespace_id, access, uint8_t out[32])` | sync, pure (`aster::AuthorityFingerprint::of`) |
| function | `aster_docs_retire_local_write_authority(rt, node, namespace_id, …)` | → `ASTER_EVENT_DOC_RETIRE_OUTCOME`: `flags` = outcome, payload = u32 access, u32 0, u64 open references. BUSY is a success event |
| function | `aster_doc_shutdown_for_role_swap(rt, doc, budget_ms, …)` | frees the handle now; leave + close under one deadline → `ASTER_EVENT_UNIT_RESULT` |
| function | `aster_docs_open(rt, node, namespace_id, …)` | → `ASTER_EVENT_DOC_OPENED` (`handle` = doc, `related` = node); absent → same kind, status `NOT_FOUND`, handle 0 |
| function | `aster_docs_import_write_namespace(rt, node, secret (32 raw bytes), …)` / `aster_docs_import_read_namespace(rt, node, namespace_id, …)` | open-or-import; → `ASTER_EVENT_DOC_OPENED` |
| function | `aster_doc_set_bytes_durable(rt, doc, author, key, value, …)` | → `ASTER_EVENT_DOC_SET` (payload = content hash hex) |
| function | `aster_doc_entry_exists(rt, doc, author, key, …)` | tombstones count → `ASTER_EVENT_DOC_ENTRY_EXISTS`, `flags` 1/0 |
| function | `aster_doc_entry_content_status(rt, doc, content_hash, …)` | → `ASTER_EVENT_DOC_CONTENT_STATUS`: `flags` = blob status, payload = u64 size |
| function | `aster_connection_paths(rt, conn, aster_path_list_t*)` / `aster_connection_selected_path(…)` | sync; selected = 0 or 1 entries; release with `aster_path_list_release` |
| function | `aster_path_list_release(aster_path_list_t*)` | frees storage, zeroes; NULL / released = no-op |
| function | `aster_send_stream_set_priority(rt, stream, int32_t, …)` / `aster_send_stream_priority(rt, stream, …)` | → `ASTER_EVENT_UNIT_RESULT` / `ASTER_EVENT_STREAM_PRIORITY` (payload i32 LE) |

**Fetch report v1** (little-endian): `u8 version (1)`, 3 reserved bytes, `u32 N`
providers, `u64 bytes_transferred`, then N 44-byte records in first-attempt
order: 32-byte raw node id, `u32 attempts`, `u32 failures`,
`u32 completed_requests`. Blob bytes follow the last record. A provider can be
both failed and served (e.g. across split children). Chosen over a decoded
`#[repr(C)]` struct because the result already travels as one event payload:
no second allocation or release call, and every record is fixed-size.

Behaviour change (no signature change): `aster_add_node_addr` now accepts a
node handle as well as an endpoint handle.

Not exported (Rust-only or not yet needed): `download_collection_from` (use
`aster_blobs_list_collection` after a `HASH_SEQ` fetch), `Docs::default_author`,
`read_entry_contents_if_complete`.

## New in 3.2 (additive)

ABI 3.2.0 puts the C ABI on the `aster` crate (plan D1, tranche F1): nodes start
from an `aster::AsterConfig` through `aster::Node::start`, and Gate 0 /
admission decisions come from the Rust `GatePolicy`. Nothing in 3.0/3.1 changed
shape; a 3.1 binding keeps working. Source: `ffi/src/node.rs`; tests:
`ffi/src/node.rs` (config round-trip / env / TOML), `ffi/tests/node_start.rs`,
`ffi/tests/abi_contract_test.c`.

| Kind | Name | Notes |
|---|---|---|
| struct | `aster_config_t` (280 B) | `struct_size, relay_mode, relay_urls, secret_key (32 raw bytes or empty), secret_key_file, storage_path, gc_interval_ms, bind_addr, alpns, identity_file, proxy_url, hook_timeout_ms (0 = 5000), hook_failure_mode, enable_hooks, enable_monitoring, local_discovery, clear_ip_transports, clear_relay_transports, portmapper, proxy_from_env, transport_set, transport_initial_mtu, transport_send_fairness, transport_enable_segmentation_offload, transport_max_concurrent_{bidi,uni}_streams, transport_{stream_receive,receive,send}_window, transport_max_idle_timeout_ms, transport_keep_alive_interval_ms, transport_datagram_{receive,send}_buffer_size, storage` |
| enum | `aster_portmapper_t` | `DEFAULT`=0 (unset), `ENABLED`=1, `DISABLED`=2 |
| enum | `aster_trust_mode_t` | `PROTECTED`=0 (default), `OPEN_DEV`=1 |
| macro | `ASTER_TRANSPORT_*` (bits 0..11), `ASTER_TRANSPORT_ALL` | presence bits of `aster_config_t.transport_set`; unknown bits rejected |
| type | `aster_gate_t` | `uint64_t` handle, runtime-scoped |
| function | `aster_config_init(aster_config_t*)` | Rust defaults; never allocates (`storage == NULL`) |
| function | `aster_config_from_env(aster_config_t*)` | `AsterConfig::from_env`; release with `aster_config_release` |
| function | `aster_config_from_toml(aster_bytes_t path, aster_config_t*)` | `AsterConfig::from_file` (file, then `ASTER_*` env); release with `aster_config_release` |
| function | `aster_config_release(aster_config_t*)` | frees `storage`, zeroes pointers; no-op when `storage == NULL` |
| function | `aster_node_start(rt, const aster_config_t*, aster_gate_t gate, user_data, out_op)` | async → `ASTER_EVENT_NODE_CREATED` / `ASTER_EVENT_ERROR`; config validated + copied before return (bad config → `INVALID_ARGUMENT`, nothing started); `gate` ≠ 0 forces hooks on |
| function | `aster_node_set_gate(rt, node, gate)` | attach/detach (0); `UNSUPPORTED` for nodes without hooks |
| function | ~~`aster_node_metrics_prometheus(rt, node, aster_bytes_t* out)`~~ | **removed in 4.6**: use `aster_metrics_prometheus` |
| function | `aster_gate_new`, `aster_gate_free`, `aster_gate_set_trust_mode`, `aster_gate_set_admission_alpns`, `aster_gate_set_reject(code, reason)`, `aster_gate_admit`, `aster_gate_revoke`, `aster_gate_is_admitted`, `aster_gate_should_allow` | peers are 64-hex node ids (case-insensitive) |
| function | `aster_admission_respond(rt, invocation, decision, reject_code, reason)` | answers `ASTER_EVENT_HOOK_{BEFORE,AFTER}_CONNECT` from nodes and endpoints; DENY of an inbound invocation closes with `reject_code`/`reason` |

Admission events on nodes: a node started with `enable_hooks` and no gate raises
the same `ASTER_EVENT_HOOK_BEFORE_CONNECT` / `ASTER_EVENT_HOOK_AFTER_CONNECT`
events as an endpoint (`event.handle` = node, `event.related` = invocation,
`event.user_data` = the start call's `user_data`, same payload encoding). With a
gate attached, inbound peers are decided in Rust (admitted / admission ALPN /
`OPEN_DEV` → accept, else close with the gate's code + reason) and outbound
dials are allowed, without events.

Behaviour fixes in 3.2 (no signature change): `aster_node_addr_info` now fills
`direct_addresses` (it always returned an empty list); `aster_endpoint_addr_info`
returns `INVALID_ARGUMENT` for a NULL scratch buffer. Handle ids are now unique across
all handle kinds within the process (one counter), so a function taking "endpoint
or node" can no longer confuse an endpoint with a node of the same number.

**Deprecated in 3.2, removed in 4.0:** `aster_node_memory`, `aster_node_memory_with_alpns`,
`aster_node_persistent`, `aster_endpoint_create` (and with it the
endpoint-only `aster_endpoint_*` functions and `aster_endpoint_config_t`). Use
`aster_node_start`; `aster_connect`, `aster_accept`, remote info and transport
metrics already take node handles.

## New in 3.1 (additive)

ABI 3.1.0 adds the AEv1 RPC envelope codec (Aster-SPEC §6.9) and zstd payload
compression (§5.6) from `aster_transport_core`, so FFI bindings stop encoding
the envelope with their own Fory. Nothing in 3.0 changed; a 3.0 binding keeps
working against a 3.1 library. Source: `ffi/src/envelope.rs`; vectors checked
by `ffi/tests/envelope_vectors.rs`.

| Kind | Name | Notes |
|---|---|---|
| struct | `aster_stream_header_t` (96 B) | `struct_size, version, call_id, session_id, deadline:i16, serialization_mode:i8, reserved0:u8, reserved1:u32, service, method, metadata_keys, metadata_values, storage` |
| struct | `aster_call_header_t` (72 B) | `struct_size, call_id, deadline:i16, reserved0:u16, reserved1:u32, method, metadata_keys, metadata_values, storage` |
| struct | `aster_rpc_status_t` (64 B) | `struct_size, code, message, detail_keys, detail_values, storage` |
| function | `aster_{stream_header,call_header,rpc_status}_encode(const T*, aster_bytes_t* out)` | caller sets `struct_size`; `out` freed with `aster_string_release` |
| function | `aster_{stream_header,call_header,rpc_status}_decode(const uint8_t*, size_t, T* out)` | library writes `struct_size` and `storage`; strings/lists point into `storage`; empty = `{NULL,0}`; not NUL-terminated |
| function | `aster_{stream_header,call_header,rpc_status}_release(T*)` | frees `storage`, zeroes pointers; no-op when `storage == NULL` |
| function | `aster_zstd_maybe_compress(data, len, threshold, aster_bytes_t* out)` | `out == {NULL,0}` means send uncompressed |
| function | `aster_zstd_decompress(data, len, aster_bytes_t* out)` | 16 MiB cap; corrupt / over-limit -> `ASTER_STATUS_INVALID_ARGUMENT` |
| macro | `ASTER_COMPRESSION_THRESHOLD` (4096), `ASTER_MAX_DECOMPRESSED_SIZE` (16 MiB), `ASTER_ENVELOPE_TAG_{STREAM_HEADER,CALL_HEADER,RPC_STATUS}` (0xA1..0xA3), `ASTER_ENVELOPE_VERSION` (1) | |

Errors: malformed / non-canonical input, wrong `struct_size`, non-UTF-8
strings and key/value length mismatches all return
`ASTER_STATUS_INVALID_ARGUMENT` with `aster_last_error_message` set.

## New in 3.0

| Kind | Name | Notes |
|---|---|---|
| function | `aster_abi_version` | packed version |
| macro | `ASTER_ABI_VERSION` | packed version of the header |
| enum-value | `ASTER_STATUS_ABI_MISMATCH` = 12 | |
| enum-value | `ASTER_RELAY_MODE_STAGING` = 3 | |
| type | `aster_connection_type_t`: `ASTER_CONNECTION_TYPE_NOT_CONNECTED`=0, `ASTER_CONNECTION_TYPE_CONNECTING`=1, `ASTER_CONNECTION_TYPE_UDP_DIRECT`=2, `ASTER_CONNECTION_TYPE_UDP_RELAY`=3, `ASTER_CONNECTION_TYPE_OTHER`=4 | `connection_type` of `aster_remote_info_t` / `aster_connection_info_t` |
| type | `aster_path_kind_t`: `ASTER_PATH_KIND_NONE`=0, `ASTER_PATH_KIND_DIRECT`=1, `ASTER_PATH_KIND_RELAY`=2 | `aster_transport_snapshot_t.path_kind` |
| type | `aster_doc_share_mode_t`: `ASTER_DOC_SHARE_READ`=0, `ASTER_DOC_SHARE_WRITE`=1 | `mode` of `aster_doc_share*` |
| type | `aster_registry_key_kind_t`: `ASTER_REGISTRY_KEY_CONTRACT`=0, `_VERSION`=1, `_CHANNEL`=2, `_LEASE`=3, `_LEASE_PREFIX`=4, `_ACL`=5 | `kind` of `aster_registry_key` |

All ABI enums are now emitted in the header (previously `iroh_event_kind_t`,
`iroh_relay_mode_t`, `iroh_doc_query_mode_t`, `iroh_download_policy_mode_t`
were missing): `aster_status_t`, `aster_event_kind_t`, `aster_relay_mode_t`,
`aster_hook_failure_mode_t`, `aster_hook_decision_t`, `aster_connection_type_t`,
`aster_path_kind_t`, `aster_doc_query_mode_t`, `aster_download_policy_mode_t`,
`aster_doc_share_mode_t`, `aster_registry_key_kind_t`. Struct fields and
arguments still carry them as `uint32_t` (C enum width is not ABI-stable
across compilers and an out-of-range value would be UB in Rust).

Struct layouts are **unchanged** (pinned in `ffi/src/layout.rs` and
`ffi/tests/abi_contract_test.c`; LP64): `aster_event_t` 80,
`aster_runtime_config_t` 16, `aster_endpoint_config_t` 152,
`aster_connect_config_t` 48, `aster_node_addr_t` 48, `aster_bytes_t` 16,
`aster_bytes_list_t` 16, `aster_remote_info_t` 72, `aster_connection_info_t` 56,
`aster_transport_snapshot_t` 56, `aster_transport_metrics_t` 136,
`aster_reactor_call_t` 112.

Symbols that were already `aster_*` / `ASTER_*` (reactor, call pool,
contract, ticket, registry, canonical/signing, `ASTER_CALL_*`,
`ASTER_RECV_FRAME_*`) are unchanged, except the `ASTER_EVENT_KIND_*` row above.

## Full rename table

One row per symbol; columns `kind | old | new | note`. Mechanical rule:
`iroh_` -> `aster_`, `IROH_` -> `ASTER_`, plus the rows below the first rule.

| kind | old | new | note |
|---|---|---|---|
| macro | `ASTER_EVENT_KIND_CALL` | `ASTER_REACTOR_EVENT_CALL` | reactor rename |
| macro | `ASTER_EVENT_KIND_CONNECTION_CLOSED` | `ASTER_REACTOR_EVENT_CONNECTION_CLOSED` | reactor rename |
| macro | `ASTER_EVENT_KIND_RESPONSE_WRITABLE` | `ASTER_REACTOR_EVENT_RESPONSE_WRITABLE` | reactor rename |
| macro | `ASTER_EVENT_KIND_RESPONSE_CLOSED` | `ASTER_REACTOR_EVENT_RESPONSE_CLOSED` | reactor rename |
| function | `iroh_accept` | `aster_accept` |  |
| function | `iroh_accept_bi` | `aster_accept_bi` |  |
| function | `iroh_accept_uni` | `aster_accept_uni` |  |
| function | `iroh_add_node_addr` | `aster_add_node_addr` |  |
| function | `iroh_blobs_add_bytes` | `aster_blobs_add_bytes` |  |
| function | `iroh_blobs_add_bytes_as_collection` | `aster_blobs_add_bytes_as_collection` |  |
| function | `iroh_blobs_add_collection` | `aster_blobs_add_collection` |  |
| function | `iroh_blobs_add_path` | `aster_blobs_add_path` |  |
| function | `iroh_blobs_add_path_with_named_tag` | `aster_blobs_add_path_with_named_tag` |  |
| function | `iroh_blobs_create_collection_ticket` | `aster_blobs_create_collection_ticket` |  |
| function | `iroh_blobs_create_ticket` | `aster_blobs_create_ticket` |  |
| function | `iroh_blobs_download` | `aster_blobs_download` |  |
| function | `iroh_blobs_has` | `aster_blobs_has` |  |
| function | `iroh_blobs_list_collection` | `aster_blobs_list_collection` |  |
| function | `iroh_blobs_local_info` | `aster_blobs_local_info` |  |
| function | `iroh_blobs_observe_complete` | `aster_blobs_observe_complete` |  |
| function | `iroh_blobs_observe_snapshot` | `aster_blobs_observe_snapshot` |  |
| function | `iroh_blobs_read` | `aster_blobs_read` |  |
| function | `iroh_blobs_status` | `aster_blobs_status` |  |
| function | `iroh_buffer_release` | `aster_buffer_release` |  |
| function | `iroh_connect` | `aster_connect` |  |
| function | `iroh_connection_authorize_tunnel_tcp` | `aster_connection_authorize_tunnel_tcp` |  |
| function | `iroh_connection_close` | `aster_connection_close` |  |
| function | `iroh_connection_closed` | `aster_connection_closed` |  |
| function | `iroh_connection_datagram_send_buffer_space` | `aster_connection_datagram_send_buffer_space` |  |
| function | `iroh_connection_free` | `aster_connection_free` |  |
| function | `iroh_connection_info` | `aster_connection_info` |  |
| function | `iroh_connection_max_datagram_size` | `aster_connection_max_datagram_size` |  |
| function | `iroh_connection_open_tunnel` | `aster_connection_open_tunnel` |  |
| function | `iroh_connection_read_datagram` | `aster_connection_read_datagram` |  |
| function | `iroh_connection_remote_id` | `aster_connection_remote_id` |  |
| function | `iroh_connection_send_datagram` | `aster_connection_send_datagram` |  |
| function | `iroh_connection_transport_snapshot` | `aster_connection_transport_snapshot` |  |
| function | `iroh_doc_del` | `aster_doc_del` |  |
| function | `iroh_doc_event_recv` | `aster_doc_event_recv` |  |
| function | `iroh_doc_free` | `aster_doc_free` |  |
| function | `iroh_doc_get_exact` | `aster_doc_get_exact` |  |
| function | `iroh_doc_id` | `aster_doc_id` |  |
| function | `iroh_doc_leave` | `aster_doc_leave` |  |
| function | `iroh_doc_query` | `aster_doc_query` |  |
| function | `iroh_doc_read_entry_content` | `aster_doc_read_entry_content` |  |
| function | `iroh_doc_set_bytes` | `aster_doc_set_bytes` |  |
| function | `iroh_doc_set_download_policy` | `aster_doc_set_download_policy` |  |
| function | `iroh_doc_share` | `aster_doc_share` |  |
| function | `iroh_doc_share_with_addr` | `aster_doc_share_with_addr` |  |
| function | `iroh_doc_start_sync` | `aster_doc_start_sync` |  |
| function | `iroh_doc_subscribe` | `aster_doc_subscribe` |  |
| function | `iroh_docs_create` | `aster_docs_create` |  |
| function | `iroh_docs_create_author` | `aster_docs_create_author` |  |
| function | `iroh_docs_join` | `aster_docs_join` |  |
| function | `iroh_docs_join_and_subscribe` | `aster_docs_join_and_subscribe` |  |
| function | `iroh_endpoint_addr_info` | `aster_endpoint_addr_info` |  |
| function | `iroh_endpoint_close` | `aster_endpoint_close` |  |
| function | `iroh_endpoint_create` | `aster_endpoint_create` |  |
| function | `iroh_endpoint_export_secret_key` | `aster_endpoint_export_secret_key` |  |
| function | `iroh_endpoint_free` | `aster_endpoint_free` |  |
| function | `iroh_endpoint_id` | `aster_endpoint_id` |  |
| function | `iroh_endpoint_remote_info` | `aster_endpoint_remote_info` |  |
| function | `iroh_endpoint_remote_info_list` | `aster_endpoint_remote_info_list` |  |
| function | `iroh_endpoint_transport_metrics` | `aster_endpoint_transport_metrics` |  |
| function | `IROH_EVENT_BLOB_OBSERVE_COMPLETE` | `ASTER_EVENT_BLOB_OBSERVE_COMPLETE` |  |
| function | `IROH_EVENT_DOC_SUBSCRIBED` | `ASTER_EVENT_DOC_SUBSCRIBED` |  |
| function | `iroh_gossip_broadcast` | `aster_gossip_broadcast` |  |
| function | `iroh_gossip_recv` | `aster_gossip_recv` |  |
| function | `iroh_gossip_subscribe` | `aster_gossip_subscribe` |  |
| function | `iroh_gossip_topic_free` | `aster_gossip_topic_free` |  |
| function | `iroh_hook_after_connect_respond` | `aster_hook_after_connect_respond` |  |
| function | `iroh_hook_before_connect_respond` | `aster_hook_before_connect_respond` |  |
| function | `IROH_HOOK_DECISION_ALLOW` | `ASTER_HOOK_DECISION_ALLOW` |  |
| function | `IROH_HOOK_DECISION_DENY` | `ASTER_HOOK_DECISION_DENY` |  |
| function | `iroh_last_error_message` | `aster_last_error_message` |  |
| function | `iroh_node_accept_aster` | `aster_node_accept_aster` |  |
| function | `iroh_node_addr_info` | `aster_node_addr_info` |  |
| function | `iroh_node_close` | `aster_node_close` |  |
| function | `iroh_node_export_secret_key` | `aster_node_export_secret_key` |  |
| function | `iroh_node_free` | `aster_node_free` |  |
| function | `iroh_node_id` | `aster_node_id` |  |
| function | `iroh_node_memory` | `aster_node_memory` |  |
| function | `iroh_node_memory_with_alpns` | `aster_node_memory_with_alpns` |  |
| function | `iroh_node_persistent` | `aster_node_persistent` |  |
| function | `iroh_open_bi` | `aster_open_bi` |  |
| function | `iroh_open_uni` | `aster_open_uni` |  |
| function | `iroh_operation_cancel` | `aster_operation_cancel` |  |
| function | `iroh_poll_events` | `aster_poll_events` |  |
| function | `iroh_recv_stream_free` | `aster_recv_stream_free` |  |
| function | `iroh_runtime_close` | `aster_runtime_close` |  |
| function | `iroh_runtime_new` | `aster_runtime_new` |  |
| function | `iroh_secret_key_generate` | `aster_secret_key_generate` |  |
| function | `iroh_send_stream_free` | `aster_send_stream_free` |  |
| function | `iroh_status_name` | `aster_status_name` |  |
| function | `IROH_STATUS_NOT_FOUND` | `ASTER_STATUS_NOT_FOUND` |  |
| function | `iroh_stream_finish` | `aster_stream_finish` |  |
| function | `iroh_stream_read` | `aster_stream_read` |  |
| function | `iroh_stream_read_exact` | `aster_stream_read_exact` |  |
| function | `iroh_stream_read_to_end` | `aster_stream_read_to_end` |  |
| function | `iroh_stream_stop` | `aster_stream_stop` |  |
| function | `iroh_stream_stopped` | `aster_stream_stopped` |  |
| function | `iroh_stream_write` | `aster_stream_write` |  |
| function | `iroh_string_release` | `aster_string_release` |  |
| function | `iroh_tags_delete` | `aster_tags_delete` |  |
| function | `iroh_tags_get` | `aster_tags_get` |  |
| function | `iroh_tags_list_prefix` | `aster_tags_list_prefix` |  |
| function | `iroh_tags_set` | `aster_tags_set` |  |
| type | `iroh_buffer_t` | `aster_buffer_t` |  |
| type | `iroh_bytes_list_t` | `aster_bytes_list_t` |  |
| type | `iroh_bytes_t` | `aster_bytes_t` |  |
| type | `iroh_connect_config_t` | `aster_connect_config_t` |  |
| type | `iroh_connection_info_t` | `aster_connection_info_t` |  |
| type | `iroh_connection_t` | `aster_connection_t` |  |
| type | `iroh_doc_query_mode_t` | `aster_doc_query_mode_t` | was missing from the old header |
| type | `iroh_download_policy_mode_t` | `aster_download_policy_mode_t` | was missing from the old header |
| type | `iroh_endpoint_config_t` | `aster_endpoint_config_t` |  |
| type | `iroh_endpoint_t` | `aster_endpoint_t` |  |
| type | `iroh_event_kind_t` | `aster_event_kind_t` | was missing from the old header |
| type | `iroh_event_t` | `aster_event_t` |  |
| type | `iroh_hook_decision_t` | `aster_hook_decision_t` |  |
| type | `iroh_hook_failure_mode_t` | `aster_hook_failure_mode_t` |  |
| type | `iroh_hook_invocation_t` | `aster_hook_invocation_t` |  |
| type | `iroh_node_addr_t` | `aster_node_addr_t` |  |
| type | `iroh_node_t` | `aster_node_t` |  |
| type | `iroh_operation_t` | `aster_operation_t` |  |
| type | `iroh_recv_stream_t` | `aster_recv_stream_t` |  |
| type | `iroh_relay_mode_t` | `aster_relay_mode_t` | was missing from the old header |
| type | `iroh_remote_info_t` | `aster_remote_info_t` |  |
| type | `iroh_runtime_config_t` | `aster_runtime_config_t` |  |
| type | `iroh_runtime_t` | `aster_runtime_t` |  |
| type | `iroh_send_stream_t` | `aster_send_stream_t` |  |
| type | `iroh_status_t` | `aster_status_t` |  |
| type | `iroh_transport_metrics_t` | `aster_transport_metrics_t` |  |
| type | `iroh_transport_snapshot_t` | `aster_transport_snapshot_t` |  |
| macro | `IROH_ABI_VERSION_MAJOR` | `ASTER_ABI_VERSION_MAJOR` |  |
| macro | `IROH_ABI_VERSION_MINOR` | `ASTER_ABI_VERSION_MINOR` |  |
| macro | `IROH_ABI_VERSION_PATCH` | `ASTER_ABI_VERSION_PATCH` |  |
| enum-value | `IROH_DOC_QUERY_KEY_EXACT` | `ASTER_DOC_QUERY_KEY_EXACT` |  |
| enum-value | `IROH_DOC_QUERY_KEY_PREFIX` | `ASTER_DOC_QUERY_KEY_PREFIX` |  |
| enum-value | `IROH_DOC_QUERY_LATEST_EXACT` | `ASTER_DOC_QUERY_LATEST_EXACT` |  |
| enum-value | `IROH_DOC_QUERY_LATEST_PREFIX` | `ASTER_DOC_QUERY_LATEST_PREFIX` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_EVERYTHING` | `ASTER_DOWNLOAD_POLICY_EVERYTHING` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_EVERYTHING_EXCEPT` | `ASTER_DOWNLOAD_POLICY_EVERYTHING_EXCEPT` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_NOTHING_EXCEPT` | `ASTER_DOWNLOAD_POLICY_NOTHING_EXCEPT` | was missing from the old header |
| enum-value | `IROH_EVENT_ASTER_ACCEPTED` | `ASTER_EVENT_ASTER_ACCEPTED` |  |
| enum-value | `IROH_EVENT_AUTHOR_CREATED` | `ASTER_EVENT_AUTHOR_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_ADDED` | `ASTER_EVENT_BLOB_ADDED` |  |
| enum-value | `IROH_EVENT_BLOB_COLLECTION_ADDED` | `ASTER_EVENT_BLOB_COLLECTION_ADDED` |  |
| enum-value | `IROH_EVENT_BLOB_COLLECTION_TICKET_CREATED` | `ASTER_EVENT_BLOB_COLLECTION_TICKET_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_DOWNLOADED` | `ASTER_EVENT_BLOB_DOWNLOADED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_READ` | `ASTER_EVENT_BLOB_READ` |  |
| enum-value | `IROH_EVENT_BLOB_TICKET_CREATED` | `ASTER_EVENT_BLOB_TICKET_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BYTES_RESULT` | `ASTER_EVENT_BYTES_RESULT` | was missing from the old header |
| enum-value | `IROH_EVENT_CLOSED` | `ASTER_EVENT_CLOSED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECT_FAILED` | `ASTER_EVENT_CONNECT_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTED` | `ASTER_EVENT_CONNECTED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTION_ACCEPTED` | `ASTER_EVENT_CONNECTION_ACCEPTED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTION_CLOSED` | `ASTER_EVENT_CONNECTION_CLOSED` | was missing from the old header |
| enum-value | `IROH_EVENT_DATAGRAM_RECEIVED` | `ASTER_EVENT_DATAGRAM_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_CREATED` | `ASTER_EVENT_DOC_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_DEL` | `ASTER_EVENT_DOC_DEL` |  |
| enum-value | `IROH_EVENT_DOC_EVENT` | `ASTER_EVENT_DOC_EVENT` |  |
| enum-value | `IROH_EVENT_DOC_GET` | `ASTER_EVENT_DOC_GET` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_JOINED` | `ASTER_EVENT_DOC_JOINED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED` | `ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED` |  |
| enum-value | `IROH_EVENT_DOC_QUERY` | `ASTER_EVENT_DOC_QUERY` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_SET` | `ASTER_EVENT_DOC_SET` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_SHARED` | `ASTER_EVENT_DOC_SHARED` |  |
| enum-value | `IROH_EVENT_ENDPOINT_CREATE_FAILED` | `ASTER_EVENT_ENDPOINT_CREATE_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_ENDPOINT_CREATED` | `ASTER_EVENT_ENDPOINT_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_ERROR` | `ASTER_EVENT_ERROR` | was missing from the old header |
| enum-value | `IROH_EVENT_FRAME_RECEIVED` | `ASTER_EVENT_FRAME_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_BROADCAST_DONE` | `ASTER_EVENT_GOSSIP_BROADCAST_DONE` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_LAGGED` | `ASTER_EVENT_GOSSIP_LAGGED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_NEIGHBOR_DOWN` | `ASTER_EVENT_GOSSIP_NEIGHBOR_DOWN` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_NEIGHBOR_UP` | `ASTER_EVENT_GOSSIP_NEIGHBOR_UP` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_RECEIVED` | `ASTER_EVENT_GOSSIP_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_SUBSCRIBED` | `ASTER_EVENT_GOSSIP_SUBSCRIBED` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_AFTER_CONNECT` | `ASTER_EVENT_HOOK_AFTER_CONNECT` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_BEFORE_CONNECT` | `ASTER_EVENT_HOOK_BEFORE_CONNECT` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_INVOCATION_RELEASED` | `ASTER_EVENT_HOOK_INVOCATION_RELEASED` | was missing from the old header |
| enum-value | `IROH_EVENT_NODE_CREATE_FAILED` | `ASTER_EVENT_NODE_CREATE_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_NODE_CREATED` | `ASTER_EVENT_NODE_CREATED` |  |
| enum-value | `IROH_EVENT_NONE` | `ASTER_EVENT_NONE` | was missing from the old header |
| enum-value | `IROH_EVENT_OPERATION_CANCELLED` | `ASTER_EVENT_OPERATION_CANCELLED` | was missing from the old header |
| enum-value | `IROH_EVENT_REGISTRY_ACL_LISTED` | `ASTER_EVENT_REGISTRY_ACL_LISTED` |  |
| enum-value | `IROH_EVENT_REGISTRY_ACL_UPDATED` | `ASTER_EVENT_REGISTRY_ACL_UPDATED` |  |
| enum-value | `IROH_EVENT_REGISTRY_PUBLISHED` | `ASTER_EVENT_REGISTRY_PUBLISHED` |  |
| enum-value | `IROH_EVENT_REGISTRY_RENEWED` | `ASTER_EVENT_REGISTRY_RENEWED` |  |
| enum-value | `IROH_EVENT_REGISTRY_RESOLVED` | `ASTER_EVENT_REGISTRY_RESOLVED` |  |
| enum-value | `IROH_EVENT_SEND_COMPLETED` | `ASTER_EVENT_SEND_COMPLETED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_ACCEPTED` | `ASTER_EVENT_STREAM_ACCEPTED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_FINISHED` | `ASTER_EVENT_STREAM_FINISHED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_OPENED` | `ASTER_EVENT_STREAM_OPENED` |  |
| enum-value | `IROH_EVENT_STREAM_RESET` | `ASTER_EVENT_STREAM_RESET` | was missing from the old header |
| enum-value | `IROH_EVENT_STRING_RESULT` | `ASTER_EVENT_STRING_RESULT` | was missing from the old header |
| enum-value | `IROH_EVENT_TAG_DELETED` | `ASTER_EVENT_TAG_DELETED` |  |
| enum-value | `IROH_EVENT_TAG_GET` | `ASTER_EVENT_TAG_GET` |  |
| enum-value | `IROH_EVENT_TAG_LIST` | `ASTER_EVENT_TAG_LIST` |  |
| enum-value | `IROH_EVENT_TAG_SET` | `ASTER_EVENT_TAG_SET` |  |
| enum-value | `IROH_EVENT_UNIT_RESULT` | `ASTER_EVENT_UNIT_RESULT` |  |
| enum-value | `IROH_HOOK_FAILURE_FAIL_CLOSED` | `ASTER_HOOK_FAILURE_FAIL_CLOSED` |  |
| enum-value | `IROH_HOOK_FAILURE_FAIL_OPEN` | `ASTER_HOOK_FAILURE_FAIL_OPEN` |  |
| enum-value | `IROH_RELAY_MODE_CUSTOM` | `ASTER_RELAY_MODE_CUSTOM` | was missing from the old header |
| enum-value | `IROH_RELAY_MODE_DEFAULT` | `ASTER_RELAY_MODE_DEFAULT` | was missing from the old header |
| enum-value | `IROH_RELAY_MODE_DISABLED` | `ASTER_RELAY_MODE_DISABLED` | was missing from the old header |
| enum-value | `IROH_STATUS_ALREADY_CLOSED` | `ASTER_STATUS_ALREADY_CLOSED` |  |
| enum-value | `IROH_STATUS_BUFFER_TOO_SMALL` | `ASTER_STATUS_BUFFER_TOO_SMALL` |  |
| enum-value | `IROH_STATUS_CANCELLED` | `ASTER_STATUS_CANCELLED` |  |
| enum-value | `IROH_STATUS_CONNECTION_REFUSED` | `ASTER_STATUS_CONNECTION_REFUSED` |  |
| enum-value | `IROH_STATUS_INTERNAL` | `ASTER_STATUS_INTERNAL` |  |
| enum-value | `IROH_STATUS_INVALID_ARGUMENT` | `ASTER_STATUS_INVALID_ARGUMENT` |  |
| enum-value | `IROH_STATUS_OK` | `ASTER_STATUS_OK` |  |
| enum-value | `IROH_STATUS_QUEUE_FULL` | `ASTER_STATUS_QUEUE_FULL` |  |
| enum-value | `IROH_STATUS_STREAM_RESET` | `ASTER_STATUS_STREAM_RESET` |  |
| enum-value | `IROH_STATUS_TIMEOUT` | `ASTER_STATUS_TIMEOUT` |  |
| enum-value | `IROH_STATUS_UNSUPPORTED` | `ASTER_STATUS_UNSUPPORTED` |  |
| function | `iroh_abi_version_major` | `aster_abi_version` | REMOVED: use `aster_abi_version() >> 16` |
| function | `iroh_abi_version_minor` | `aster_abi_version` | REMOVED: use `(aster_abi_version() >> 8) & 0xff` |
| function | `iroh_abi_version_patch` | `aster_abi_version` | REMOVED: use `aster_abi_version() & 0xff` |

Not listed: `iroh_buffer_free`, `iroh_doc_create`, `iroh_doc_join` appeared only in stale doc comments of the old header (no such functions; the real ones are `aster_string_release`, `aster_docs_create`, `aster_docs_join`).
