# Aster Auth — WebAuthn + M-R Custodian

**Status:** Design sketch (2026-05-02). Not implemented.
**Scope:** The sole authentication mechanism for Aster, replacing
ad-hoc per-binding auth with a single WebAuthn-bound, custodian-mediated,
device-admitted identity model.
**Replaces:** No prior implementation; supersedes the simpler "custodian
holds the user's key" model that was an early shape in conversation.
**Depends on:** `aster-trust` (rcan), `aster-transport-salvo` (the wire
the custodian listens on), `webauthn-rs` (Rust WebAuthn implementation).

## Decisions locked in

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Auth mechanism | **WebAuthn only** | Origin-bound, hardware-backed, unphishable. No passwords ever. |
| User identity | **Custodian key model with McCallum-Relyea exchange** | Custodian helps recover but never sees the user's master key |
| Device admission | **Required** — every device must be explicitly admitted by an existing admitted device or a recovery code | Stolen passkeys / unauthorized devices cannot bootstrap |
| Curve for M-R + UserKey | **X25519** (and Ed25519 for signing) | Aligns with the rest of Aster's crypto stack |
| Recovery code format | **BIP-39 word lists** | Industry-standard, transcribable, error-detecting |
| Vault storage backend | **Pluggable; iroh-docs default** | Reuses iroh-docs CRDT replication for HA out of the box; PostgreSQL / S3 / filesystem can plug in |

Open items (see [Open decisions](#open-decisions)):
- Single-tenant vs multi-tenant custodian
- Rate-limiting policy specifics
- Audit-log distribution shape

## Why this design

WebAuthn is the only browser primitive that gives you origin-bound,
hardware-backed, unphishable client identity. Everything else (passwords,
client TLS certs in browsers, OAuth password grants) is strictly worse.
But WebAuthn alone has three gaps for Aster's needs:

1. **WebAuthn doesn't sign arbitrary application data.** Assertions sign
   server-supplied challenges in a CBOR wrapper. They can't directly sign
   Aster wire bytes; they're a session-bootstrap proof, not a per-call
   signature.
2. **WebAuthn is per-device.** A user with three devices has three
   credentials. You need a separate "user identity" abstraction that
   spans devices.
3. **WebAuthn alone has no device admission.** Once enrolled, a credential
   is just a credential. There's no "this device must be approved by
   another device first" semantics.

The custodian model fills (1) and (2): a stable User Identity (a Ed25519
keypair) lives across devices, and per-call signing uses ephemeral Device
Session Keys derived per-session. The trust ladder rolls up to UserKey,
which is held with a custodian.

Naive custodians (just "the server holds your key") trade convenience for
trust — a custodian compromise is a full key compromise. **McCallum-Relyea**
fixes that: the custodian helps decrypt the user's vault without ever
seeing the contents, and a server compromise reveals nothing useful
offline. M-R's classic gap — that anyone reaching the server can request
unblinding — is closed by gating vault retrieval behind WebAuthn.

The admission requirement (3) is independent of M-R; it's a vault
invariant. The vault stores an `admission_set` of approved device pubkeys,
each with a signed Admission rcan from UserKey. A device that isn't in
the set can't get a Device Authorization rcan even if it has a valid
WebAuthn credential, because it can't prove membership.

The combination — WebAuthn + M-R + admission set — gives a model where:
- Custodian compromise reveals no user keys.
- Lost device doesn't compromise the user (revoke admission entry).
- New device can't sneak in (admission ceremony required).
- Phishing fails (WebAuthn origin-bound).
- Recovery is possible (printed BIP-39 codes redeemable as admission
  proofs).

## The four-layer model

```
┌─ Layer 0: User Vault ──────────────────────────────────────────────┐
│ Encrypted blob stored at the custodian. Sealed with K (M-R-derived)│
│ Contents:                                                          │
│   - UserKey_priv (Ed25519 master signing key)                      │
│   - admission_set: [DeviceId → AdmissionRcan signed by UserKey]    │
│   - vault_version, schema_hint, recovery_code_hashes               │
│ Retrieval: gated by WebAuthn assertion from an admitted device.    │
└────────────────────────────────────────────────────────────────────┘
                               │
                               │ WebAuthn-gated fetch
                               ▼
┌─ Layer 1: M-R Unblinding ──────────────────────────────────────────┐
│ Device performs blinded round-trip with custodian:                 │
│   1. Has stored ephemeral pubkey E from initial seal.              │
│   2. Picks random e, sends (E + e·G) to custodian.                 │
│   3. Custodian returns s·(E + e·G).                                │
│   4. Device computes s·E = s·(E + e·G) − s·(e·G) = s·(E + e·G) − e·sG. │
│   5. K = HKDF(s·E), decrypts vault → UserKey_priv.                 │
│ Custodian saw only blinded points. Cannot derive K offline.        │
└────────────────────────────────────────────────────────────────────┘
                               │
                               ▼
┌─ Layer 2: Brief UserKey Use ───────────────────────────────────────┐
│ UserKey_priv lives in device memory for SECONDS, used to:          │
│   - mint Device Authorization rcan for D_session_pub               │
│   - add/remove entries in admission_set (during admission /        │
│     revocation ceremonies)                                         │
│   - re-seal vault with new K (rotating ephemeral on every unseal)  │
│ Then UserKey is wiped. Device runs on session key + Device Auth.   │
└────────────────────────────────────────────────────────────────────┘
                               │
                               ▼
┌─ Layer 3: Device Session ──────────────────────────────────────────┐
│ D_session_pub signs all per-call rcans for the session lifetime.   │
│ Validation chain at the service:                                   │
│   per-call rcan → Device Auth rcan → UserKey                       │
│ Service trusts UserKey (announced in the user's vault metadata).   │
└────────────────────────────────────────────────────────────────────┘
```

Each layer's credentials have different lifetimes:

| Credential | Lifetime | Where it lives |
|------------|----------|----------------|
| UserKey | Permanent (rotatable) | Encrypted in vault; transiently in device RAM |
| Device WebAuthn credential | Permanent until revoked | Hardware authenticator (Secure Enclave, TPM, FIDO key) |
| Admission rcan | Permanent until revoked | In vault's admission_set |
| Device Authorization rcan | 30–90 days | Device storage (encrypted at rest with platform keystore) |
| Device Session Key | Per-session (browser tab / app run) | Device RAM only, never persisted |
| Per-call rcan | 5–60 minutes | Re-minted on demand |

## Curves and crypto choices

- **UserKey, Device Authorization rcan signatures, per-call rcan
  signatures, Admission rcans:** Ed25519. Same as Iroh `EndpointId`,
  same as `noq` and `aster-trust` use today.
- **M-R exchange curve:** X25519. The unblinding math works on any
  curve where scalar multiplication is defined; X25519 keeps us on the
  same curve family as the rest of the stack.
- **Vault encryption (with K from M-R):** XChaCha20-Poly1305 AEAD.
  K is 32 bytes (HKDF output from the M-R-derived shared secret), used
  with a fresh 24-byte nonce per re-seal. AAD includes the vault
  version + admission_set hash to prevent rollback / mix-and-match.
- **Recovery code KDF:** Argon2id with high parameters (memory 256 MiB,
  iterations 3, parallelism 1). Recovery codes are the only offline-
  attackable factor; the KDF cost makes brute-forcing infeasible even
  for 128-bit codes.
- **WebAuthn:** RP signing whatever algorithm the authenticator offers.
  Prefer ES256 (P-256), accept Ed25519 (RFC 8032), accept RS256 (RSA).
  We don't pick the curve for WebAuthn — the authenticator does.

## Recovery codes (BIP-39)

When a user enrolls their first device, the custodian generates **N
recovery codes**, each independently usable for one disaster-recovery
ceremony. Each code is encoded as a BIP-39 word list (12-word default,
24-word for high-assurance — same word list as Bitcoin/Ethereum
seed phrases, lowercased English).

Each recovery code:
- Decodes to 128 bits of entropy (12 words) or 256 bits (24 words).
- Is hashed with Argon2id and the hash stored in the vault as
  `recovery_code_hashes[i]`.
- Authorizes a single admission ceremony: redemption proves possession,
  enrolls a new device with WebAuthn, marks the code as redeemed in the
  vault.
- Is consumed on use — never re-usable.

User-facing UX:
- Codes are shown **once** during enrollment. User instructed to print
  them and store offline.
- Codes are shown again only via an authenticated rotation flow (user
  proves identity via existing devices, generates a new code set,
  invalidates the old).
- Default `N = 5` codes. Configurable per deployment policy.

This matches established patterns (1Password Secret Key, Bitwarden
Emergency Access, hardware wallet seed phrases) — users already know
the BIP-39 shape from the crypto-wallet ecosystem.

## Vault storage — pluggable, iroh-docs default

The custodian's persistence layer is behind a `VaultStore` trait:

```rust
pub trait VaultStore: Send + Sync {
    async fn get(&self, vault_id: &VaultId) -> Result<Option<VaultBlob>>;
    async fn put(&self, vault_id: &VaultId, blob: &VaultBlob, version: VaultVersion) -> Result<()>;
    // Optimistic concurrency: write fails if version doesn't match the
    // last version the caller saw, prevents races during admission.
    async fn list_for_recovery(&self, recovery_code_hash: &[u8; 32]) -> Result<Vec<VaultId>>;
    async fn delete(&self, vault_id: &VaultId) -> Result<()>;
    async fn audit_append(&self, vault_id: &VaultId, event: AuditEvent) -> Result<()>;
}
```

### Default backend: iroh-docs

Reuses what we already ship. Each user has an iroh-docs **document**
keyed by `vault_id`; entries within the doc:

| Key | Value |
|-----|-------|
| `vault/blob` | The encrypted vault bytes |
| `vault/version` | Monotonic counter |
| `audit/<timestamp>/<event_id>` | Audit log entries (append-only) |
| `recovery/<code_hash>` | Pointer back to vault_id (lookup table for recovery flow) |

Properties this gives us for free:
- **Replication** — iroh-docs syncs across multiple custodian replicas
  via the standard iroh-docs protocol. Threshold-custodian deployments
  use this.
- **Multi-writer with author identity** — the custodian's own author key
  signs every write; tampering is detectable.
- **Range queries** for the audit feed — users can subscribe to
  `audit/...` and pull recent events.
- **Hash-addressed content** — large vault blobs go to the iroh-blobs
  backend automatically.

Caveats:
- iroh-docs is a CRDT, so writes are eventually consistent across
  replicas. Optimistic-concurrency on `vault/version` requires per-
  replica coordination during admission — not a CRDT-friendly pattern.
  Practical answer: the custodian replica that handles the admission
  request holds a brief write-lock; the lock + version-check are inside
  one replica's transaction; replication propagates afterward. Two
  custodians servicing concurrent admissions for the same user collide,
  rare in practice; if it happens, one ceremony retries.
- Privacy: iroh-docs entry **keys** are not encrypted. The vault blob
  body is. Each user's entry keys are scoped under their `vault_id`,
  but the existence of a vault for a given `vault_id` is visible to
  anyone with read access to the doc. This is fine if `vault_id` is
  itself non-PII (e.g. random 32-byte ID, not "alice@example.com").

### Other backends

The trait is small enough that adding backends is straightforward:

- **PostgreSQL** — single-tenant deployments, ACID transactions, easy
  ops. `VaultId` → row in `vaults` table; audit feed is a separate
  append-only table.
- **S3-compatible object store** — write-once vault blobs, audit feed
  in DynamoDB or similar. Cheap at scale.
- **Filesystem** — for local development, single-process testing,
  homelab-scale single-user deployments.

A deployment chooses one. Backend selection is a config flag on the
custodian.

## Ceremonies

### Enrollment (first device, cold start)

```
User on Device A (browser):                              Custodian:
  1. Visit auth.example.com
  2. Click "Create account"
  3. Browser generates UserKey_priv + UserKey_pub
     (Ed25519, in WASM)
  4. Browser performs WebAuthn create()
     for RP=auth.example.com
                                          ─────────►   record D_A WebAuthn pubkey + AAGUID
  5. Browser receives WebAuthn assertion
  6. Browser computes D_A_session keypair
  7. Browser signs Admission rcan for D_A
     (Admission rcan: "D_A_session_pub admitted at T, scope: full")
     using UserKey_priv
  8. Browser builds initial vault blob:
     { UserKey_priv, admission_set: [D_A entry],
       recovery_code_hashes: [N hashes], v: 1 }
  9. Browser generates N recovery codes (BIP-39),
     hashes each with Argon2id
 10. Browser performs initial M-R seal:
       picks ephemeral c, computes E = c·G,
       fetches custodian's M-R server pubkey s·G,
       computes K = HKDF(c·sG),
       encrypts vault with K, stores E in plaintext alongside
 11. Upload {ciphertext, E, recovery_code_hashes}    
                                          ─────────►   create vault record
                                                      bind WebAuthn cred to vault_id
 12. Browser displays N recovery codes ONCE
 13. UserKey_priv wiped from memory
 14. Browser receives Device Authorization rcan from custodian
     (signed by UserKey_priv during step 11, before wipe)
```

### Login (admitted device, returning session)

```
User on Device A (browser):                              Custodian:
  1. Visit app
  2. App needs auth → redirect to auth.example.com
  3. Browser performs WebAuthn get() for RP=auth.example.com
                                          ─────────►   verify assertion
                                                      check D_A is in vault's admission_set
  4. Browser fetches encrypted vault   ◄──────────   return vault blob + E
  5. Browser performs M-R round-trip:
       picks fresh e, sends (E + e·G)
                                          ─────────►   compute s·(E + e·G) using server key s
                                          ◄─────────   return s·(E + e·G)
  6. Browser computes s·E = s·(E + e·G) − e·s·G
  7. Browser derives K, decrypts vault → UserKey_priv
  8. Browser generates D_A_session keypair (fresh per-session)
  9. Browser signs Device Authorization rcan
     for D_A_session_pub with UserKey_priv (90-day expiry)
 10. Browser wipes UserKey_priv + K
 11. App now has Device Authorization rcan; signs per-call rcans
     with D_A_session_priv as needed.
```

Total user interaction: one WebAuthn prompt. Browser does the M-R
math, vault decrypt, key minting, and wipe transparently in WASM.

### Admission (existing user adds a new device)

```
User on Device B (new):                Custodian:                User on Device A (existing):
  1. Browser generates D_B_session
     keypair
  2. Visit auth.example.com,
     "Add device"
  3. Display QR with admission
     request (D_B_session_pub +
     fingerprint string)
                          ─────────►  store pending admission request
                                                                 4. On Device A:
                                                                    open auth.example.com
                                                                    "Approve new device"
                                                                 5. Show admission requests
                                                                 6. User confirms (sees
                                                                    fingerprint matches what
                                                                    Device B is showing)
                                                                 7. WebAuthn assertion
                                                                                 ─────────►  verify
                                                                 8. Login flow on Device A
                                                                    (steps 4–7 of Login above)
                                                                    → UserKey in memory
                                                                 9. Sign Admission rcan for
                                                                    D_B_session_pub
                                                                10. Add to admission_set,
                                                                    re-seal vault
                                                                                 ─────────►  store updated vault
                                                                11. UserKey wiped
 12. On Device B:                                                
     poll for approval         ◄─────────  approval ready
 13. WebAuthn create() for D_B
     (registers Device B's
     WebAuthn cred with custodian)
                          ─────────►  bind D_B WebAuthn cred to vault
 14. Pull updated vault, run login flow → UserKey
 15. Mint Device Auth for D_B_session
 16. Wipe UserKey
```

User interaction: one prompt per device. Total ceremony ~30 seconds.

### Recovery (all devices lost)

```
User (no devices):                                     Custodian:
  1. Visit auth.example.com on a fresh device
  2. Click "Lost all devices"
  3. Enter BIP-39 recovery code (12 or 24 words)
  4. Browser hashes the code with Argon2id
  5. Browser submits hash               ─────────►   look up vault_id by recovery_code_hash
                                        ◄─────────   return vault blob + E + hash list
  6. Browser performs M-R unseal as in Login
  7. Browser decrypts vault → UserKey_priv
  8. Browser generates new device keypair, runs WebAuthn create()
  9. Browser signs Admission rcan for new device
 10. Browser MARKS RECOVERY CODE AS REDEEMED in vault
 11. Browser re-seals vault with fresh ephemeral
 12. Upload + UserKey_priv wipe                    ─────────►   replace vault record
 13. Mint Device Auth for new device
 14. Display: "1 of N recovery codes remaining. Generate replacements?"
```

The recovery flow is functionally a high-stakes admission ceremony where
the printed BIP-39 code substitutes for an existing-device approver.
The recovery code is consumed and cannot be reused.

## Custodian service contract

The custodian is itself an Aster service running on the Salvo HTTP
transport. Its contract is defined in `contracts/aster-custodian.contract`
(roughly):

```
service AsterCustodian {
  // Vault lifecycle
  rpc CreateVault(CreateVaultRequest) -> VaultMetadata
  rpc GetVault(GetVaultRequest) -> VaultBlob
  rpc PutVault(PutVaultRequest) -> VaultMetadata          // optimistic concurrency
  rpc DeleteVault(DeleteVaultRequest) -> Empty            // requires UserKey signature

  // M-R operations
  rpc Unblind(UnblindRequest) -> UnblindResponse          // s·(E + e·G)
  rpc GetServerPubkey() -> ServerPubkey                   // s·G + key generation metadata

  // Admission
  rpc CreatePendingAdmission(PendingAdmissionRequest) -> AdmissionRequestId
  rpc GetPendingAdmission(AdmissionRequestId) -> PendingAdmission
  rpc ApprovePendingAdmission(ApproveAdmissionRequest) -> Empty
  rpc CancelPendingAdmission(AdmissionRequestId) -> Empty

  // WebAuthn ceremonies
  rpc StartWebAuthnRegistration(StartRegRequest) -> RegistrationChallenge
  rpc CompleteWebAuthnRegistration(CompleteRegRequest) -> Empty
  rpc StartWebAuthnAssertion(StartAssertRequest) -> AssertionChallenge
  rpc CompleteWebAuthnAssertion(CompleteAssertRequest) -> AssertionResult

  // Recovery
  rpc StartRecovery(RecoveryRequest) -> RecoveryChallenge
  rpc CompleteRecovery(CompleteRecoveryRequest) -> VaultBlob   // returns vault if code valid

  // Audit
  rpc StreamAuditFeed(AuditFeedRequest) -> stream AuditEvent
  rpc GetAuditEvent(AuditEventId) -> AuditEvent
}
```

The contract is normal Aster — it benefits from the contract-identity,
codec, and interceptor machinery. Bindings are auto-generated for
Python, TypeScript, Java by the existing codegen.

The custodian *itself* enforces:
- Rate limits on every endpoint.
- WebAuthn-required gating on `GetVault`, `PutVault`, `DeleteVault`,
  `CreatePendingAdmission`, `ApprovePendingAdmission`, `StartRecovery`.
- M-R `Unblind` is rate-limited per-vault (not per-IP).
- Audit log appended to on every state-changing call.

## Browser library

The browser-side glue lives in `bindings/typescript/aster-auth/`:

```
aster-auth/
  index.ts                    public API: createAccount, login, addDevice, recover
  webauthn.ts                 ceremony wrappers (navigator.credentials.*)
  vault.ts                    seal/unseal with WASM M-R
  session.ts                  session keypair + per-call rcan signing
  bip39.ts                    BIP-39 wordlist + entropy ↔ words
  storage.ts                  IndexedDB for cached Device Auth + non-sensitive metadata
```

Critical browser-side invariants:

1. **UserKey lifetime is seconds.** Generated/derived for ceremony,
   used for signing, immediately wiped via overwriting the underlying
   `Uint8Array` and dropping the WASM allocation.
2. **Device Session Key never leaves browser RAM.** Not persisted to
   IndexedDB or any storage.
3. **Device Authorization rcan IS persisted** in IndexedDB, encrypted
   with a key sealed by `navigator.locks` API + WebAuthn-bound key
   wrapping where available. Treated as bearer credential within a
   browser profile.
4. **WASM module is loaded from the custodian origin** to keep
   integrity-checked. SRI hash + signed releases.
5. **No telemetry, no analytics, no third-party scripts** on the
   custodian origin. Standard hardening for an auth surface.

## Wire formats

Vault blob (after AEAD decrypt):

```
VaultPlaintext = CBOR-encoded:
  {
    schema_version: 1,
    vault_version: u64,         // monotonic, used for optimistic concurrency
    user_key_priv: [u8; 32],    // Ed25519 seed
    admission_set: [
      AdmissionEntry {
        device_id: bytes,
        device_pubkey: [u8; 32],   // Ed25519 device session pubkey at admission time (not stable across sessions)
        webauthn_credential_id: bytes,  // links to the WebAuthn cred at custodian
        admission_rcan: AdmissionRcan,  // signed by UserKey
        admitted_at: u64,
        admitted_by_device: Option<device_id>,  // None if redeemed via recovery code
        scopes: bytes,           // capability scope payload, format TBD
      },
      ...
    ],
    recovery_codes: [
      {
        hash: [u8; 32],          // Argon2id of the BIP-39 code
        salt: [u8; 16],
        params: Argon2idParams,
        created_at: u64,
        redeemed_at: Option<u64>,
      },
      ...
    ],
    rotation_history: [          // for UserKey rotation events
      {
        from_pubkey: [u8; 32],
        to_pubkey: [u8; 32],
        rotated_at: u64,
        reason: RotationReason,  // user-initiated / scheduled / compromise
      }
    ],
  }
```

Vault wire (what's sent to custodian):

```
VaultBlob = {
  vault_id: [u8; 32],
  ciphertext: bytes,             // AEAD output
  nonce: [u8; 24],               // XChaCha20-Poly1305 nonce
  ephemeral_pubkey_E: [u8; 32],  // M-R ephemeral, plaintext
  server_pubkey_id: bytes,       // which custodian server key was used
  vault_version: u64,
  aad_digest: [u8; 32],          // hash of (schema_version, vault_version, server_pubkey_id) bound into AEAD
}
```

Admission rcan (within `aster-trust`'s rcan format):

```
AdmissionRcan = {
  iss: UserKey_pub,                // issuer
  aud: device_pubkey,              // who this admission is for
  sub: "device-admission",
  scope: capabilities_bytes,
  iat: u64,
  exp: Option<u64>,                // device admission has no expiry by default
  nonce: [u8; 16],
  parent: None,
  signature: Ed25519Sig,           // signs all of the above
}
```

Device Authorization rcan:

```
DeviceAuthRcan = {
  iss: UserKey_pub,
  aud: D_session_pub,              // ephemeral session key
  sub: "device-session",
  scope: capabilities_bytes,
  iat: u64,
  exp: u64,                        // 30–90 days
  nonce: [u8; 16],
  parent: AdmissionRcan,           // chains back
  signature: Ed25519Sig,
}
```

Per-call rcan (already in `aster-trust`):

```
PerCallRcan = {
  iss: D_session_pub,
  aud: target_service_id,
  sub: method_name,
  scope: per-call constraints,
  iat: u64,
  exp: u64,                        // 5–60 minutes
  nonce: [u8; 16],
  parent: DeviceAuthRcan,
  signature: Ed25519Sig,
}
```

Service validation walks the chain:

```
verify(per_call_rcan):
  verify Ed25519 signature with iss = D_session_pub
  fetch parent = DeviceAuthRcan
  verify Ed25519 signature with iss = UserKey_pub
  fetch parent = AdmissionRcan
  verify Ed25519 signature with iss = UserKey_pub
  check D_session_pub appears in AdmissionRcan.aud chain
  check none of the rcans appear in revocation list
  check exp/iat for each
  return identity = UserKey_pub
```

The chain is fully verifiable offline (no custodian round-trip needed
per call). Revocation lists are the only thing that needs distribution
to services.

## Threat model

| Threat | Mitigation |
|--------|-----------|
| Custodian server compromise | Attacker gets vaults; cannot extract UserKey offline (M-R blinding). Cannot forge admission entries (vault is encrypted with K). |
| Custodian operator malice | Same as compromise. Operator cannot mint Device Auth rcans (no UserKey). |
| Custodian operator silently adds device to admission_set | Impossible. Vault is encrypted; admission_set tampering breaks AEAD. |
| Custodian denial of service | Single-server: no logins. Threshold-custodian (Shamir over multiple): tolerates 1-of-3 outage. |
| Lost device | Revoke the device's admission entry (requires another admitted device) and add to revocation list. Device Auth rcan invalidated within revocation refresh interval. |
| Stolen device with active session | Per-call rcan expiry (5–60 min) bounds attacker's window. Revocation accelerates closure. |
| Stolen device with valid passkey, no active session | Attacker must pass WebAuthn user-verification (biometric/PIN). Devices with synced passkeys + weak local lock = highest risk; mitigate via deployment policy requiring device-bound passkeys. |
| Phishing | WebAuthn is RP-ID-bound. Attacker domain cannot trigger user's auth.example.com credential. |
| MITM during M-R round-trip | TLS to custodian (HTTPS via Salvo H2/H3). M-R is not the layer that authenticates the channel. |
| Replay of M-R round-trip | Round-trip bound to TLS session. Custodian rate-limits per-vault. Each unblinding logged. |
| New unauthorized device with stolen WebAuthn credential | Device must be in admission_set. Stolen credential not in set = no vault retrieval. |
| Recovery code theft | Argon2id hash + 128-bit entropy resists offline attack. Codes consumed on use; user sees redemption in audit log. Threshold custodian raises the bar further. |
| All devices lost AND recovery codes lost | Identity unrecoverable. By design — no custodian-side override. (Optional opt-in: enterprise deployments can configure social-recovery via M-of-N other admins.) |
| Quantum attack on Ed25519 / X25519 | UserKey rotatable. M-R re-key pulls a fresh ephemeral. Vault re-seal cheap. Plan to add hybrid PQ (Kyber + Dilithium) when constructions stabilize. |
| Phishing the admission ceremony QR | User must visually compare fingerprint shown on both devices. Mismatched fingerprints = abort. |
| Custodian deletion of user vault | Threshold custodian: deletion needs majority; single custodian: trust the operator. Audit log makes deletion visible if backed up off-custodian. |

## Where this lives in the codebase

```
crates/
  aster-trust/                        # rcan structures, validation chain (exists)
  aster-mr/                           # NEW — McCallum-Relyea client + server
    src/
      lib.rs                          # public API
      curve.rs                        # X25519 scalar-mult primitives
      client.rs                       # vault sealing/unsealing, blinded round-trip
      server.rs                       # custodian-side unblinding service
      shamir.rs                       # threshold split/reconstruct (feature-gated)
  aster-bip39/                        # NEW — recovery code generation/parsing
    src/
      lib.rs
      wordlist.rs                     # BIP-39 English wordlist (vendored, MIT)
      argon.rs                        # Argon2id KDF wrappers
  aster-custodian/                    # NEW — custodian service
    src/
      lib.rs                          # service entry point
      contract.rs                     # generated from aster-custodian.contract
      webauthn.rs                     # webauthn-rs wrappers, RP config
      vault_store/                    # VaultStore trait + impls
        iroh_docs.rs                  # default backend (uses iroh-docs fork)
        postgres.rs                   # feature-gated
        s3.rs                         # feature-gated
        filesystem.rs                 # feature-gated; for dev
      admission.rs                    # admission ceremony state machine
      recovery.rs                     # recovery code redemption
      audit.rs                        # audit log append + query
      policy.rs                       # rate limits, scope policies
  aster-auth-wasm/                    # NEW — WASM build of mr + bip39 + vault crypto + rcan signing
    src/
      lib.rs                          # wasm-bindgen surface
      seal.rs                         # vault seal/unseal entry points
      sign.rs                         # rcan signing entry points
contracts/
  aster-custodian.contract            # NEW — service contract for the custodian
bindings/typescript/aster-auth/       # NEW — browser library
  src/
    index.ts
    webauthn.ts
    vault.ts                          # calls into aster-auth-wasm
    session.ts
    bip39.ts
    storage.ts
```

The custodian is built on the Salvo HTTP transport (so browsers reach
it natively); other Aster services delegate auth to it via Aster RPC
(the `AsterCustodian.CompleteWebAuthnAssertion` call returns an rcan
the calling service can validate locally afterwards).

## Open decisions

These were deferred during design discussion. Each has reasonable
defaults; revisit when a concrete deployment forces the question.

### 4. Single-tenant vs multi-tenant custodian

Single-tenant (one custodian per organization) is simplest:
- One server keypair (s, sG).
- One iroh-docs document (or PostgreSQL DB) per deployment.
- All users share one operator's trust assumption.

Multi-tenant (one custodian serves multiple orgs / apps):
- Per-tenant server keypairs (so tenants can't unblind each others' vaults).
- Per-tenant vault namespaces.
- Per-tenant rate limits, audit log scopes.
- Per-tenant policies (recovery code count, code length, M-R curve choice).

**Default:** single-tenant. Multi-tenancy added when a deployment needs
it; isolation is rigorous (separate server keys, separate iroh-docs
namespaces) so the upgrade path is clean.

### 5. Rate-limiting policy

Initial target numbers, to revise after deployment:

| Endpoint | Limit |
|----------|-------|
| `Unblind` | 10 / minute / vault_id; 100 / minute / IP |
| `GetVault` | 30 / minute / vault_id; 300 / minute / IP |
| `StartWebAuthnAssertion` | 60 / minute / IP |
| `StartRecovery` | 5 / hour / IP |
| `CompleteRecovery` | 1 / minute / IP (after challenge issued) |
| `CreateVault` | 5 / hour / IP |

Per-IP limits sit behind rate-limit middleware in Salvo. Per-vault_id
limits sit in the custodian service logic. Distinguish "anonymous IP
attacker probing recovery codes" (needs aggressive limits) from
"legitimate user logging in" (needs leniency).

### 6. Audit log distribution

Two shapes:

a) **Custodian-only.** Audit log lives in iroh-docs / Postgres, user
   queries it via `StreamAuditFeed`. Single source of truth, simple.
   Trust: custodian could omit entries.

b) **User-replicated.** Each significant event is signed by the
   custodian + delivered to the user's devices, which keep a local
   audit replica. Tampering by the custodian is detectable (missing
   sequence numbers).

Default: (a). Move to (b) when verifiable audit becomes a deployment
requirement (e.g. regulated environments).

## What this does *not* change

- **rcan format** — `aster-trust` chain validation is unchanged. Adding
  Admission and Device Authorization rcans is just new instances of the
  existing rcan structure with new `sub` types.
- **Service code** — services validate rcan chains exactly as before;
  they don't know whether the chain bottoms out in a custodian-managed
  UserKey or any other root.
- **Iroh transport** — Iroh peers can still authenticate via raw
  `EndpointId` for service-to-service calls. Browser auth is custodian-
  mediated; native auth can still be peer-direct if a service opts in.
- **Codegen** — bindings generate from the custodian contract like any
  other Aster service.

The custodian is a normal Aster service. Its specialness is operational
(the only one allowed to attest WebAuthn to UserKey) not architectural.
