# Binding Implementation Completeness

Tracks feature/behavior/capability implementation across bindings. Unlike `BINDING_PARITY.md` (which tracks spec conformance), this tracks whether each binding has the feature at all — even when language-specific implementations may differ.

**Legend:** `done` = implemented + tested | `code` = code exists, needs tests | `napi` = NAPI-RS wrapper exists, needs TS integration | `stub` = types/interfaces only, no logic | `—` = not started

**Last updated:** 2026-04-08

## Transport Layer (NAPI-RS / PyO3 over Rust core)

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| IrohNode.memory() | done | napi | |
| IrohNode.persistent() | done | napi | |
| IrohNode.memoryWithAlpns() | done | napi | |
| IrohNode.persistentWithAlpns() | done | napi | |
| IrohNode.connect() | done | done | Client→server QUIC connection, E2E tested |
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
| BlobsClient.blobObserveSnapshot() | done | done | Returns { isComplete, size } — E2E tested |
| BlobsClient.blobLocalInfo() | done | done | Returns { isComplete, localBytes } — E2E tested |
| DocsClient.create() | done | napi | |
| DocsClient.join() | done | napi | |
| DocsClient.createAuthor() | done | done | E2E tested |
| DocsClient.joinAndSubscribe() | done | done | NAPI DocWithEvents, E2E tested |
| DocHandle.setBytes() | done | napi | |
| DocHandle.getExact() | done | napi | |
| DocHandle.queryKeyExact() | done | napi | |
| DocHandle.queryKeyPrefix() | done | napi | |
| DocHandle.readEntryContent() | done | napi | |
| DocHandle.share() | done | napi | |
| DocHandle.shareWithAddr() | done | napi | |
| DocHandle.subscribe() | done | done | NAPI DocEventReceiver with 7 event types, E2E tested |
| DocHandle.startSync() | done | napi | |
| DocHandle.leave() | done | napi | |
| DocHandle.setDownloadPolicy() | done | done | String-based policy format, E2E tested |
| DocHandle.getDownloadPolicy() | done | done | E2E tested |
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
| Server accept loop (QUIC) | done | done | RpcServer.serve() over real QUIC — E2E tested |
| Server stream dispatch (4 patterns) | done | done | All 4 patterns via LocalTransport + unary/server_stream E2E tested |
| IrohTransport (client-side QUIC) | done | done | E2E tested: unary + server_stream over real QUIC |
| LocalTransport (in-process) | done | done | Tested |
| Session-scoped services (CALL/CANCEL) | done | code | Types + session server, untested |
| BidiStream over IrohTransport | done | done | All 4 patterns E2E tested over real QUIC |
| Metadata class (@wire_type) | done | done | `_aster/Metadata` with description field |
| Metadata on decorators | done | done | @rpc(metadata=), @service(metadata=), docstring auto-capture (Python) |
| Metadata on @wire_type fields | done | done | Field-level metadata dict |

## Codec & Compression

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| ForyCodec (XLANG serialization) | done | stub | ForyCodec class exists, calls @apache-fory/core API — **needs wire compat validation** |
| JsonCodec (testing/dev) | done | done | Tested |
| Zstd compression (auto threshold) | done | done | Uses node:zlib zstdCompressSync (Node 21.7+), tested |
| Zstd decompression with size limit | done | done | MAX_DECOMPRESSED_SIZE enforced, tested |
| Type graph walking + auto-registration | done | done | walkTypeGraph() discovers nested @WireType classes from default values, tested |

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
| Consumer admission handshake (protocol) | done | code | performAdmission() implemented, needs E2E test |
| Producer admission handshake (protocol) | done | done | handleProducerAdmission() + serveProducerAdmission(), tested |
| Gate 0 connection hooks (NAPI) | done | napi | NodeHookReceiver exposed |
| ConnectionPolicy interface | done | done | AllowAll / DenyAll |
| MeshEndpointHook (background loop) | done | done | NAPI NodeHookReceiver with recv/respond for before_connect + after_handshake |
| Security limits + validation | done | done | |
| Nonce store | done | done | InMemoryNonceStore with TTL expiry, tested |
| RCAN validation | done | done | evaluateCapability, validateRcan, encodeRcan/decodeRcan |
| IID (cloud identity) | done | done | verifyIID + AWS/GCP/Azure/Mock backends |
| Clock drift detection | done | done | ClockDriftTracker + grace period, tested |
| Producer mesh bootstrap | done | code | serveProducerAdmission() accept loop |
| Producer mesh gossip | done | done | RegistryGossip with all event types |
| Mesh state persistence | done | done | saveMeshState() / loadMeshState() to ~/.aster/ |

## Registry

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| RegistryClient | done | done | Manifest tracking, lease management, endpoint discovery |
| Registry publisher | done | done | RegistryPublisher with lease management + withdrawal |
| Endpoint lease model | done | done | EndpointLease type + findEndpoints() |
| Service summary | done | done | ServiceSummary type in consumer.ts |
| ArtifactRef model | done | done | |
| Registry key encoding | done | done | registryKey() |
| ACL filtering | done | done | RegistryACL with open/restricted modes |
| Gossip-based lease updates | done | done | RegistryGossip.broadcastEndpointLeaseUpserted() |

## Production Features

| Feature | Python | TypeScript | Notes |
|---------|--------|------------|-------|
| AsterServer (high-level wrapper) | done | done | TS is simpler |
| AsterClient (high-level wrapper) | done | done | TS is simpler |
| Config (env vars) | done | done | |
| Config (TOML file) | done | done | configFromFile() with built-in TOML parser |
| Config print / debug | done | done | |
| .aster-identity file loading | done | done | loadIdentity() with peer selection by name/role |
| Structured logging (JSON/text) | done | done | |
| Request correlation (contextvars/AsyncLocalStorage) | done | done | |
| Sensitive field masking | done | done | |
| Health server (/healthz, /readyz) | done | done | |
| Prometheus metrics (/metrics/prometheus) | done | done | |
| Connection metrics | done | done | ConnectionMetrics with accept/reject/close counters |
| Admission metrics | done | done | AdmissionMetrics with attempt/success/reject/error counters |
| RPC duration tracking | done | done | Python: OTel histogram. TS: totalDurationS + lastDurationS |
| RPC in-flight gauge | done | done | |
| Stream metrics (active/total) | done | done | Both: ConnectionMetrics.streamsActive/streamsTotal |
| Uptime gauge | done | done | Both: aster_uptime_seconds in Prometheus output |
| Admission last duration (ms) | done | done | Both: lastAdmissionMs field |
| OTel integration (optional) | done | done | Both: optional @opentelemetry/api, fallback to in-memory |
| Transport metrics (iroh endpoint) | done | — | Python: via `net_client.transport_metrics()` (PyO3→core). TS: **needs NAPI wrapper** |
| Graceful drain (SIGTERM) | done | done | installSignalHandlers() |
| Connection retry (exp backoff) | done | done | reconnect() |
| Grafana dashboard template | done | — | |

## Testing

| Area | Python | TypeScript | Notes |
|------|--------|------------|-------|
| Unit tests | 944 | 203 | framing, status, limits, decorators, transport, interceptors, codec, trust, registry, config, metadata, rcan, iid |
| E2E tests (real QUIC RPC) | 4 | 15 | Unary + server stream, blobs observe/localInfo, docs policy/subscribe, createClient |
| Shell tests | 34 | n/a | CLI is Python-only |
| MCP tests | 38 | n/a | CLI is Python-only |
| Security limit tests | 39 | 15 | |
| Conformance vector tests | done | done | Framing + contract identity |
| Native integration tests (NAPI) | n/a | 9 | Golden vectors via real Rust |
| Cross-language interop | — | — | **Not yet tested** |
| Code coverage tool | — | — | vitest/coverage-v8 installed but alias issue |

## Summary of Remaining Gaps (TypeScript)

| Gap | Impact | Effort |
|-----|--------|--------|
| ForyCodec wire compat validation | Can't interop with Python serialization | Medium |
| Cross-language interop tests | Don't know if Python↔TS actually works | Medium |
| Transport metrics (TS NAPI wrapper) | Python has it, TS needs NAPI `transportMetrics()` | Small — mirror Python PyO3 pattern |
| Grafana dashboard template | Observability template | Trivial (JSON file) |
