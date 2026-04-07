# Binding Implementation Completeness

Tracks feature/behavior/capability implementation across bindings. Unlike `BINDING_PARITY.md` (which tracks spec conformance), this tracks whether each binding has the feature at all — even when language-specific implementations may differ.

**Legend:** `done` = implemented + tested | `code` = code exists, needs tests | `napi` = NAPI-RS wrapper exists, needs TS integration | `stub` = types/interfaces only, no logic | `—` = not started

**Last updated:** 2026-04-07

## Transport Layer (NAPI-RS / PyO3 over Rust core)

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| IrohNode.memory() | done | napi | |
| IrohNode.persistent() | done | napi | |
| IrohNode.memoryWithAlpns() | done | napi | |
| IrohNode.persistentWithAlpns() | done | napi | |
| IrohNode.acceptAster() | done | napi | |
| IrohNode.takeHookReceiver() | done | napi | |
| IrohNode.hasHooks() | done | napi | |
| IrohNode.addNodeAddr() | done | napi | |
| IrohNode.exportSecretKey() | done | napi | |
| IrohNode.blobsClient() | done | napi | |
| IrohNode.docsClient() | done | napi | |
| IrohNode.gossipClient() | done | napi | |
| IrohConnection.openBi() | done | napi | |
| IrohConnection.acceptBi() | done | napi | |
| IrohConnection.openUni() | done | napi | |
| IrohConnection.acceptUni() | done | napi | |
| IrohConnection.sendDatagram() | done | napi | |
| IrohConnection.readDatagram() | done | napi | |
| IrohConnection.connectionInfo() | done | napi | |
| IrohConnection.maxDatagramSize() | done | napi | |
| IrohConnection.close() | done | napi | |
| IrohSendStream.writeAll() | done | napi | |
| IrohSendStream.finish() | done | napi | |
| IrohRecvStream.readExact() | done | napi | |
| IrohRecvStream.readToEnd() | done | napi | |
| BlobsClient.addBytes() | done | napi | |
| BlobsClient.read() | done | napi | |
| BlobsClient.createTicket() | done | napi | |
| BlobsClient.downloadBlob() | done | napi | |
| BlobsClient.addBytesAsCollection() | done | napi | |
| BlobsClient.createCollectionTicket() | done | napi | |
| BlobsClient.has() | done | napi | |
| BlobsClient.tagSet() | done | napi | |
| BlobsClient.tagGet() | done | napi | |
| BlobsClient.tagDelete() | done | napi | |
| BlobsClient.tagListPrefix() | done | napi | |
| BlobsClient.blobStatus() | done | napi | |
| BlobsClient.blobObserveComplete() | done | napi | |
| BlobsClient.blobObserveSnapshot() | done | — | Complex return type |
| BlobsClient.blobLocalInfo() | done | — | Complex return type |
| DocsClient.create() | done | napi | |
| DocsClient.join() | done | napi | |
| DocsClient.createAuthor() | done | — | |
| DocsClient.joinAndSubscribe() | done | — | |
| DocHandle.setBytes() | done | napi | |
| DocHandle.getExact() | done | napi | |
| DocHandle.queryKeyExact() | done | napi | |
| DocHandle.queryKeyPrefix() | done | napi | |
| DocHandle.readEntryContent() | done | napi | |
| DocHandle.share() | done | napi | |
| DocHandle.shareWithAddr() | done | napi | |
| DocHandle.subscribe() | done | — | Needs event receiver type |
| DocHandle.startSync() | done | napi | |
| DocHandle.leave() | done | napi | |
| DocHandle.setDownloadPolicy() | done | — | Needs policy type |
| DocHandle.getDownloadPolicy() | done | — | Needs policy type |
| GossipClient.subscribe() | done | napi | |
| GossipTopicHandle.broadcast() | done | napi | |
| GossipTopicHandle.recv() | done | napi | |

## RPC Framework (pure language code)

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| @service / @Service decorator | done | done | TC39 in TS |
| @rpc / @Rpc decorator | done | done | |
| @server_stream / @ServerStream | done | done | |
| @client_stream / @ClientStream | done | done | |
| @bidi_stream / @BidiStream | done | done | |
| @wire_type / @WireType | done | done | |
| ServiceInfo / MethodInfo types | done | done | |
| ServiceRegistry | done | done | |
| Client stub generation | done | done | Proxy-based in TS |
| Server accept loop (QUIC) | done | — | **Critical gap** — TS has LocalTransport dispatch but not the QUIC accept loop |
| Server stream dispatch (4 patterns) | done | done | Via LocalTransport |
| IrohTransport (client-side QUIC) | done | code | Implemented but untested over real QUIC |
| LocalTransport (in-process) | done | done | Tested |
| Session-scoped services (CALL/CANCEL) | done | code | Types + session server, untested |

## Codec & Compression

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| ForyCodec (XLANG serialization) | done | stub | ForyCodec class exists, calls @apache-fory/core API — **needs wire compat validation** |
| JsonCodec (testing/dev) | done | done | |
| Zstd compression (auto threshold) | done | code | Uses node:zlib zstdCompressSync (Node 21.7+) |
| Zstd decompression with size limit | done | code | MAX_DECOMPRESSED_SIZE enforced |
| Type graph walking + auto-registration | done | — | Python walks dataclass fields; TS needs equivalent |

## Interceptors

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| Interceptor base / CallContext | done | done | |
| apply{Request,Response,Error}Interceptors | done | done | |
| normalizeError | done | done | |
| DeadlineInterceptor | done | done | |
| AuthInterceptor | done | done | |
| RetryInterceptor | done | done | |
| CircuitBreakerInterceptor | done | done | |
| MetricsInterceptor (OTel) | done | done | OTel optional in both |
| RateLimitInterceptor | done | done | |
| CompressionInterceptor | done | done | |
| AuditLogInterceptor | done | done | |
| CapabilityInterceptor | done | done | |

## Contract Identity

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| Canonical XLANG encoding | done (Rust) | done (Rust) | Both delegate to core |
| BLAKE3 hashing | done (Rust) | done (Rust) | Both delegate to core |
| Canonical signing bytes | done (Rust) | done (Rust) | NAPI function added |
| Canonical JSON | done (Rust) | done (Rust) | NAPI function added |
| Golden vector conformance (4 vectors) | done | done | Via NAPI native tests |
| ContractManifest type | done | done | |
| Manifest JSON serialization/parsing | done | done | |
| Manifest startup verification | done | done | verifyManifestOrFatal() |
| Contract publication (blobs + registry) | done | code | publishContract() exists |
| Collection building | done | done | buildCollection() |
| Dynamic type synthesis | done | done | DynamicTypeFactory |

## Trust & Security

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| Ed25519 keypair generation | done | done | @noble/ed25519 + Node fallback |
| Ed25519 sign / verify | done | done | @noble/ed25519 + Node fallback |
| EnrollmentCredential type | done | done | |
| ConsumerEnrollmentCredential type | done | done | |
| Credential verification (signature) | done | code | verify{Consumer,Producer}Credential() |
| Consumer admission handshake (protocol) | done | — | **Gap** — needs IrohTransport |
| Producer admission handshake (protocol) | done | — | **Gap** — needs IrohTransport |
| Gate 0 connection hooks (NAPI) | done | napi | NodeHookReceiver exposed |
| ConnectionPolicy interface | done | done | AllowAll / DenyAll |
| MeshEndpointHook (background loop) | done | — | **Gap** — needs hook receiver wiring |
| Security limits + validation | done | done | |
| Nonce store | done | — | |
| RCAN validation | done | — | |
| IID (cloud identity) | done | — | Low priority |
| Clock drift detection | done | — | |
| Producer mesh bootstrap | done | — | |
| Producer mesh gossip | done | — | |
| Mesh state persistence | done | — | |

## Registry

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| RegistryClient | done | — | **Gap** — service discovery |
| Registry publisher | done | — | |
| Endpoint lease model | done | — | |
| Service summary | done | — | |
| ArtifactRef model | done | done | |
| Registry key encoding | done | — | |
| ACL filtering | done | — | |
| Gossip-based lease updates | done | — | |

## Production Features

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| AsterServer (high-level wrapper) | done | done | TS is simpler |
| AsterClient (high-level wrapper) | done | done | TS is simpler |
| Config (env vars) | done | done | |
| Config (TOML file) | done | — | cosmiconfig planned |
| Config print / debug | done | done | |
| .aster-identity file loading | done | — | |
| Structured logging (JSON/text) | done | done | |
| Request correlation (contextvars/AsyncLocalStorage) | done | done | |
| Sensitive field masking | done | done | |
| Health server (/healthz, /readyz) | done | done | |
| Prometheus metrics (/metrics/prometheus) | done | done | |
| Connection metrics | done | — | |
| Admission metrics | done | — | |
| Graceful drain (SIGTERM) | done | done | installSignalHandlers() |
| Connection retry (exp backoff) | done | done | reconnect() |
| Grafana dashboard template | done | — | |

## Testing

| Area | Python | TypeScript | Notes |
|------|--------|------------|-------|
| Unit tests | 941 | 112 | **Gap** — TS needs more tests |
| E2E tests (real AsterServer) | 4 | 0 | **Gap** — needs NAPI build |
| Shell tests | 34 | n/a | CLI is Python-only |
| MCP tests | 38 | n/a | CLI is Python-only |
| Security limit tests | 39 | 15 | |
| Conformance vector tests | done | done | Framing + contract identity |
| Native integration tests (NAPI) | n/a | 9 | Golden vectors via real Rust |
| Cross-language interop | — | — | **Not yet tested** |
| Code coverage tool | — | — | vitest/coverage-v8 installed but alias issue |

## Summary of Critical Gaps (TypeScript)

| Gap | Impact | Effort |
|-----|--------|--------|
| Server QUIC accept loop | Can't run TS server over real network | Medium |
| Consumer admission protocol | Can't authenticate TS clients | Medium |
| ForyCodec wire compat validation | Can't interop with Python serialization | Medium |
| Registry subsystem | Can't discover services at runtime | Large |
| Cross-language interop tests | Don't know if Python↔TS actually works | Medium |
| More unit tests | Low confidence in edge cases | Ongoing |
