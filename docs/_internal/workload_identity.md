# Workload Identity — How Kubernetes (and Friends) Attest a Caller

> **2026-06-10 update.** The OIDC verifier described here survives unchanged
> under [trust-directory.md](trust-directory.md), but its **output changes**:
> instead of minting an admission rcan bound to `sdk_pk` (with the
> refresh-before-expiry loop), the verifier gates a **directory write** — a
> designated *enroller* validates the JWT and writes the node's `Admission`
> row into its designated namespace. The JWT's job ends at enrollment;
> ongoing authority is the row; the credential-refresh lifecycle disappears.

**Status:** Reference notes
**Date:** 2026-05-05
**Companion docs:**
- [working_ideas/identity-worked-example.md](working_ideas/identity-worked-example.md) — Portal use case, where `KubernetesProjectedToken` appears as one variant of the pluggable `EnrollmentProof`
- [ownership-attestations.md](ownership-attestations.md) — anchor → deployment-root → node delegation chain (supersedes the retired `trust-anchor-inversion.md`)

---

## Why this doc exists

The `EnrollmentProof` enum in the worked example lists `KubernetesProjectedToken`, `GithubActionsOidc`, and similar variants alongside the cloud Instance Identity Documents. AWS / GCP / Azure IIDs are familiar — the cloud signs a document saying "this VM is in account X." Workload-identity tokens (k8s, GHA, Fly, etc.) are less familiar but follow the same pattern with a different issuer.

This doc explains:

1. What a Kubernetes-mounted workload identity token actually is.
2. What the customer pastes into Portal's control panel to wire it up.
3. What our control plane has to do to validate it.
4. Why this is operationally better than a `RegistrationToken` for customers running on k8s.
5. How the same code path generalises to GitHub Actions OIDC, IRSA, GCP Workload Identity Federation, etc.

---

## What the workload presents

In a Kubernetes pod the kubelet mounts a **projected service-account token** at a path the workload reads. The pod spec opts into it:

```yaml
spec:
  serviceAccountName: portal-app
  containers:
  - name: app
    volumeMounts:
    - name: portal-token
      mountPath: /var/run/secrets/portal
  volumes:
  - name: portal-token
    projected:
      sources:
      - serviceAccountToken:
          path: token
          expirationSeconds: 3600
          audience: portal.io        # ← customer specifies this
```

Kubelet mints a fresh JWT every hour, signed by the cluster's service-account signing key. The Portal SDK reads `/var/run/secrets/portal/token` and presents the JWT as the `KubernetesProjectedToken` proof.

Decoded payload looks like:

```json
{
  "iss": "https://oidc.eks.us-east-1.amazonaws.com/id/ABC123",
  "sub": "system:serviceaccount:my-namespace:portal-app",
  "aud": ["portal.io"],
  "exp": 1234567890,
  "kubernetes.io": {
    "namespace":      "my-namespace",
    "serviceaccount": { "name": "portal-app", "uid": "..." },
    "pod":            { "name": "app-7d9f", "uid": "..." }
  }
}
```

Three properties matter for validation:

- **`iss`** identifies the cluster (more precisely, its OIDC issuer URL).
- **`sub`** identifies *which* service account inside that cluster.
- **`aud`** is what we required the customer to scope the token to — prevents a token minted for service A from enrolling at service B.

---

## What we need to validate it

Two things, both fetched from the cluster's **OIDC discovery endpoint** — the same standard OIDC clients have used for a decade. Every modern Kubernetes cluster exposes one:

| Cluster type | Issuer URL pattern |
|---|---|
| EKS | `https://oidc.eks.<region>.amazonaws.com/id/<cluster-id>` |
| GKE | `https://container.googleapis.com/v1/projects/.../clusters/...` |
| AKS | cluster-specific URL surfaced via the AKS control plane |
| Self-hosted | whatever `kube-apiserver --service-account-issuer` is set to |

From that issuer URL we fetch (and cache, with rotation):

1. `<issuer>/.well-known/openid-configuration` — points at the JWKS URL
2. `<issuer>/keys` (or whatever the discovery doc gave) — the public keys

Then any standard JWT library validates: signature against JWKS, `iss` matches our pinned issuer, `aud` contains our expected audience, `exp` not passed.

This is plain OIDC. We don't need a Kubernetes client library on the control-plane side; we need a JWT library and an HTTP client.

---

## What the customer provides in the control panel

Customer-side setup is one form with three fields:

| Field | Example | Purpose |
|---|---|---|
| **OIDC issuer URL** | `https://oidc.eks.us-east-1.amazonaws.com/id/ABC123` | Tells us which cluster's tokens to trust for this tenant. We refuse tokens from any other issuer. |
| **Allowed `sub` patterns** | `system:serviceaccount:prod:portal-app` (exact match or glob like `system:serviceaccount:prod:*`) | Scopes which service accounts in that cluster may enroll as this tenant. Without this, *any* workload in the cluster could enroll. |
| **Audience** (we pre-fill) | `portal.io` | Forces customer to mint tokens scoped to us, preventing token confusion across services. |

Behind the scenes the control plane stores per-tenant configuration:

```
Tenant T → enrollment_authorities: [
    { kind: "k8s",
      issuer: "https://oidc.eks.us-east-1.amazonaws.com/id/ABC123",
      sub_patterns: ["system:serviceaccount:prod:portal-app"],
      audience: "portal.io" },
    ...
]
```

A customer with multiple clusters adds multiple entries. A customer who also uses GitHub Actions for jobs adds a `GithubActionsOidc` entry beside the k8s ones — same shape, different `kind`.

---

## Control-plane validation flow

```
SDK in pod                                       Portal control plane
  │                                                       │
  │ Read /var/run/secrets/portal/token (JWT)              │
  │ Generate sdk_pk on first run                          │
  │                                                       │
  │ PortalEnrollment.register({                           │
  │   sdk_pk,                                             │
  │   proof: KubernetesProjectedToken {                   │
  │     jwt: "<the JWT>",                                 │
  │     audience: "portal.io"                             │
  │   }                                                   │
  │ })                                                    │
  ├──────────────────────────────────────────────────────▶│
  │                                                       │
  │                                                       │ 1. Decode JWT header → kid + alg
  │                                                       │ 2. Look up issuer from JWT claim
  │                                                       │ 3. Find tenant whose enrollment_authorities
  │                                                       │    contain this issuer
  │                                                       │ 4. Fetch JWKS from issuer (cached, refresh
  │                                                       │    on unknown kid)
  │                                                       │ 5. Verify signature with matching JWK
  │                                                       │ 6. Verify aud contains "portal.io"
  │                                                       │ 7. Verify exp/nbf
  │                                                       │ 8. Match sub against tenant's sub_patterns
  │                                                       │ 9. Mint admission rcan bound to sdk_pk
  │                                                       │    for tenant T, scope per tenant policy
  │                                                       │
  │ admission rcan                                        │
  │◀─────────────────────────────────────────────────────│
  │                                                       │
  │ Cache rcan; attach to subsequent RPCs;                │
  │ refresh by re-running register before exp.            │
```

The JWKS fetch is the only out-of-band call — and it's cacheable for hours. For high-throughput enrollment (autoscaling fleets) the control plane keeps a per-issuer JWKS cache with proactive refresh.

---

## Why this is operationally cleaner than `RegistrationToken`

| Concern | RegistrationToken | KubernetesProjectedToken |
|---|---|---|
| Customer-side secret on disk | Long-lived bearer in env var | None — kubelet mints fresh tokens |
| Compromise of one pod | Token is reusable from anywhere | Token expires in ≤1 hour, scoped to one SA |
| Rotation | Manual, requires customer action | Automatic, every hour, by kubelet |
| Granularity of trust | "Anyone with the bytes" | "Specific SA in specific namespace in specific cluster" |
| Customer-side audit | Limited (we see use; they don't) | Full — k8s audit log shows every token mint |
| Setup friction (Day 0) | Paste env var | Configure projected volume + paste issuer URL |
| Setup friction (Day N+) | Rotation pain | None |

For customers running on Kubernetes, this is strictly better. For customers running anywhere else (laptops, bare VMs, traditional servers), `RegistrationToken` is still the right answer.

---

## The same shape covers GitHub Actions, IRSA, GCP WIF, Azure WI

Every modern workload-identity system is OIDC discovery + JWT validation. The differences are cosmetic:

| System | `iss` | `sub` shape |
|---|---|---|
| Kubernetes (any flavour) | Cluster's service-account issuer URL | `system:serviceaccount:<ns>:<sa>` |
| GitHub Actions OIDC | `https://token.actions.githubusercontent.com` | `repo:<org>/<repo>:ref:refs/heads/<branch>` |
| AWS IAM Roles for Service Accounts (IRSA) | EKS OIDC issuer (same as projected token) | k8s `sub` |
| GCP Workload Identity Federation | `https://accounts.google.com` or per-pool issuer | Email or numeric ID of identity |
| Azure Workload Identity | `https://login.microsoftonline.com/<tenant>/v2.0` | Object ID of managed identity |
| GitLab CI OIDC | `https://gitlab.com` (or self-hosted URL) | `project_path:<path>:ref_type:branch:ref:<branch>` |
| CircleCI OIDC | `https://oidc.circleci.com/org/<org-id>` | `org/<org-id>/project/<project-id>/user/<user-id>` |
| Buildkite OIDC | `https://agent.buildkite.com` | `organization:<slug>:pipeline:<slug>:...` |

One verifier in the control plane — `OidcJwtVerifier { issuer, audience, sub_patterns }` — covers all of these. The `EnrollmentProof` variants are mostly typing convenience for the SDK side ("which env var / file path do I read?"); on the server they collapse to the same code path.

---

## The one customer-side gotcha — issuer reachability

The cluster's OIDC issuer needs to be **publicly reachable from our control plane.** EKS, GKE, and AKS handle this by default — their managed control planes expose the OIDC discovery endpoint on the public internet.

Self-hosted clusters often need:

```
kube-apiserver --service-account-issuer=https://oidc.example.com
               --service-account-jwks-uri=https://oidc.example.com/keys
```

…plus a small public endpoint (or S3 bucket / Cloudflare R2 / static-hosted JSON) serving the discovery doc and JWKS. This is the same setup customers do for AWS IRSA, GCP Workload Identity Federation, etc. — well-trodden, but a real Day-0 step.

**Recommendation:** the control panel should include a "Validate connectivity" button next to the issuer-URL field. On click, the control plane fetches `<issuer>/.well-known/openid-configuration`, parses it, fetches the JWKS, and reports back what it found. Catches typos, firewall problems, and misconfigured issuers before the customer wastes time wondering why enrollment fails.

---

## Implementation note for the verifier

A pragmatic Rust shape:

```rust
pub struct OidcJwtVerifier {
    issuer:        String,
    audience:      String,
    sub_patterns:  Vec<SubPattern>,        // exact strings or glob patterns
    jwks_cache:    Arc<RwLock<JwksCache>>, // refreshed on cache miss / on rotation
    http:          reqwest::Client,
}

impl OidcJwtVerifier {
    pub async fn verify(&self, jwt: &str) -> Result<VerifiedClaims> {
        let header = decode_header(jwt)?;
        let key    = self.jwks_cache.read().await.get(&header.kid)
                       .or_else_async(|| self.refresh_and_get(&header.kid))?;

        let claims: Claims = decode(jwt, &key, &Validation {
            iss:      Some(self.issuer.clone()),
            aud:      Some(self.audience.clone()),
            algorithms: vec![Alg::RS256, Alg::ES256, Alg::EdDSA],
            ..Default::default()
        })?;

        if !self.sub_patterns.iter().any(|p| p.matches(&claims.sub)) {
            return Err(Error::SubjectNotAllowed);
        }

        Ok(VerifiedClaims {
            tenant_id:   self.tenant_id,
            workload_id: claims.sub,
            issuer:      claims.iss,
            ..
        })
    }
}
```

Library choice: `jsonwebtoken` for verification, `reqwest` for fetching, an LRU `JwksCache` with TTL + on-demand refresh. ~300 LOC including tests; nothing exotic.

---

## What this enables

Once the OIDC verifier ships, every workload-identity-emitting platform (k8s, GHA, IRSA, WIF, Azure WI, GitLab CI, CircleCI, Buildkite, Vercel, Render, Fly, …) is supported by adding one entry to the per-tenant configuration. No new code per platform. The pluggable enrollment-proof framework from the worked example becomes "OIDC + a small library of cloud-IID verifiers" — and OIDC alone covers ~80% of customer environments in 2026.
