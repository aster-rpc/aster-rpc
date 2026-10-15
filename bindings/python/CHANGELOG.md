# Changelog — `aster-rpc` (Python)

All notable changes to the Python binding. Versions follow the release tags of
the repository; entries under **Unreleased** ship with the next tag.

## Unreleased

### Changed: `sync_finished` says how the round ended

A `sync_finished` `DocEvent` now sets `sync_origin` (`"accept"` or
`"connect"`), `sync_ok`, and `sync_error` (the document layer's message, or
`None` when the round completed). Before, a failed round and a completed one
looked the same.

### Changed: the credential sets the maximum roles; runtime can only restrict

For a peer with a live enrollment session, the roles Gate 3 checks are now the
roles from its credential (`[enrollment.roles]`) intersected with an optional
runtime restriction. New `AsterServer.restrict_peer_roles(peer, roles)`
narrows them from the peer's next call (calls in flight finish); `[]` leaves
the peer no roles; a role the credential lacks is never granted.
`AsterServer.clear_peer_restriction(peer)` restores the credential roles. A
restriction survives renewals of the same credential subject and ends with
the peer's enrollment session (revocation, eviction, expiry);
`restrict_peer_roles` raises `LookupError` for a peer without a live session.
Runtime-set `aster.role` never applies to an enrolled peer. Peers without an
enrollment session (an open server, a custom `gate=`) are unchanged.

### Breaking change: consumer admission removed, Enrollment replaces it

The legacy consumer admission is gone, with no compatibility path: the
`aster.consumer_admission` ALPN and its handshake, the consumer credential
JSON / `.aster-identity` peer credentials, `allow_all_consumers` and
`allow_all_producers`, and the cloud instance-identity token. Admission is now
**Enrollment**, configured through `AsterConfig` and run in Rust by
`AsterServer` and `AsterClient`, with the same schema, names, defaults and
fail-closed validation as the Rust `AsterConfig` (design: section 15 of
`docs/_internal/working_ideas/aster-enrollment.md`).

A node is open exactly when enrollment is not configured. There is no "allow
all" switch: a typo can no longer turn a protected server into an open one.

**What replaces it**

- `AsterServer(services, config=AsterConfig.from_env())` installs Enrollment
  when the configuration enables it: only nodes holding a credential from
  `[trust] root_pubkey` connect (Enrollment owns Gate 0), each peer's
  verified claims become the roles `requires=` checks (evaluated on every
  call, so revocation and renewal apply to the next call), a root revocation
  closes the revoked peer's connections, policy is synchronized from
  `[enrollment] replicas` or held for the root to publish into, and the
  server's own credential renews.
- `AsterClient(config=..., address=...)` presents its credential over the
  reciprocal enrollment handshake (it verifies the server too), synchronizes
  the fleet namespace the server offers, and renews from `renew_from` or from
  the server when the server signed its credential.
- Credentials are delegation chains issued by the Rust `aster` CLI
  (`aster-cli`): `aster enroll node`, `aster enroll delegate`,
  `aster enroll revoke`.

**Removed APIs and settings**

| Removed | Replacement |
| --- | --- |
| `AsterConfig.allow_all_consumers`, `allow_all_producers`; `[trust] allow_all_consumers` / `allow_all_producers`; `ASTER_ALLOW_ALL_CONSUMERS` / `ASTER_ALLOW_ALL_PRODUCERS` | none: open exactly when enrollment is not configured |
| `AsterServer(allow_all_consumers=, allow_all_producers=, nonce_store=)` | configuration (`[enrollment]`); `gate=` for a custom `GatePolicy` on a server without enrollment |
| `AsterConfig.enrollment_credential_iid`, `ASTER_ENROLLMENT_CREDENTIAL_IID` | none (no meaning under Enrollment) |
| `aster.ALPN_CONSUMER_ADMISSION` / `aster._aster.ALPN_CONSUMER_ADMISSION` | `aster.ENROLLMENT_ALPN` (served by `AsterServer` itself) |
| `aster.trust.consumer` (`ConsumerAdmissionRequest`/`Response`, `handle_consumer_admission_rpc`, `handle_consumer_admission_connection`, `consumer_cred_to_json`/`from_json`) | Enrollment in `AsterServer` / `AsterClient` |
| `aster.trust.credentials` (`EnrollmentCredential`, `ConsumerEnrollmentCredential`, `AdmissionResult`, `ATTR_*`) | credential chains from `aster enroll node` |
| `aster.trust.admission` (`admit`, `check_offline`, `check_runtime`), `aster.trust.nonces`, `aster.trust.iid`; `aster.trust.signing.sign_credential` / `verify_signature`; the `iid` extra (PyJWT) | Enrollment verification in Rust |
| `aster.limits.MAX_SERVICES_IN_ADMISSION`, `MAX_ADMISSION_PAYLOAD_SIZE` | `MAX_ADVERTISED_SERVICES` |
| `AsterServer._admission_addr_b64` / `_consumer_admission_addr_b64`; ephemeral dev root key generation | none |
| Python CLI `aster enroll node`, `aster trust keygen`, `aster trust sign` | Rust CLI: `aster key root generate`, `aster trust init`, `aster enroll node`, `aster enroll delegate`, `aster enroll revoke` |

**Migration**

1. Create the root and the fleet with the Rust CLI:

   ```bash
   aster key root generate
   ROOT=$(aster key root show --json | jq -r .root_pubkey)
   FLEET=$(aster enroll namespace create --out-secret fleet.secret --json | jq -r .namespace)
   ```

   Optionally pin root rotation with `aster trust init` (`[trust]
   next_root_commitment`, `root_seq`, `root_rotations`).

2. Give every node a key file and a credential for it (roles go in the
   `aster.role` claim, comma-separated):

   ```bash
   SERVER=$(aster key node generate --out server.key --json | jq -r .node_id)
   aster enroll node --subject $SERVER --audience <fleet> --out server.cred
   echo '{"aster.role":{"type":"string","value":"ops.status,ops.logs"}}' > roles.json
   AGENT=$(aster key node generate --out agent.key --json | jq -r .node_id)
   aster enroll node --subject $AGENT --audience <fleet> --attributes roles.json --out agent.cred
   ```

3. Replace the old configuration:

   ```toml
   # before
   [trust]
   root_pubkey_file = "root.pub"
   allow_all_consumers = false
   enrollment_credential = "consumer.token"     # JSON / .aster-identity credential

   # after
   [trust]
   root_pubkey = "<ROOT>"                        # or root_pubkey_file
   enrollment_credential = "server.cred"         # chain from `aster enroll node`

   [network]
   secret_key_file = "server.key"                # the key the credential names

   [enrollment]
   audience = "<fleet>"
   fleet_namespace = "<FLEET>"
   state_path = "/var/lib/app/enrollment.json"   # servers: or a persistent [storage] path
   ```

   or the environment: `ASTER_ROOT_PUBKEY`, `ASTER_ENROLLMENT_CREDENTIAL`,
   `ASTER_SECRET_KEY_FILE`, `ASTER_ENROLLMENT_AUDIENCE`,
   `ASTER_ENROLLMENT_FLEET_NAMESPACE`, `ASTER_ENROLLMENT_STATE_PATH`.
   `[trust] enrollment_credential` / `ASTER_ENROLLMENT_CREDENTIAL` keeps its
   name but is now the path of an enrollment chain, and setting it enables
   enrollment. Drop `allow_all_consumers`, `allow_all_producers` and
   `enrollment_credential_iid` (the Python `AsterConfig` refuses them as
   unknown fields; stray TOML keys are ignored and open nothing).

4. Code: remove `allow_all_consumers=` / `allow_all_producers=` from
   `AsterServer(...)`; keep `requires=` on your methods. Clients need no code
   change beyond configuration. `server.gate` is now `None` unless you pass
   `gate=`.

5. Revoke with `aster enroll revoke --subject <node id> --fleet-only
   --fleet-namespace $FLEET --fleet-secret-file fleet.secret --audience
   <fleet> --peer <server id>`; the node's next call is refused and its
   connections close.

### Added

- `AsterConfig` gains the Enrollment schema of the Rust `AsterConfig`, from
  kwargs, TOML (`AsterConfig.from_file`) and `ASTER_*` environment variables:
  `next_root_commitment`, `root_seq`, `root_rotations`,
  `enrollment_credential_chain` (`[trust]`), and `enrollment`
  (`aster.EnrollmentConfig`: `enabled`, `audience`, `fleet_namespace`,
  `state_path`, `replicas`, `policy`, `require_synchronized_clock`,
  `activation_leeway_secs`, `limits` (`aster.EnrollmentLimits`), `roles`
  (`aster.RoleMappingConfig`), `issuer_delegation_file`,
  `issuer_lifetime_secs`, `renew_from`, `renew`, `synchronize`), plus
  `AsterConfig.is_enrollment_enabled`.
- `AsterServer.enrollment` / `AsterClient.enrollment` (the installed
  `aster.Enrollment`: `credential`, `is_renewing`, `trusted_root`,
  `fleet_namespace`, `audience`, and `issue_for(subject, attributes=...)` on a
  server configured as a delegated issuer); `AsterClient.is_renewing`.
- A server with `[enrollment.issuer] delegation_file` serves
  `aster.enrollment.IssuerV1` renewals.
- `AdmissionDeniedError` is raised by `AsterClient.connect` when a server
  refuses this client (no credential, or the enrollment handshake was
  refused), with hints naming the CLI commands.

### Changed

- A Python `AsterServer` advertises its services and registry namespace in
  the baseline `aster.ops.NodeInfo` (`aster.services`,
  `aster.registry_namespace` attributes); `AsterClient.connect` reads them
  (`client.services`, `client.registry_namespace`). Typed clients
  (`client.client(Service)`) also work against servers that do not advertise
  services (Rust, other bindings).
- `AsterClient.open_gate` is `True` when the client connected without
  enrollment.
- `AsterServer.gate` is `None` unless a `GatePolicy` was passed.
- `AsterServer.root_pubkey` reports the configured root; no ephemeral root
  key is generated any more.
- Enrollment misconfiguration fails `start()` / `connect()` with `ValueError`
  naming the setting, before any node starts.
