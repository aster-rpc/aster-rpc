# Aster .NET binding

The .NET API wraps Aster's shared Rust engine through the C ABI. This checkout
targets .NET 9 and requires native ABI **4.17 or newer within major 4**.
`Runtime` checks compatibility when it opens. Supply the native library through
the repository build or an explicit build property:

```sh
dotnet test src/Aster.Tests/Aster.Tests.csproj \
  -p:NativeLibraryPath=/absolute/path/to/native/library/directory \
  --filter 'FullyQualifiedName!~SoakTests'
```

This is a checkout build instruction; it does not establish public NuGet or
native package availability. The project copies the native library into build
output. Without an override it looks in `target/release`, then `target/debug`.

## Authenticated HTTPS and WebTransport serving

An existing `AsterServer` can serve its registered RPC services through HTTPS
and optionally WebTransport. Register a typed authenticator on an upcall table
from that server's runtime before starting the listener:

```csharp
using Aster;
using Aster.Upcall;

// server is an AsterServer with registered services.
await using var upcalls = Upcalls.Create(server.Node.Runtime);
upcalls.ServeRpcAuthenticator(async (request, cancellationToken) => {
    // Validate credentials against your application's identity provider.
    var identity = await ValidateCredentialsAsync(request.Metadata, cancellationToken);
    return identity is null
        ? RpcAuthResponse.Rejected()
        : new RpcAuthResponse(true, identity.Subject,
            new Dictionary<string, string> { ["aster.role"] = identity.Role });
});
upcalls.SetPolicy(UpcallKind.RpcAuthenticate, TimeSpan.FromSeconds(2),
    HookFailureMode.FailClosed);

await using var listener = await server.StartHttpListenerAsync(new HttpListenerOptions {
    BindAddress = "127.0.0.1:0",
    CertificatePem = await File.ReadAllBytesAsync("certificate.pem"),
    PrivateKeyPem = await File.ReadAllBytesAsync("private-key.pem"),
    WebTransport = true,
    MaxPendingAuth = 256,
    MaxAuthPayloadBytes = 65536,
}, upcalls, cancellationToken);
Console.WriteLine($"HTTPS {listener.TcpAddress}; WebTransport {listener.QuicAddress}");
// Later, stop accepting, allow application work to drain, then join cleanup.
await listener.StopAsync(TimeSpan.FromSeconds(5));
```

`ValidateCredentialsAsync` and `identity` above belong to your application.
This authenticated API requires authentication for every HTTP/WT RPC.
An accepted result requires a nonempty principal.
Rejections, callback failures, malformed outcomes and timeouts fail closed,
even if the upcall policy requests fail-open behavior. Rust applies service and
method capability requirements to the returned attributes. Handlers receive
that principal in `ServerCallContext.PeerId`, those per-call attributes in
`Attributes`, and `IrohAuthenticated == false`. `SetPeerAttributes` applies to
authenticated Iroh peers; it does not override foreign authentication results.
HTTPS credentials come from request headers; WebTransport credentials come
from each RPC's `StreamHeader.Metadata`, so opening a transport session alone
does not authenticate later RPCs.

Start returns an `AsterHttpListener` only after TCP and optional QUIC sockets
bind. QUIC uses the TCP listener's assigned address and port. Only PEM
certificate/key inputs are supported. Zero authentication limits select the
defaults shown above. HTTPS serves all four RPC patterns at
`POST /aster/{service}/{method}` with `application/aster-frames`; HTTP request
bodies are currently buffered before dispatch. This binding has no managed
WebTransport client; protocol interoperability is covered by shared native
tests, while .NET tests cover HTTPS patterns and QUIC socket ownership.

Dispose listeners before their upcall table and server. The listener retains
its runtime until successful stop, and server disposal also joins its native
listeners. A child wrapper still needs disposal to release its retained
runtime. `StopAsync` is idempotent and preserves ownership on native errors so
cleanup can be retried. Its grace period bounds application draining; QUIC
protocol and socket cleanup can extend beyond it. Cancellation is honored
before native stop begins; after that the method awaits joined cleanup.
Cancellation during startup waits for cleanup of any late-created listener.
If startup and cleanup both fail, `HttpListenerStartException.Listener`
provides the owned listener for a cleanup retry.

### Explicit public serving and certificate helpers

ABI 4.12 adds `StartPublicHttpListenerAsync`, a separately named API with no
upcall table. Missing authentication on `StartHttpListenerAsync` still fails.
Public calls have empty attributes and no Iroh-authenticated provenance;
Gate 3 and Iroh-only checks still apply. All session-scoped services return
`UNAUTHENTICATED` before their factories run. Both authentication-limit fields
must be zero. The public listener has the same readiness, cancellation and
joined cleanup contract as an authenticated listener.

```csharp
var cert = TlsCertificates.GenerateWebTransportCertificate("my-service",
    ["localhost", "127.0.0.1"]);
await using var listener = await server.StartPublicHttpListenerAsync(new HttpListenerOptions {
    CertificatePem = cert.CertificatePem,
    PrivateKeyPem = cert.PrivateKeyPem,
    WebTransport = true,
}, cancellationToken);
// cert.Sha256 hashes the complete DER certificate for serverCertificateHashes.
```

`TlsCertificates.GenerateSelfSignedCertificate(sans, nodeSecret)` produces
ECDSA material, with `localhost` when SANs are empty. The WebTransport variant
uses P-256, validity at most 14 days and includes `localhost`. Its subject is a
display name. Omit `nodeSecret` for an unbound certificate or supply exactly
32 bytes of an Aster node secret to sign its TLS public-key binding. No unsigned
node-identity claim is emitted. Protect the returned private-key PEM and
distribute the certificate hash through a trusted channel.

`TlsCertificates.VerifyCertificateBinding(cert.CertificatePem, trustedNodeId)`
verifies a signed binding against a node ID learned independently through a
trusted channel. It proves the TLS public-key binding, not the whole
certificate, TLS chain, hostname, validity or trust in the expected ID.
Generation is not ACME and makes no FIPS validation claim. Owned ACME,
static-mount/projection configuration and managed HTTP/WT clients remain
outside this surface.

## Document metadata and sync

```csharp
await using var node = await Node.StartAsync(new AsterConfig {
    RelayMode = "disabled", BindAddr = "127.0.0.1:0"
});
using var doc = await node.Docs().CreateAsync();
string author = await node.Docs().CreateAuthorAsync();
await doc.SetBytesAsync(author, "manifest/root", "value"u8.ToArray());

var entries = await doc.QueryAsync(DocQueryMode.KeyPrefix, "manifest/");
foreach (var entry in entries) {
    // Key is binary; ContentLen and Timestamp preserve native u64 values.
    byte[] content = await doc.ReadEntryContentAsync(entry.ContentHash);
}
ulong removed = await doc.DeleteAsync(author, "manifest/");
string ticket = await doc.ShareWithAddrAsync(mode: 0); // 0 read, 1 write
```

`QueryAsync` returns metadata without fetching content blobs. The four modes
are `KeyExact`, `KeyPrefix`, `LatestExact`, and `LatestPrefix`. A zero `limit`
means unbounded; `LatestExact` returns at most one entry. `DocEntry.Timestamp`
uses microseconds since the Unix epoch. Query and deletion also accept binary
keys/prefixes. An empty deletion prefix deletes all entries by that author.

To start sync with explicit peers, supply their endpoint-ID hex strings to
`doc.StartSyncAsync(peerIds)`. Teach the node routing addresses with
`node.AddNodeAddr(peer.NodeAddr())` first. `LeaveAsync` stops syncing.
`ShareWithAddrAsync` includes relay and direct-address hints in a Doc ticket;
`Docs.JoinAsync` opens that ticket's namespace.

## Live document events

```csharp
using var events = await doc.SubscribeAsync(cancellationToken);
await foreach (var ev in events.ReadAllAsync(cancellationToken)) {
    if (ev.Kind == DocEventKind.InsertRemote) {
        Console.WriteLine($"{ev.PeerId}: {ev.Entry!.ContentHash}");
    }
}

// Atomic join and subscription returns two independently owned handles.
var joined = await node.Docs().JoinAndSubscribeAsync(ticket, cancellationToken);
using var joinedDoc = joined.Doc;
using var joinedEvents = joined.Events;
```

Dispose each event receiver, including receivers returned by atomic joins.
`Dispose` or `DisposeAsync` releases its native subscription independently
from the Doc handle and wakes pending receives. `ReceiveAsync` returns one
typed event or `null` at EOF; `ReadAllAsync` ends at EOF and leaves receiver
ownership with the caller. Multiple receives on one receiver are serialized
by the native engine. Dispose is idempotent.

Receivers and active receive calls retain their runtime, so disposing the node
does not strand a live receiver. Release the receiver to wake its idle reads
and allow final runtime shutdown. Final runtime close faults any remaining
registered operation waits with `ObjectDisposedException` and rejects new
registrations.

Receive cancellation leaves the receiver usable and does not consume a later
event. Cancellation waits for the native terminal response: an already
committed event can win a racing cancellation and be returned successfully.
If cancellation wins, `ReceiveAsync` throws `OperationCanceledException`.
Cancellation of subscribe or atomic join likewise preserves a committed
successful result so callers can dispose its returned owners.

## File import and retention

```csharp
string hash = await node.Blobs().AddPathWithNamedTagAsync(
    "/absolute/path/to/file", "portal/tree/verified", cancellationToken);
await node.Tags().DeleteAsync("portal/tree/verified", cancellationToken);
await node.Blobs().GcRunOnceAsync(cancellationToken);
```

`AddPathAsync` copies a UTF-8 file path into the store and creates an automatic
persistent retention tag. `AddPathWithNamedTagAsync` uses exactly the supplied
UTF-8 tag name and replaces that named root. It creates no extra automatic
tag; deleting the named tag permits GC when nothing else retains the content.
Later source-file edits or deletion do not change imported bytes.

These imports return the store-computed hash. They do not check a caller's
expected hash before updating the named root. Pre-canceled calls do not start
an import; cancellation after submission abandons the wait and an import or
tag update already running may still finish. Temporary custody and verified
retained imports are separate ABI 4.13 capabilities described below.

## Temporary custody and verified retained imports (ABI 4.13)

```csharp
using var scope = node.Blobs().CreateCustodyScope();
await scope.ProtectAsync(expectedHash);
var fetched = await scope.DownloadHashFromAsync(expectedHash, providers, ct: ct);
// Verify application-specific content in fetched.Data while scope is alive.
await scope.RetainAsync(expectedHash, "verified/object", ct: ct);
```

A scope protects multiple `(hash, Raw|HashSeq)` roots before local bytes exist.
`AddBytesAsync` and `AddPathAsync` copy data under temporary protection without
an automatic persistent tag. Scoped `ReadAsync` and `DownloadHashFromAsync`
retain all guards through accepted operations if disposal races completion;
disposal seals new submissions. Keep the scope alive for host-side verification
and for old read/fetch APIs outside the scope. HashSeq protects its graph without
requiring Collection metadata, and reads return raw sequence bytes.

`Blobs.AddBytesVerifiedAsync(data, expectedHash, tagName)` and
`AddPathVerifiedAsync(path, expectedHash, tagName)` compute the copied content's
hash in the store, commit exactly the named tag only on a match, and preserve its
old root on mismatch. No anonymous persistent tag is created. Named replacement
and restart durability retain the existing store semantics; no stronger
fsync/power-loss guarantee is added.

Cancellation waits for the sole native terminal outcome and cleanup. Once tag
commit begins, its actual success/error wins cancellation. Fetch cancellation
can wait for downloader/network drain and is not a prompt deadline for a stalled
peer. Dispose scopes after verification or successful named retention, even if
the node has already been disposed.

## Locally complete batch Docs reads (ABI 4.14)

```csharp
IReadOnlyList<byte[]?> values = await doc.ReadEntryContentsIfCompleteAsync(
    [firstHash, missingHash, emptyHash, firstHash], ct);
// Null is missing/partial; a non-null zero-length array is complete empty data.
```

One native operation returns ordered, copied values with duplicates preserved.
No fetch, tags or retention are added. Empty input returns an empty list; any
hash that is not exactly 64 hexadecimal characters rejects the entire request.
Missing/partial blobs at the initial check are null. Inline complete values are
copied in the store transaction; larger values use validated export. The batch
is not a global snapshot: GC/deletion before large export can fail it as a
whole, and I/O/integrity errors are never turned into null slots. Keep a custody
scope alive when stable host-side verification is required.

## Coordination authority provisioning (ABI 4.15)

`Aster.Coordination.Provisioning.CanonicalGenesis(GenesisDocument)` and
`CanonicalJoinPermit(JoinPermitDocument)` return native canonical Fory bytes.
Identifiers/hashes/nonces use hex32 strings. Input ordering is preserved; native
validation never sorts lists or selects a deployment profile. `GenesisSigningBytes`
and `JoinSigningBytes` return exact preimages for your external Ed25519 signer;
`SignedDocumentFromParts` assembles the returned signature (raw key32/signature64).
`CanonicalCandidateTicket` derives the native open ticket, while `GenesisHash`
hashes opaque bytes without asserting authority.

`VerifyGenesis` pins an independently configured key and domain. Never use the
key supplied beside a signature as the trust anchor. `VerifyJoinPermitForStart`
also pins the candidate but extracts group/genesis: compare independently expected
values where required. Summaries are copied metadata, not native sealed authority
handles. No helper creates a group, starts a voter or commits membership.
`ProvisioningException.Code` is stable; `Detail` is diagnostic native text.

## Local and shared Topology (ABI 4.16)

`node.Topology` exposes `HasMonitoring`, `Peers()` and `Peer(id)` without opening
an RPC. Start with `AsterConfig.EnableMonitoring = true` for passive observations.
RTT/jitter retain nullable u64 values; unknown peers return null. `RemoteAddr` is
local-only and deliberately absent from baseline RPC.

`EnableSharedAsync(secret32, filter, bootstrap, options, ct)` joins the shared
namespace; null admission filter admits every secret holder. Nullable uint32
millisecond/coverage options retain native defaults when omitted. Explicit zero
is preserved except position refresh zero means 60s. Canceling a join wait does
not roll back installed publishing work, which remains owned by the node.

Dispose the `TopologyObservation` from `SnapshotAsync`. All its `Clusters()`,
`ClusterOf(id)` and `Separated(a,b)` queries answer from the same immutable
native observation, even after later views or node shutdown. Unknown is a real
answer, never evidence of distance. Live `ClustersAsync`, `MyClusterAsync` and
`SeparatedAsync` capture separately and may observe different eventual states;
use one snapshot for a placement decision. Observations are local evidence, not
serialized authorization proof.

Baseline `aster.net.Topology` RPC remains off by default. Opt in with
`AsterServerOptions.Topology = true` and an explicit `TopologyRequirement`
(e.g. `CapabilityRequirement.Role("observer")`). Monitoring is required and
native Gate 3 enforces the requirement before RPC handlers.


## Raw WebTransport (ABI 4.17)

`RawWTListener.StartAsync(runtime, options, upcalls)` authenticates before the
CONNECT upgrade using `ServeRpcAuthenticator`. `StartPublicAsync` explicitly
serves anonymous sessions. Metadata includes the exact route and unnormalized
query; remote addresses are diagnostic, not authenticated Iroh provenance.
Accepted sessions retain the authenticated principal and attributes in `Info`.
This is a separate listener, not arbitrary HTTP/RPC router composition.

`AcceptAsync` returns an owned session. `OpenBiAsync` / `OpenUniAsync` and
`AcceptBiAsync` / `AcceptUniAsync` return directional raw byte streams. Bounded
`ReadAsync` returns null at EOF; it does not add RPC framing. `WriteAsync`
cancellation can leave a sent prefix, so never retry automatically. `FinishAsync`
queues FIN while retaining the send owner. `TransportAckedAsync` separately
proves transport receipt; false means reset or loss. `ResetAsync` is explicit
and remains available after FIN. Priority is signed Quinn priority, larger first.
Cancellation requests native cancellation and collects its terminal event;
a committed success can win the race and still belongs to the caller.

`SendDatagram` / `ReceiveDatagramAsync` preserve empty messages.
`MaxDatagramSize` is nullable, distinguishing unavailable support from zero.
`Info`, `Stats` and `ClosedAsync` expose versioned copied snapshots.
`CloseConnection` actively closes the underlying QUIC connection, including
other sessions on it. Dispose streams and sessions, then the listener and
borrowed upcalls, before the runtime. Listener stop joins cleanup beyond its
application grace budget. First-stream proof-of-possession and media gating
remain application responsibilities.
