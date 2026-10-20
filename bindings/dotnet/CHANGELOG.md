# Changelog: Aster .NET binding

## Unreleased

## 0.3.1313 - 2026-10-20

### Added: `ServerCallContext.TokenClaims` (C ABI 7.9)

Every claim of the verified forwarded token, as signed (a provider token's custom claims such as `tenant` included), as a read-only dictionary of `JsonElement` (`ForwardedIdentity.Claims`); null for a call without a verified forwarded token. Against an older native library the dictionary is empty.

### Added: server peer events (C ABI 7.8 required)

`Native.RequiredAbiMinor` is now 8; a 7.0 - 7.7 library is refused.
`await server.PeerEventsAsync(cancellationToken)` returns a `PeerEventStream`:
`Snapshot` (the peers connected now: `PeerSnapshot`, `PeerIdentity`) and an
`IAsyncEnumerable<PeerEvent>` of `PeerConnected`, `PeerDisconnected`
(`DisconnectReason`: `Closed`, `TimedOut`, `Revoked`, `Drained`, `Error`) and
`PeerLagged(Missed)`, taken atomically so no peer falls between the two.
Events are per peer (first admitted `aster/1` connection to last), only after
admission. A consumer that falls behind the 1024-event queue gets `PeerLagged`
and should open a new stream. Dispose the stream (or cancel the token) to
release the receiver; disposing the server does too.

### Changed: default maximum forwarded-token lifetime is 3600 s

The default maximum forwarded-token lifetime (`exp - iat` of a token from a
trusted issuer) is now 3600 s (was 900 s), so hour-long access tokens are
accepted. Config key: `[[identity.trusted_issuers]] max_token_lifetime_secs`
(`TrustedIssuer.MaxTokenLifetimeSecs`; also inside `ASTER_IDENTITY_TRUSTED_ISSUERS`). Set it explicitly to keep
the old 900 s limit; the cap stays 86400 s.

### Added: an optional clock for the client retry, breaker and deadline interceptors

`CircuitBreakerInterceptor`, `RetryInterceptor` and `DeadlineInterceptor` take
an optional `TimeProvider timeProvider` (default `TimeProvider.System`). The
breaker measures its recovery timeout on it; the client call loop reads
deadlines and waits retry back-offs on the first non-system clock its deadline
or retry interceptors carry. Tests use it to run these policies without sleeps.

### Changed: `EndpointConfigBuilder.EnableDiscovery(false)` turns off all address lookup

It used to turn off only local mDNS, leaving the n0 pkarr/DNS lookups on; it
now also selects `DiscoveryMode.None`, so peers are dialled by full address.
`EndpointConfigBuilder.BindAddr(string)` and `Endpoint.NodeAddr()` are new, so
an endpoint can bind loopback and hand its address to
`ConnectWithAddrAsync`.

### Added: gateway identity (C ABI 7.7 required)

`Native.RequiredAbiMinor` is now 7; a 7.0 – 7.6 library is refused. Gateway
identity runs on the config-driven `AsterServer` / `AsterClient` path, with
Rust doing all validation and wiring:

- `AsterConfig.Identity` (`IdentityConfig`: `AcceptOnBehalfOf`,
  `TrustedIssuers`, `EdgeTokenMaxLifetimeSecs`, `ClockLeewaySecs`,
  `MaxKeySetAgeSecs`, `Publisher`) and `AsterConfig.RemoteConfig`
  (`Disabled`, `Lock`), read from `ASTER_IDENTITY_*` / `ASTER_REMOTE_CONFIG_*`
  and TOML by Rust; file / env sources survive into NodeConfig `get` unless
  code changes the value.
- `AsterServerOptions.AcceptOnBehalfOf` (overrides the config) and
  `AuditOutbox` (durable NodeConfig set / clear audit).
- `ServerCallContext.Actor`, `OnBehalfOf` (`ForwardedIdentity`),
  `AuthMethods`, `RequireAuthMethod(m)`, `NodeProperties`,
  `NodeAttribute(key)`; `AsterServer.NodeProperties()` (cached per
  NodeConfig generation).
- `CallOptions.OnBehalfOf` (the `aster-on-behalf-of` entry; a client running
  `[identity.publisher]` notices the token), `AsterClient.MintUserToken` /
  `Enrollment.MintUserToken`, `Enrollment.NoticeKid` / `NoticeToken` /
  `PublisherStatus`, `GatewayIdentity` constants and `BearerToken`.
- `NodeConfigClient` (`GetAsync`, `SetAsync`, `ClearAsync`, `AckAsync`) over
  an `RpcConnection`.

### Changed: native C ABI 7.5 required

`Native.RequiredAbiMinor` was raised to 5 (now 7, above). Added on the way:

- **7.1** `AuditOutbox.AcknowledgementsAsync` / `WaitAcknowledgementsAsync` /
  `WatchAcknowledgementsAsync` (`IAsyncEnumerable<AuditAcknowledgement>`).
- **7.2** `AsterServer.DrainAsync(grace)` returning `DrainReport`,
  `AsterServer.IsDraining`, `DrainCloseCode`, `RpcError.IsDraining`.
- **7.3** `AsterConfig.BindAddrV6`, `AdvertiseAddrs`, `RelayCaCerts`,
  `RelayCaOnly`, `Discovery` (`DiscoveryMode`), `DiscoveryDnsOrigin`,
  `DiscoveryPkarrRelays`, read from env / TOML by Rust.
- **7.4** `Blobs.TransferStats()`; fetch report v2 (`FetchProviderStats`
  gains `InFlight`, `PlainFallbacks`, `BytesTransferred`, `WireBytes`; the
  parser now bounds-checks and raises `IrohException` on a short report);
  `TopologyPeerView.QuicTraffic` / `BlobTransfer`;
  `AsterConfig.MetricsPeerLabels` / `MetricsPeerLabelCap`.
- **7.5** `ServerCallContext.Peer` (transport) apart from `Principal`
  (`PeerId` stays the principal), `HasPrincipal`, `CallId`, `WireCallId`,
  `MetadataValue`, `Remaining`, `IsCancelled`, `CancelledAsync()`, and the
  async-local `ServerCallContext.Current` / `Require()`.

### Added: contract types and recursive types

`DateTimeOffset` (timestamp), `DateOnly` (date), `ForyDecimal` (decimal;
`[WireDecimal(p, s)]` for decimal(p,s)), `byte[]` with `[WireUuid]` /
`[WireUlid]`, and recursive type graphs (SELF_REF), matching Rust and
Python contract ids and wire bytes (`conformance/vectors/contract-types.json`).
Helpers: `AsterUuid` (RFC 9562 byte order), `Ulid`, `AsterDecimal`.
`ForyCodec` now applies the encode / decode rules (timestamp truncation,
decimal(p,s) rescale, 16-byte identifiers) and throws
`ContractValueException` (`INVALID_ARGUMENT`) for a value that does not fit.

### Changed (breaking): native C ABI 7.0, zero means fail closed

Requires native C ABI **7.0** or a later 7.x (`Native.AbiVersionMajor` = 7,
`Native.RequiredAbiMinor` = 0). ABI 7.0 swaps `aster_hook_failure_mode_t`:
`HookFailureMode.FailClosed` is now 0 and `FailOpen` 1, so a zero-filled
native config or `default(HookFailureMode)` fails closed. The layout is
unchanged; `Native.EnsureAbi` refuses a 6.x library.

### Changed (breaking): admission fails closed by default

`AsterConfig.HookFailureMode` now defaults to `HookFailureMode.FailClosed`:
with `EnableHooks`, a connection whose admission request is not answered
within `HookTimeoutMs` (or whose `Admissions` loop is gone) is rejected with
`403 "hook unavailable"` instead of accepted. Set `HookFailureMode.FailOpen`
to keep the old behaviour.

### Added: `AsterConfig.BlobZstdLevel` and `ReuseLastPort` (native C ABI 6.0)

Blob transport compression and port reuse on restart can now be set, and in
particular turned off, from .NET: `BlobZstdLevel` (`null` = default 3, `0` =
off, 1..19) and `ReuseLastPort` (`null` = default on). `FromEnv` / `FromToml`
read `ASTER_BLOB_ZSTD_LEVEL` / `[storage] blob_zstd_level` and
`ASTER_REUSE_LAST_PORT` / `[network] reuse_last_port`. Before, both were lost
when the native config was copied and rebuilt, so they stayed on. The
`aster_config_t` mirror grows to 288 bytes, which needs native C ABI 6.0 or
later (now 7.0, above). 6.0 is a breaking ABI release because the struct grew from 280 to 288 bytes; the
binding performs the new `aster_abi_accept(6)` handshake (`Native.EnsureAbi`)
before filling a config, without which the library refuses to write one.

### Changed: `SyncFinished` says how the round ended

A `DocEventKind.SyncFinished` event now carries `Sync`, a `DocSyncResult`
with `Origin` (`DocSyncOrigin.Accept` or `Connect`), `Succeeded`, and the
document layer's `Error` for a failed round. Before, a failed round and a
completed one looked the same. The native payload for this event changed
(length-prefixed peer, origin, ok flag, length-prefixed error), so this
binding needs the matching native library.

### Changed: the credential sets the maximum roles; runtime can only restrict

For a peer with a live enrollment session, the roles Gate 3 checks are now the
roles from its credential (`[enrollment.roles]`) intersected with an optional
runtime restriction. New `AsterServer.RestrictPeerRoles(peerId, roles)` narrows
them from the peer's next call (calls in flight finish); an empty list leaves
the peer no roles; a role the credential lacks is never granted.
`AsterServer.ClearPeerRestriction(peerId)` restores the credential roles. A
restriction survives renewals of the same credential subject and ends with the
peer's enrollment session (revocation, eviction, expiry); `RestrictPeerRoles`
throws an `IrohException` (`Status.NotFound`) for a peer without a live
session. `SetPeerAttributes` with `aster.role` for an enrolled peer now throws
(`Status.InvalidArgument`); other keys still apply (the credential's
attributes win on a clash). Peers without an enrollment session are unchanged.

### Breaking change: legacy consumer admission retired; Enrollment from configuration

The consumer admission path is removed with no backward compatibility, in favour
of Enrollment driven by `AsterServer` / `AsterClient` and `AsterConfig`
(design `docs/_internal/working_ideas/aster-enrollment.md` §15). A node is open
exactly when enrollment is not configured; there is no "allow all" switch,
because one would turn a typo into an open server.

**Removed**

- `AsterServer.ConsumerAdmissionAlpn` (`aster.consumer_admission`): the server
  no longer opens or routes that ALPN, and no longer creates its own Gate-0 gate.
- `AsterConfig.AllowAllConsumers`, `AsterConfig.AllowAllProducers` and their
  `[trust] allow_all_consumers` / `allow_all_producers` and
  `ASTER_ALLOW_ALL_CONSUMERS` / `ASTER_ALLOW_ALL_PRODUCERS` sources.
- `AsterConfig.EnrollmentCredentialIid` (`[trust] enrollment_credential_iid`,
  `ASTER_ENROLLMENT_CREDENTIAL_IID`): no meaning under Enrollment.
- `Aster.Registry.ServiceSummary` (the consumer-admission response descriptor).

**Changed**

- Requires native ABI **5.1** (`Native.RequiredAbiMinor` = 1).
- `AsterServer.Gate` is now nullable: it is the caller's explicit
  `AsterServerOptions.Gate` (an escape hatch, owned by the caller), or null.
  Without one an unenrolled server is open (previously a PROTECTED gate admitted
  no one unless `AllowAllConsumers` was set) and an enrolled one is guarded by
  Enrollment. Setting a gate together with enrollment is an `ArgumentException`.
- `[trust] enrollment_credential` / `ASTER_ENROLLMENT_CREDENTIAL` keeps its name
  but is now the path of an enrollment chain (`aster enroll node --out`), and
  setting it enables enrollment.
- `[trust]` and `[enrollment]` (and `ASTER_ROOT_*`, `ASTER_ENROLLMENT_*`) are
  resolved by Rust (`aster_config_enrollment_from_env` / `_from_toml`), so names,
  defaults and validation are the Rust SDK's; a malformed value now throws.

**Added**

- `AsterConfig`: `NextRootCommitment`, `RootSeq`, `RootRotations`,
  `EnrollmentCredentialChain`, `Enrollment` (`EnrollmentConfig` with
  `EnrollmentLimits`, `RoleMapping`, `EnrollmentPolicy`) and `IsEnrollmentEnabled`.
- `AsterServer.Enrollment`, `AsterServer.Issuer` (`EnrollmentIssuer`),
  `AsterServerOptions.EnrollmentPolicy` / `EnrollmentRoles`;
  `AsterClient.Enrollment`, `AsterClient.IsRenewing`, `AsterClient.ReconnectAsync`,
  `AsterClientOptions` (with `EnrollmentPolicy`). Roles reach
  `CapabilityRequirement`s per call with no application code; clients enroll
  before their first call to each server and renew automatically.
- `Enrollment` escape hatches: `Status()`, `Credential()`, `PeerClaims(peer)`,
  `CheckPeerAsync`, `NextRevocationAsync`; `EnrollmentAdmissionException`,
  `IssuerRefusalException`.
- Lease grantor: `AsterServerOptions.LeaseGrantor` (`LeaseGrantorOptions`),
  `AsterServer.LeaseSerializer`, `AsterClient.LeaseGrantorAsync`.
- `examples/`: the Mission Control auth chapter (`mc-server --auth`, `mc-agent`).

**Migration**

1. Delete `AllowAllConsumers = true` (and `allow_all_consumers` /
   `ASTER_ALLOW_ALL_CONSUMERS`): a server without `[enrollment]` is already open.
2. Delete `EnrollmentCredentialIid` / `enrollment_credential_iid`.
3. To guard a server, produce files with the Rust `aster` CLI and configure them
   (see `examples/README.md`):
   `aster key root generate`, `aster enroll namespace create`,
   `aster enroll node --subject <node id> --audience <a> [--attributes roles.json] --out node.cred`,
   then set `[trust] root_pubkey`, `[trust] enrollment_credential`,
   `[enrollment] audience`, `fleet_namespace` and `state_path` (server) on both
   sides. Put roles in the credential's `aster.role` attribute
   (`{"aster.role": {"type": "string", "value": "ops.status,ops.logs"}}`) instead
   of calling `SetPeerAttributes`; at runtime you can only narrow them with
   `RestrictPeerRoles`.
4. Code that read `server.Gate` to admit peers must pass its own `Gate` in
   `AsterServerOptions.Gate` (only without enrollment), or use Enrollment.
5. Code that handled `aster.consumer_admission` through `NonRpcConnection`
   has nothing to replace: Enrollment runs inside the native server.
