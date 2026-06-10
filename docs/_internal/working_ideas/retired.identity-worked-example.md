# Identity Worked Example — Cloud-Marketplace Fleet Enrollment

> **RETIRED 2026-06-10.** Superseded by
> [../trust-directory.md](../trust-directory.md). The durable parts were
> harvested before retirement: the pluggable `EnrollmentProof` platform
> analysis lives in [../workload_identity.md](../workload_identity.md), and
> the enrollment flow's *output* changed from "mint an rcan" to "a designated
> enroller writes the Admission row into the directory" — the credential
> issuance, refresh, and tenant-rcan lifecycle described below is the
> machinery the directory model deletes. The cloud-IID / tenant-binding walk
> remains useful background reading; do not implement from it.

**Status:** Retired working idea (was: concrete use-case grounding for the trust spec)
**Date:** 2026-05-05 (retired 2026-06-10)
**Companion docs:**
- [ownership-attestations.md](../ownership-attestations.md) — the trust spine: reciprocal Ed25519 attestations, anchor → deployment root → node delegation chains, locked-in wire format (Fory + opaque body + domain separator). Supersedes the retired `trust-anchor-inversion.md`.
- [retired.trust-discovery-thinking-session.md](retired.trust-discovery-thinking-session.md) — handle registry, registration server (also retired)
- [aster-auth-webauthn-custodian.md](aster-auth-webauthn-custodian.md) — user-side identity (separate axis from operator/node identity)

---

## Why this document

The trust-spec docs design the cryptographic primitives in the abstract. This one walks a concrete, commercially representative deployment end-to-end and shows where each primitive lands, where Aster's responsibility ends, and where the downstream product picks up. It is also the document that exposes the "management layer" question for what it actually is — most of it is product-shaped tenancy logic on top of Aster's identity spine, not more crypto.

The example: **Portal**, a workspace-fabric product (`/Users/emrul/dev/emrul/portal/docs`) sold as a VM image on AWS Marketplace. Portal customers launch VMs in their own AWS accounts; those VMs need to enroll with Portal's control plane and be discoverable to the customer's CLI — without the customer ever touching ed25519 hex.

---

## The actors

| Actor | Identifier(s) | Who runs it |
|---|---|---|
| **Portal vendor** (Emrul) | offline anchor key + CI-issued deployment root | Vendor |
| **Portal control plane** | endpoint key signed by deployment root; `@portal-cp` handle in the @aster registry; serves the `PortalEnrollment` and `PortalControl` contracts | Vendor |
| **Customer** | AWS account ID (ground truth from AWS), Portal tenant ID (issued by vendor at subscription time) | Customer |
| **Portal VM (node)** | fresh ed25519 keypair generated at first boot | Customer's AWS account |
| **Portal CLI** | rcan minted at customer login, scoped to tenant ID | Customer's laptop |

The customer never holds an Aster key. They hold an AWS subscription and a Portal account. The trust artifacts under the hood are scoped to a tenant ID Portal owns.

---

## Why this case is interesting

The classic fleet-enrollment puzzle: a freshly booted VM has nothing but a random ed25519 keypair. How does Portal's control plane know which VM belongs to which customer, without:

- requiring the customer to copy-paste tokens at launch time,
- trusting any single VM's self-claim,
- baking customer secrets into the AMI (which is shared across all customers).

The answer is that **the cloud platform already knows.** AWS Marketplace knows which AWS account subscribed. AWS Instance Identity Documents prove which AWS account a running VM lives in. The two halves connect at enrollment time. Aster's job is to carry the proof and issue the trust artifact; AWS's job is to be ground truth for "which customer is this?"; Portal's job is to glue the two together.

---

## The primitive AWS already provides

Every EC2 instance can fetch:

```
GET http://169.254.169.254/latest/dynamic/instance-identity/document
GET http://169.254.169.254/latest/dynamic/instance-identity/signature
```

The document contains:

```json
{
  "accountId":      "123456789012",
  "instanceId":     "i-0abc123def456",
  "imageId":        "ami-0portalAMI",
  "region":         "us-east-1",
  "pendingTime":    "2026-05-05T12:00:00Z",
  ...
}
```

The signature is RSA-2048 over the document, signed by an AWS regional public key whose value is published and stable. Any process can verify it offline.

This is the customer-binding primitive. It is signed by AWS, not by the customer, not by the VM. A compromised VM cannot forge a different `accountId` — the IID would no longer verify under AWS's key.

Equivalent primitives exist on every modern compute platform:

| Platform | Equivalent |
|---|---|
| AWS EC2 | Instance Identity Document |
| GCP | Instance metadata identity token (signed JWT) |
| Azure | Instance Metadata Service attested document |
| Kubernetes | Projected service-account token (signed by the cluster) |
| GitHub Actions | OIDC token (`ACTIONS_ID_TOKEN_REQUEST_TOKEN`) |
| Fly.io / Render / Railway | Platform-issued workload tokens |

Same shape: the platform issues a signed assertion of "this workload is X for customer Y." Different verifiers, same enrollment-input pattern.

---

## Subscription-time setup (one-time per customer)

```
Customer                          AWS Marketplace                    Portal control plane
   │                                    │                                    │
   │ subscribe to Portal                │                                    │
   ├───────────────────────────────────▶│                                    │
   │                                    │                                    │
   │                                    │ subscription webhook               │
   │                                    │ { customerAwsAccountId: "1234…",   │
   │                                    │   customerIdentifier: "abc…",      │
   │                                    │   productCode: "portal-pro" }      │
   │                                    ├───────────────────────────────────▶│
   │                                    │                                    │
   │                                    │                                    │ create Tenant {
   │                                    │                                    │   tenant_id,
   │                                    │                                    │   aws_account_id: "1234…",
   │                                    │                                    │   plan: "pro" }
   │                                    │                                    │
   │ welcome email (CLI bootstrap link) │                                    │
   │◀───────────────────────────────────────────────────────────────────────│
```

The Portal control plane is now ready to admit VMs from AWS account `1234…` as tenant `T` and to authenticate `T`'s CLI sessions.

This step is pure product logic. No Aster primitives involved.

---

## VM launch — auto-enrollment

The AMI is **identical for every customer**. It contains:

- Portal binaries
- Portal's bundled config:
  - Portal control-plane handle (`@portal-cp`)
  - `PortalEnrollment` contract id
  - Portal vendor's anchor pubkey (so the VM can verify the control plane's identity, not the other way around)

Nothing customer-specific. The AMI is reproducible and signable.

```
Customer's AWS account                                   Portal control plane
   │                                                              │
   │ launch VM from Portal AMI                                    │
   ├──────────────────────────────────────┐                       │
   │                                      │                       │
   │  ┌──────────────── VM ─────────────┐ │                       │
   │  │ 1. boot                          │ │                       │
   │  │ 2. generate node_pk (ed25519)    │ │                       │
   │  │ 3. fetch AWS IID + signature     │ │                       │
   │  │    from 169.254.169.254          │ │                       │
   │  │ 4. resolve @portal-cp via Aster  │─┼──────────────────────▶│
   │  │    handle registry               │ │                       │
   │  │                                  │ │  resolve              │
   │  │                                  │ │◀──────────────────────│
   │  │                                  │ │  endpoint id          │
   │  │ 5. open Aster connection to      │ │                       │
   │  │    control plane                 │─┼──────────────────────▶│
   │  │                                  │ │                       │
   │  │ 6. RPC PortalEnrollment.register │ │                       │
   │  │    { node_pk,                    │ │                       │
   │  │      iid, iid_signature,         │ │                       │
   │  │      requested_contracts: [..] } │─┼──────────────────────▶│
   │  │                                  │ │                       │
   │  └──────────────────────────────────┘ │                       │
   │                                                                │
   │                                                                │ verify AWS sig over IID
   │                                                                │ extract account_id
   │                                                                │ lookup Tenant by account_id
   │                                                                │ check subscription is active
   │                                                                │ mint admission token:
   │                                                                │   sign(deployment_root_priv,
   │                                                                │        node_pk
   │                                                                │     || tenant_id
   │                                                                │     || contracts
   │                                                                │     || not_before, not_after)
   │                                                                │ store Node {
   │                                                                │   node_pk, tenant_id,
   │                                                                │   instance_id, region }
   │                                                                │
   │  ┌──────────────────────────────────┐ │                       │
   │  │ 7. receive admission token       │ │                       │
   │  │◀─────────────────────────────────┼─┼──────────────────────│
   │  │ 8. start serving PortalNode      │ │                       │
   │  │    contracts                     │ │                       │
   │  └──────────────────────────────────┘ │                       │
```

Time to enrollment: ~1 second after first boot. No customer interaction.

The admission token is exactly the artifact specified in `Aster-trust-spec.md` — same anchor → deployment-root → admission-token chain. The novelty is not the artifact; it is **what the control plane required as proof input before issuing it**: the AWS IID instead of an out-of-band human approval.

---

## Control-plane HA — how `@portal-cp` actually resolves

Step 4 of the auto-enrollment flow ("resolve `@portal-cp` via Aster handle registry") and the equivalent step in the CLI flow both depend on this section. It is also where the HA design lives.

### Two-layer resolution

The single phrase "find the control plane" hides two distinct lookups:

| Layer | Question | Answer | Operated by |
|---|---|---|---|
| **(a) Handle → anchor pubkey** | "What is `@portal-cp`?" | Stable cryptographic identity (Portal's anchor key) | @aster's handle registry |
| **(b) Anchor → reachable endpoints** | "Where can I dial it?" | One or more iroh endpoint IDs + addressing hints | Portal itself |

Layer (a) is roughly DNS-shaped and changes maybe once a decade (when an anchor rotates). Layer (b) is the operationally-active part — endpoints come and go as control-plane VMs are added, replaced, scaled, or fail.

### Don't share Aster identities across processes

Tempting first instinct: run three control-plane VMs with the same iroh endpoint keypair behind a load balancer. **Don't.** An iroh endpoint key is the cryptographic identity *for the QUIC session* — encryption state, stream IDs, congestion control, connection migration are all per-session per-key. Two processes with the same private key produce split-brain sessions: the relay forwards to whichever registered last, mid-session migration breaks, iroh-docs author keys collide, and a client can't tell which "real" node it spoke to. It is fightable in theory and fighting the tide in practice. Every healthy P2P system (Tailscale, Bitcoin, ZeroTier moons, libp2p) gives each node its own identity and does HA at the directory layer instead.

### The pattern: signed node list, multiple distribution channels

1. Run N control-plane VMs, each with its own iroh endpoint keypair.
2. Each VM is admitted by Portal's deployment root (the existing admission-token primitive). The control-plane VMs are stateless from Aster's POV — they share whatever durable store the product uses (RDS, Postgres, etc.).
3. Portal's anchor (or its deployment root) signs a **node list**:

   ```json
   {
     "handle": "@portal-cp",
     "anchor_pubkey": "ed25519:...",
     "contracts": ["PortalEnrollment", "PortalControl"],
     "nodes": [
       { "endpoint_id": "abc...", "region": "us-east-1", "addrs": [...] },
       { "endpoint_id": "def...", "region": "us-west-2", "addrs": [...] },
       { "endpoint_id": "ghi...", "region": "eu-west-1", "addrs": [...] }
     ],
     "not_before": "...",
     "not_after":  "...",
     "sig": "..."
   }
   ```

4. Publish the signed blob to **multiple distribution channels** — same bytes, different transports. Distribution can be compromised; the signature can't.

   | Channel | Where | When it shines |
   |---|---|---|
   | DNS TXT under operator zone | `_nodes.portal-cp.id.aster0.net` (or `_nodes.portal.io`) | Fast (10–50 ms cold), ubiquitous, works through DoH for browsers |
   | `/.well-known/aster-nodes.json` | `https://portal.io/.well-known/aster-nodes.json` | Trivial to host, CDN-cacheable, debuggable with curl, survives DNS outage |
   | Pkarr under anchor pubkey | BitTorrent Mainline DHT | Works when DNS + HTTPS both down; self-hostable; cryptographically rooted |

5. Client behavior:
   - Try channels in order (DNS → well-known → Pkarr) with short timeouts; cache the first success.
   - Verify signature against Portal's anchor pubkey (resolved from @aster, or pinned by the client).
   - From the verified node list, dial endpoints in order (or by latency / region affinity); fall back on connection failure.
   - iroh already has the multi-endpoint dial shape — this is just a longer list.

### HA budget per channel

| Channel | Typical SLA | Failure mode |
|---|---|---|
| Route53 / Cloudflare DNS | four 9s | DNS propagation lag during record updates |
| Static `.well-known` behind CDN | three to four 9s | CDN edge eviction; rare global outages |
| Pkarr / Mainline DHT | best-effort, multi-million-node | Per-node lookup latency 100 ms – seconds; record decays without republish |

Stacking them gives you survival when any one goes dark. The signed-blob property means a stale or partially-degraded channel is detectable, not exploitable — clients won't accept an unsigned, expired, or wrong-anchor list.

### Same recipe one level up — @aster itself

The `@aster` registry is exactly this pattern with the @aster anchor instead of Portal's:

- @aster runs N nodes under the @aster anchor key (offline, vendor-owned).
- Signed node list published at `_aster-nodes.id.aster0.net`, `https://aster0.net/.well-known/aster-nodes.json`, and Pkarr under the @aster anchor.
- Every Aster client ships with @aster's anchor pubkey **embedded** (same trust-distribution shape as Sigstore, Signal, or any OS vendor's package signing key). Anchor rotation is a binary update + a transitional-signing window. Out of scope here; covered in `trust-discovery-thinking-session.md` §"@aster's own bootstrap."

The recursion bottoms out: clients trust @aster's embedded anchor, @aster's anchor signs the @aster node list, @aster's nodes serve the handle registry, the handle registry resolves operators' anchors, operators' anchors sign their own node lists, operators' nodes serve product RPC. One pattern, three uses.

### What goes into the trust spec vs the product

This is an Aster concern in shape (the signed-node-list artifact, the distribution channels, the verifier algorithm) and a product concern in operation (which DNS provider, which CDN, how many regions, which database for the control-plane VMs). The shape lives in `Aster-trust-spec.md` v0.8 — likely as a new `code` value in the statement registry (e.g. `OPERATOR_NODE_LIST`, structurally equivalent to an aggregated endorsement). The operation lives in each operator's runbook.

---

## Customer CLI — finding their nodes

```
Customer laptop                                        Portal control plane
   │                                                            │
   │ $ portal login                                             │
   │                                                            │
   │ open https://portal.io/auth/cli                            │
   ├───────────────────────────────────────────────────────────▶│
   │                                                            │
   │ webauthn / oauth flow (re-uses the                         │
   │ webauthn-custodian primitive — see                         │
   │ aster-auth-webauthn-custodian.md)                          │
   │◀──────────────────────────────────────────────────────────▶│
   │                                                            │
   │ receive rcan {                                             │
   │   sub: cli_session,                                        │
   │   aud: PortalControl,                                      │
   │   tenant_id: T,                                            │
   │   exp: 24h,                                                │
   │   sig: signed by Portal control plane }                    │
   │◀──────────────────────────────────────────────────────────│
   │                                                            │
   │ $ portal nodes list                                        │
   │                                                            │
   │ resolve @portal-cp (or cached)                             │
   │ open Aster connection                                      │
   │ RPC PortalControl.list_nodes(rcan)                         │
   ├───────────────────────────────────────────────────────────▶│
   │                                                            │ verify rcan
   │                                                            │ extract tenant_id
   │                                                            │ query Node
   │                                                            │   where tenant_id = T
   │                                                            │ return [
   │                                                            │   { node_pk, endpoint,
   │                                                            │     instance_id,
   │                                                            │     region, last_seen },
   │                                                            │   ...
   │                                                            │ ]
   │ [{ node_pk, endpoint, ... }]                               │
   │◀──────────────────────────────────────────────────────────│
   │                                                            │
   │ $ portal workspace launch wine-dev --on i-0abc…            │
   │                                                            │
   │ dial node directly over Aster using                        │
   │ returned endpoint info; transport                          │
   │ verifies node_pk on QUIC handshake                         │
   ├──────────────────────────────────────▶ Portal VM ──────────│
```

The CLI never holds the customer's "Aster identity" because there is no such thing. The customer's identity in this product *is* their AWS account / Portal tenant ID. The rcan binds the CLI session to that tenant, and the control plane is the gatekeeper that scopes node visibility per tenant.

---

## SDK client enrollment — "API key" reframed

The third actor that needs to authenticate is the **customer's own application code** using the Portal SDK to spin up sessions, list workspaces, etc. Traditionally this is "paste an API key into an env var." With Aster, the right shape is materially different — and materially better.

### Mental model: registration token + ephemeral rcan

| Traditional SaaS | Aster SDK |
|---|---|
| API key is the bearer credential — whoever has the bytes can call. | Registration token is one-shot proof to enroll. SDK_pk is the actual identity. Rcan is the bound capability. |
| Key sits in env var, used on every call. | Token sits in env var briefly, exchanged once. Rcan auto-refreshes from cache. |
| Leaked key = full compromise until revoked. | Leaked rcan is useless without SDK_priv. Leaked registration token enables a *new* enrollment but doesn't let the attacker impersonate an existing pod. |

The customer's UX stays familiar — paste a token in `PORTAL_TOKEN` like they would for Stripe. The cryptographic shape underneath is meaningfully stronger.

### Flow

```
At dashboard time (web UI):
1. Customer logs into Portal dashboard.
2. Creates "Application credential" for tenant T, scope ["session.create", ...].
3. Dashboard returns a registration token (opaque ~30-char string, tenant-scoped,
   one-shot or use-N-times per policy).
4. Customer pastes token into PORTAL_TOKEN env var.

At SDK init (in customer's app process):
5. SDK generates SDK_pk on first run; persists SDK_priv to OS keystore
   (or in-memory only for ephemeral workloads — Lambda, Fargate spot, etc.).
6. SDK reads PORTAL_TOKEN, calls
     PortalEnrollment.register({ sdk_pk, proof: RegistrationToken { token } })
7. Control plane verifies token → tenant T → mints rcan
   bound to sdk_pk, scoped per the token's scopes, exp ~hours.
8. SDK caches rcan; attaches to subsequent RPCs;
   auto-refreshes before expiry by re-running step 6.
```

The registration token is the long-lived secret (months/years, like an API key). The rcan is the short-lived per-session capability (hours). SDK_pk is what makes the rcan unforgeable: possession of the rcan alone doesn't authenticate — the SDK has to sign the QUIC handshake with `sdk_priv`. So a leaked rcan in logs, in transit, in a memory dump — none of those alone grant access.

### "OIDC/SAML/SSO" is the same code path with a different proof input

This is the punchline that connects back to the pluggable `EnrollmentProof` from §"VM launch — auto-enrollment". Same enum, more variants:

```rust
enum EnrollmentProof {
    AwsInstanceIdentity   { document: Bytes, signature: Bytes },  // VM
    RegistrationToken     { token: String },                      // SDK, "API key" path
    OidcToken             { jwt: String, audience: String },      // SDK in IdP-aware env
    WorkloadIdentity      { ... },                                // SDK on k8s / GHA / Fly
    OperatorSignedToken   { token: AdmissionToken },              // BYO trust root
}
```

Every variant mints the same rcan-bound-to-pubkey artifact. The SDK only changes which env vars it reads at startup. The control plane only changes which verifier runs.

### Customer maturity ladder — same SDK, different proof type

| Customer profile | Proof type | Setup cost |
|---|---|---|
| Indie dev, first-day Portal user | `RegistrationToken` | Paste one env var, done |
| Customer running on k8s | `WorkloadIdentity` (projected service-account token) | One annotation on the pod |
| Customer running on AWS Lambda / ECS / GitHub Actions | `WorkloadIdentity` (platform OIDC token) | None — auto-detected |
| Enterprise with Okta / Azure AD / Auth0 | `OidcToken` (client-credentials flow) | One IdP app registration |
| Enterprise that doesn't trust Portal | `OperatorSignedToken` | Cross-anchor trust setup |

This is the AWS gateway-drug pattern (root user → IAM users → IAM roles → workload identity federation): start at "looks like an API key," graduate to platform-attested as the customer matures, never break the simple path.

### Prior art — what's the closest existing thing?

| Pattern | Shape | Closeness |
|---|---|---|
| Stripe API key | Bearer string, no key binding | Familiar UX; cryptographically weak |
| AWS IAM access keys | Bearer key + HMAC-signed requests | Closer — request signing gives binding |
| Kubernetes ServiceAccount tokens | Bearer JWT, rotated per-pod | Mid — bearer but with rotation |
| **HashiCorp Vault AppRole** | RoleID (public ref) + SecretID (one-shot to mint a token) | **Closest existing analog** |
| OIDC client credentials | `client_id` + `client_secret` (or mTLS-bound) | Standard, but pure bearer unless mTLS |
| GitHub Actions OIDC | Platform-issued JWT, exchanged for cloud creds | Closest to `WorkloadIdentity` variant |

Naming Vault AppRole as the closest prior art helps readers anchor: "two-step credential where the second step is bound to a keypair the workload owns." Aster's flavor adds the per-call signed-handshake property that mTLS gives, without requiring the customer to provision certs.

### Multi-replica subtlety

Customer apps usually run as N replicas (k8s pods, Lambda concurrency, etc.). Each replica generates its own SDK_pk and enrolls separately under the same registration token. The token is **"permission to register," not the identity itself.**

Two granularities of revocation:

- **Revoke the registration token** → future enrollments fail; existing replicas continue serving until their rcans expire (hours).
- **Revoke a specific SDK_pk** → one replica kicked out immediately; others unaffected.

Cleaner than traditional API keys, where leaking from one pod compromises every replica's traffic equally and rotation requires coordinated redeploy.

### Threat-model deltas vs traditional API keys

| Threat | API key | Aster SDK enrollment |
|---|---|---|
| Key leaked via log line / sentry / stack trace | Full compromise | Token leak enables new registration; rcan leak is inert |
| Key leaked via heap dump of one replica | Full compromise of all replicas' traffic | Compromise of that replica only (its SDK_priv); other replicas unaffected |
| Adversary-in-the-middle reads request | Replays the call | Cannot replay — handshake is bound to live QUIC session |
| Lateral movement: attacker compromises a replica, wants to pivot | API key works from anywhere | Stolen SDK_priv works only until rcan refresh denies it; revocation is per-replica |
| Customer rotation cadence | Painful (coordinate every replica) | Background — old rcans expire naturally |

---

## The responsibility split

This is the heart of the worked example: **what is Aster's job, what is the downstream product's job, what is the cloud platform's job?**

| Concern | Aster | Portal (downstream product) | AWS (cloud platform) |
|---|---|---|---|
| Node cryptographic identity (ed25519 endpoint key) | ✓ | | |
| Transport, framing, RPC dispatch, contract identity | ✓ | | |
| Anchor → deployment-root → admission-token chain | ✓ (primitives + verifier) | ✓ (issuing policy: when to mint, with what scope) | |
| Handle registry — find `@portal-cp` | ✓ | | |
| Pluggable enrollment-proof verifier | ✓ (the verifier framework) | ✓ (which proofs to accept for which tenant) | |
| Customer ground truth (which AWS account paid?) | | | ✓ (Marketplace + IID) |
| Tenant model (customer ↔ nodes mapping) | | ✓ | |
| Subscription / billing integration | | ✓ | ✓ (Marketplace) |
| Customer CLI authentication | | ✓ (uses webauthn-custodian primitive) | |
| SDK client enrollment (token / OIDC / workload-id → rcan) | ✓ (verifier framework + rcan minting) | ✓ (which proofs to accept, scope policy) | |
| Per-tenant access control on node listing / RPC | | ✓ | |
| Node-to-node communication, NAT traversal | ✓ | | |

The line is clean. **Aster is the identity + transport + trust spine.** The downstream product is a multi-tenant control plane built as Aster RPC services, plus the platform-specific glue that bridges cloud-platform identity to Aster admission tokens. The cloud platform is the customer-binding root.

---

## The pluggable enrollment-proof pattern

The interesting generalisation: every modern compute platform issues a signed "this workload is X" attestation. The enrollment service shouldn't hard-code AWS — it should accept a tagged proof and dispatch to the right verifier.

Sketch (Rust-ish):

```rust
enum EnrollmentProof {
    // VM / workload self-attestation by the cloud platform
    AwsInstanceIdentity      { document: Bytes, signature: Bytes },
    GcpInstanceIdentity      { jwt: String },
    AzureAttestedDocument    { document: Bytes, signature: Bytes },
    KubernetesProjectedToken { jwt: String, audience: String },
    GithubActionsOidc        { jwt: String },

    // SDK client paths (see §"SDK client enrollment")
    RegistrationToken        { token: String },                  // "API key" reframed
    OidcToken                { jwt: String, audience: String },  // customer's IdP

    // Escape hatch for BYO trust root
    OperatorSignedToken      { token: AdmissionToken },
}

trait EnrollmentProofVerifier {
    fn verify(&self, proof: &EnrollmentProof) -> Result<VerifiedClaims>;
}

// On the control plane:
fn handle_register(req: RegisterRequest) -> Result<AdmissionToken> {
    let claims = verifier.verify(&req.proof)?;
    let tenant = lookup_tenant(&claims)?;
    check_subscription_active(&tenant)?;
    let token = mint_admission_token(req.node_pk, tenant, req.contracts)?;
    record_node(req.node_pk, tenant, claims.workload_id);
    Ok(token)
}
```

The verifiers are platform-specific code; the framework is reusable. "Portal sold on AWS Marketplace" and "Portal self-hosted on a customer's k8s cluster" become the same control-plane code with different verifiers enabled.

The `OperatorSignedToken` variant is the escape hatch: a customer who pre-provisions an enrollment token out-of-band (e.g. baked into UserData by their own automation) skips the cloud-platform verifier entirely. Useful for air-gapped, on-prem, or custom-cloud deployments. Same admission-token output; different proof input.

This is also where customers who eventually want their **own anchor** (the multi-tenant anchor question) plug in: their VMs present an admission token signed by their own anchor → deployment-root chain, Portal's control plane recognises the customer's anchor as a registered enrollment authority for that tenant, and Portal mints its own admission token (or accepts the customer's directly, depending on policy). The chain mechanics (anchor authorising deployment root, deployment root authorising node, multi-anchor trust set) live in [ownership-attestations.md](../ownership-attestations.md) under "Delegation chains" and "Multi-anchor resolution."

---

## What this clarifies about "the management layer"

The original framing — "I can't wrap my head around the management layer over ed25519 identity" — gets concrete here. The management layer for a cloud-marketplace product like Portal is:

1. **A subscription model** (tenant table, AWS account → tenant mapping). Pure product DB.
2. **An enrollment service** — Aster RPC service, accepts platform-attested proofs, mints tenant-scoped admission tokens. ~one screen of handler code.
3. **A control service** — Aster RPC service, lists/manages tenant's nodes, scoped by rcan claims. Standard CRUD over Aster transport.
4. **A customer auth flow** — webauthn-custodian primitive (already designed) issues rcans bound to tenant IDs.
5. **The trust spine underneath** — anchor inversion, COSE_Sign1 admission tokens (per `ownership-attestations.md` §recommendation), handle registry. Provided by Aster.

That's it. **It is not a new identity primitive.** It is conventional multi-tenant SaaS plumbing layered on top of Aster's existing trust artifacts, with the IID-style enrollment proof acting as the single non-obvious bridge between cloud identity and Aster identity.

The reason "management layer" felt unwieldy in earlier sessions: it was being designed as if it needed to be an Aster-native primitive. It does not. It is product code, written *as* Aster RPC services, *using* Aster's identity primitives — but the tenancy, billing, and operator UX live in the product, not in the protocol.

---

## What's still open

1. **Customer wants their own anchor.** Some Portal customers (regulated, enterprise) will eventually want admission tokens signed by *their* anchor, not Portal's. This is the cross-operator discovery / federation problem — chain-walking and multi-anchor trust sets are designed in [ownership-attestations.md](../ownership-attestations.md). The pluggable verifier above is the hook.
2. **Discovery records for tenant-scoped nodes.** Portal control plane is the authoritative directory for a customer's nodes today. Whether to also publish per-tenant Pkarr/DNS aggregation records (so the customer's CLI can work even if Portal's control plane is briefly down) is a v2 question — the trust artifacts are already designed to support it. The signed-node-list shape from §"Control-plane HA" generalises to the per-tenant case directly.
3. **Multi-product reuse of the enrollment framework.** If Aster ships the pluggable enrollment-proof verifier as part of `aster-trust`, every downstream product (not just Portal) gets cloud-marketplace enrollment for free. Worth scoping into the v0.8 trust-spec revision.
4. **Per-VM identity rotation.** Today the node_pk is generated once at first boot. Strategies for rotation (re-enrollment vs in-place key rotation) need a small spec extension.
5. **Decommissioning.** When a customer terminates the VM, AWS doesn't notify Portal. Liveness comes from the heartbeat / discovery-record decay model in `trust-discovery-thinking-session.md` §"Freshness and revocation." Worth a concrete worked example here too.

---

## Mental model

> **The cloud platform proves the customer. Aster proves the node. The product proves the tenant.**
>
> Aster's identity primitives are the spine; cloud-platform attestations are the bridge from "human customer" to "ed25519 keypair"; the product owns the tenancy logic in between.

This is the same shape every successful cloud-marketplace fleet product has converged on. Aster is what makes the "node identity" half of the story crisp; AWS Marketplace + IID is what makes the "customer identity" half free; the downstream product is a thin tenancy layer that connects them.
