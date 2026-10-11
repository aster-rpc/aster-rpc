# Python HTTPS, WebTransport and certificate helpers

Python services registered on `AsterServer` can also serve through the shared
Rust HTTP/WT dispatcher. `start_http` binds an owned listener using
operator-supplied PEM and an explicit authenticator. It starts the Python RPC
pump and returns after TCP and optional QUIC are bound.

```python
import asyncio
from pathlib import Path

from aster import AsterServer, RPCAuthRequest, RPCAuthResponse


async def authenticate(request: RPCAuthRequest) -> RPCAuthResponse:
    # Replace this demonstration token with your credential backend.
    if request.metadata.get("authorization") != "Bearer secret":
        return RPCAuthResponse()  # reject
    return RPCAuthResponse("alice", {"aster.role": "operator"})


async def run(services, shutdown: asyncio.Event):
    async with AsterServer(services=services, allow_all_consumers=True) as server:
        listener = await server.start_http(
            authenticator=authenticate,
            certificate_pem=Path("server.pem").read_bytes(),
            private_key_pem=Path("server-key.pem").read_bytes(),
            bind_addr="127.0.0.1:0",
            webtransport=True,
            max_pending_auth=256,
            max_auth_payload_bytes=65536,
        )
        try:
            print(listener.tcp_addr, listener.quic_addr)
            await shutdown.wait()
            await listener.stop(grace=2.0)
        finally:
            await listener.aclose()
```

`allow_all_consumers` in this example selects the Iroh admission policy. The
HTTP authenticator is a separate policy on a dispatcher clone; it does not
change Iroh admission. A lower-level `aster.Server` also exposes `start_http`
with the same arguments. Services, Fory payloads, all four RPC patterns, Gate 3,
sessions and deadlines use the existing native dispatch path.

The authenticator may be a function or an object implementing
`authenticate(request)`, synchronous or async. It receives a typed
`RPCAuthRequest` containing `peer_id`, `service`, `method`, `session_id` and
`metadata`. `RPCAuthResponse()` rejects; acceptance requires a nonempty string
principal. Attributes apply to that call's Gate 3 checks. Exceptions, malformed
answers, missing handlers, overload, timeout and closed upcall channels fail
closed, even under a `fail_open` upcall policy. Native capacity remains occupied
until an answer, cancellation or timeout; both encoded request and response
sizes are bounded. `peer_id` is an address hint, and application authentication
does not make HTTP/WT into an Iroh-authenticated transport. The handler's
`CallContext.iroh_authenticated` remains false.

Callbacks run on the event loop that starts the listener. Keep it running, and
let async callbacks observe `asyncio.CancelledError` when Rust stops waiting.
For custom callback deadlines, pass an `aster.upcalls.UpcallRuntime` through
`upcall_runtime=` and configure its `rpc_authenticate` policy before starting.
Each listener gets a dedicated auth channel, which is unbound after stop.

HTTP clients POST canonical Aster request frames to
`/aster/{service}/{method}` with `Content-Type: application/aster-frames` and
credentials in ordinary HTTP headers. Responses contain data frames followed
by an Aster status trailer. WebTransport clients connect to `/aster/wt` and open
one bidirectional stream per RPC. Put credentials in **each call's**
`StreamHeader.metadata_keys` / `metadata_values`, for example
`["authorization"]` / `["Bearer secret"]`. Credentials in the WT session
handshake do not authorize its streams. The listener authenticates every RPC
before Gate 3 and HTTP-scoped session creation.

With WebTransport enabled, `quic_addr` equals the actual `tcp_addr`, including
the assigned port when binding to `:0`; otherwise it is `None`. No readiness
sleep or retry is needed after startup. `stop(grace)` stops new calls, allows
application work to drain, then cancels remaining work and joins native cleanup.
QUIC close/drain and final socket release may extend beyond the application
grace. Success guarantees released TCP/UDP ports. Cancelling a stop retains the
native owner: call stop again to finish cleanup. `aclose()` uses zero grace;
`async with listener` calls it on exit. Server close joins its listeners before
stopping the dispatch pump. Dropping an owner schedules forced native cleanup;
await close for a completed cleanup guarantee.

This Python surface currently accepts PEM only. It has no ACME ownership,
static mount/projection configuration or Python HTTP/WT client wrapper. The
shared Rust transport's static mounts remain public files when configured there. See the [HTTP transport guide](../../docs/aster-http-getstarted.md)
for wire and TLS details.

Integration tests use a pinned test-only HTTP/3 client:

```bash
uv pip install -r bindings/python/requirements-test.txt
PYTHONPATH=bindings/python uv run --no-sync pytest tests/python/test_http_listener.py tests/python/test_http_public_tls.py -v --timeout=30
```


Use `start_public_http` when anonymous calls are intentional. It never invokes an
application authenticator, supplies empty attributes, preserves Gate 3 and
Iroh-only requirements, and rejects all session-scoped services before creating
an instance. Caller addresses, Authorization headers and session IDs grant no
identity or authority. It has the same readiness and cleanup semantics as
`start_http`; omitting that API's authenticator does not select public mode.

```python
from aster import generate_webtransport_certificate

cert = generate_webtransport_certificate("local-demo", sans=["127.0.0.1"])
listener = await server.start_public_http(
    certificate_pem=cert.certificate_pem,
    private_key_pem=cert.private_key_pem,
    webtransport=True,
)
try:
    print(listener.tcp_addr)
finally:
    await listener.aclose()
```

`generate_self_signed_certificate(sans=None, node_secret=None)` and
`generate_webtransport_certificate(subject, sans=None, node_secret=None)` return
`GeneratedCertificate` with `certificate_pem`, `private_key_pem` and raw 32-byte
`sha256` of certificate DER. The WT helper generates P-256 material valid for
less than 14 days; use the digest in browser `serverCertificateHashes`. Keep the
private key secret. These self-signed certificates do not establish CA trust.

An optional exact 32-byte Ed25519 `node_secret` adds a signed node binding.
`verify_certificate_binding(certificate_pem, expected_node_id)` returns `None`
on success and raises `ValueError` on failure. Obtain the expected node ID from
a trusted channel. The signature proves that node authorized the TLS public key
(SPKI), not the entire certificate; hostname, validity and certificate trust
remain separate checks. Unsigned material cannot pass binding verification.


## Blob custody and verified retained imports

`blobs_client(node).custody()` owns temporary GC protection for multiple
`(hash, Raw|HashSeq)` roots. Pin a known root before fetching it and add roots
discovered during verification. These guards create no anonymous or staging
persistent tags. HashSeq means a sequence of hashes; these APIs do not impose
the Collection filename/metadata schema.

```python
from aster import BlobFormat, blobs_client

blobs = blobs_client(node)
async with blobs.custody() as scope:
    await scope.protect(expected_root, format=BlobFormat.HASH_SEQ)
    await scope.download(expected_root, providers, format=BlobFormat.HASH_SEQ)
    root_bytes = await scope.read(expected_root)  # raw root bytes
    # Verify the application manifest and discover any additional roots here.
    await scope.protect(discovered_hash, format=BlobFormat.RAW)
    await scope.retain(expected_root, "verified/manifest", format=BlobFormat.HASH_SEQ)
```

`scope.add_bytes(data, format=...)` and `scope.add_path(path, format=...)` import
under temporary custody; paths always use Copy, so later source edits cannot
corrupt store-owned content. `scope.read` protects its raw root and preserves
any existing HashSeq guard. `scope.download` fetches to the store and returns
`FetchReport`. `scope.retain(hash, tag_name, format=...)` requires that exact
root and format to be protected, then sets/replaces the named tag. Scope guards
remain until close and all admitted native work completes.

For a single verified handover use
`await blobs.add_bytes_verified(data, expected_hash, tag_name, format=...)` or
`await blobs.add_path_verified(path, expected_hash, tag_name, format=...)`.
The store computes the hash, compares it to the expected hash, and commits the
exact named root only on a match. `ValueError` reports mismatch; the previous
tag remains intact. There is no anonymous persistent root. Both methods
return the computed hash string on success.

`scope.close()` rejects new work immediately; work admitted before close
retains all guards until completion. `await scope.aclose()` additionally joins
that work and releases temporary custody, and the async context manager uses
it on exit. Cancellation requests are cooperative: Python waits for the
native terminal before reporting cancellation. If a named-tag commit has
already started, its actual success/error wins the cancellation race. Canceling
a fetch may wait for native downloader drain, including other active downloads;
this API does not promise prompt interruption of a stalled native transfer.
Abandoned waits still leave native workers owning their guards through cleanup.
Named tag replacement and clean node-shutdown persistence use the existing
store semantics; this API adds no stronger fsync or power-loss guarantee.

## Local Docs batch reads

`await doc.read_entry_contents_if_complete(content_hashes)` returns one
`bytes | None` value for every input hash, preserving order and duplicates.
Pass `Hash` objects or 64-character ASCII hexadecimal strings. All hashes are
validated before reading; malformed input fails the whole call. An empty input
returns `[]`. A locally complete empty blob returns `b""`; missing or partial
content returns `None`.

```python
values = await doc.read_entry_contents_if_complete([first_hash, second_hash, first_hash])
for content_hash, value in zip([first_hash, second_hash, first_hash], values):
    if value is not None:  # b"" is a valid complete value
        verify_local_value(content_hash, value)
```

The method makes one native batch request and reads only the local blob store.
It does not fetch from peers, create tags, or retain content. Inline values use
the store's batch transaction; larger values use validated export. This is not
a global snapshot: GC or local deletion may race a later export. Storage,
integrity and export failures raise an error for the whole call, rather than
returning partial results or disguising an error as `None`. Keep a blob custody
scope protecting the roots when reads and host verification need stable
retention. No document entry lookup is required when the content hashes are
already known.

## Coordination authority provisioning

`aster.provisioning` provides synchronous helpers for founding a group and
minting join permits using the same canonical Fory bytes and verification as
Rust consumers. No node or runtime is required. Keys, nonces, group policy,
signing, publication and persistence remain with the provisioning application.

```python
from aster import (GenesisSpec, GenesisVoter, canonical_candidate_ticket,
                   canonical_genesis, genesis_signing_bytes,
                   signed_document_from_parts, verify_genesis)

spec = GenesisSpec(
    group_id=group_id.hex(), authority_domain="production.example",
    genesis_nonce=nonce.hex(), deployment_profile_code=2,  # DevSingle
    initial_voters=[GenesisVoter(voter_id.hex(), canonical_candidate_ticket(voter_id))],
    initial_admins=[admin_id.hex()],
)
document = canonical_genesis(spec)
signature = external_signer.sign(genesis_signing_bytes(document))
signed = signed_document_from_parts(document, configured_root_key, signature)
verified = verify_genesis(configured_root_key, "production.example", signed)
publish_signed_genesis(signed, verified.genesis_hash)
```

Fixed byte fields in specs are exactly 64 ASCII hexadecimal characters;
`recovery_export_hash` is an optional 32-byte digest in that form. Input JSON
DTOs reject unknown fields and are bounded to 1 MiB. Format version 1 is selected
natively. Encoding preserves supplied order and does not sort or deduplicate
sets, choose deployment profiles, validate governance, or prove authority.
Verification enforces the existing native profile/count, sorted unique sets,
ticket, ROOT separation, signature and canonical spelling rules. Obtain the
configured authority key and domain through a trusted channel, never from the
untrusted envelope itself.

`canonical_join_permit(JoinPermitSpec(...))` derives the candidate's canonical
addressless Open ticket natively. Sign its bytes using `join_signing_bytes`,
then assemble the same signed envelope. `verify_join_permit_for_start(key,
domain, candidate_id, signed)` verifies the configured pins and local candidate,
but **extracts** the group and genesis from the verified permit; it does not
compare independently expected group/genesis values. A permit alone does not
add a member or bypass the current quorum's membership governance. There is no
invented expiry, randomness, signing-secret storage or authority policy.

`verify_genesis` and `verify_join_permit_for_start` return immutable informational
summaries of the native verified accessors; these Python objects are not native
authority seals. `ProvisioningError.code` is a stable machine-readable native
reason, and `.detail` retains diagnostics such as `UnknownProfile(99)`.
`genesis_hash(document)` returns a 32-byte digest of supplied bytes, and the two
signing-preimage helpers accept opaque bytes: none of those three calls proves
that a document is valid. Byte-exact cross-language vectors are maintained in
`tests/fixtures/coordination_provisioning_v1.json`; their embedded test key is
public fixture material and must never be used as a deployment authority.

## Topology observations and the RPC baseline

Enable monitoring when starting the node to obtain local proximity observations:

```python
from aster import AsterConfig, Node, TopologyTiming

node = await Node.start(AsterConfig(enable_monitoring=True))
topology = node.topology()
assert topology.has_monitoring
for peer in topology.peers():
    print(peer.node_id, peer.level, peer.rtt_us, peer.confidence)

# The namespace secret and the admitted peers come from application policy.
await topology.enable_shared(
    namespace_secret, bootstrap=bootstrap_ids,
    timing=TopologyTiming(min_hold=30.0), admitted=is_admitted,
)
snapshot = await topology.snapshot()
verdict = snapshot.separated(first_node_id, second_node_id)
await node.shutdown()
```

`peer(id)` returns `None` for an unknown peer. RTT, jitter, relay and local
socket address can be absent; an absent measurement is not zero latency.
Local peer observations include the socket address, while the native
`aster.net.Topology` RPC intentionally excludes that local-only field. A shared
snapshot is immutable and remains readable after node shutdown. Its
`cluster_of(id)` returns `None` when the node is not represented. Separation is
`"unknown"`, `"connected"`, or `"separated"`: insufficient live coverage remains
unknown, and must not be interpreted as evidence of distance. Snapshot reads
use one derived view; separate live queries can observe different views.

Shared topology requires monitoring. Timing values use seconds, retain native
defaults when omitted, and reject negative, non-finite or unrepresentable
durations. Zero `position_refresh` keeps the native 60-second default. Rejoining
replaces the node-owned publisher; shutting down the node stops publishing even
if callers retain topology or snapshot objects. Canceling an enable operation
does not promise rollback of a join that has already begun.

The optional RPC baseline exposes the same native `peers`, `peer`, `clusters`
and `separated` methods. It is disabled by default and requires monitoring when
enabled. Configure it explicitly on either `Server` or `AsterServer`:

```python
from aster import AsterConfig, AsterServer, any_of

async with AsterServer(
    services=application_services,
    config=AsterConfig(enable_monitoring=True),
    builtin_topology=True,
    topology_requires=any_of("ops", "admin"),
) as server:
    await run_application(server)
```

`topology_requires` also accepts a single role or `all_of(...)`. Native Gate 3
checks the caller's admission attributes for every baseline call. Enabling
this service does not grant roles or bypass admission. Without a shared join,
the RPC cluster list is empty and separation is Unknown. The baseline uses
the native Fory payloads and is not a new discovery or placement protocol.
