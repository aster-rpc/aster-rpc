# @aster-rpc/aster

Aster RPC framework for TypeScript — peer-to-peer services with typed
payloads, streaming, and enrollment-based admission, built on [Iroh](https://github.com/n0-computer/iroh)
(QUIC, blobs, docs, gossip).

Runs on Node.js 20+, Bun 1.0+, and Deno via Node compat.

---

## Install

```sh
npm install @aster-rpc/aster
# or
bun add @aster-rpc/aster
```

## Defining a service

Write a class with `@Service`, wire types with `@WireType`, and
methods with `@Rpc` / `@ServerStream` / `@ClientStream` / `@BidiStream`.
The scanner derives the schema from method parameter and return types.

```ts
import {
  Service, Rpc, ServerStream, WireType,
  type i64,
} from '@aster-rpc/aster';

@WireType('mission/StatusRequest')
export class StatusRequest {
  agentId = '';
  nonce: i64 = 0n as i64;
}

@WireType('mission/StatusResponse')
export class StatusResponse {
  status = '';
  uptime: i64 = 0n as i64;
  warnings: string[] = [];
}

@WireType('mission/StatusEvent')
export class StatusEvent {
  at = new Date();
  status = new StatusResponse();
  note?: string;
}

@Service({ name: 'MissionControl', version: 1 })
export class MissionControlService {
  @Rpc({ timeout: 30, idempotent: true })
  async getStatus(req: StatusRequest): Promise<StatusResponse> {
    const res = new StatusResponse();
    res.status = 'running';
    return res;
  }

  @ServerStream()
  async *watchStatus(req: StatusRequest): AsyncGenerator<StatusEvent> {
    yield new StatusEvent();
  }
}
```

## Wire types are TypeScript types

Field types map to wire types per spec §11.3.2.3:

| TS type | Wire type | Notes |
|---|---|---|
| `number` | `float64` | `number` is IEEE 754 double |
| `bigint` | `int64` | Use for integers on the wire |
| `boolean` | `bool` | |
| `string` | `string` | |
| `Uint8Array` | `binary` | |
| `Date` | `timestamp` | |
| `T[]`, `Array<T>`, `ReadonlyArray<T>` | `list<T>` | |
| `Map<K,V>` | `map<K,V>` | |
| `Set<T>` | `set<T>` | |
| `T \| null`, `T \| undefined`, `field?:` | `nullable<T>` | All three forms collapse to the same wire type |
| class tagged `@WireType('foo/Bar')` | ref | Transitive wire types auto-discovered |

**For narrower integers**, import the branded aliases and annotate
the field:

```ts
import type { i32, u32, f32 } from '@aster-rpc/aster';

@WireType('billing/Quantity')
export class Quantity {
  count: i32 = 0 as i32;
  scale: f32 = 1.0 as f32;
}
```

Available brands: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`,
`u64`, `f32`, `f64`. `i64` / `u64` wrap `bigint`; the rest wrap
`number`.

**Suspicious-field warning.** A plain `number` field whose name
matches `count|id|size|length|index|offset|timestamp|epoch|...`
emits a build-time warning suggesting `bigint` or a brand. Suppress
by fixing the type or explicitly annotating with `f64`.

## Running the scanner

Wire types are erased at runtime, so `@aster-rpc/aster` ships a
build-time scanner — `aster-gen` — that walks your `tsconfig.json`,
reads field and parameter types from the TypeScript compiler API,
and emits a `aster-rpc.generated.ts` file with ready-made service +
wire-type metadata.

```sh
bunx aster-gen
# or
npx aster-gen
```

Defaults: reads `./tsconfig.json`, writes `./aster-rpc.generated.ts`.
Override with `-p` / `--project` and `-o` / `--out`:

```sh
bunx aster-gen -p tsconfig.app.json -o build/aster-rpc.generated.ts
```

Add it to `package.json` so builds and CI always regenerate:

```json
{
  "scripts": {
    "gen": "aster-gen",
    "build": "aster-gen && tsc"
  }
}
```

### Vite

```ts
// vite.config.ts
import { defineConfig } from 'vite';
import { asterGen } from '@aster-rpc/aster/vite-plugin';

export default defineConfig({
  plugins: [
    asterGen({
      project: 'tsconfig.json',
      out: 'aster-rpc.generated.ts',
    }),
  ],
});
```

The plugin regenerates on every `buildStart` and on HMR when a `.ts`
file changes.

### Webpack

```js
// webpack.config.js
const { AsterGenWebpackPlugin } = require('@aster-rpc/aster/webpack-plugin');

module.exports = {
  plugins: [
    new AsterGenWebpackPlugin({
      project: 'tsconfig.json',
      out: 'aster-rpc.generated.ts',
    }),
  ],
};
```

### Wiring the generated file

`AsterServer.start()` auto-imports `aster-rpc.generated.js` from the
working directory — no manual import needed:

```ts
import { AsterServer } from '@aster-rpc/aster';
import { MissionControlService } from './services/mission_control.js';

const server = new AsterServer({
  services: [new MissionControlService()],
});
await server.start();  // auto-imports aster-rpc.generated.js
```

The auto-import stamps the pre-built metadata onto each class
constructor before services are registered, so the existing runtime
paths (`ServiceRegistry`, manifest publication, JSON shape validation)
consume the generated data instead of reflecting at runtime.

## What happens if you don't run `aster-gen`?

The runtime falls back to reflection
(`new cls()` + `Object.keys`, plus `Function.length` for CallContext
detection) and logs a warning once per class / service / method:

```
[aster] StatusRequest: falling back to runtime introspection (new cls() + Object.keys).
  Run 'bunx aster-gen' and import the generated file once at startup...
```

Reflection has documented limitations:

- Empty array fields don't reveal their element type
- Optional / `null` nested types don't get recursed into
- Classes that need constructor arguments can't be introspected

Run `aster-gen` to obtain these types from the TypeScript AST.

## Brand types vs `@Rpc({ request, response })`

Earlier versions of `@aster-rpc/aster` required
`@Rpc({ request: Req, response: Res })` so the runtime could reach
the constructors despite type erasure. With `aster-gen` the
decorator options are redundant — the scanner reads the types from
the AST directly. You can delete them from new code. They still
work (and still trigger the runtime manifest path) if you haven't
run the scanner yet.

## Enrollment: who may connect, and their roles

`AsterServer` and `AsterClientWrapper` admit peers with Enrollment, driven
by `AsterConfig` alone (the same schema, TOML keys and `ASTER_*` variables as
the Rust `AsterServer` / `AsterClient`). Without enrollment configured the
server is open. With it, only nodes holding a credential from your root
connect, and each peer's `aster.role` claim is what `requires` checks, on
every call:

```ts
const server = new AsterServer({
  services: [new MissionControl()],
  config: {
    rootPubkey: ROOT,                    // [trust] root_pubkey
    secretKeyFile: 'server.key',         // aster key node generate --out server.key
    enrollmentCredentialFile: 'server.cred', // aster enroll node --subject … --out server.cred
    enrollment: { audience: 'mission-control', fleetNamespace: FLEET, statePath: './enrollment.json' },
  },
});
```

A client sets the same root, audience and fleet namespace plus its own key
and credential; it enrolls with the server before its RPC connection opens
and renews its credential from a configured issuer. Every missing or
malformed value fails `start()` / `connect()` with an error naming it. See
`examples/typescript/missionControl` (Chapter 5) for the CLI steps, and
`CHANGELOG.md` for migrating from the retired consumer admission.

## Authenticated HTTPS and WebTransport listeners

`RpcServer.startHttp(options, authenticate)` serves the registered services
through an owned HTTPS listener. `AsterServer.startHttp(...)` also starts its
node/server when needed. Supply the operator's PEM certificate and private key;
startup resolves after TCP and optional QUIC sockets are bound. WebTransport
binds UDP on the selected TCP address and port.

```ts
const listener = await server.startHttp({
  bindAddr: '127.0.0.1:0',
  certificatePem: await readFile('server-cert.pem'),
  privateKeyPem: await readFile('server-key.pem'),
  webTransport: true,
  maxPendingAuth: 256,       // zero/absent selects the default
  maxAuthPayloadBytes: 65536,
}, async (request, context) => {
  const identity = await verifyCredential(request.metadata.authorization, {
    signal: context.signal,
  });
  if (!identity) return null;
  return { principal: identity.subject, attributes: { 'aster.role': 'operator' } };
});
console.log(listener.tcpAddr, listener.quicAddr);
await listener.stop(2000); // application drain grace in milliseconds
// await listener.close() uses zero grace; both operations are idempotent.
```

Authentication runs independently for each routed call, before Gate 3 and
session creation. The request includes service, method, session ID, HTTP
metadata, and an incoming peer string. That incoming string is not an
authenticated identity. Return `null` for `UNAUTHENTICATED`, or a nonempty
principal with attributes local to this call. Callback/encoding errors and auth
timeout fail closed with `UNAVAILABLE`; admission failure modes never grant
access. Auth attributes are passed to the normal Gate 3 checks and
`CallContext.attributes`. `CallContext.peer` is the resolved principal, while
`CallContext.irohAuthenticated` remains false for HTTPS/WebTransport. Iroh policy
and provenance remain native properties.

The callback's `context.signal` aborts on timeout, transport cancellation,
listener shutdown, or table shutdown. Pass it to cancellable application work;
late responses are discarded. Each high-level listener owns a private upcall
table and closes it with the listener. `stop(graceMs)` allows active calls to
drain, then cancels and joins remaining native calls/connections before releasing
TCP/UDP ports. QUIC protocol/socket drain may extend beyond the application
grace. Failed stops retain ownership for retry. Parent server close also joins
child listeners. Use explicit `await listener.close()` or `await using` for
deterministic resource release; finalizers only schedule forced cleanup.
Cached address strings remain readable after close. An aborted startup cleans
up any listener that finished binding before it rejects.
If cleanup itself fails, `HttpListenerStartError` preserves the original error
in `cause`, the cleanup error in `cleanupError`, and the owned `listener` so the
caller can retry `await error.listener.close()`.

Canonical HTTPS requests are `POST /aster/{service}/{method}` with
`Content-Type: application/aster-frames` and Aster frames. All four RPC patterns
are supported. HTTPS buffers the request body before dispatch; WebTransport
supports live bidirectional streams. This API opens the native WebTransport
server and does not add a WebTransport client. The SDK requires matching native
`HttpListener` and `DispatchServer.startHttp` exports and rejects stale addons
when loading them.

## Explicit public listeners and certificate helpers

`RpcServer.startPublicHttp(options)` and `AsterServer.startPublicHttp(options)`
serve anonymous shared services without allocating an authentication callback
table. Attribute gates and Iroh-only restrictions remain enforced; session
services are rejected before instance creation. `startHttp` always requires its
authenticator and never falls back to public mode. Both modes use the same
owned listener, cancellation and joined shutdown semantics. Public mode accepts
only zero/absent authentication limits.

```ts
import { generateWebTransportCertificate } from '@aster-rpc/aster';
const cert = generateWebTransportCertificate('my-server', ['localhost']);
await using listener = await server.startPublicHttp({ ...cert, webTransport: true });
console.log(listener.tcpAddr);
```

`generateSelfSignedCertificate(sans = [], nodeSecret?)` and
`generateWebTransportCertificate(subject, sans = [], nodeSecret?)` return
`certificatePem`, `privateKeyPem`, and the 32-byte `sha256` of certificate DER.
The optional identity secret must be exactly 32 bytes. Without it the
self-signed certificate has no node identity binding; no unsigned node identity
claim is exposed.
WebTransport material uses P-256 and a validity period shorter than 14 days.
Keep private key bytes secret.

`verifyCertificateBinding(certificatePem, expectedNodeId)` throws unless the
certificate proves ownership of the TLS key by that trusted node, whose id
must come from a trusted source. This verifies the signed TLS public-key
binding, not the whole certificate or chain. Expiry, hostname and PKI trust
remain separate TLS checks. The DER hash can be used for
explicit certificate pinning.

## Blob custody and verified imports

`node.blobs().custody()` provides temporary GC protection without persistent
anonymous tags. Use `using` or `close()` to seal admission; accepted operations
retain every scope guard until native work and cleanup finish.

```ts
using custody = node.blobs().custody();
const hash = await custody.addBytes(bytes, 'raw');
await custody.retain(hash, 'raw', 'tree/current');
```

`protect(hash, format)` can protect an absent root before
`downloadHashFrom(hash, providers, format, strategy)`, which fetches to the
store. A `'hash_seq'` root is concatenated 32-byte child hashes, and custody
protects that graph. `read(hash)` returns root bytes without requiring a
Collection schema. Named-root retention requires this scope to hold the root
with exactly the same format.

`blobs.addBytesVerified(data, expectedHash, format, tag)` and
`blobs.addPathVerified(path, expectedHash, format, tag)` verify bytes before
atomically replacing the exact named tag. Path imports copy bytes, never retain
a mutable source reference. A mismatch throws `BlobCustodyError` with kind
`'hash_mismatch'` and leaves the old named root intact. No anonymous tag is
created; deleting the final named tag makes the data eligible for GC. This adds
no fsync guarantee beyond the store's existing persistence semantics.

Every new async method accepts a final `{ signal }` option. An abort requests
cooperative native cancellation and the promise waits for cleanup. Cancellation
before tag commit publishes no named root; once commit begins, its actual
success or error wins. Fetch cancellation may wait for a stalled native
transfer and other downloader work to drain. The other typed error kinds are
`'closed'`, `'cancelled'`, `'unprotected_root'`, and `'native'`.

## Batch local document content reads

`doc.readEntryContentsIfComplete(hashes)` uses one shared native batch read and
returns `(Buffer | null)[]` in the exact input order, including duplicates.
Missing or initially partial blobs are `null`; a complete zero-byte value is
an empty `Buffer`. Empty input returns `[]`. Pass content hashes directly,
for example `entries.map(entry => entry.contentHash)`.

This reads only the local store, fetches nothing, and creates no tags or GC
protection. Storage or integrity errors fail the whole batch. It is not a
global snapshot: concurrent GC between the completeness check and a large-value
export may also fail the batch. Use blob custody when content must remain
available during host verification.

## Coordination authority provisioning

`CoordinationProvisioning` provides offline canonical genesis/join documents,
domain-separated signing preimages, signed envelopes, candidate tickets,
genesis hashes and verification. All byte-exact encoding, domain and authority
rules run in Rust; authority private keys stay with your external signer.

```ts
const document = CoordinationProvisioning.canonicalGenesis(input);
const signature = externalSigner.sign(CoordinationProvisioning.genesisSigningBytes(document));
const envelope = CoordinationProvisioning.signedDocumentFromParts(document, authorityPublicKey, signature);
const verified = CoordinationProvisioning.verifyGenesis(trustedAuthorityPublicKey, configuredDomain, envelope);
```

Typed genesis/join inputs accept ids, hashes and nonces as raw bytes or hex.
Version is fixed to 1, and Rust derives the join candidate's canonical
addressless Open ticket. Callers choose randomness and deployment profiles and
provide lists in protocol order. Nothing is sorted, deduplicated or inferred.
Canonical encoding alone establishes neither authorization nor profile validity.

Verify against independently trusted authority key/domain pins, never just the
key inside an envelope. `verifyJoinPermitForStart` also checks the candidate;
the group/genesis are extracted rather than independently pinned.
`genesisHash` hashes supplied bytes without verification. Verified summaries
are informational metadata, not Rust sealed values or transferable capabilities;
preserve their exact `documentBytes` for persistence/forwarding. Summary ids and
hashes are lowercase hex. Failures throw `ProvisioningError` with stable native
`code` and diagnostic `detail`. These helpers do not sign or host a voter.

## Observability

Logs (into pino, winston or your own sink), custom metrics, tracing and the
`/healthz` / `/readyz` / `/metrics` listener are covered in the
[observability guide](../../../../docs/aster-observability-getstarted.md).

## API reference

Generated API docs (TypeDoc): `bun run docs` produces `docs-api/`.

## License

Apache-2.0.

### Topology baseline policy

Local `node.topology()` exposes measured peers, shared swarm timing and immutable
snapshots. Measurements require `enableMonitoring: true`. Use one `snapshot()`
for several placement decisions: separate convenience calls can observe different
eventual views, and `unknown` never proves separation. Local peer measurements
include the selected remote address; the remote baseline omits it.

`AsterServer`, `RpcServer` and `DispatchLoop` accept explicit
`builtinTopology: true` and optional
`topologyRequires: { kind: 'role', roles: ['operator'] }` (also `any_of` / `all_of`).
The native baseline stays disabled by default; enabling it requires monitoring.
Rust checks the policy before every topology method, including HTTP/WebTransport
calls. Public listener mode cannot bypass it. The native addon exposes
`DispatchServerOptions.topologyRequiresJson` for direct consumers with the same
native capability requirement JSON. Out-of-range durations return argument errors.

## Raw WebTransport media (PC13)

`WebTransportListener.start(options, authenticate)` exposes an exact raw-media
route, separate from `/aster/wt` RPC. `startPublic` is the explicit anonymous
alternative. The existing `RpcAuthenticator` runs **before CONNECT upgrade**,
with service `webtransport`, method `CONNECT`, HTTP headers and native-supplied
`aster.webtransport.path` / `aster.webtransport.query` metadata. The raw query
is application input, and the peer socket address is not Iroh identity.

```typescript
await using listener = await WebTransportListener.start({
  certificatePem, privateKeyPem, path: '/wt',
}, async request => verifyTicket(request) ? { principal: 'desktop-user' } : null);
using session = await listener.accept({ signal });
const frame = await session.openUni({ signal });
await frame.setPriority(20); // signed transport priority; higher goes first
await frame.write(encodedFrame, { signal });
await frame.finish({ signal }); // FIN, not delivery acknowledgment
const delivered = await frame.transportAcked({ signal });
```

The session exposes bidirectional and unidirectional streams, datagrams and
connection statistics. Keep finished streams alive until transport acknowledgment
or explicit reset; releasing an owner aborts its directions. `read()` returns
empty bytes at EOF. Cancellation of reads preserves unread bytes; cancellation
of writes can leave a sent prefix. `reset()` interrupts a blocked send and
`stop()` interrupts receive. Operations accept an `AbortSignal` and settle
through native cancellation rather than abandoning an unobserved owner.

`session.close()` closes its entire QUIC connection. Await `listener.close()`
for joined shutdown and released TCP/UDP sockets. Limits bound accepted sessions,
streams, datagrams and read sizes. The standalone listener owns a separate
port and supplies transport primitives; codecs, frame pacing, application PoP
and arbitrary HTTP route composition remain application responsibilities.
`statsJson()` preserves exact counter integers when JavaScript number precision
would be insufficient.
