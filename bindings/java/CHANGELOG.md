# Changelog

All notable changes to the Java and Kotlin binding (`site.aster:aster-runtime`
and the annotation / codegen modules) are recorded here.

## Unreleased

### Changed: default maximum forwarded-token lifetime is 3600 s

The default maximum forwarded-token lifetime (`exp - iat` of a token from a
trusted issuer) is now 3600 s (was 900 s), so hour-long access tokens are
accepted. Config key: `[[identity.trusted_issuers]] max_token_lifetime_secs`
(`TrustedIssuer.maxTokenLifetimeSecs`; also inside `ASTER_IDENTITY_TRUSTED_ISSUERS`). Set it explicitly to keep
the old 900 s limit; the cap stays 86400 s.

### Changed: native C ABI 7.7; gateway identity

The native library must be C ABI **7.7** or a later 7.x
(`REQUIRED_ABI_MINOR = 7`).

### Added (gateway identity, C ABI 7.6-7.7)

- `[identity]`, `[identity.publisher]` and `[remote_config]` on `AsterConfig`
  (`acceptOnBehalfOf`, `trustedIssuers`, `edgeTokenMaxLifetimeSecs`,
  `clockLeewaySecs`, `maxKeySetAgeSecs`, `identityPublisher`,
  `remoteConfigEnabled`, `remoteConfigLock`), read from env and TOML through
  Rust; where each setting came from round-trips for `aster.ops.NodeConfig`.
- `AsterServer.Builder.acceptOnBehalfOf(Boolean)` and `.auditOutbox(Config)`,
  `AsterServer.nodeProperties()` / `auditOutbox()`.
- `CallContext.actor()`, `onBehalfOf()`, `authMethods()`,
  `requireAuthMethod(m)`, `nodeProperties()`, `nodeAttribute(k)`.
- `CallOptions.withOnBehalfOf(jwt)`, `GatewayIdentity` (roles, `AMR_*`,
  `bearerToken`), `AsterClient.mintUserToken` / `Enrollment.mintUserToken`,
  `Enrollment.noticeKid` / `noticeToken` / `publisherStatus` (the client
  notices forwarded tokens itself).
- `NodeConfigClient` (`AsterClient.nodeConfig(target)`): get / set / clear /
  ack.

### Changed: native C ABI 7.5; handler peer and principal

The native library must be C ABI **7.5** or a later 7.x
(`REQUIRED_ABI_MINOR = 5`). `CallContext.peer()` is now the transport peer
(the remote address over HTTP / WebTransport) and the new `principal()` the
`Authenticator`'s principal; before, `peer()` was the principal. Code that
keyed on `peer()` for HTTP callers should use `principal()`. `callId()` is the
caller's wire call id instead of a random UUID. `CallContext.current()` is a
`ScopedValue` the server binds around every handler.

### Added

- `AuditOutbox.acknowledgements` / `waitAcknowledgements` /
  `watchAcknowledgements` and the log bounds on `AuditOutbox.Config` (ABI 7.1).
- `AsterServer.drain(Duration)` returning `DrainReport`, `isDraining()`, and
  `RpcError.isDraining()` (ABI 7.2).
- Cluster network settings (`bindAddrV6`, `advertiseAddrs`, `relayCaCerts`,
  `relayCaOnly`, `discovery`, `discoveryDnsOrigin`, `discoveryPkarrRelays`)
  and `[metrics]` peer labels on `AsterConfig`, from env and TOML (ABI 7.3 /
  7.4).
- `IrohBlobs.transferStats()`, fetch report v2 per-provider counters,
  `PeerView.quicTraffic()` / `blobTransfer()` (ABI 7.4).
- `CallContext.principal()`, `dispatchCallId()`, `wireCallId()`,
  `metadataValue`, `attribute`, `deadlineInstant()`, `remaining()`,
  `cancelled()`, `currentOrNull()` (ABI 7.5).
- Contract types: `Instant` (`timestamp`), `LocalDate` (`date`),
  `BigDecimal` (`decimal`), `@FixedDecimal` (`decimal(p,s)`), `UUID`
  (`uuid`) and `Ulid` (`ulid`), matching Rust's contract ids and scalar wire
  bytes (`conformance/vectors/contract-types.json`).


### Changed (breaking): native C ABI 7.0, zero means fail closed

The native library must be C ABI **7.0** or a later 7.x
(`IrohLibrary.EXPECTED_ABI_MAJOR = 7`, `REQUIRED_ABI_MINOR = 0`). ABI 7.0
swaps `aster_hook_failure_mode_t` so `FAIL_CLOSED` is 0 and `FAIL_OPEN` is 1:
a zero-filled native config or a 0 upcall failure mode now fails closed.
`FailureMode.FAIL_CLOSED.code` is 0 and `FAIL_OPEN.code` is 1; the layout is
unchanged. A 6.x library is refused when the binding loads.

### Changed (breaking): admission fails closed by default

`AsterConfig.hookFailureMode()` now defaults to `"fail_closed"`: with
`enableHooks(true)`, a connection whose admission request is not answered
within `hookTimeoutMs` (or whose admission loop is gone) is rejected with
`403 "hook unavailable"` instead of accepted. Any value other than
`"fail_open"` now fails closed; set `hookFailureMode("fail_open")` to keep the
old behaviour.

### Added: blob compression level and port reuse in `AsterConfig`

`AsterConfig.blobZstdLevel()` (`null` = level 3, `0` = off, 1..19) and
`AsterConfig.reuseLastPort()` (`null` = on), with builder setters
`blobZstdLevel(Integer)` and `reuseLastPort(Boolean)`, mirror Rust
`AsterConfig::blob_zstd_level` / `reuse_last_port`. `fromEnv()` and
`fromFile()` read `ASTER_BLOB_ZSTD_LEVEL` / `[storage] blob_zstd_level` and
`ASTER_REUSE_LAST_PORT` / `[network] reuse_last_port` through Rust. Before,
both settings were lost when the config crossed the C ABI, so neither could
be turned off from Java. `aster_config_t` is now 288 bytes, which needs C ABI
6.0 or later (now 7.0, above). 6.0 is a breaking ABI release because the struct grew
from 280 to 288 bytes; loading the library also performs the new
`aster_abi_accept(6)` handshake, without which the library refuses to fill an
`aster_config_t`.

### Changed: `SYNC_FINISHED` says how the round ended

`DocEvent` gains a `sync()` component, a `DocSyncResult` with `origin()`
(`DocSyncOrigin.ACCEPT` or `CONNECT`), `succeeded()`, and the document
layer's `error()` for a failed round; it is null for other event types.
Before, a failed round and a completed one looked the same. The record's
constructor gained the component, and the native payload for this event
changed, so this binding needs the matching native library.

### Changed: the credential sets the maximum roles; runtime can only restrict

For a peer with a live enrollment session, the roles Gate 3 checks are now the
roles from its credential (`[enrollment.roles]`) intersected with an optional
runtime restriction. New `AsterServer.restrictPeerRoles(peerId, roles)`
narrows them from the peer's next call (calls in flight finish); an empty list
leaves the peer no roles; a role the credential lacks is never granted.
`AsterServer.clearPeerRestriction(peerId)` restores the credential roles. A
restriction survives renewals of the same credential subject and ends with
the peer's enrollment session (revocation, eviction, expiry);
`restrictPeerRoles` throws an `IrohException` (`NOT_FOUND`) for a peer without
a live session. `setPeerAttributes` with `aster.role` for an enrolled peer now
throws (`INVALID_ARGUMENT`); other keys still apply (the credential's
attributes win on a clash). Peers without an enrollment session are
unchanged.

### Breaking change: legacy consumer admission removed, replaced by Enrollment

The consumer admission handshake (the `aster.consumer_admission` ALPN, the
`.aster-identity` / JSON consumer credential and the cloud instance identity
token) is removed with no backward compatibility. `AsterServer` and
`AsterClient` now admit peers with **Enrollment**, configured entirely through
`AsterConfig`, exactly as the Rust `AsterServer` / `AsterClient` do (design §15
of `docs/_internal/working_ideas/aster-enrollment.md`):

- With enrollment configured, only nodes holding a credential from your root
  connect. Each peer's verified `aster.role` claim (and optionally its scopes
  and tags) becomes the role set `requires` is checked against, evaluated in
  Rust on every call, so a revocation from the root refuses the next call and
  closes the peer's connection. Credentials renew automatically from a
  configured online issuer.
- Without enrollment configured the server is open: there is no "allow all"
  switch any more, because one would turn a typo into an open server.
- Any missing or malformed enrollment value fails `AsterServer.Builder.build()`
  / `AsterClient.Builder.build()` with an error naming the setting. Nothing
  falls back to an open node.
- The native library must be ABI **5.1** or newer
  (`IrohLibrary.REQUIRED_ABI_MINOR = 1`).

**Removed configuration** (the `ASTER_*` variables and `aster.toml` keys are
ignored and never open anything):

| Removed | Replacement |
| --- | --- |
| `AsterConfig.allowAllConsumers()`, `Builder.allowAllConsumers(..)`, `ASTER_ALLOW_ALL_CONSUMERS`, `[trust] allow_all_consumers` | None. A server is open exactly when enrollment is not configured. |
| `AsterConfig.allowAllProducers()`, `Builder.allowAllProducers(..)`, `ASTER_ALLOW_ALL_PRODUCERS`, `[trust] allow_all_producers` | None. |
| `AsterConfig.enrollmentCredentialIid()`, `Builder.enrollmentCredentialIid(..)`, `ASTER_ENROLLMENT_CREDENTIAL_IID`, `[trust] enrollment_credential_iid` | None. |
| `enrollmentCredentialFile` pointing at a consumer `.cred` (JSON) or `.aster-identity` (`[[peers]]`) file | Same name and variable (`ASTER_ENROLLMENT_CREDENTIAL`, `[trust] enrollment_credential`), now the path of an enrollment **chain** written by `aster enroll node --out`. Setting it enables enrollment. |
| `AsterConfig.resolveRootPubkey()` | Rust reads `root_pubkey_file`; `rootPubkey()` / `rootPubkeyFile()` remain. |

**Removed APIs:** `site.aster.trust.ConsumerAdmissionClient`,
`ConsumerAdmissionHandler`, `ConsumerAdmissionWire` (the whole
`site.aster.trust` package); `site.aster.config.AsterIdentity`;
`AsterServer.CONSUMER_ADMISSION_ALPN`; `AsterClient.Builder.consumerAdmission(..)`,
`identity(path, peerName)` and `iidToken(..)`; the role-from-metadata
`MetadataRoleInterceptor` of the Mission Control example.
`AsterClient.Builder.identity(path)` now only keeps the node key in a 64-hex
secret key file (Rust `AsterClientBuilder::identity`); the credential comes
from the configuration.

**Added:** `AsterConfig` carries the §15.3 schema — `nextRootCommitment()`,
`rootSeq()`, `rootRotations()` (`Builder.rootRotation(..)`),
`enrollmentCredentialChain()`, `enrollment()` (`site.aster.enrollment.EnrollmentConfig`
with `EnrollmentLimits` and `RoleMapping`) and `isEnrollmentEnabled()` —
filled by `fromEnv()` / `fromFile()` through the Rust loader. Package
`site.aster.enrollment`: `Enrollment` (`status`, `credential`, `peerClaims`,
`checkPeer`, `nextRevocation`), `EnrollmentIssuer` (`info`, `issue`),
`Policy` (the `PolicyConfig` tree), `AdmissionException`,
`IssuerRefusedException` and the result records. `AsterServer.enrollment()`,
`issuer()`, `leaseSerializer()`; `AsterServer.Builder.enrollmentPolicy(..)`,
`enrollmentRoles(..)`, `leaseGrantor(..)`; `AsterClient.enrollment()`,
`isRenewing()`, `reconnect(addr)`, `leaseGrantor(addr)`,
`AsterClient.Builder.enrollmentPolicy(..)`; `site.aster.lease.LeaseGrantor`
and `NativeLeaseSerializer.grantor(..)`.

**Migration:**

1. Produce the files with the Rust `aster` CLI instead of a consumer
   credential:

   ```bash
   aster key root generate
   ROOT=$(aster key root show --json | jq -r .root_pubkey)
   FLEET=$(aster enroll namespace create --out-secret fleet.secret --json | jq -r .namespace)
   SERVER=$(aster key node generate --out server.key --json | jq -r .node_id)
   CLIENT=$(aster key node generate --out client.key --json | jq -r .node_id)
   aster enroll node --subject $SERVER --audience my-fleet --out server.cred
   echo '{"aster.role":{"type":"string","value":"ops.status,ops.logs"}}' > roles.json
   aster enroll node --subject $CLIENT --audience my-fleet --attributes roles.json --out client.cred
   ```

   A credential is bound to one node key: run each node with the key file it
   was issued for (`secretKeyFile` / `ASTER_SECRET_KEY_FILE`).

2. Replace the old configuration.

   Before:

   ```java
   AsterServer.builder().config(AsterConfig.builder().allowAllConsumers(false).build())
       .interceptors(List.of(new CapabilityInterceptor(services))) ...;
   AsterClient.builder().identity(Path.of("edge.aster-identity")).build();
   ```

   After (or the matching `ASTER_*` variables / `aster.toml` keys, read with
   `AsterConfig.fromEnv()` / `AsterConfig.fromFile(..)`):

   ```java
   AsterServer.builder()
       .config(AsterConfig.builder()
           .rootPubkey(HexFormat.of().parseHex(ROOT))
           .secretKeyFile("server.key")
           .enrollmentCredentialFile("server.cred")
           .enrollment(EnrollmentConfig.builder()
               .audience("my-fleet").fleetNamespace(FLEET).statePath("./enrollment.json").build())
           .build())
       ...;
   AsterClient.builder()
       .config(AsterConfig.builder()
           .rootPubkey(HexFormat.of().parseHex(ROOT))
           .secretKeyFile("client.key")
           .enrollmentCredentialFile("client.cred")
           .enrollment(EnrollmentConfig.builder().audience("my-fleet").fleetNamespace(FLEET).build())
           .build())
       .build();
   ```

   ```toml
   [trust]
   root_pubkey = "<ROOT>"
   enrollment_credential = "server.cred"

   [network]
   secret_key_file = "server.key"

   [enrollment]
   audience = "my-fleet"
   fleet_namespace = "<FLEET>"
   state_path = "./enrollment.json"
   ```

3. Put roles on methods with `@Requires` (Rust enforces them, Gate 3) instead
   of copying roles into attributes yourself; drop `CapabilityInterceptor`
   wiring and any `setPeerAttributes` calls that granted roles. The roles come
   from each peer's credential, and runtime code can only narrow them with
   `restrictPeerRoles` (see "the credential sets the maximum roles" above).

4. Revoke with `aster enroll revoke --subject <id> --fleet-only … --peer <server>`;
   the revoked peer's next call is refused and its connection closes. Observe
   revocations with `server.enrollment().nextRevocation()` if the application
   keeps state per peer.

Serve enrollment-gated services over Iroh only: HTTP/WebTransport callers
never get enrollment roles, and methods without `requires` stay open to them.
