# Known Issues

Tracked gaps and caveats in the Aster API surface. Each entry notes scope,
impact, and what closing it would take.

## HPKE identity-derivation is Rust-only

**Scope:** `hpke_x25519_secret_from_identity`, `hpke_x25519_public_from_identity`,
and `hpke_x25519_public_from_node_id` are exposed on the Rust `aster` facade
(`aster/src/crypto.rs`) only. They have **no** Python, TypeScript, or FFI
(Java/Kotlin) bindings yet.

**Impact:** Low. The only current consumer is portal-sync, whose daemon/CLI is
Rust, so the facade is sufficient. Cross-language callers cannot derive an HPKE
recipient keypair from an Aster (Ed25519) identity.

**Closing it:** The logic lives in `aster_transport_core::hpke_envelope`
(`hpke_x25519_secret_from_identity` / `hpke_x25519_public_from_identity`), so each
binding is a thin wrapper following the existing `hpke_*` pattern already present
in those layers:

- Python — `bindings/python/rust/src/crypto.rs`
- TypeScript — `bindings/typescript/native/src/crypto.rs`
- FFI (Java/Kotlin) — `ffi/`

No core changes are required; this is mechanical parity work to be done when a
cross-language consumer needs it.
