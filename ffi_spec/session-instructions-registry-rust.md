# Session Instructions: Registry Rust Centralization

Paste this as the opening message of a new Claude Code session.

---

I need you to centralize the Aster registry logic in Rust and expose it via the C FFI. Read the design doc at `ffi_spec/registry-rust-centralization.md` fully first.

The Python reference implementation lives at `bindings/python/aster/registry/`. Read all files there — they are the authoritative behavior to replicate in Rust.

## Phase 1: Port data models + keys to Java/Go/.NET

Port these from `bindings/python/aster/registry/models.py` and `bindings/python/aster/registry/keys.py`:

**Data models** (per language):
- `HealthStatus` — string enum: starting, ready, degraded, draining
- `GossipEventType` — int enum: CONTRACT_PUBLISHED=0, CHANNEL_UPDATED=1, ENDPOINT_LEASE_UPSERTED=2, ENDPOINT_DOWN=3, ACL_CHANGED=4, COMPATIBILITY_PUBLISHED=5
- `ServiceSummary` — name, version, contract_id, channels map, pattern, serialization_modes
- `ArtifactRef` — contract_id, collection_hash, provider_endpoint_id, relay_url, ticket, published_by, published_at_epoch_ms, collection_format. JSON serialization.
- `EndpointLease` — 17 fields including endpoint_id, contract_id, service, version, lease_expires_epoch_ms, health_status, load, etc. JSON serialization. Methods: isFresh(leaseDurationS), isRoutable().
- `GossipEvent` — type, service, version, channel, contract_id, endpoint_id, key_prefix, timestamp_ms. JSON serialization.

**Key helpers** (per language):
- `contractKey(contractId)` → `"contracts/{contractId}"`
- `versionKey(name, version)` → `"services/{name}/versions/v{version}"`
- `channelKey(name, channel)` → `"services/{name}/channels/{channel}"`
- `leaseKey(name, contractId, endpointId)` → `"services/{name}/contracts/{contractId}/endpoints/{endpointId}"`
- `leasePrefix(name, contractId)` → `"services/{name}/contracts/{contractId}/endpoints/"`
- `aclKey(subkey)` → `"_aster/acl/{subkey}"`
- `REGISTRY_PREFIXES` list

**Where to create:**
- Java: `bindings/java/src/main/java/com/aster/registry/` package
- Go: `bindings/go/registry.go`
- .NET: `bindings/dotnet/src/Aster/Registry/` directory

Build-verify each: `mvn compile -P fast`, `go build ./...`, `dotnet build src/Aster/`

## Phase 2: Rust registry module

Create `core/src/registry.rs` implementing:

### Resolution (from client.py lines 53-408)

```rust
pub struct ResolveOptions {
    pub service: String,
    pub version: i32,
    pub strategy: String,  // "round_robin", "least_load", "random"
    pub caller_alpn: String,
    pub caller_policy_realm: Option<String>,
    pub lease_duration_s: i32,
}

pub fn resolve(node: &CoreNode, opts: &ResolveOptions) -> Result<EndpointLease>
```

Must apply the 5 mandatory filters in normative order:
1. contract_id match (version → contract_id lookup, then lease contract_id match)
2. ALPN supported by caller
3. health in {READY, DEGRADED}
4. lease freshness (now - updated_at_epoch_ms <= lease_duration_s * 1000)
5. policy_realm compatible

Then apply ranking: round_robin (stateful), least_load, random. READY preferred over DEGRADED within each strategy.

### Publishing (from publisher.py)

```rust
pub fn publish(node: &CoreNode, lease: &EndpointLease, artifact: Option<&ArtifactRef>) -> Result<()>
```

Writes: ArtifactRef at `contracts/{contract_id}`, EndpointLease at the lease key, version pointer, gossip broadcast of CONTRACT_PUBLISHED + ENDPOINT_LEASE_UPSERTED.

### Lease renewal

```rust
pub fn renew_lease(node: &CoreNode, service: &str, contract_id: &str, health: &str, load: f32) -> Result<()>
```

Updates the lease entry with new health/load/timestamp, gossip broadcast of ENDPOINT_LEASE_UPSERTED.

### ACL (from acl.py)

```rust
pub struct RegistryAcl { ... }
impl RegistryAcl {
    pub fn is_trusted_writer(&self, author_id: &str) -> bool;
    pub async fn reload(&mut self, doc: &CoreDocsClient) -> Result<()>;
    pub async fn add_writer(&mut self, doc: &CoreDocsClient, author_id: &str) -> Result<()>;
    pub async fn remove_writer(&mut self, doc: &CoreDocsClient, author_id: &str) -> Result<()>;
}
```

## Phase 3: FFI exposure

Add to `ffi/src/lib.rs`:

```c
int32_t aster_registry_resolve(runtime, node, service_ptr, service_len, version, strategy_ptr, strategy_len, out_buf, out_len);
int32_t aster_registry_publish(runtime, node, lease_json_ptr, lease_json_len, artifact_json_ptr, artifact_json_len);
int32_t aster_registry_renew_lease(runtime, node, service_ptr, service_len, contract_id_ptr, contract_id_len, health_ptr, health_len, load);
int32_t aster_registry_acl_add_writer(runtime, node, author_id_ptr, len);
int32_t aster_registry_acl_remove_writer(runtime, node, author_id_ptr, len);
int32_t aster_registry_acl_list_writers(runtime, node, out_buf, out_len);
```

Add to `ffi/iroh_ffi.h` the corresponding C declarations.

## Phase 4: Wire FFI bindings

Add the FFI function wrappers to:
- Java: `IrohLibrary.java` + new `Registry.java` high-level wrapper
- Go: new `registry_ffi.go` with cgo bindings
- .NET: `Native.cs` + new `Registry.cs` high-level wrapper

## Key files to read first

- `ffi_spec/registry-rust-centralization.md` — the design doc
- `bindings/python/aster/registry/models.py` — data models
- `bindings/python/aster/registry/keys.py` — key helpers
- `bindings/python/aster/registry/client.py` — resolution logic (the main thing to port to Rust)
- `bindings/python/aster/registry/publisher.py` — publishing logic
- `bindings/python/aster/registry/acl.py` — ACL enforcement
- `bindings/python/aster/registry/gossip.py` — gossip integration
- `core/src/lib.rs` — CoreNode, CoreDocsClient, CoreGossipClient (Rust core you'll call into)
- `ffi/src/lib.rs` — existing FFI patterns to follow

## Testing

- Write Rust unit tests in `core/src/registry.rs` for resolution filters and ranking
- Existing Python tests must not break
- Build-verify: `cargo test -p aster_transport_core`, `cargo build -p aster_transport_ffi`
- Build-verify each language binding after Phase 4
