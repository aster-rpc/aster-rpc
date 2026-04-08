# Aster Identity & Join — End-to-End Design

**Status:** Design  
**Date:** 2026-04-08  
**Focus:** Security + developer UX for an audience that hates premature signups

---

## Principles

1. **Useful before signup.** The shell works fully offline. You see your local services, invoke RPCs, browse peers. No account needed.
2. **Signup is one command, not a web form.** Developers live in the terminal. Don't make them context-switch.
3. **Root key is the identity.** No passwords. The operator's ed25519 root key (already generated for trust) is the authentication credential. Your key *is* your login.
4. **Email is verification, not identity.** We collect email only to (a) confirm you're human, (b) send rare security notifications, (c) handle recovery. It's not a username.
5. **Opt-out is first-class.** A single config flag disables all `@aster` service communication. The shell still works.

---

## 1. Identity Model

### Local Identity (exists today)

When a user runs `aster keygen root` or `aster init`, they get:

```
~/.aster/
├── config.toml          # active_profile, root_pubkey per profile
└── root.key             # ed25519 private key (fallback; prefer OS keyring)
```

The root public key is their cryptographic identity. It's a 32-byte ed25519 key, hex-encoded. This already exists and is used for enrollment signing.

### Display Handle (derived, pre-registration)

Before joining `@aster`, the shell needs a display name. We derive one from the root pubkey:

```
pubkey: 7f3a2bc9de01...  →  display: @7f3a2bc9de01
```

This is the first 12 hex chars of the root pubkey, prefixed with `@`. It appears in the shell prompt and VFS path. It's not a "real" handle — it's a local placeholder.

### Registered Handle (post-join)

After `aster join`, the user has a claimed handle on `@aster`:

```
pubkey: 7f3a2bc9de01...  →  handle: @emrul
```

The mapping `handle ↔ root_pubkey` is stored on `@aster` and cached locally in `config.toml`.

### Config Extensions

```toml
active_profile = "default"

[profiles.default]
root_pubkey = "7f3a2bc9de01..."
signer = "local"

# NEW: @aster registration state
handle = "emrul"                    # claimed handle (empty if unregistered)
handle_status = "verified"          # "unregistered" | "pending" | "verified"
handle_claimed_at = "2026-04-08T14:30:00Z"
email = "emrul@example.com"        # stored locally for display only

[aster_service]
enabled = true                      # false = disable all @aster service communication
node_id = ""                        # override for self-hosted registry (default: hardcoded)
relay = ""                          # override relay URL (default: public Iroh relay)
offline_banner = true               # show online/offline indicator
```

### CLI Override

```bash
# Air-gapped mode: no @aster comms for this session (relays for hole-punching still work)
aster shell --air-gapped

# Permanently disable @aster service
aster config set aster_service.enabled false
```

`--air-gapped` disables all communication with `@aster` (join, verify, discover, publish, status checks). It does **not** disable Iroh relays — those are transport infrastructure for P2P hole-punching, not an external service dependency.

---

## 2. Shell UX States

### State A: No Root Key

User has never run `aster init` or `aster keygen root`. The shell still works for connecting to peers, but there's no local identity.

```
┌─ aster shell ────────────────────────────────────┐
│ No identity configured.                          │
│ Run 'aster join' to get started.                 │
└──────────────────────────────────────────────────┘
```

The shell works — they can browse `/services`, invoke RPCs against connected peers. But `/aster` is empty.

**`aster join` handles this state automatically** — if no root key exists, it creates one before proceeding to handle registration. The user never needs to run `aster init` or `aster keygen root` as a separate step. One command from zero to registered.

### State B: Root Key, Unregistered

User has a root key but hasn't joined `@aster`. This state occurs if the user created a key via `aster init` or `aster keygen root` but hasn't run `aster join` yet, or if they hit Ctrl+C during `aster join` before the handle was reserved.

```
┌─ aster ──────────────────────────────────────────┐
│ @7f3a2bc9 · 2 local services · not registered    │
│                                                  │
│ ░ Claim your handle: aster join                  │
└──────────────────────────────────────────────────┘

@7f3a2bc9:/aster/7f3a2bc9$ ls
  TaskManager      v3  3 methods  ⬡ unpublished
  InvoiceService   v1  2 methods  ⬡ unpublished
```

The banner is:
- Dimmed/subtle (`░` prefix or muted color) — not aggressive
- Single line — doesn't waste vertical space
- Shows the command to run — no "click here" or "visit website"
- Disappears after `aster join` or can be dismissed with `aster config set aster_service.offline_banner false`

### State C: Pending Verification

User has run `aster join` and is waiting for email verification.

```
┌─ aster ──────────────────────────────────────────┐
│ @emrul · pending verification · check your email │
│                                                  │
│ ░ Paste your code: aster verify <code>           │
│ ░ Resend: aster verify --resend                  │
└──────────────────────────────────────────────────┘

@emrul:/aster/emrul$ ls
  TaskManager      v3  3 methods  ⬡ unpublished
  InvoiceService   v1  2 methods  ⬡ unpublished
```

Handle is shown optimistically (reserved but unverified). If verification expires (24h), reverts to State B.

### State D: Verified / Registered

```
┌─ aster ──────────────────────────────────────────┐
│ @emrul · 2 services · online                     │
└──────────────────────────────────────────────────┘

@emrul:/aster/emrul$ ls
  TaskManager      v3  3 methods  ● published
  InvoiceService   v1  2 methods  ⬡ unpublished
```

No banner. Clean. The `● published` / `⬡ unpublished` indicators tell the story.

### State E: Offline (`@aster` unreachable)

```
┌─ aster ──────────────────────────────────────────┐
│ @emrul · 2 services · offline                    │
└──────────────────────────────────────────────────┘
```

Everything works locally. Published services show cached state. Discovery of other handles uses cached data (if any). No error — just `offline` in the status line.

### State F: Air-Gapped (opt-out)

```
┌─ aster ──────────────────────────────────────────┐
│ @emrul · 2 local services                        │
└──────────────────────────────────────────────────┘
```

No online/offline indicator. No banner. No `@aster` service calls. Pure local mode.

---

## 3. `aster join` Flow

### Command Syntax

```bash
# Interactive (recommended)
aster join

# Non-interactive
aster join --handle emrul --email emrul@example.com
```

Also available inside the shell as the `join` command.

### Step-by-Step Interactive Flow

```
$ aster join

  Creating your Aster identity...
  ✓ Identity created.                          ← only if no root key exists; silent if it does

  Choose a handle (3+ chars, lowercase, letters/numbers/hyphens):
  ▸ emrul

  Checking availability... ✓ available

  Email address (for verification only — never shared):
  ▸ emrul@example.com

  ┌─────────────────────────────────────────────────┐
  │ Optional: receive Aster announcements?          │
  │ ~2-3 emails/year. No spam. No third parties.    │
  │                                                 │
  │ [Y]es  [n]o                                     │
  └─────────────────────────────────────────────────┘
  ▸ y

  Registering...
  ✓ Handle 'emrul' reserved.
    Verification code sent to emrul@example.com
    Paste it here or run: aster verify <code>

    ⚠ Handle released if not verified within 24 hours.

  Verification code:
  ▸ 847291

  ✓ Email verified. Welcome to Aster, @emrul!
```

If the user hits **Ctrl+C** at the verification code prompt, the join is not lost. The handle is reserved (State C) and they can resume later with `aster verify <code>`. The shell will show the State C banner on next launch.

### What Happens If They Don't Verify Immediately

The shell remembers `handle_status = "pending"`. Next time they open the shell, State C banner appears. They can:

```bash
aster verify 847291          # paste the code
aster verify --resend        # resend verification email
```

### Error States

These are the unhappy paths. Every one should feel recoverable, not fatal.

**`@aster` unreachable during join:**
```
$ aster join
  ...
  Choose a handle: ▸ emrul
  Checking availability...

  ✗ Can't reach @aster right now. Your identity is saved locally.
    Try again later with: aster join
    Or use --air-gapped to work without @aster.
```
No blame on the user. Identity is created locally regardless (State B).

**Handle taken:**
```
  Choose a handle: ▸ emrul
  Checking availability... ✗ taken

  Try another:
  ▸ emrul-dev
  Checking availability... ✓ available
```
Re-prompt immediately. Don't dump them back to the shell.

**Email already associated:**
```
  Email address: ▸ emrul@example.com

  ✗ This email is already associated with an account.
    Use a different email, or run: aster recover --handle <your-handle>
```
Never reveal which handle owns the email. Privacy.

**Wrong verification code:**
```
  Verification code: ▸ 123456
  ✗ Invalid code. 4 attempts remaining.

  Verification code: ▸ _
```
Show remaining attempts. Re-prompt immediately.

**Code expired:**
```
  Verification code: ▸ 847291
  ✗ Code expired. Run: aster verify --resend
```

**Ctrl+C during verification prompt:**
```
  Verification code: ^C

  Handle 'emrul' is reserved. Verify later with:
    aster verify <code>
```
Graceful exit. Handle stays reserved (24h TTL). State C persisted to config.

### Handle Validation Rules

| Rule | Detail |
|------|--------|
| Length | 3–39 characters |
| Characters | Lowercase `a-z`, digits `0-9`, hyphens `-` |
| Start/end | Must start and end with a letter or digit |
| No consecutive hyphens | `my--handle` rejected |
| Reserved words | See appendix A |
| Uniqueness | Checked against `@aster` service directory |

### Email Rules

| Rule | Detail |
|------|--------|
| Format | Standard RFC 5322 validation |
| Uniqueness | One verified handle per email address |
| Pending | If email has a pending (unverified) handle, inform user and offer to resend or claim a different handle |
| Already verified | If email is already verified with a different handle, reject with "This email is already associated with an account" (do not reveal the existing handle — privacy) |

---

## 4. Verification Code Design

### Why Codes, Not Links

- Developers are in the terminal. Switching to a browser to click a link is friction.
- A 6-digit code can be typed or pasted in 2 seconds.
- The same mechanism works for future MFA / re-authentication.

### Code Properties

| Property | Value |
|----------|-------|
| Format | 6 digits, numeric (easy to read aloud / type) |
| Entropy | 10^6 = ~20 bits. Acceptable with rate limiting. |
| TTL | 15 minutes per code |
| Attempts | 5 attempts per code before invalidation |
| Resend cooldown | 60 seconds between resends |
| Max resends | 5 per 24h period |
| Delivery | Email only (SMTP) |

### Code Generation

Generate a random 6-digit code, store `argon2(code)` server-side. The code itself is never stored in plaintext — only its hash. Single-use, short-lived, simple.

Server stores: `{pubkey, email, code_hash, attempts_remaining, expires_at, created_at}`.

### Rate Limiting (Anti-Brute-Force)

| Scope | Limit |
|-------|-------|
| Per pubkey | 5 verification attempts per 15 minutes |
| Per email | 3 join requests per hour |
| Per IP | 10 join requests per hour |
| Global | Circuit breaker at 1000 joins/hour (alerts ops) |

After 5 failed attempts, the code is invalidated and a new one must be requested via `aster verify --resend`. This makes brute-forcing a 6-digit code infeasible: 5 guesses out of 10^6 = 0.0005% chance.

### Future: MFA for Sensitive Operations

The same code mechanism extends to:

```bash
aster confirm <code>     # general-purpose verification
```

Used for: handle transfer, email change, account deletion, publishing to a new handle. The server sends a code to the registered email; the user pastes it back.

---

## 5. Security Model

### Request Signing

Every request to `@aster` is signed by the operator's root key. No passwords, no bearer tokens, no OAuth.

#### Signed Request Format

```json
{
  "payload": {
    "action": "join",
    "handle": "emrul",
    "email": "emrul@example.com",
    "announcements": true,
    "timestamp": 1744123800,
    "nonce": "a1b2c3d4e5f6..."
  },
  "pubkey": "7f3a2bc9de01...",
  "signature": "ed25519_sign(canonical_json(payload), root_private_key)"
}
```

#### Verification on Server

1. Parse `pubkey` and `signature` from request
2. Canonical-JSON encode `payload` (sorted keys, no whitespace — same as enrollment credential signing)
3. `ed25519_verify(signature, canonical_payload, pubkey)` → reject if invalid
4. Check `timestamp` is within ±5 minutes of server time → reject if stale (replay protection)
5. Check `nonce` has not been seen before (in a 10-minute sliding window) → reject if replayed
6. Proceed with action

#### Why Not Bearer Tokens?

- Bearer tokens require a "login" step — extra UX friction
- Bearer tokens can be stolen from disk/memory and reused
- Ed25519 signatures are non-replayable (nonce + timestamp)
- The root key already exists — no new credential to manage
- Stateless server-side: no session store, no token refresh logic

#### Canonical JSON

Same encoding used for enrollment credential signing (already implemented in `aster/trust/signing.py`):
- Keys sorted lexicographically
- No whitespace
- No trailing commas
- UTF-8 encoded
- Numbers as integers (no floats)

### Transport Security

All communication with `@aster` uses Aster RPC over QUIC (Iroh transport). The QUIC connection provides TLS 1.3 encryption and mutual endpoint authentication. The ed25519 request signing is defense-in-depth on top of the transport — it proves the request came from the holder of the root key, not just any connected peer.

No DNS, no HTTPS, no HTTP proxies. The CLI connects to `@aster` by node ID (hardcoded in the CLI, overridable via config) through Iroh's relay network for NAT traversal. This is the same transport every Aster service uses.

### Handle Squatting Prevention

| Mechanism | Detail |
|-----------|--------|
| 3-char minimum | Short enough for real names (e.g., `dan`, `ali`), long enough to prevent single-char squatting |
| Reserved word list | Blocks `aster`, `admin`, `system`, etc. (appendix A) |
| Email verification | One handle per email — can't mass-register |
| Rate limiting | 3 join requests per hour per email, 10 per IP |
| 24h verification TTL | Unverified handles auto-release |
| Abuse review | Handles reported for squatting reviewed by ops |

### Key Loss: Why It's a Big Deal

Losing the root key isn't just losing access to `@aster`. The root key is the **trust anchor for the entire deployment** (see Aster-trust-spec.md §1.2). Everything downstream depends on it:

| What breaks | Why |
|-------------|-----|
| **Handle ownership** | Can't sign requests to `@aster` |
| **New producer enrollment** | Can't sign `EnrollmentCredential` for new mesh nodes |
| **New consumer enrollment** | Can't sign `ConsumerEnrollmentCredential` (policy or OTT) |
| **Existing producer credentials** | Still work until `expires_at` — then nodes can't re-enroll |
| **Existing consumer credentials** | Still work until expiry — then consumers can't re-enroll |
| **`@aster` delegation** | Consumers enrolled via `@aster` still work (signed by `@aster`'s key, not yours) — but you can't manage access grants |
| **Published services** | Still listed, still reachable — but you can't update or unpublish them |

The blast radius is proportional to credential TTLs. Short TTLs (2h) mean the mesh degrades within hours. Long TTLs (24h) buy time but widen the window if the key was stolen rather than lost.

### Prevention: Recovery Codes (Generated at First Publish)

Recovery codes are generated **at first publish, not at join time**. Why: at join time the user hasn't invested anything yet — they won't take the time to safely store codes for a handle they just claimed 5 seconds ago. At first publish, they're putting a real service online. Now they care.

```
$ aster publish TaskManager
  ...
  ✓ TaskManager published to @emrul.

  ┌─────────────────────────────────────────────────┐
  │ RECOVERY CODES — save these somewhere safe      │
  │                                                 │
  │ Your handle now has a published service.         │
  │ These codes can restore your handle if you      │
  │ lose your root key. Each code works once.       │
  │                                                 │
  │   1. KFBR-7942-GMTN     5. XVPW-3618-DLQJ    │
  │   2. HNCX-5103-YRWZ     6. AQMS-8274-UFCE    │
  │   3. TDLP-6837-VASK     7. JZRN-4059-WHBG    │
  │   4. WMGE-2491-BFJX     8. CPVL-9306-KTYA    │
  │                                                 │
  │ Treat these like a password. Store offline.     │
  │ We cannot recover your account without them     │
  │ or access to your email.                        │
  └─────────────────────────────────────────────────┘

  Save these codes now? [Enter to continue]
```

Only shown once (first publish). Subsequent publishes don't re-show. If the user needs new codes: `aster recovery-codes regenerate`.

**Code properties:**

| Property | Value |
|----------|-------|
| Format | 4-4-4 uppercase alphanumeric (no ambiguous chars: no 0/O, 1/I/L) |
| Count | 8 codes generated at first publish |
| Entropy | ~62 bits per code (sufficient for single-use offline secret) |
| Storage (server) | argon2 hash of each code |
| Storage (client) | Displayed once, never stored by us. User's responsibility. |
| Usage | Single-use. Consumed on successful recovery. |
| Regeneration | `aster recovery-codes regenerate` (requires current root key signature) — invalidates all old codes, generates 8 new ones |

**Why not just email?** Email accounts get compromised. Recovery codes are offline — they can be printed, stored in a password manager, or locked in a safe. They're the strong factor. Email is the convenience fallback for people who lose both their key and their codes (which is bad, but happens).

**What about handles with no published services?** If a user never publishes, they only have email recovery. This is acceptable — they haven't put anything at stake yet. The moment they publish, we generate codes and show them.

### Recovery Paths (Ranked by Security)

#### Path 1: Recovery Code (Strongest)

User has a recovery code but not the root key.

```
$ aster recover --handle emrul

  Recovery method:
  [1] Recovery code (recommended)
  [2] Email verification
  ▸ 1

  Enter a recovery code:
  ▸ KFBR-7942-GMTN

  Generating new root key...
  ✓ New root key created.

  Verifying recovery code with @aster...
  ✓ Recovery code accepted. Code consumed (7 remaining).

  ┌─────────────────────────────────────────────────┐
  │ Handle @emrul rebound to new root key.          │
  │                                                 │
  │ New root pubkey: 9a8b7c6d5e4f...                │
  │                                                 │
  │ What you need to do now:                        │
  │ 1. Re-enroll your producer nodes (within 24h    │
  │    if using default credential TTL)             │
  │ 2. Re-issue consumer credentials                │
  │ 3. Update published services: aster republish   │
  │                                                 │
  │ Cooldown: 24 hours before new publishes         │
  │ (existing published services remain live)       │
  └─────────────────────────────────────────────────┘
```

**Properties:**
- No email round-trip needed — instant recovery
- Recovery code is consumed (single-use)
- 24-hour publish cooldown (not 72h — code-based recovery is higher assurance)
- Notification email sent to registered address: "Your root key was changed via recovery code"

#### Path 2: Email Verification (Fallback)

User has neither root key nor recovery codes, but still controls the email.

```
$ aster recover --handle emrul

  Recovery method:
  [1] Recovery code (recommended)
  [2] Email verification
  ▸ 2

  Confirm email address for @emrul:
  ▸ emrul@example.com

  Sending verification code...
  ✓ Code sent to em***@example.com

  Enter verification code:
  ▸ 847291

  Generating new root key...
  ✓ New root key created.

  ┌─────────────────────────────────────────────────┐
  │ Handle @emrul rebound to new root key.          │
  │                                                 │
  │ ⚠ 72-HOUR COOLDOWN active.                     │
  │ Publishing and access management are disabled   │
  │ until 2026-04-11 14:35 UTC.                     │
  │                                                 │
  │ This protects against email-based takeover.     │
  │ Your existing published services remain live.   │
  │                                                 │
  │ What you need to do now:                        │
  │ 1. Re-enroll producer nodes (after cooldown)    │
  │ 2. Re-issue consumer credentials                │
  │ 3. Regenerate recovery codes: aster             │
  │    recovery-codes regenerate                    │
  └─────────────────────────────────────────────────┘
```

**Properties:**
- 72-hour publish cooldown (email-only recovery is lower assurance)
- All old recovery codes invalidated (attacker may have them)
- Notification email sent immediately and again at cooldown expiry
- Rate limit: 1 email recovery attempt per handle per 24 hours

#### Path 3: Key Rotation (Proactive, Not Recovery)

User still has the old root key and wants to rotate to a new one. This is **not recovery** — it's planned rotation.

```
$ aster key rotate

  Current root key: 7f3a2bc9de01...
  Generating new root key...

  Signing rotation request with OLD key...
  ✓ Rotation complete.

  New root pubkey: 9a8b7c6d5e4f...
  No cooldown — old key authorized the change.

  Re-enroll producer and consumer nodes at your convenience.
  Old credentials remain valid until their natural expiry.
```

**Properties:**
- Signed by old key → no cooldown, no email verification needed
- Old key remains valid for credential verification until the credential TTLs expire (grace period)
- Server stores both old and new pubkeys during a transition window (configurable, default 7 days)
- This is the recommended way to rotate keys periodically

### What Happens to the Mesh During Recovery

This is the critical operational question. The answer depends on credential TTLs:

```
Timeline after root key loss (2-hour credential TTL):

T+0h    Key lost. Everything still works (credentials valid).
T+1h    Still fine. Credentials haven't expired yet.
T+2h    Producer credentials start expiring.
        Nodes can't re-enroll. Mesh starts shrinking.
T+2-4h  Consumer credentials start expiring.
        Consumers can't re-enroll via self-issued creds.
        @aster-delegated consumers still work (different signing key).
T+24h   Most/all self-issued credentials expired.
        Only @aster-delegated consumers remain functional.
```

**With recovery code (Path 1):**
- Recovery is instant. New root key generated immediately.
- Operator re-enrolls producers within the TTL window.
- If they act within 2 hours, zero downtime.

**With email recovery (Path 2):**
- Recovery takes minutes (email delivery + code entry).
- But 72-hour cooldown blocks new publishes.
- Existing published services remain live during cooldown.
- Producer re-enrollment can happen immediately (cooldown only affects `@aster` publishing, not local mesh enrollment).

**Mitigation: credential TTL guidance**

| Scenario | Recommended TTL | Why |
|----------|----------------|-----|
| Solo developer | 24-48h | Gives time to notice and recover |
| Small team | 4-8h | Balance between security and operational margin |
| Production / regulated | 1-2h | Tight blast radius, requires operational readiness |

### The `aster republish` Command

After recovery, published services need to be re-associated with the new root key. The contract hash doesn't change (it's content-addressed), but the ownership proof does.

```bash
aster republish --all
# Re-signs all published service manifests with the new root key
# and updates @aster's ownership records
```

This is a convenience command that:
1. Lists all services published under the handle
2. Re-signs each manifest with the new root key
3. Updates `@aster` (after cooldown expires, if applicable)

### Recovery Codes: Server-Side Storage

```
recovery_codes/<handle>/
├── code_1_hash    # argon2(KFBR-7942-GMTN)
├── code_2_hash    # argon2(HNCX-5103-YRWZ)
├── ...
├── code_8_hash
├── created_at     # when codes were generated
└── codes_remaining # count of unused codes
```

The server never stores plaintext codes. On recovery attempt: client sends the code via Aster RPC (QUIC-encrypted). Server iterates stored hashes, runs `argon2_verify(stored_hash, user_input)` for each unconsumed code. Match → consume and proceed.

This is fine because:
- QUIC transport encrypts the code in transit
- The code is single-use — even if intercepted, it's already consumed
- Server stores only hashes — database breach doesn't reveal codes

### Threat Model (Comprehensive)

| Threat | Mitigation |
|--------|-----------|
| **Root key lost (accident)** | Recovery codes (Path 1) or email (Path 2). Guidance: act within credential TTL to avoid mesh disruption. |
| **Root key stolen** | Attacker can impersonate. Detect via: unexpected enrollments, `@aster` audit log, transparency log (future). Respond: rotate key via `aster key rotate` if you still have the old key, or recover via codes/email if attacker rotated first. Attacker's changes are subject to the same cooldown. |
| **Email compromise** | Attacker can attempt Path 2 recovery. Mitigated by: 72-hour cooldown (owner gets notification and can counter-recover with recovery code during cooldown), 1 attempt per 24h rate limit, and the cooldown gives the real owner time to react. |
| **Email + recovery code compromise** | Attacker has both factors. This is equivalent to full account compromise. Mitigation: recovery codes should be stored separately from email credentials (different password manager, offline storage). Detection: notification emails on recovery. Response: contact support for manual review. |
| **MITM on transport** | QUIC provides TLS 1.3 + mutual endpoint authentication. Ed25519 signatures on all non-recovery requests. Recovery requests are unsigned (by definition — key is lost) but protected by transport encryption + verification code + cooldown. |
| **Replay attack** | Timestamp (±5 min) + nonce (10 min sliding window) on signed requests. Recovery codes are single-use (replay = already consumed). |
| **Handle enumeration** | Rate limiting on all endpoints. No distinction between "taken" and "pending". Recovery endpoint doesn't confirm handle existence (always says "code sent" even for non-existent handles). |
| **Spam registrations** | Email uniqueness + rate limiting + verification TTL. |
| **Server compromise** | Server stores: pubkeys, email hashes, recovery code hashes (argon2), verification code hashes (argon2). No private keys, no plaintext secrets. Attacker can disrupt service (DoS) but can't impersonate users or recover accounts without the codes. |
| **Cooldown bypass attempt** | Attacker recovers via email, tries to publish immediately. Server enforces cooldown at the API level — not bypassable. Owner receives notification, can counter-recover with recovery code (no cooldown on code-based recovery). |

---

## 6. `@aster` Service (Aster RPC — Dogfooding)

`@aster` is the public Aster service we run — it's the service directory and handle registry. It's an Aster service itself, running on the Python server. Clients connect over QUIC (Iroh transport) and call methods via the standard Aster RPC protocol. Dogfooding from day one.

The CLI uses the same `AsterClient` that every other Aster consumer uses. `@aster` is just another service in the ecosystem. (Any producer mesh can run its own private service directory — `@aster` is the public default.)

### Connection

The CLI ships with a hardcoded node ID for the `@aster` service (overridable via `aster_service.node_id` in config). Connection uses standard consumer admission — but the registry service is **public** (no enrollment credential needed for read methods).

Write methods (join, verify, etc.) require a signed payload in the request body. The server's admission handler accepts unauthenticated consumers for public methods but verifies ed25519 signatures in the request payload for mutating calls.

### Service Definition

```python
@service(name="AsterService", version=1)
class AsterService:
    """The @aster handle registry and directory service."""

    # ── Day 1 methods (see §12 for scope) ─────────────────────

    @unary
    async def check_availability(self, handle: str) -> AvailabilityResult:
        """Check if a handle is available. Public, no auth."""
        ...

    @unary
    async def join(self, request: SignedRequest[JoinPayload]) -> JoinResult:
        """Reserve a handle. Signed by root key."""
        ...

    @unary
    async def verify(self, request: SignedRequest[VerifyPayload]) -> VerifyResult:
        """Confirm email with verification code. Signed by root key."""
        ...

    @unary
    async def resend_verification(self, request: SignedRequest[ResendPayload]) -> ResendResult:
        """Resend verification code. Signed by root key."""
        ...

    @unary
    async def handle_status(self, request: SignedRequest[StatusPayload]) -> HandleStatusResult:
        """Get current handle state. Signed by root key."""
        ...

    # ── Post-Day 1 methods ────────────────────────────────────

    @unary
    async def recover_with_code(self, request: RecoverCodeRequest) -> RecoverResult:
        """Recover handle using a recovery code. Unsigned (key is lost)."""
        ...

    @unary
    async def recover_with_email(self, request: RecoverEmailRequest) -> RecoverEmailResult:
        """Initiate email-based recovery. Unsigned."""
        ...

    @unary
    async def recover_confirm(self, request: RecoverConfirmRequest) -> RecoverResult:
        """Confirm email-based recovery with code. Unsigned."""
        ...

    @unary
    async def rotate_key(self, request: SignedRequest[RotateKeyPayload]) -> RotateResult:
        """Rotate root key. Signed by OLD key."""
        ...

    @unary
    async def regenerate_recovery_codes(self, request: SignedRequest[RegenCodesPayload]) -> RegenCodesResult:
        """Regenerate recovery codes. Signed by current key."""
        ...

    @unary
    async def recovery_codes_status(self, request: SignedRequest[CodesStatusPayload]) -> CodesStatusResult:
        """Check how many recovery codes remain. Signed."""
        ...
```

### Wire Types

```python
@wire_type
@dataclass
class SignedRequest(Generic[T]):
    """Wrapper for all authenticated requests."""
    payload: T
    pubkey: str         # hex-encoded ed25519 public key
    signature: str      # hex-encoded ed25519 signature over canonical JSON of payload

# ── Day 1 payloads ────────────────────────────────────────

@wire_type
@dataclass
class JoinPayload:
    action: str         # "join"
    handle: str
    email: str
    announcements: bool
    timestamp: int
    nonce: str

@wire_type
@dataclass
class VerifyPayload:
    action: str         # "verify"
    handle: str
    code: str
    timestamp: int
    nonce: str

@wire_type
@dataclass
class ResendPayload:
    action: str         # "resend_verification"
    handle: str
    timestamp: int
    nonce: str

@wire_type
@dataclass
class StatusPayload:
    action: str         # "handle_status"
    timestamp: int
    nonce: str

# ── Day 1 results ─────────────────────────────────────────

@wire_type
@dataclass
class AvailabilityResult:
    available: bool
    reason: str | None  # "taken", "reserved", "invalid" — only when unavailable

@wire_type
@dataclass
class JoinResult:
    handle: str
    status: str                     # "pending_verification"
    verification_expires_at: str    # ISO 8601
    code_expires_at: str            # ISO 8601

@wire_type
@dataclass
class VerifyResult:
    handle: str
    status: str     # "verified"
    pubkey: str

@wire_type
@dataclass
class ResendResult:
    code_expires_at: str
    resends_remaining: int

@wire_type
@dataclass
class HandleStatusResult:
    handle: str
    status: str             # "pending" | "verified"
    email_masked: str       # "em***@example.com"
    registered_at: str
    services_published: int
```

### Signature Verification (Server-Side Interceptor)

The `AsterService` service uses an Aster interceptor to verify signed requests:

```python
@interceptor
class SignatureVerificationInterceptor:
    """Verifies ed25519 signatures on SignedRequest payloads."""

    async def intercept(self, ctx: CallContext, request, next):
        if isinstance(request, SignedRequest):
            if not verify_signature(request.payload, request.pubkey, request.signature):
                raise AsterError("invalid_signature")
            if not check_timestamp(request.payload.timestamp, tolerance=300):
                raise AsterError("stale_request")
            if not check_nonce(request.payload.nonce):
                raise AsterError("replayed_nonce")
        return await next(ctx, request)
```

This is a standard Aster interceptor — nothing registry-specific about the pattern. Any Aster service could use the same signed-request approach.

### Rate Limiting (Server-Side Interceptor)

```python
@interceptor
class RateLimitInterceptor:
    """Per-method rate limiting based on pubkey and peer EndpointId."""
    ...
```

Rate limits per method:

| Method | Limit |
|--------|-------|
| `check_availability` | 30/min per peer |
| `join` | 3/hour per peer, 3/hour per email |
| `verify` | 5 attempts per code, then code invalidated |
| `resend_verification` | 5/day per handle |
| `handle_status` | 10/min per peer |

### Why Aster RPC, Not REST

- **Dogfooding.** `@aster` is the first public Aster service anyone encounters. If it runs on Aster, it proves the framework works.
- **NAT traversal for free.** Developers behind corporate NATs reach `@aster` via Iroh relay.
- **Signed requests are natural.** The Aster trust model already handles ed25519 credentials. Signed payloads fit the existing patterns.
- **Contract-first.** The `@aster` contract is published, inspectable, and versioned like any other Aster service. `aster contract inspect AsterService` just works.
- **One transport.** The CLI only needs an Aster client. No HTTP, no proxy configuration. (DNS is only used for bootstrap node discovery, not for the RPC transport.)

### Bootstrap: DNS-Based Service Discovery

How does a brand-new CLI find the `@aster` registry node?

**The CLI hardcodes one thing:** the `@aster` root public key. Everything else — node ID, relay URL — is resolved at runtime via a signed DNS TXT record.

#### DNS Record

```
_aster-registry.aster.site  TXT  "v=aster1 node=<hex_node_id> relay=<relay_url> ts=<unix_epoch> sig=<hex_signature>"
```

| Field | Purpose |
|-------|---------|
| `v=aster1` | Version tag — allows future format changes |
| `node=<hex>` | Iroh node ID (ed25519 public key) of the registry node |
| `relay=<url>` | Relay URL for NAT traversal (e.g., `https://relay1.iroh.network`) |
| `ts=<epoch>` | Unix timestamp when the record was signed — prevents replay of stale records |
| `sig=<hex>` | Ed25519 signature over the preceding fields, signed by `@aster`'s root key |

#### Signature Verification

The CLI:
1. Queries `_aster-registry.aster.site` TXT record via standard DNS
2. Parses the fields
3. Reconstructs the signed payload: `v=aster1 node=<hex> relay=<url> ts=<epoch>` (everything before `sig=`)
4. Verifies the signature against the hardcoded `@aster` root public key
5. Checks `ts` is within an acceptable window (e.g., record not older than 7 days)
6. Connects to the resolved node ID via Iroh transport

If DNS is unreachable (corporate firewall, truly air-gapped network), the CLI falls back to a cached node ID from a previous successful resolution, or a last-resort hardcoded node ID. In `--air-gapped` mode, DNS resolution is skipped entirely and only the cache/hardcoded fallback is used.

#### Why This Works

- **DNS is untrusted transport.** Cache poisoning, BGP hijack, compromised registrar — none of it matters because the record is signed by `@aster`'s root key. An attacker who swaps the TXT record can't forge the signature.
- **Node rotation without CLI updates.** We can migrate the registry to a new node by updating the DNS record and re-signing. Every CLI resolves the new node ID automatically.
- **Root key is the only hardcoded thing.** And it almost never changes. If it does, that's a CLI update — but that's a once-in-years event, not an operational concern.
- **Standard DNS infrastructure.** No custom discovery protocol. Works with every DNS resolver, cacheable by CDN/ISP resolvers, debuggable with `dig`.
- **Timestamp prevents replay.** An attacker who captured an old signed record can't replay it beyond the 7-day window.

#### Record Signing (Ops-Side)

When rotating the registry node or relay:

```bash
# Operator signs the new record with @aster's root key
aster registry sign-dns \
  --node <new_node_id> \
  --relay https://relay1.iroh.network \
  --root-key /path/to/aster-root.key

# Output: TXT record value to publish
# v=aster1 node=7f3a2bc9... relay=https://relay1.iroh.network ts=1744123800 sig=ab12cd34...
```

Then update the DNS TXT record at `_aster-registry.aster.site` with the new value. Standard DNS tooling (Cloudflare API, Route 53, etc.).

#### Client Implementation

```python
# In cli/aster_cli/registry.py
import dns.resolver  # dnspython

ASTER_ROOT_PUBKEY = "..."            # hardcoded — the ONE thing that never changes
ASTER_DNS_NAME = "_aster-registry.aster.site"
ASTER_FALLBACK_NODE_ID = "..."       # last-resort hardcoded, updated with CLI releases
RECORD_MAX_AGE = 7 * 86400          # reject records older than 7 days

async def resolve_registry() -> tuple[str, str]:
    """Resolve @aster registry node ID and relay from signed DNS record."""
    config = load_config()

    # Config override — for self-hosted registries or testing
    override_node = config.get("aster_service", {}).get("node_id")
    override_relay = config.get("aster_service", {}).get("relay")
    if override_node:
        return override_node, override_relay or ""

    try:
        answers = dns.resolver.resolve(ASTER_DNS_NAME, "TXT")
        record = _parse_aster_txt(answers[0].to_text())

        # Verify signature against hardcoded root pubkey
        signed_payload = f"v={record.version} node={record.node} relay={record.relay} ts={record.ts}"
        if not ed25519_verify(ASTER_ROOT_PUBKEY, signed_payload.encode(), record.sig):
            raise RegistryResolutionError("DNS record signature invalid")

        # Check freshness
        if time.time() - record.ts > RECORD_MAX_AGE:
            raise RegistryResolutionError("DNS record too old")

        # Cache for offline use
        _cache_resolution(record.node, record.relay)
        return record.node, record.relay

    except Exception:
        # Fallback: cached from last successful resolution
        cached = _load_cached_resolution()
        if cached:
            return cached
        # Last resort: hardcoded
        return ASTER_FALLBACK_NODE_ID, ""

async def get_registry_client() -> AsterClient:
    """Connect to the @aster registry service."""
    node_id, relay = await resolve_registry()
    return await AsterClient.connect(node_id=node_id, relay=relay)
```

#### DNSSEC (Nice-to-Have, Not Required)

DNSSEC would provide an additional layer of DNS-level authenticity. But we don't depend on it — our ed25519 signature is the trust anchor. DNSSEC adoption is patchy (~30% of domains), and we'd rather have a design that works universally. If `aster.site` has DNSSEC enabled, it's defense-in-depth.

---

## 7. VFS Structure (`/aster`)

### Directory Layout

```
/ (ROOT)
├── services/              # local peer services (existing — unchanged)
├── blobs/                 # local blobs (existing — unchanged)
└── aster/                 # NEW: @aster directory
    ├── emrul/             # your handle (or pubkey prefix if unregistered)
    │   ├── TaskManager/   # published service
    │   │   ├── submitTask
    │   │   ├── watchProgress
    │   │   └── cancelTask
    │   └── InvoiceService/  # unpublished (local only)
    │       ├── createInvoice
    │       └── listInvoices
    ├── acme-corp/         # another registered handle (discovered)
    │   └── PaymentGateway/
    └── alice-dev/         # another handle
        └── DocumentSummarizer/
```

### How Services Appear Under Your Handle

Your handle's service list is a **merge** of:
1. **Published services** — fetched from `@aster` (online) or cache (offline)
2. **Local services** — detected from the local project's service definitions (via manifest or decorator scan)

Display:
```
@emrul:/aster/emrul$ ls
  TaskManager      v3  3 methods  ● published    abc123de
  InvoiceService   v1  2 methods  ⬡ local        (not published)
```

- `● published` — green dot, live on `@aster`
- `⬡ local` — hollow diamond, exists locally only
- The short hash (`abc123de`) is the contract identity hash for published services

### How Other Handles Appear

Other handles are discovered via:
- **Browsing**: `ls /aster` shows handles the user has interacted with (cached)
- **Search**: `discover <query>` searches `@aster`
- **Direct navigation**: `cd /aster/acme-corp` fetches that handle's public services

Other handles only show published public services. Private services are invisible unless the current user has been granted access.

### Lazy Loading & Caching

| Data | Fetch trigger | Cache TTL | Offline behavior |
|------|--------------|-----------|-----------------|
| Own handle status | Shell startup | 1 hour | Use cached state |
| Own published services | `cd /aster/<handle>` | 5 minutes | Show cached |
| Own local services | `cd /aster/<handle>` | Immediate (filesystem) | Always available |
| Other handle's services | `cd /aster/<other>` | 15 minutes | Show cached or empty |
| Handle search results | `discover` command | Not cached | Unavailable offline |

---

## 8. Shell Startup Sequence

```
1. Load ~/.aster/config.toml
2. Check aster_service.enabled (or --air-gapped flag)
   ├── false → State F (local only, skip steps 3-5)
   └── true → continue
3. Check profile has root_pubkey
   ├── no → State A (no identity)
   └── yes → continue
4. Check cached handle_status
   ├── "unregistered" → State B (show join banner)
   ├── "pending" → check if TTL expired
   │   ├── expired → revert to State B, clear cached handle
   │   └── valid → State C (show verify banner)
   └── "verified" → continue
5. Ping `@aster` (non-blocking, 2s timeout)
   ├── reachable → State D (online)
   └── unreachable → State E (offline, use cache)
6. Display welcome banner (appropriate state)
7. Set initial CWD to /aster/<handle>
8. Enter REPL
```

The `@aster` ping (step 5) is **non-blocking** — the shell doesn't wait for it. If the response arrives after the welcome banner is displayed, the online/offline status updates silently (no visible change unless the user runs `status`).

---

## 9. New CLI Commands

### `aster join`

Top-level command (not under `aster trust` or `aster shell`). Also available as `join` inside the shell.

```
aster join [--handle HANDLE] [--email EMAIL] [--no-announcements]
```

Registration: add `register_join_subparser(subparsers)` in a new `cli/aster_cli/join.py`.

### `aster verify`

```
aster verify <code>
aster verify --resend
```

Registration: add `register_verify_subparser(subparsers)` in `cli/aster_cli/join.py` (same module).

### `aster status` / `aster whoami`

Shows current identity state. `whoami` is an alias for `status` — muscle memory for developers.

```
$ aster whoami

  Handle:    @emrul
  Status:    verified
  Root key:  7f3a2bc9de01... (profile: default)
  @aster:    online
  Services:  1 published, 1 local
```

Registration: add `register_status_subparser(subparsers)` with `whoami` alias.

### `aster recover`

Interactive recovery wizard:

```
aster recover --handle HANDLE
```

Prompts for recovery method (code or email). Generates new root key automatically. Registration: add `register_recover_subparser(subparsers)` in `cli/aster_cli/join.py`.

### `aster key rotate`

Proactive key rotation (requires current root key):

```
aster key rotate
```

Signs rotation request with old key, generates new key, no cooldown. Registration: add `register_key_subparser(subparsers)` in `cli/aster_cli/join.py`.

### `aster recovery-codes`

Manage recovery codes:

```
aster recovery-codes regenerate     # invalidate old codes, generate 8 new ones
aster recovery-codes status         # show how many codes remain
```

Registration: add `register_recovery_codes_subparser(subparsers)` in `cli/aster_cli/join.py`.

### Shell-Only Commands

Inside the shell REPL:

| Command | Action |
|---------|--------|
| `join` | Same as `aster join` |
| `verify <code>` | Same as `aster verify` |
| `status` / `whoami` | Same as `aster status` |
| `discover <query>` | Search `@aster` for services |
| `refresh` | Re-fetch cached data from `@aster` |

---

## 10. `aster publish` (Preview)

This is the next piece to flesh out. Brief sketch of the flow:

```bash
# From within a project that defines an Aster service
aster publish TaskManager

# What happens:
# 1. Scans local service definition (decorators / manifest)
# 2. Computes contract identity hash
# 3. Signs publish request with root key
# 4. Posts contract manifest + endpoint info to @aster
# 5. Service appears as ● published under /aster/<handle>/
```

Key questions for the publish design doc:
- What exactly is uploaded? (manifest only, or also contract hash, types, docs?)
- Endpoint registration — automatic heartbeat or manual?
- Version management — what happens when the contract changes?
- Unpublish flow
- Visibility (public/private) default

Deferred to a separate design doc.

---

## 11. Implementation Plan

### 🎯 DAY 1 — Ship This Week

Everything below is the minimum to get the join flow working end-to-end. A user can install the CLI, open the shell, claim a handle, and see their local services under it.

#### D1-A: Config & Local Identity (no network, client-side)

- [ ] Extend `config.toml` schema: `handle`, `handle_status`, `email` on profile; `[aster_service]` section with `enabled`, `node_id`, `relay`
- [ ] Config read/write helpers in `profile.py` for new fields
- [ ] `--air-gapped` CLI flag on `aster shell` (disables all `@aster` comms, relays still work)
- [ ] Shell: display welcome banner appropriate to state (A/B only for Day 1)
- [ ] `aster status` / `aster whoami` command (local-only version — shows key, handle state, profile)

#### D1-B: Handle Validation (client-side, no network)

- [ ] Handle validation module: 3-39 chars, lowercase alphanumeric + hyphens, no consecutive hyphens, start/end with letter or digit
- [ ] Reserved word list (hardcoded — appendix A)
- [ ] Validation used by both `aster join` client-side and server-side

#### D1-C: Join Flow — Client Side

- [ ] `cli/aster_cli/join.py` — `aster join` command (interactive prompts)
- [ ] `aster join` auto-creates root key if missing (subsumes `aster init` for identity creation)
- [ ] `aster verify <code>` command
- [ ] `aster verify --resend` command
- [ ] Signed request builder (reuse canonical JSON from `trust/signing.py`)
- [ ] Shell commands: `join`, `verify`, `whoami` wired into REPL
- [ ] State transitions: A → B (key created) → C (join sent) → D (verified) with config persistence
- [ ] Banner updates on state change
- [ ] Error handling: service down, handle taken (re-prompt), email taken (privacy-safe message), wrong code (show remaining attempts), Ctrl+C (graceful save to State C)

#### D1-D: Aster Service — Server Side (Python, Aster RPC)

- [ ] `AsterService` service definition with `@service` / `@unary` decorators
- [ ] Wire types: `SignedRequest[T]`, `JoinPayload`, `VerifyPayload`, `ResendPayload`, `StatusPayload` + result types
- [ ] `check_availability` method — validates handle, checks against DB
- [ ] `join` method — reserves handle, sends verification email
- [ ] `verify` method — checks code, marks handle verified
- [ ] `resend_verification` method — resends code with cooldown
- [ ] `handle_status` method — returns current state for a pubkey
- [ ] `SignatureVerificationInterceptor` — ed25519 verify + timestamp + nonce dedup
- [ ] `RateLimitInterceptor` — per-method limits (in-memory for v0.1)
- [ ] Email delivery — verification codes via SMTP (use a service like Resend or SES)
- [ ] Storage backend — SQLite for v0.1 (handles, verification codes as argon2)
- [ ] Reserved word enforcement (server-side, same list as client)
- [ ] Handle TTL: background task expires unverified handles after 24h

#### D1-E: Client ↔ Server Integration

- [ ] `cli/aster_cli/registry.py` — `resolve_registry()` via signed DNS TXT + `get_registry_client()`
- [ ] DNS resolution: query `_aster-registry.aster.site`, verify signature against hardcoded root pubkey
- [ ] Fallback chain: DNS → cached resolution → hardcoded node ID
- [ ] `aster registry sign-dns` ops command for record signing
- [ ] Dependency: `dnspython` for TXT record resolution
- [ ] `aster join` calls `check_availability` then `join` on the registry service
- [ ] `aster verify` calls `verify` on the registry service
- [ ] Shell startup calls `handle_status` (non-blocking, 2s timeout) to determine state
- [ ] Online/offline indicator in shell banner

#### D1-F: VFS & Discovery

- [ ] VFS: `/aster/<handle>` node with merged local + published services
- [ ] `discover` command (search registry for services by name/tag)
- [ ] Other handles browsable in VFS via `cd /aster/<handle>`
- [ ] Cache layer: handle status, published services, TTL-based
- [ ] `refresh` command to re-fetch cached data
- [ ] `● published` / `⬡ local` visual distinction in `ls` output

#### D1-G: Publish

- [ ] `aster publish <ServiceName>` CLI command
- [ ] Scans local service definition → computes contract identity hash
- [ ] Signs publish request with root key
- [ ] `publish` method on `AsterService` service (server-side)
- [ ] Contract manifest upload (methods, types, version, hash)
- [ ] Endpoint registration (node ID + TTL)
- [ ] Published services appear under `/aster/<handle>/` with `● published` indicator
- [ ] `aster unpublish <ServiceName>` CLI command
- [ ] First-publish triggers recovery code generation (server generates, client displays once)
- [ ] Recovery code storage (argon2 hashed, server-side)

#### D1 Total: ~25 items across client + server. One Aster service with ~8 RPC methods, two interceptors, SQLite storage, email delivery, VFS integration, publish flow.

---

### LATER — Post-Day 1

These are designed and documented above but **not built this week**. Ordered by priority.

#### L1: Recovery & Key Rotation

- [ ] `recover_with_code` method on `AsterService`
- [ ] `recover_with_email` + `recover_confirm` methods
- [ ] `rotate_key` method (signed by old key)
- [ ] Recovery code management methods: `regenerate_recovery_codes`, `recovery_codes_status`
- [ ] Publish cooldown enforcement (24h code, 72h email)
- [ ] Key rotation transition window (7 days, both keys accepted)
- [ ] Notification emails (key changed, recovery initiated)
- [ ] `aster recover` CLI command (interactive wizard)
- [ ] `aster key rotate` CLI command
- [ ] `aster recovery-codes regenerate` / `status` CLI commands

#### L2: Publish Enhancements

- [ ] Endpoint heartbeat (automatic periodic re-registration)
- [ ] Version management (what happens when contract changes)
- [ ] Visibility toggle (public/private)
- [ ] `aster republish --all` (re-sign after key rotation)

#### L3: Hardening

- [ ] Rate limiting backed by persistent store (Redis or similar)
- [ ] Nonce dedup with sliding window (in-memory is fine for single-node v0.1)
- [ ] Abuse detection / handle squatting review
- [ ] Monitoring & alerting for the registry service

---

## Appendix A: Reserved Handles

Handles that cannot be claimed by users. Sourced from GitHub's reserved list + Aster-specific terms.

### Aster-Specific

```
aster, admin, administrator, system, registry, service, services,
api, support, help, abuse, security, noreply, no-reply, postmaster,
webmaster, operator, root, daemon, staff, owner, official, platform,
status, trust, publish, discover, enroll, verify, recover, config,
shell, proxy, gateway, relay, control, monitor, audit, billing
```

### Common Reserved (from GitHub)

```
about, access, account, accounts, blog, cache, changelog, cloud,
codespace, contact, contribute, dashboard, developer, developers,
docs, documentation, download, downloads, enterprise, explore,
features, feedback, forum, gist, graphql, guide, guides, home,
hosting, import, integrations, issues, jobs, learn, legal, login,
logout, marketplace, members, mention, mentions, messages, mobile,
new, none, notifications, null, oauth, offer, offers, opensource,
organization, organizations, orgs, pages, password, payment,
payments, plan, plans, popular, pricing, privacy, profile, project,
projects, readme, releases, render, replies, report, reports,
search, settings, setup, shop, signin, signout, signup, site,
sitemap, sponsors, store, stories, suggestions, team, teams,
terms, topics, training, trending, undefined, user, users, welcome,
wiki, www
```

### Pattern Blocks

- Any handle starting with `aster-` (e.g., `aster-bot`, `aster-test`)
- Any handle starting with `admin-`
- Any handle that is purely numeric (e.g., `12345`)

---

## Appendix B: Email Templates

### Verification Email

**Subject:** Your Aster verification code: 847291

**Body:**
```
Your verification code is: 847291

Enter it in your terminal:

    aster verify 847291

This code expires in 15 minutes.

If you didn't request this, ignore this email.
The handle will be released automatically in 24 hours.

— Aster
```

Plain text only. No HTML. No images. No tracking pixels. Developers notice and appreciate this.

### Security Notification (Future)

Sent when a handle's root key is changed via recovery:

**Subject:** [Aster] Root key changed for @emrul

**Body:**
```
The root key for your Aster handle @emrul was changed
via account recovery on 2026-04-08 at 14:35 UTC.

New key: 9a8b7c6d5e4f... (first 12 chars)

If this wasn't you, contact security@aster.site immediately.

A 72-hour cooldown is in effect — no services can be
published from this handle until 2026-04-11 14:35 UTC.

— Aster
```

---

## Appendix C: Wire Format for Signed Requests

### Client-Side Signing (Python)

```python
import json
import time
import os
from aster.trust.signing import LocalSigner

def sign_request(signer: LocalSigner, payload: dict) -> dict:
    """Sign a request payload for @aster."""
    # Add timestamp and nonce
    payload["timestamp"] = int(time.time())
    payload["nonce"] = os.urandom(16).hex()

    # Canonical JSON (sorted keys, no whitespace)
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":"))

    # Sign
    signature = signer.sign(canonical.encode("utf-8"))

    return {
        "payload": payload,
        "pubkey": signer.public_key_hex(),
        "signature": signature.hex(),
    }
```

### Server-Side Verification (Pseudocode)

```python
def verify_signed_request(request: dict) -> tuple[bool, str]:
    pubkey = bytes.fromhex(request["pubkey"])
    signature = bytes.fromhex(request["signature"])
    payload = request["payload"]

    # 1. Verify signature
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":"))
    if not ed25519_verify(pubkey, canonical.encode(), signature):
        return False, "invalid_signature"

    # 2. Check timestamp (±5 minutes)
    now = int(time.time())
    if abs(now - payload["timestamp"]) > 300:
        return False, "stale_request"

    # 3. Check nonce uniqueness
    if nonce_seen(payload["nonce"]):
        return False, "replayed_nonce"
    record_nonce(payload["nonce"], ttl=600)

    return True, "ok"
```

---

## Open Questions

1. **Handle transfer.** Can a verified handle be transferred to another pubkey without recovery? (Probably yes, with email confirmation + old key signature.) Deferred.

2. **Multiple handles per key.** Can one root key own multiple handles? (Probably yes for orgs. Deferred to org/team design.)

3. **Handle deletion.** Can a user delete their handle and release it? (Yes, with email confirmation + old key signature. 30-day grace period before release.)

4. **Custom email domains.** For orgs: verify domain ownership to allow `@company.com` handles. Deferred.

5. **Webhook on verification.** Should `@aster` notify anything when a handle is verified? (Probably not for v0.1.)

6. **Recovery code physical format.** Should `aster join` offer to save codes to a file (`recovery-codes.txt`) in addition to displaying them? Risk: user saves to the same machine and loses both key and codes together. Probably display-only with a "copy to clipboard" option.

7. **Counter-recovery race.** If attacker starts email recovery and real owner has a recovery code, the owner can counter-recover immediately (code-based recovery has no cooldown and invalidates the pending email recovery). But what if the attacker has a recovery code and the owner only has email? The attacker wins (code > email in the priority order). This is the correct security trade-off — recovery codes are the stronger factor.

8. **Key escrow service.** Should `@aster` offer optional encrypted key backup? (e.g., encrypt root key with a passphrase, store ciphertext on `@aster`, retrieve with passphrase + email verification.) High value for solo developers. Risk: passphrase reuse, false sense of security. Deferred — recovery codes cover the common case.

9. **Recovery audit trail.** Should recovery events be visible in the `@aster` transparency log (future)? Yes — handle rebinding is a significant trust event that consumers of published services should be able to detect.
