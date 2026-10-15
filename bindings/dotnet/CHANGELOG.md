# Changelog: Aster .NET binding

## Unreleased

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
