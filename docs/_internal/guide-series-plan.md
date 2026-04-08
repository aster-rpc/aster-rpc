# Guide Series Plan

The Mission Control example serves as a multi-part guide series starting
right after the quickstart on the docs site. Written in parallel for
each language using the docs site language switcher.

## Part 1: Build a P2P Ops Platform (30 min)

`examples/mission-control/GUIDE.md`

Covers: all 4 RPC patterns, sessions, auth & capabilities with full CLI
flow (keygen → enroll → shell), proxy + typed clients (typed shown inline,
not as a standalone chapter), cross-language interop (TS agent → Python
control plane), benchmarks.

Goal: instant recognition of problems solved. No lecturing. Every chapter
earns its place by making the reader feel the problem before showing the
solution.

## Part 2+ Topics (post v1)

These are features and stories that didn't fit the 30-minute Part 1 arc
but need their own guides:

### Interceptors & Middleware
- Retry, deadline, circuit-breaker, rate-limit, compression, metrics
- Story angle: "hardening Mission Control for production"
- Circuit breaker fits naturally when an agent goes offline mid-stream

### Contract Identity & Publication
- Canonical encoding, BLAKE3 hashing, content-addressed registry
- Story angle: "publishing your service so others can discover it"
- Ties into aster.site marketplace

### Service Registry & Discovery
- Gossip-based notifications, blob-based artifact storage, doc-based leases
- Story angle: "scaling beyond a single control plane"

### Producer Mesh & Load Balancing
- Multi-producer coordination, signed gossip, clock drift detection
- Story angle: "running Mission Control across regions"

### Testing with LocalTransport
- `LocalTransport`, `create_local_client()`, in-process testing
- Story angle: "testing your services without a network"

### Error Handling & Failure Modes
- `StatusCode`, `RpcError`, mid-stream disconnects, deadline expiry, auth failures
- Story angle: "what happens when things go wrong"

### Advanced Cross-Language
- TS service that Python calls (bidirectional interop, not just TS-consumes-Python)
- WASM consumer story
- Story angle: "a polyglot fleet"

### Blob / Doc / Gossip Primitives
- Using iroh Layer 1 directly for artifact distribution, shared config, broadcast
- Story angle: "beyond RPC — the full P2P toolkit"

### Security Hardening
- Trust model deep dive, enrollment flows, credential fields, nonce stores
- Multiple admission gates, offline root key model
- Story angle: "zero-trust in a P2P world"
