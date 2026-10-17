# Java and Kotlin runtime

The Java runtime requires JDK 25 and native Aster C ABI **7.0 or newer** (within major 7). Set
`IROH_LIB_PATH` or `-Daster.ffi.lib` to the native library when it is not bundled.

## Enrollment: who may connect, and with which roles

`AsterServer` and `AsterClient` admit peers with **Enrollment**, configured
entirely through `AsterConfig`, exactly as the Rust `AsterServer` /
`AsterClient` do (`[trust]` and `[enrollment]` in `aster.toml`,
`ASTER_ROOT_PUBKEY` / `ASTER_ENROLLMENT_*` in the environment; the same names,
defaults and precedence, resolved by the Rust loader). With enrollment
configured (`config.isEnrollmentEnabled()`):

- the server admits only nodes holding a credential from your root, and each
  peer's verified `aster.role` claim (and optionally its scopes and tags,
  `[enrollment.roles]`) becomes the role set `requires` is checked against, in
  Rust, on every call. A root revocation refuses the peer's next call and
  closes its connections;
- a client presents its credential, enrolls with each server before its first
  call, and renews from a configured issuer;
- any missing or malformed value fails `build()` with an error naming the
  setting; nothing falls back to an open node.

Without enrollment configured the server is open. Produce the files with the
Rust `aster` CLI (`aster key root generate`, `aster enroll namespace create`,
`aster key node generate`, `aster enroll node`); see the
[Mission Control auth chapter](aster-examples-mission-control/README.md).

```java
// ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_AUDIENCE, ASTER_ENROLLMENT_FLEET_NAMESPACE,
// ASTER_SECRET_KEY_FILE, ASTER_ENROLLMENT_CREDENTIAL, ASTER_ENROLLMENT_STATE_PATH
AsterServer server = AsterServer.builder()
    .config(AsterConfig.fromEnv())
    .service(new MissionControl())
    .build().get();

AsterClient client = AsterClient.builder().config(AsterConfig.fromEnv()).build().get();
client.call(serverAddr, "MissionControl", "getStatus", req, StatusResponse.class).get();
```

Escape hatches mirror Rust and Go: `server.enrollment()` / `client.enrollment()`
(`status()`, `credential()`, `peerClaims(peer)`, `checkPeer(peer)`,
`nextRevocation()`), `server.issuer()` (`info()`, `issue(subject, claims,
registration)`) when `[enrollment.issuer] delegation_file` is set,
`client.isRenewing()`, and the builder overrides
`AsterServer.Builder.enrollmentPolicy(Policy)` /
`enrollmentRoles(RoleMapping)` and `AsterClient.Builder.enrollmentPolicy`.

`AsterServer.Builder.leaseGrantor(LeaseGrantor)` serves `aster.lease.Grantor`
(over a Java `LeaseSerializer`, or Rust's in-memory one), and
`client.leaseGrantor(serverAddr)` returns a `NativeLeaseSerializer` that
acquires through it; the grantor binds every lease to the caller's
authenticated node id.

See [CHANGELOG.md](CHANGELOG.md) for the retired consumer admission.

## Blob compression and port reuse

Two node settings are on by default and can be turned off from `AsterConfig`,
the environment or `aster.toml`, as in Rust:

| Builder | Env | TOML | Default | Off |
| --- | --- | --- | --- | --- |
| `blobZstdLevel(Integer)` | `ASTER_BLOB_ZSTD_LEVEL` | `[storage] blob_zstd_level` | `null` = level 3 | `0` (env/TOML also `off`) |
| `reuseLastPort(Boolean)` | `ASTER_REUSE_LAST_PORT` | `[network] reuse_last_port` | `null` = on | `false` |

`blobZstdLevel` sets the zstd level of blob transport (1..19; larger values
are clamped to 19). With `0` the node serves blobs only on the plain ALPN and
fetches plain. `reuseLastPort` makes a persistent node (`storagePath` set,
port left to the OS) re-bind the UDP ports it last used, recorded in
`bind-ports.json`; with `false` it binds fresh ports and records nothing.
Both cross the C ABI in `aster_config_t` (C ABI 6.0).

```java
AsterConfig cfg = AsterConfig.builder().blobZstdLevel(0).reuseLastPort(false).build();
```

## Cluster network and metrics settings

C ABI 7.3 / 7.4 settings beside the node config, from `AsterConfig`, the
environment or `aster.toml`, resolved by the Rust config as in every binding:

| Builder | Env | TOML |
| --- | --- | --- |
| `bindAddrV6(String)` | `ASTER_BIND_ADDR_V6` | `[network] bind_addr_v6` |
| `advertiseAddrs(List)` | `ASTER_ADVERTISE_ADDRS` | `[network] advertise_addrs` |
| `relayCaCerts(List)` / `relayCaOnly(boolean)` | `ASTER_RELAY_CA_CERTS` / `ASTER_RELAY_CA_ONLY` | `[network] relay_ca_certs` / `relay_ca_only` |
| `discovery(String)`: `default`, `none`, `local`, `custom` | `ASTER_DISCOVERY` | `[network] discovery` |
| `discoveryDnsOrigin(String)` / `discoveryPkarrRelays(List)` | `ASTER_DISCOVERY_DNS_ORIGIN` / `ASTER_DISCOVERY_PKARR_RELAYS` | `[network] discovery_dns_origin` / `discovery_pkarr_relays` |
| `metricsPeerLabels(boolean)` / `metricsPeerLabelCap(int)` | `ASTER_METRICS_PEER_LABELS` / `ASTER_METRICS_PEER_LABEL_CAP` | `[metrics] peer_labels` / `peer_label_cap` |

Setting DNS / pkarr servers alone selects `custom` discovery. Rust validates
the settings when the config is lowered (an `IrohException`), and again when
the node starts (addresses, CA files).

## Graceful drain

`AsterServer.drain(Duration grace)` (C ABI 7.2) takes a server out of
rotation, then closes it: an attached ops listener reports not ready, new
connections are closed with `DrainReport.DRAIN_CLOSE_CODE` (503) and new calls
answer `UNAVAILABLE` with the `aster.draining` detail (`RpcError.isDraining()`;
calls on an existing session still run), idle connections are closed, and the
calls still running at `grace` are cancelled (their `CallContext` reports
cancelled). It blocks and returns a `DrainReport` (calls in flight /
completed / cancelled / refused, connections closed / refused, elapsed,
whether the deadline was reached). A second `drain` with a shorter grace cuts
a running one short; `isDraining()` reports a started drain.

## Handler context

`CallContext` (C ABI 7.5) separates `peer()`, the transport caller id (the
64-hex endpoint id over Iroh, the remote address over HTTP / WebTransport),
from `principal()`, the `Authenticator`'s principal (else the peer). It also
carries `callId()` (the caller's wire call id, as Rust's `call_id()`),
`dispatchCallId()`, `metadata()` / `metadataValue(key)`, `attributes()` /
`attribute(key)` (the set Gate 3 checked), `irohAuthenticated()` (the
provenance), `deadlineInstant()` / `remaining()` (the effective deadline),
`isCancelled()` and `cancelled()`, a future that completes when the peer
cancels, the deadline passes or the connection goes. The server binds it as
`CallContext.current()` (a `ScopedValue`) around every handler, all four
patterns; threads a handler starts itself do not inherit it. There is no
Kotlin coroutine context element yet (handlers are not `suspend`).

## Gateway identity

An edge (a web gateway) holding `aster.gateway` calls services on behalf of a
signed-in user with one JWT per call (C ABI 7.7; `site.aster.identity`). It is
all configuration on the server: `[identity] accept_on_behalf_of = true` (env
`ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF`, builder `acceptOnBehalfOf(true)`) for
tokens the edge mints, and for an identity provider's own
tokens `[[identity.trusted_issuers]]` (`ASTER_IDENTITY_TRUSTED_ISSUERS`, JSON;
`AsterConfig.Builder.trustedIssuer(TrustedIssuer.of(iss, audiences)
.withRoles("groups", roleMap))`); the issuer keys come from the fleet
namespace, written there by a node running `[identity.publisher]`
(`identityPublisher(...)`, `ASTER_IDENTITY_PUBLISHER`). The limits are
`edgeTokenMaxLifetimeSecs`, `clockLeewaySecs` and `maxKeySetAgeSecs`.
`AsterServer.Builder.acceptOnBehalfOf(Boolean)` overrides the config.

On the edge:

```java
String token = client.mintUserToken("alice", List.of("notes.read"),
    List.of(GatewayIdentity.AMR_WEBAUTHN), Duration.ofMinutes(1));
// or forward the provider's: GatewayIdentity.bearerToken(authorizationHeader)
client.call(server, "Notes", "read", req, Reply.class,
    CallOptions.DEFAULT.withOnBehalfOf(token));
```

Minted roles must be within the credential's `aster.identity.issuable_roles`
delegation. A client running a publisher notices every forwarded token, so a
provider's key rotation is published at once (`Enrollment.noticeKid` /
`noticeToken` / `publisherStatus`).

In the handler, `CallContext.current()` gives `principal()` = `iss|sub`,
`actor()` = the edge's node id, `attributes()` = the token's roles,
`authMethods()` and `onBehalfOf()` (issuer, subject, jti, expiry);
`requireAuthMethod(GatewayIdentity.AMR_WEBAUTHN)` refuses weaker sign-ins
(`@Requires` checks roles only). A server with `accept_on_behalf_of` off
refuses the entry with `PERMISSION_DENIED`.

`aster.ops.NodeConfig` is served behind `aster.config-admin` unless
`[remote_config] enabled = false` (`remoteConfigEnabled(false)`;
`remoteConfigLock(List.of("identity"))` locks sections). An admin edge uses
`client.nodeConfig(server)` → `NodeConfigClient.get()` / `set(...)` /
`clear(gen)` / `ack(gen, timeout)`; handlers read the node's name, tags and
typed attributes with `CallContext.nodeProperties()` / `nodeAttribute(key)`
(never for authorisation), and `AsterServer.nodeProperties()` reads them
outside a call. `AsterServer.Builder.auditOutbox(AuditOutbox.Config)` records
every set and clear durably.

## Transfer metrics and the audit acknowledgement log

`IrohBlobs.transferStats()` returns the node-wide `BlobTransferStats` (C ABI
7.4). The reporting fetches (`downloadHashFromAsync`, custody downloads) ask
for fetch report v2, so `FetchReport.ProviderOutcome` carries `inFlight`,
`plainFallbacks`, `bytesTransferred` and `wireBytes` (`null` when no attempt
went over zstd), and `FetchReport.inFlight()` / `plainOnly()` list providers.
`Topology.PeerView` carries `quicTraffic()` (bytes per protocol family:
`blobs`, `blobs-zstd`, `docs`, `gossip`, `rpc`, `other`) and `blobTransfer()`.

`AuditOutbox.acknowledgements(after, limit)` reads the acknowledgement log
(C ABI 7.1) from an exclusive cursor; `waitAcknowledgements` waits up to an
hour for the next entry, and `watchAcknowledgements(after, consumer)` streams
them until its `Watch` is closed. `Config.withAcknowledgementLog(maxEntries,
maxAgeMs)` bounds the log (defaults 100,000 entries and 7 days).

## Contract types

| Java | Contract type | Wire |
| --- | --- | --- |
| `java.time.Instant` | `timestamp` | Fory TIMESTAMP, truncated to microseconds |
| `java.time.LocalDate` | `date` | Fory DATE |
| `java.math.BigDecimal` | `decimal` | Fory DECIMAL, the value's own scale |
| `BigDecimal` with `@FixedDecimal(precision = p, scale = s)` | `decimal(p,s)` | Fory DECIMAL at scale `s` |
| `java.util.UUID` | `uuid` | 16-byte BINARY, RFC 9562 byte order |
| `site.aster.codec.Ulid` | `ulid` | 16-byte BINARY; `toString()` is Crockford base32 |

Recursive types are `SELF_REF`, as in Rust and Python. `ForyCodec` zero-pads a
`@FixedDecimal` value to its scale and refuses one that would need rounding or
has too many digits, on encode and on decode, without modifying the caller's
object; it refuses a `uuid` / `ulid` of any length but 16. Declare an optional
field with Fory's type-use `@org.apache.fory.annotation.Nullable` (Fory does
not serialize `Optional` in xlang mode); contract identity reads it as
optional. A field's contract name is its declared name, verbatim
(`ffi_spec/Aster-FieldNaming.md`); a class implementing a contract defined in
another language names the field with `@WireName("written_by")` on a
`writtenBy` member. Contract identity refuses a name that is not plain
(`userID`), a member whose Fory snake_case form differs from its name's
(`author` named `writtenBy`, or a member starting with a capital, which Fory
Java writes as `_value`), and two fields with one snake_case form
(`FieldNameVectorTest`). `ContractTypesVectorTest` pins all of this to
`conformance/vectors/contract-types.json`. Fory Java writes collection
elements as nullable and ASCII strings as LATIN1, so payloads holding a list
or a string are not byte-identical to Rust's (they decode both ways); the
scalar encodings are.

## Authenticated HTTPS and WebTransport listeners

`AsterServer.startHttp(upcalls, config)` exposes the server's registered services
over a separately owned TLS listener. Startup returns after TCP and optional
QUIC sockets are bound. Supply the operator's PEM certificate and private key;
`127.0.0.1:0` selects an available port. WebTransport uses that same address and
port over UDP. Both addresses are cached and remain readable after close.

```java
try (Upcalls upcalls = Upcalls.create(server.node().runtime())) {
  upcalls.serveRpcAuthenticator((request, invocation) -> {
    // Verify the credential using your application, then return a stable principal.
    if (!verify(request.metadata().get("authorization"))) {
      return Ucv1.RpcAuthResponse.rejected();
    }
    return new Ucv1.RpcAuthResponse(true, "user/alice",
        Map.of("aster.role", "operator"));
  });
  HttpListenerConfig config = new HttpListenerConfig(
      "127.0.0.1:0", certificatePem, privateKeyPem, true, 256, 65536);
  try (HttpListener listener = server.startHttp(upcalls, config)) {
    System.out.println(listener.tcpAddress());
    listener.stop(Duration.ofSeconds(2));
  }
}
```

The table must belong to the server runtime and have a kind-80 handler registered.
Each call authenticates independently before Gate 3 and session creation.
`RpcAuthRequest` includes the routed service/method, session ID, incoming peer
string, and HTTP metadata. The incoming peer string is not an authenticated
identity. Explicit rejection returns `UNAUTHENTICATED`; callback or encoding
errors, a closed table, or auth timeout fail closed with `UNAVAILABLE`.
Admission failure-mode settings cannot grant access. Auth attributes apply to
that call, and `CallContext.peer()` holds the resolved principal.
`CallContext.irohAuthenticated()` remains false on HTTPS/WebTransport.

Auth callbacks run on upcall workers. Native timeout, transport cancellation,
listener stop, and runtime shutdown interrupt pending callbacks; inspect
`invocation.isCancelled()`, `cancelReason()`, and `deadlineUnixMs()` for context.
Blocking callbacks should preserve interruption. Late answers are discarded.
`HttpListener.stop(grace)` first allows calls to drain, then cancels and joins
remaining native calls/connections before releasing TCP/UDP ports. Close uses
zero grace and is idempotent. Failed stops keep the owner for retry. Server close
also joins child listeners. Explicit runtime close forces child cleanup; close
listeners and upcall tables before the runtime when deterministic port release
is required. Startup and stop are synchronous; run them off latency-sensitive
threads. An interrupted startup fails before binding or cleans up a listener
that finished binding before it reports cancellation.
If cleanup also fails, `HttpListenerStartException` retains `listener()` for a
close retry, the original cause, and `cleanupError()`; addresses can be empty
when startup failed while copying them.

Canonical HTTPS requests use `POST /aster/{service}/{method}` with
`Content-Type: application/aster-frames` and native Aster frames. All four RPC
patterns are supported. HTTPS buffers the request body before dispatch;
WebTransport provides live bidirectional streams. Enabling WebTransport opens
its authenticated native listener; this Java package does not include a
WebTransport client implementation.

Kotlin services generated by `aster-codegen-ksp` use this same JVM runtime and
`AsterServer` API. No separate Kotlin native adapter is required. The Java SAM
authenticator accepts Kotlin lambdas, and `HttpListener`/`Upcalls` support `use`:

```kotlin
Upcalls.create(server.node().runtime()).use { upcalls ->
    upcalls.serveRpcAuthenticator { request, invocation ->
        if (verify(request.metadata()["authorization"]))
            Ucv1.RpcAuthResponse(true, "user/alice", mapOf("aster.role" to "operator"))
        else Ucv1.RpcAuthResponse.rejected()
    }
    server.startHttp(upcalls, HttpListenerConfig("127.0.0.1:0", certPem, keyPem))
        .use { listener -> println(listener.tcpAddress()) }
}
```

## Docs API

`Doc.getExactAsync(author, key)` returns raw `byte[]` values, or `null` when the
entry is absent. It no longer decodes those bytes as a `DocEntry`. Both string
(UTF-8) and arbitrary byte keys are supported. Empty values are rejected by the
native Docs engine.

Queries return metadata through `DocEntry`: `keyBytes()`, `author()`,
`contentHash()`, `contentLen()`, and `timestamp()`. `key()` provides a UTF-8
convenience view; use `keyBytes()` to preserve binary keys. Values are obtained
separately with `readEntryContentAsync(entry.contentHash())`. The length and
timestamp retain the native unsigned 64-bit representation in Java `long` bits.
Malformed, truncated, and trailing payload bytes fail the query.

Use the native query modes `KEY_EXACT`, `KEY_PREFIX`, `LATEST_EXACT`, and
`LATEST_PREFIX`. The former `AUTHOR`, `ALL`, and `PREFIX` names had incorrect
native meanings and have been removed. `queryAsync(mode, key, limit)` supports
a nonnegative limit; zero means unbounded. `LATEST_EXACT` always returns at most
one entry. The two-argument overload remains unbounded.

`deleteAsync(author, prefix)` removes entries under a prefix and returns the
native deletion count. Deletions create tombstones, so deleting the same prefix
again can count a replaced tombstone. An empty prefix deletes all of that
author's entries. `startSyncAsync(List<String> peers)` accepts explicit endpoint
IDs; `startSyncAsync()` remains available. `shareWithAddrAsync(mode)` includes
address hints. `joinAsync(ticket)` imports the capability and hints; call
`startSyncAsync(peers)` to start synchronization.

`subscribeAsync()` returns a separately owned `DocEvents`. Its receiver remains
usable when the original document handle closes. `joinAndSubscribeAsync(ticket)`
now returns `DocSubscription`, which owns both `doc()` and `events()`:

```java
try (DocSubscription subscription = docs.joinAndSubscribeAsync(ticket).get()) {
  DocEvent event = subscription.events().nextAsync(Duration.ofSeconds(10)).get();
  if (event != null && event.entry() != null) {
    byte[] value = subscription.doc()
        .readEntryContentAsync(event.entry().contentHash()).get();
  }
}
```

One receive may be outstanding at a time. `nextAsync()` returns `null` on EOF.
Canceling its future uses standard `CompletableFuture` cancellation and also
requests native cancellation. A committed event racing cancellation is retained
for the next receive. The duration overload settles native cancellation before
reporting timeout; a committed event can win that race. Use this overload instead
of `CompletableFuture.orTimeout` when the receive itself needs a deadline.
Closing the receiver wakes pending receives with EOF, although an already
committed event can still complete successfully. Close every receiver or
`DocSubscription` before closing the node/runtime. Runtime shutdown fails any
outstanding receive with `ALREADY_CLOSED` and rejects subsequent operations.

Focused native validation from this directory:

```sh
IROH_LIB_PATH=/absolute/path/libaster_transport_ffi.dylib \
  mvn -Pfast -pl aster-runtime -am -Dtest=DocsContractTest \
  -Dsurefire.failIfNoSpecifiedTests=false \
  -DargLine=--enable-native-access=ALL-UNNAMED test
```

## Explicit public listeners and certificate helpers

`startPublicHttp(config)` is a separate anonymous API on `AsterServer` and
`Dispatch`. It requires no `Upcalls`. Shared services accept anonymous calls;
attribute requirements and Iroh-only restrictions remain enforced. Session
services are rejected before creating an instance. `startHttp` always requires
an authenticator and never falls back to public mode.

```java
var cert = TlsCertificates.generateWebTransportCertificate("my-server", List.of("localhost"));
var config = new HttpListenerConfig("127.0.0.1:0", cert.certificatePem(), cert.privateKeyPem(), true, 0, 0);
try (HttpListener listener = server.startPublicHttp(config)) {
  System.out.println(listener.tcpAddress());
}
```

`generateSelfSignedCertificate(sans[, nodeSecret])` and
`generateWebTransportCertificate(subject, sans[, nodeSecret])` return PEM
certificate/key bytes and `sha256()` of certificate DER. The optional secret
must contain exactly 32 Ed25519 identity bytes; omission creates a self-signed
certificate without a node identity binding. No unsigned node identity claim is exposed. WebTransport material
uses P-256 and a validity period shorter than 14 days. Keep private keys secret.

`TlsCertificates.verifyCertificateBinding(certificatePem, expectedNodeId)`
throws unless the certificate proves the trusted node owns its TLS key. Obtain
the expected node id from a trusted source. This verifies the signed TLS
public-key binding, not the whole certificate or chain, and checks neither
expiry, hostname nor PKI trust; consumers must apply those TLS
checks separately, or deliberately use the returned DER hash for pinning.
Kotlin uses the same methods and defensive byte-array accessors directly:

```kotlin
val cert = TlsCertificates.generateSelfSignedCertificate(listOf("localhost"), nodeSecret)
TlsCertificates.verifyCertificateBinding(cert.certificatePem(), expectedNodeId)
server.startPublicHttp(HttpListenerConfig("127.0.0.1:0", cert.certificatePem(), cert.privateKeyPem())).use { listener ->
    println(listener.tcpAddress())
}
```

## Blob custody and verified imports

Use `node.blobs().custody()` for temporary GC protection without anonymous
persistent tags. `close()` seals new work; operations admitted before close keep
every protected root alive until their native work and cleanup finish.

```java
try (BlobCustody scope = node.blobs().custody()) {
  BlobId hash = scope.addBytesAsync(bytes, BlobFormat.RAW).get();
  // Validate application metadata while GC cannot remove this root.
  scope.retainAsync(hash, BlobFormat.RAW, "tree/current").get();
}
```

`protectAsync` can protect an absent root before `downloadHashFromAsync`.
`HASH_SEQ` means concatenated 32-byte child hashes; custody protects that graph
and `readAsync` returns its root bytes without imposing a Collection schema.
Retention requires a root held by this scope with the same format.
`addBytesVerifiedAsync(data, expectedHash, format, tag)` and
`addPathVerifiedAsync(path, expectedHash, format, tag)` verify imported bytes
before atomically replacing the exact named tag. File imports copy bytes, never
retain a mutable source reference. A mismatch leaves the old named root intact;
deleting the final named tag makes the data eligible for GC. These methods add
no fsync guarantee beyond the store's existing persistence semantics.

These APIs return `BlobOperation<T>`. Call `requestCancel()` and await the same
operation to acknowledge cleanup. `cancel(boolean)` returns false because an
immediate cancelled future would lose that acknowledgement. Cancellation before
commit publishes no named root; once the tag commit starts, its actual success
or error wins. Fetch cancellation may wait for a stalled native transfer and
other downloader work to drain. Kotlin uses the same scope with `use` and the
same cooperative operation methods; no separate native adapter is required.

## Batch local document content reads

`doc.readEntryContentsIfCompleteAsync(List<BlobId>)` submits one native batch
operation and returns `List<Optional<byte[]>>` in the exact input order,
including duplicates. Missing or initially partial blobs are `Optional.empty()`;
a complete zero-byte blob is a present empty array. Empty input returns an
empty list. Hashes need not come from `DocEntry` objects.

```java
var values = doc.readEntryContentsIfCompleteAsync(
    entries.stream().map(DocEntry::contentHash).toList()).get();
```

This reads only the local store: it fetches nothing and creates no tags or GC
protection. It is not a global snapshot. Storage or integrity errors fail the
whole batch; GC between the completeness check and a large-value export can
also fail the batch. Use blob custody if content must remain available during
host verification. Cancelling the returned future abandons that waiter; native
completion buffers are still released and later reads remain usable. Kotlin
calls the same method and uses `Optional.isEmpty` / `orElseThrow()` for each slot.

## Coordination authority provisioning

`site.aster.coordination.CoordinationProvisioning` provides offline helpers for
canonical genesis/join documents, domain-separated signing preimages, signed
envelopes, canonical candidate tickets, genesis hashes and native verification.
All byte-exact rules run in Rust. Authority private keys stay with your external
signer; these helpers neither sign nor host a coordination voter.

```java
byte[] document = CoordinationProvisioning.canonicalGenesis(input);
byte[] signature = externalSigner.sign(CoordinationProvisioning.genesisSigningBytes(document));
byte[] envelope = CoordinationProvisioning.signedDocumentFromParts(document, authorityPublicKey, signature);
var verified = CoordinationProvisioning.verifyGenesis(trustedAuthorityPublicKey, configuredDomain, envelope);
```

`GenesisInput` and `JoinPermitInput` accept raw id/hash/nonce bytes. Version is
fixed to 1. Rust derives the join candidate's addressless Open ticket. Callers
choose profiles and randomness and supply lists in protocol order; the helpers
do not sort, deduplicate or infer policy. Canonical encoding alone does not
establish that a document is authorized or valid under a deployment profile.

Verify against an independently trusted authority key and configured domain,
never just the key carried by the envelope. `verifyJoinPermitForStart` additionally
checks the candidate; it **extracts** the group/genesis rather than comparing
them with independent expectations. `genesisHash` only hashes supplied bytes.
Verified results are informational metadata, not Rust sealed authority values
or transferable capabilities. Preserve their exact `documentBytes()` when
persisting or forwarding verified material. Returned arrays are defensive copies.
`ProvisioningException.code()` gives the stable native reason, with diagnostic
`detail()`. Kotlin uses the same records and static methods directly.

### Topology views and RPC baseline

Start a node with `enableMonitoring(true)` and obtain `node.topology()` for
`hasMonitoring()`, full local `peers()` / `peer(id)` measurements (including the
local-only remote address; unsigned 64-bit metrics use `BigInteger`), and `enableSharedAsync(secret, options)`. The typed
`SwarmOptions` and `Timing` forward optional refresh, hold, liveness, edge, skew,
coverage and witness rules to Rust. Missing fields use native defaults; zero is
retained except position refresh zero means 60 seconds. Millisecond/rule values
must fit unsigned 32 bits. A canceled join wait may still finish; the node owns
the publisher until close or rejoin.

Use `try (var snapshot = topology.snapshotAsync().get())` for a placement decision:
`clusters()`, `clusterOf(id)` and `separated(a,b)` all query the same immutable
native observation. Close is idempotent, and canceled creation frees a late owner.
`UNKNOWN` is insufficient evidence, never evidence of separation. The convenience
`clustersAsync()`, `myClusterAsync()` and `separatedAsync()` each capture and close
a fresh snapshot; separate calls may observe different eventual views. Snapshots
are local evidence, not authorization proofs. Kotlin uses these Java APIs directly
with `use`.

The remote `aster.net.Topology` baseline is disabled by default. Enable it with
`AsterServer.builder().builtinTopology(true)` and optionally
`.topologyRequires(new CapabilityRequirement(CapabilityKind.ROLE, List.of("operator")))`.
Enabling requires monitoring. Rust enforces the role requirement; the RPC peer view
omits local socket addresses. Low-level `Dispatch.Builder.topology(enabled, requirement)`
provides the same explicit opt-in and policy. These additions require C ABI 4.16.

Standalone raw WebTransport (C ABI 4.17) is available as
`site.aster.webtransport.RawWebTransport`. Generate browser-compatible PEM using
`TlsCertificates.generateWebTransportCertificate`, then use `start(runtime,
upcalls, config)` for foreign pre-upgrade authentication or explicit
`startPublic(runtime, config)`. Listener/session/stream owners are AutoCloseable.
Sessions expose bi/uni accept/open, datagrams, current peer and authenticated
principal/attributes, close info, and network statistics. Stream reads return
bytes plus EOF; FIN retains the send half for `transportAcked()` or reset.
Higher signed priorities are served first. Cancelling a returned future cancels
native work immediately; a cancelled write may have delivered a prefix and must
not be replayed automatically. Session `close(code, reason)` actively closes
QUIC while retaining its owner for stats; `close()` frees the owner.
