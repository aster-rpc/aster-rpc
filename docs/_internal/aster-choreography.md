# Aster — End-to-End Choreography

This document traces every end-to-end flow in the Aster system from the perspective of "what actually happens when." Use it to verify that the implementation matches the spec and that no steps are missing.

## Flow 1: Producer Startup

**Trigger:** `async with AsterServer(services=[...]) as srv`

```
AsterServer.__aenter__()
  └── start()
        │
        ├── 1. Create IrohNode (memory_with_alpns)
        │     ALPNs: aster/rpc/1, aster.consumer_admission
        │     + aster.producer_admission (if Gate 0 enabled)
        │     → self._node, addr_b64
        │
        ├── 2. Build ServiceSummary list
        │     For each @service class:
        │       contract_id_from_service(cls)
        │         → build_type_graph → resolve_with_cycles
        │         → ServiceContract.from_service_info
        │         → canonical_xlang_bytes → BLAKE3 → contract_id
        │       ServiceSummary(name, version, contract_id, channels)
        │     → self._service_summaries
        │
        ├── 3. Manifest verification (if .aster/manifest.json exists)
        │     For each service in manifest:
        │       assert live contract_id == manifest contract_id
        │       → FatalContractMismatch on mismatch
        │
        ├── 4. Contract publication to registry doc    ★ NEW
        │     a. Create registry doc (dc.create)
        │     b. Create author (dc.create_author)
        │     c. For each service:
        │         build_collection(contract, type_defs, service_info)
        │           → contract.bin (canonical bytes)
        │           → manifest.json (with methods + fields)
        │           → types/{hash}.bin
        │         upload_collection(blobs, entries)
        │           → individual blob uploads → index blob
        │           → collection_hash
        │         bc.create_collection_ticket(collection_hash)
        │           → blob_ticket
        │         Write ArtifactRef to doc at contracts/{contract_id}
        │         Write manifest JSON to doc at manifests/{contract_id}
        │         Write version pointer at services/{name}/versions/v{N}
        │     d. registry_doc.share_with_addr("read")
        │        → self._registry_ticket (read-only)
        │
        ├── 5. Gate 3: CapabilityInterceptor setup
        │     Build service → ServiceInfo map
        │     Auto-wire CapabilityInterceptor if any requires= declared
        │     Emit UserWarning if allow_all_consumers + no auth
        │
        └── 6. Create Server(net_client, services, codec, interceptors)
              → self._server, self._started = True
```

## Flow 2: Consumer Admission

**Trigger:** Consumer connects to producer's `aster.consumer_admission` ALPN

```
Consumer                                    Producer
   │                                           │
   ├── AsterClient.connect()                   │
   │   ├── Create IrohNode                     │
   │   ├── net_client(node) → self._ep         │
   │   └── _run_admission()                    │
   │       ├── Connect to peer on              │
   │       │   ALPN_CONSUMER_ADMISSION         │
   │       ├── open_bi() → send, recv          │
   │       ├── ConsumerAdmissionRequest {      │
   │       │     credential_json,              │
   │       │     iid_token                     │
   │       │   }.to_json()                     │
   │       ├── send.write_all(req_json)        ──→  accept connection
   │       ├── send.finish()                   │    ├── recv.read_to_end(64KB)
   │       │                                   │    ├── ConsumerAdmissionRequest.from_json
   │       │                                   │    ├── Verify credential (or auto-admit)
   │       │                                   │    ├── ConsumerAdmissionResponse {
   │       │                                   │    │     admitted: true,
   │       │                                   │    │     services: [ServiceSummary...],
   │       │                                   │    │     registry_ticket: "docaaa...",
   │       │                                   │    │     root_pubkey: hex
   │       │                                   │    │   }
   │       ├── recv.read_to_end(64KB)    ←──   │    └── send.write_all(resp_json)
   │       ├── ConsumerAdmissionResponse       │
   │       │   .from_json(raw)                 │
   │       ├── self._services = resp.services  │
   │       └── self._registry_ticket =         │
   │             resp.registry_ticket           │
   │                                           │
   └── connected                               │
```

## Flow 3: Shell Contract Reflection

**Trigger:** Shell's `PeerConnection.connect()` after admission

```
Shell (PeerConnection._fetch_manifests)
   │
   ├── Get registry_ticket from AsterClient
   │
   ├── dc.join_and_subscribe(ticket)
   │     → (doc, event_receiver)
   │
   ├── Wait for sync_finished event (up to 5s)
   │     Events: neighbor_up → insert_remote × N → sync_finished
   │
   ├── For each service in self._services:
   │     ├── doc.query_key_exact(b"contracts/{contract_id}")
   │     │     → entries (DocEntry with content_hash)
   │     │
   │     ├── doc.read_entry_content(entry.content_hash)
   │     │     → ArtifactRef JSON
   │     │     → collection_hash, ticket
   │     │
   │     ├── bc.download_blob(ticket)       ★ Downloads full collection
   │     │     → index + contract.bin + manifest.json + types/*.bin
   │     │
   │     └── fetch_from_collection(bc, collection_hash, "manifest.json")
   │           → manifest dict with methods, fields, types
   │           → self._manifests[svc.name] = manifest
   │
   └── VFS populated with method schemas
         /services/HelloService/
           say_hello  (unary, fields: [{name: str}])
```

## Flow 4: Unary RPC Call

**Trigger:** `./sayHello name="World"` in shell, or `await client.say_hello(req)`

```
Consumer                                    Producer
   │                                           │
   ├── transport.unary(service, method, req)   │
   │   ├── conn.open_bi() → send, recv        │
   │   ├── StreamHeader {                      │
   │   │     service, method, version,         │
   │   │     contract_id, deadline,            │
   │   │     serialization_mode, metadata      │
   │   │   }                                   │
   │   ├── codec.encode(StreamHeader)          │
   │   ├── write_frame(send, header_bytes,     │
   │   │              flags=HEADER)            │
   │   ├── codec.encode(request)               │
   │   ├── write_frame(send, payload,          │
   │   │              flags=CALL)              │
   │   ├── send.finish()                ──→    │  Server._handle_stream()
   │   │                                       │    ├── read_frame → HEADER
   │   │                                       │    ├── codec.decode → StreamHeader
   │   │                                       │    ├── registry.lookup(service, version)
   │   │                                       │    ├── read_frame → CALL
   │   │                                       │    ├── codec.decode → request object
   │   │                                       │    ├── interceptor chain
   │   │                                       │    ├── handler.method(request)
   │   │                                       │    │     → response object
   │   │                                       │    ├── codec.encode(response)
   │   │                                       │    ├── write_frame(send, payload)
   │   │                                       │    ├── RpcStatus(OK)
   │   │                                       │    └── write_frame(send, status,
   │   │                                       │                   flags=TRAILER)
   │   ├── read_frame ← response       ←──    │
   │   ├── codec.decode → response             │
   │   ├── read_frame ← TRAILER               │
   │   ├── codec.decode → RpcStatus            │
   │   └── return response                     │
   │                                           │
   └── display result                          │
```

## Flow 5: Contract Publication (Collection Layout)

**What gets stored in blobs:**

```
Collection (HashSeq):
  ├── Index blob (JSON):
  │     {
  │       "version": 1,
  │       "entries": [
  │         {"name": "contract.bin", "hash": "544f...", "size": 113},
  │         {"name": "types/8eaa...bin", "hash": "8eaa...", "size": 57},
  │         {"name": "manifest.json", "hash": "c6ca...", "size": 915}
  │       ]
  │     }
  │
  ├── contract.bin — canonical XLANG bytes (WRITE-ONLY, for hashing)
  │     ServiceContract { name, version, methods[], serialization_modes[], scoped }
  │     BLAKE3(contract.bin) == contract_id
  │
  ├── types/{hash}.bin — canonical XLANG bytes per TypeDef
  │     BLAKE3(type.bin) == type_hash
  │
  └── manifest.json — READABLE schema (for discovery/shell/tooling)
        {
          "service": "HelloService",
          "version": 1,
          "contract_id": "544f3aa9...",
          "methods": [{
            "name": "say_hello",
            "pattern": "unary",
            "request_type": "HelloRequest",
            "response_type": "HelloResponse",
            "fields": [{"name": "name", "type": "str", "required": false, "default": ""}]
          }],
          "type_hashes": ["8eaa04fc..."],
          ...
        }
```

**What gets stored in the registry doc:**

```
contracts/{contract_id}
  → ArtifactRef { contract_id, collection_hash, ticket, published_by, collection_format }

manifests/{contract_id}
  → manifest.json bytes (fast path — avoids blob download for shell)

services/{name}/versions/v{N}
  → contract_id (pointer)
```

## Dual-Format Design Rationale

The same information exists in two formats:

| Format | Purpose | Consumer | Security |
|--------|---------|----------|----------|
| `contract.bin` (canonical XLANG) | Identity verification | `BLAKE3(bytes) == contract_id` | Never deserialize from untrusted source |
| `manifest.json` (JSON) | Human/machine readable schema | Parse with `json.loads()` | Standard JSON — battle-hardened parsers |

This is intentional duplication. The canonical format is write-only — designed for deterministic hashing. The JSON format is read-only — designed for tooling. Never cross the streams.

## Known Gaps

| Gap | Status | Impact |
|-----|--------|--------|
| Consumer registry doc persistence | Not implemented | Consumer must re-admit to get ticket on restart |
| Collection blob download timeout | No timeout | Slow producer can hang consumer |
| Gossip-triggered manifest updates | Not implemented | Consumer doesn't auto-refresh when contracts change |
| FsStore for consumer node | Not implemented | Consumer uses memory node — no persistence across restarts |
