# Changelog

All notable changes to `@aster-rpc/aster` (and its native addon
`@aster-rpc/transport`) are recorded here.

## Unreleased

### Changed (breaking): admission fails closed by default

`AsterConfig.hookFailureMode` now defaults to `'fail_closed'`: with
`enableHooks`, a connection whose admission request is not answered within
`hookTimeoutMs` (or with no `admission()` iterator open) is rejected with
`403 "hook unavailable"` instead of accepted. Set `hookFailureMode:
'fail_open'` to keep the old behaviour.

### Added: `blobZstdLevel` and `reuseLastPort` in `AsterConfig`

`AsterConfig.blobZstdLevel` sets the zstd level of blob transport (unset = 3,
`0` = off: the node serves and fetches blobs on the plain ALPN only, larger
than 19 is clamped) and `AsterConfig.reuseLastPort` (default `true`) lets a
persistent node re-bind its last UDP ports on restart. Both mirror the Rust
`AsterConfig`, including `ASTER_BLOB_ZSTD_LEVEL` / `[storage]
blob_zstd_level` and `ASTER_REUSE_LAST_PORT` / `[network] reuse_last_port`.
Before, `fromEnv` / `fromFile` dropped both and every node ran with the
defaults (on).

### Changed: `sync_finished` says how the round ended

A `sync_finished` `DocEvent` now sets `syncOrigin` (`'accept'` or
`'connect'`), `syncOk`, and `syncError` (the document layer's message, absent
when the round completed). Before, a failed round and a completed one looked
the same.

### Changed: the credential sets the maximum roles; runtime can only restrict

For a peer with a live enrollment session, the roles Gate 3 checks are now the
roles from its credential (`[enrollment.roles]`) intersected with an optional
runtime restriction. New `AsterServer.restrictPeerRoles(peer, roles)` narrows
them from the peer's next call (calls in flight finish); `[]` leaves the peer
no roles; a role the credential lacks is never granted.
`AsterServer.clearPeerRestriction(peer)` restores the credential roles. A
restriction survives renewals of the same credential subject and ends with
the peer's enrollment session (revocation, eviction, expiry);
`restrictPeerRoles` throws for a peer without a live session. The native
`DispatchServer.setPeerAttributes` now throws when given `aster.role` for an
enrolled peer (other keys still apply; the credential's attributes win on a
clash). Peers without an enrollment session are unchanged.

### Breaking change: legacy consumer admission removed, replaced by Enrollment

The consumer admission handshake (`aster.consumer_admission` ALPN, the
`.aster-identity` / JSON consumer credential, OTT nonces and cloud instance
identity checks) is removed with no backward compatibility. `AsterServer` and
`AsterClientWrapper` now admit peers with **Enrollment**, configured entirely
through `AsterConfig`, exactly as the Rust `AsterServer` / `AsterClient` do:

- With enrollment configured, only nodes holding a credential from your root
  connect. Each peer's verified `aster.role` claim (and optionally its scopes
  and tags) becomes the role set `requires` is checked against, evaluated on
  every call, so a revocation from the root refuses the next call and closes
  the peer's connection. Credentials renew automatically from a configured
  online issuer.
- Without enrollment configured the server is open: there is no "allow all"
  switch any more, because one would turn a typo into an open server.
- Any missing or malformed enrollment value fails `server.start()` /
  `client.connect()` with an error naming the setting. Nothing falls back to
  an open node.

**Removed configuration** (`AsterConfig` fields, `ASTER_*` variables and
`aster.toml` keys are ignored by the TypeScript binding and never open
anything):

| Removed | Replacement |
| --- | --- |
| `allowAllConsumers`, `ASTER_ALLOW_ALL_CONSUMERS`, `[trust] allow_all_consumers`, `AsterServerOptions.allowAllConsumers` | None. A server is open exactly when enrollment is not configured. |
| `allowAllProducers`, `ASTER_ALLOW_ALL_PRODUCERS`, `[trust] allow_all_producers` | None (producer admission was already unsupported). |
| `enrollmentCredentialIid`, `ASTER_ENROLLMENT_CREDENTIAL_IID` | None. |
| `enrollmentCredentialFile` pointing at a consumer `.cred` (JSON) or `.aster-identity` (`[[peers]]`) file | Same name and variable (`ASTER_ENROLLMENT_CREDENTIAL`, `[trust] enrollment_credential`), now the path of an enrollment **chain** written by `aster enroll node --out`. Setting it enables enrollment. |
| `AsterClientOptions.enrollmentCredentialFile` | `config.enrollmentCredentialFile` (or `config.enrollmentCredentialChain`). |
| `AsterClientOptions.identity` / `peer` as a consumer credential | `identity` only pins the node key (`[node] secret_key`); the credential comes from the configuration. |

**Removed APIs:** `AdmissionDeniedError`; `server.gate` (`AsterServer` no
longer exposes a `GatePolicy`); `GatePolicy.fromConfig`;
`CONSUMER_ADMISSION_ALPN` and `PRODUCER_ADMISSION_ALPN`;
`performAdmission`, `handleConsumerAdmissionConnection`,
`handleConsumerAdmissionRpc`, `serveConsumerAdmission`,
`consumerCredToJson`, `consumerCredFromJson` and the
`ConsumerAdmissionRequest` / `ConsumerAdmissionResponse` /
`ConsumerAdmissionOpts` types; the legacy credential model
(`EnrollmentCredential`, `ConsumerEnrollmentCredential`, `canonicalJson`,
`producerSigningBytes`, `consumerSigningBytes`, `credentialSigningBytes`,
`signCredential`, `verifyCredentialSignature`, `verifyConsumerCredential`,
`verifyProducerCredential`, `checkOffline`, `checkRuntime`, `admit`,
`AdmissionResult`, `AdmitOptions`); the OTT nonce store (`InMemoryNonceStore`,
`NonceStore`); cloud instance identity (`verifyIID`, `getIIDBackend`,
`MockIIDBackend`, `AWSIIDBackend`, `GCPIIDBackend`, `AzureIIDBackend`,
`IIDBackend`, `ATTR_IID_*`); the `MAX_SERVICES_IN_ADMISSION` and
`MAX_ADMISSION_PAYLOAD_SIZE` limits. `ServiceSummary` is now exported from
the runtime (it described the admission response before).

`GatePolicy`, `PeerAdmissionStore`, `node.admission()` and delegated admission
(`aster.admission`, `handleDelegatedAdmissionConnection`) remain as low-level
escape hatches for nodes you start and serve yourself.

**Migration:**

1. Produce the files with the Rust `aster` CLI instead of
   `aster trust keygen` / `aster enroll node --role consumer`:

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

   ```ts
   new AsterServer({ services, allowAllConsumers: false, config: { rootPubkeyFile: 'root.pub' } });
   new AsterClientWrapper({ address, enrollmentCredentialFile: 'edge.cred' });
   ```

   After (or the matching `ASTER_*` variables / `aster.toml` keys):

   ```ts
   new AsterServer({
     services,
     config: {
       rootPubkey: ROOT,
       secretKeyFile: 'server.key',
       enrollmentCredentialFile: 'server.cred',
       enrollment: { audience: 'my-fleet', fleetNamespace: FLEET, statePath: './enrollment.json' },
     },
   });
   new AsterClientWrapper({
     address,
     config: {
       rootPubkey: ROOT,
       secretKeyFile: 'client.key',
       enrollmentCredentialFile: 'client.cred',
       enrollment: { audience: 'my-fleet', fleetNamespace: FLEET },
     },
   });
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

   A dev server that set `allowAllConsumers: true` needs no replacement: leave
   enrollment unconfigured.

3. Drop code that read credentials or set peer attributes by hand: `requires`
   reads the credential roles directly. Revoke with
   `aster enroll revoke --subject <id> --fleet-only … --peer <server id>`.

See `examples/typescript/missionControl/README.md` (Chapter 5) for the full
walk-through.

### Added

- `AsterConfig` gains the Rust enrollment schema with the same names in TOML
  and the environment: `nextRootCommitment`, `rootSeq`, `rootRotations`,
  `enrollmentCredentialChain`, and the `enrollment` section (`enabled`,
  `audience`, `fleetNamespace`, `statePath`, `replicas`, `policy`,
  `requireSynchronizedClock`, `activationLeewaySecs`, `limits`, `roles`,
  `issuerDelegationFile`, `issuerLifetimeSecs`, `renewFrom`, `renew`,
  `synchronize`). Types `EnrollmentConfig`, `EnrollmentLimits`,
  `EnrollmentRoles` and `PolicyConfig`; helpers `isEnrollmentEnabled(config)`
  and `mergeConfig(base, override)`.
- `AsterServer.enrolled`, `AsterServer.peerAttributes(peer)`,
  `AsterServer.node`, `AsterServer.registryNamespace`.
- `AsterClientWrapper.enrolled`, `isRenewing()`, `credential()`, `node`,
  `listServices()`.
- `REGISTRY_NAMESPACE_ATTRIBUTE` (`aster.registry_namespace`): the server
  announces its contract registry in `aster.ops.NodeInfo`.
- `RpcServer.startWithConfig(config)`: serve on the Rust `AsterServer` (its
  own node, enrollment from the configuration).
- Native addon: `AsterClient` (the Rust declarative client),
  `DispatchServer.node()`, `DispatchServer.enrolled`,
  `DispatchServer.setNodeAttribute()`, `asterConfigIsEnrollmentEnabled()`.

### Changed

- `AsterServer` runs on the Rust `AsterServer` (one node, owned by the native
  server) instead of starting a node and serving on it from TypeScript; the
  startup banner reports `mode: enrollment` or `mode: open`.
- `AsterClientWrapper` runs on the Rust `AsterClient`. It discovers services
  from the server's contract registry (announced in `aster.ops.NodeInfo`)
  rather than from an admission response: `client.services` is filled by
  `connect()` when the server announces a registry, and `listServices()`
  fetches it on demand. Generated `fromConnection` clients call
  `listServices()`.
- `AsterServerOptions.config` / `AsterClientOptions.config` merge the
  `enrollment` section field by field over `AsterConfig.fromEnv()`.
- A client `address` may fall back to `config.endpointAddr`
  (`ASTER_ENDPOINT_ADDR`).
