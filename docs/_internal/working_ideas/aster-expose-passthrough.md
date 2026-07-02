# aster-expose-passthrough — zero-trust edge via TLS passthrough + cert-shipping

**Status:** Working idea
**Date:** 2026-06-27
**Scope:** A public edge fronts a hostname (e.g. `somehost.emrul.dev`) but **never terminates TLS**. It obtains the ACME cert for that hostname, ships only the signed leaf down to the private backend node (the key never leaves the node), and from then on does pure **L4 SNI/CID routing**: peek the destination once, forward opaque packets over Aster, let the backend terminate the client's TLS. Result: browser↔backend is **end-to-end encrypted past the edge**, and the edge is an untrusted dumb pipe.

References:
- The L7 sibling that *does* terminate (and why): `docs/_internal/working_ideas/aster-expose-http.md` — esp. §3 (CC reasoning), §6.3 (TLS termination ⇒ no E2E secrecy). **This doc is the blind-forwarder mode that one defers.**
- Layer-1 transport primitive: `ffi_spec/Aster-tunneling.md` (`core/src/tunnel.rs`); §11 "blind-forwarder mode" is what this realises.
- Datagram plane (reused verbatim): `core/src/datagram.rs` (`DatagramRouter`), `core/src/lib.rs:2039` (`send_datagram`/`read_datagram`), `:2076` (`max_datagram_size`).
- QUIC-LB: [draft-ietf-quic-load-balancers](https://quicwg.org/load-balancers/draft-ietf-quic-load-balancers.html) (routable Connection IDs).
- Borrowed design, not wire protocol: [draft-ietf-masque-quic-proxy](https://ietf-wg-masque.github.io/draft-ietf-masque-quic-proxy/draft-ietf-masque-quic-proxy.html) forwarded mode (CID translation).

---

## 1. What this is, and where it sits next to expose-http

`aster-expose-http` terminates browser TLS at the edge and re-originates over Aster. That is **ideal for HTTP/3 throughput** (each hop is single-CC, native) but has one inherent cost it states plainly: the edge can read and modify everything (§6.3). For a CDN that's fine. For "I run a private service and the edge is rented infrastructure I don't trust," it isn't.

This design takes the opposite trade on purpose:

| | expose-http (terminate-and-relay) | **expose-passthrough (this doc)** |
|---|---|---|
| Edge sees plaintext | yes | **no — never** |
| Edge holds the cert **key** | yes | **no — key born and dies on backend** |
| Browser↔backend encryption | terminates at edge | **true E2E past the edge** |
| Edge can do L7 (cache/WAF/path-routing) | yes | **no** (it's bytes) |
| Per-hop congestion control | ideal (native each hop) | nested for QUIC (see §6) |
| Edge capability | smart | **dumb pipe** |

They are **complementary, not competing**: same `register_with_edge(hostname)` control plane, same Aster carriage, different datapath. A deployment picks per-hostname which mode an edge runs. Pick passthrough when the edge is untrusted or the backend insists on owning its TLS; pick terminate-and-relay when you want edge L7 and max H3 throughput.

This doc is **hostname-based fan-out**: one edge IP fronts *many* backends keyed by SNI. (If it were one-node-per-IP you'd need none of §5–6 — just blind-forward the whole socket. The entire CID machinery exists only because many nodes share an edge IP.)

---

## 2. The cert flow — key never leaves the backend

The elegant half. ACME proves control of `somehost.emrul.dev`; the edge owns that hostname's DNS/IP, so the edge runs the ACME client. But the edge must **not** end up holding the private key, or "untrusted edge" is a lie.

So invert the usual push:

```
backend node                              edge
  │ 1. generate keypair (key stays here)
  │ 2. build CSR for somehost.emrul.dev
  │ ───────────  CSR over Aster  ────────▶ │
  │                                         │ 3. ACME order w/ this CSR
  │                                         │    (HTTP-01/DNS-01/TLS-ALPN-01,
  │                                         │     edge controls the hostname)
  │ ◀──────  signed leaf + chain  ───────── │ 4. return cert only — NO key
  │ 5. install cert against the in-memory
  │    key; serve via SNI resolver (§4)
```

ACME lets you submit your own CSR (`finalize` takes a CSR, not a key), so the edge signs a key it never sees. The private key is generated on the backend, lives only in backend memory, and transits nothing. The edge's ACME credential and the cert are the only secrets it holds — and a leaf cert is public anyway.

Renewal is the same loop on a timer; the backend re-CSRs (or reuses the key and re-CSRs) and the edge re-orders. The edge↔backend channel for CSR/cert exchange is a normal authorised Aster RPC — reuse the `register_with_edge` consent handshake (expose-http §6.2): the edge already decides whether this backend may *claim* the hostname; here it additionally *acquires the cert* for it. Two consents, one channel.

**Edge ACME challenge note.** Because the edge never terminates TLS in the data path, it can't answer TLS-ALPN-01 *inline*. That's fine — ACME runs out-of-band as a control-plane process on the edge (its own listener / DNS API), wholly separate from the forwarding plane. DNS-01 is cleanest (no inbound), HTTP-01 works if the edge keeps a tiny control listener on :80.

### 2.1 Issuance, rate limits, and the default/premium split

Let's Encrypt caps **50 certificates per *registered domain* (eTLD+1) per 7 days** — and crucially, **renewals are exempt** (they hit only a separate *Duplicate Certificate* limit of 5/week for the identical hostname set). Since early 2025 it's a token bucket, not a hard wall. ([rate-limits](https://letsencrypt.org/docs/rate-limits/), [scaling post](https://letsencrypt.org/2025/01/30/scaling-rate-limits))

So this is **not** a fleet-size limit. Steady-state operation at any scale is free; the only throttle is **onboarding rate of *new* hostnames under a single registered domain you own** (e.g. multi-tenant `tenant1.emrul.dev … tenantN.emrul.dev` > 50/week). Everything else — renewals, customer-brought domains, sharded domains — is unaffected.

**The wildcard tension (why this shapes the architecture).** The obvious escape is a wildcard `*.emrul.dev`: one issuance covers every subdomain, sidesteps the limit entirely (DNS-01 required). But a wildcard is **one cert with one key**, and in *this* passthrough design that key would have to be shared across every backend (the edge can't hold it — it doesn't terminate). A fleet-wide shared key means one compromised backend impersonates every hostname — **which destroys the per-node key isolation that is the entire point of passthrough.** Same problem bounded to 100, plus re-issue-to-add-one churn, for SAN certs. So: **wildcard is ideal for terminate-and-relay, poison for passthrough.** That is not a coincidence; it tells us how the two designs divide:

| Tier | Path | Cert | Issuance load |
|---|---|---|---|
| **Default** | terminate-and-relay (`expose-http`) | **one wildcard at the edge** | zero per-tenant — scales infinitely |
| **Zero-trust / E2E** | passthrough (this doc) | **per-hostname** | rare opt-in — comfortably < 50/week |

For default tenants the edge already holds the key and sees plaintext (expose-http §6.3), so a shared wildcard key costs **no additional trust** — it's free. For zero-trust tenants the per-hostname cert is exactly the price of key isolation, and being opt-in it stays well under the cap. **The rate limit is the economic signal that passthrough is the premium path and wildcard-terminate is the default** — it unifies the two docs rather than fighting them.

**Other levers (in preference order):**
- **Customer-brought domains** (`app.customerco.com`) — each customer's registered domain has its *own* 50/week; no single customer onboards 50 hostnames a week, so the limit effectively vanishes. Best fix when the model allows it.
- **Shard across registered domains** you own (`emrul.dev`, `aster0.net`, `aster1.net`, …) → N×50/week, mechanical.
- **Multiple / alternative CAs** — the cap is per-CA. ZeroSSL [advertises no per-registered-domain limit](https://help.zerossl.com/hc/en-us/articles/17864245480093-Advantages-over-Using-Let-s-Encrypt); Google Trust Services also does ACME. Caveat: ZeroSSL ACME requires **External Account Binding (EAB)** credentials (an account + HMAC key), unlike LE's open ACME — a small onboarding wrinkle, not a blocker. Useful as the passthrough-tier issuer precisely because it sidesteps the registered-domain cap for the case (many subdomains, one owned domain, needing isolation) where wildcard is off the table.

**Issuance hygiene (mandatory, both tiers):**
- **Never re-issue on edge restart or backend reconnect.** A crash-loop that re-orders blows the 5-duplicate/week limit within an hour and locks the hostname out. Persist the cert cache (the edge already has `cache_path`); issue only on genuinely-new-hostname or near-expiry.
- **Single issuer per hostname.** In passthrough the *backend* holds the cert, so it is the natural sole issuer — multiple HA edges must **not** each run ACME for the same hostname (that multiplies duplicates). Backend issues once; edges only forward.
- **2026 shorter lifetimes** ([45-day certs](https://letsencrypt.org/2026/02/24/rate-limits-45-day-certs)) mean more renewals, but renewals are exempt and 5 duplicates/week covers even short cadences — no registered-domain impact.

---

## 3. Datapath split: TCP is free, QUIC is the work

### 3.1 TCP (TLS-over-TCP, HTTP/1.1 & h2) — commodity
Peek SNI from the ClientHello without terminating, then splice. This is `ngx_stream_ssl_preread`, HAProxy `mode tcp` + `req.ssl_sni`, or `dlundquist/sniproxy` — solved for a decade. For us the "splice" target is an Aster reliable stream to the backend; the backend feeds the raw TLS byte stream into its terminator. Nothing novel; we may not even build our own — an off-the-shelf L4 SNI proxy in front of the edge process is a legitimate cut-1.

### 3.2 QUIC / HTTP-3 (UDP) — nothing off-the-shelf works
Confirmed during research: **stock L4 proxies do not SNI-route QUIC.** `nginx ssl_preread` is broken on UDP/QUIC ([nginx#784](https://github.com/nginx/nginx/issues/784)); HAProxy/nginx QUIC support is *termination*, not passthrough. So the QUIC path is ours to build, and it's the rest of this doc.

Two facts make it tractable:
1. QUIC **Initial** packets are protected with keys derived from a *well-known* version salt + the client DCID (RFC 9001) — anyone can decrypt them, parse the CRYPTO frame, read the ClientHello SNI. Gets us the **first** routing decision.
2. We **own the backend**, so we can make it emit **routable Connection IDs** (QUIC-LB) — every *subsequent* packet routes by CID with **zero crypto** and survives migration.

---

## 4. Backend side: SNI cert resolver + TLS origin

Trivial and standard. The backend terminates the client's QUIC/TLS with a `rustls` `ResolvesServerCert`:

```
SNI == somehost.emrul.dev   → present the ACME leaf from §2
otherwise                   → present default self-signed (or refuse)
```

No packet inspection on the backend — SNI arrives through the normal handshake. One backend can serve several hostnames (several leaves, one resolver). This is the only TLS termination in the whole system, and it's where it belongs.

---

## 5. QUIC-LB CID-mode routing (the core mechanism)

### 5.1 What the edge holds
A QUIC-LB **config**: an encoding key + server-ID layout. With it the edge decodes a server-ID out of *any* conforming Connection ID — long- or short-header, first packet or millionth — by cheap arithmetic / one block-cipher op, never touching the connection's real keys. Use the **encrypted-CID** variant so the CID doesn't leak backend topology to the public.

### 5.2 What the backend does
Its QUIC stack generates CIDs that encode its server-ID per the shared config. In quinn terms this is a custom `ConnectionIdGenerator`. **Open item:** verify noq/iroh lets us substitute the CID generator (quinn supports it; our fork must expose it) — see §9.

### 5.3 The bootstrap gap (first packet) — the one subtlety
QUIC-LB routes by the **server's** routable CID, but the client's *first* Initial carries a **client-chosen random DCID** — the server hasn't issued its CID yet. So:

```
client Initial #1  (random DCID)  ──▶ edge: NOT routable yet
                                       └─ decrypt Initial (well-known salt),
                                          read SNI → pick backend B,
                                          remember {random DCID → B} (short TTL)
edge ──▶ B over Aster; B's Initial reply carries a ROUTABLE SCID
client adopts that SCID as its DCID ──▶ edge: routable from here on,
                                          decode server-ID, drop the TTL entry
```

So: **first packet = decrypt-Initial-for-SNI; everything after = CID decode.** The `{client-random-DCID → backend}` table is tiny and short-lived (only until the client adopts the server CID — typically 1 RTT), covering 0-RTT/coalesced Initials sent before the server reply lands. After adoption it's pure stateless CID routing.

### 5.4 Migration & NAT rebind — why we bother
When the client migrates (new 4-tuple) or a NAT rebinds, packets still carry the routable DCID, so the edge routes correctly **by CID** with no per-flow 4-tuple state. This is precisely the property stock 4-tuple forwarders lack and the reason QUIC-LB exists. Decrypt-the-Initial alone could not survive this; QUIC-LB is what makes the dumb edge correct, not just lucky.

---

## 6. Carriage edge→backend: Aster datagrams, forwarded-mode — NOT MASQUE

The client's UDP packets cross edge→backend as **Aster unreliable datagrams** (`core/src/lib.rs:2039`), reusing the `DatagramRouter` framing already shipped (`core/src/datagram.rs`): `[flow-id varint][payload]`, where flow-id maps to the backend's QUIC connection. The backend re-injects each payload into its quinn/noq socket; its inner QUIC sees real loss and runs its own CC correctly (RFC 9221: datagrams are congestion-controlled, not retransmitted).

**Why Aster datagrams and not a reliable stream:** tunnelling QUIC inside a *reliable* stream double-retransmits and reintroduces HoL blocking on data the inner QUIC already manages — expose-http §3.1 calls this the worst possible payload. Datagrams avoid it.

**Why not MASQUE / CONNECT-UDP (RFC 9298):** it solves carriage we already have. Aster *is* an authenticated, multiplexed QUIC channel between two cooperating endpoints — CONNECT-UDP's entire Extended-CONNECT + context-ID + target-negotiation layer is overhead, because our edge↔backend trust is pre-established and the "target" is the backend's own local socket, not a client-chosen host. CONNECT-UDP earns its keep only for *forward* proxying (client picks an arbitrary target); this is a fixed reverse tunnel. We **borrow** the MASQUE quic-proxy *forwarded mode* idea — pass short-header packets through with **CID translation** rather than re-encapsulating — which is exactly what QUIC-LB CID decode gives us, and skip the wire protocol.

**Inherent cost (state honestly).** Two serial CC domains: client↔backend QUIC runs *inside* the edge↔backend Aster datagram path, whose own CC shapes datagram emission. This is the nested-CC penalty expose-http §3.2 flags for forwarding. We accept it **knowingly** as the price of E2E secrecy + key-isolation — it is the whole point, not a regression. Mitigations: keep the edge close (short edge↔backend RTT dominates), BBR on the Aster leg, clamp advertised `max_datagram_size` (`:2076`) so inner packets fit one outer datagram and never fragment. This is the §3.2 "hard E2E-QUIC requirement" lane that doc explicitly reserved for exactly this case.

---

## 7. Security properties (what the edge can and cannot do)

- **Cannot read or modify traffic** — never holds the key, never terminates. Browser↔backend is E2E.
- **Holds:** the QUIC-LB config (→ can map CIDs to backends, i.e. learns *which backend* a flow hits — a metadata leak, mitigated by encrypted-CID so it's only its own routing table, not topology), the ACME credential, and public leaf certs.
- **Can still:** drop/delay/reorder packets (it's the path), observe traffic *volume* and timing, and learn SNI of new flows (it decrypts Initials — by necessity). It cannot forge or downgrade TLS.
- **ECH caveat:** this depends on SNI being visible for the first-packet decision. Encrypted ClientHello hides it — but ECH activates only if the *edge* publishes ECH configs in DNS, which it controls. Until/unless we adopt ECH, SNI routing holds. If we ever want ECH, the edge would terminate the outer ECH layer, which breaks the zero-trust property — note and defer.

---

## 8. Staged plan

**Stage A — TCP passthrough (cut 1, mostly free).**
1. Edge: L4 SNI peek (ssl_preread-equivalent or front with `sniproxy`) → Aster reliable stream → backend.
2. Backend: `ResolvesServerCert` SNI resolver (§4); terminate TLS; serve local service.
3. Cert flow §2 (CSR-from-backend → ACME-at-edge → leaf back). **This is the highest-value, lowest-risk slice and ships the whole cert story.**

**Stage B — QUIC-LB CID routing (the real work).**
4. Expose quinn `ConnectionIdGenerator` through noq/iroh (§9 open item) — *gating*.
5. Backend emits routable (encrypted) CIDs per shared QUIC-LB config.
6. Edge: QUIC-LB config + CID decode → server-ID → backend; first-packet Initial-decrypt for SNI + short-TTL random-DCID table (§5.3).
7. Carriage over `DatagramRouter` (`core/src/datagram.rs`) with size clamp (§6).

**Stage C — hardening.**
8. Migration/rebind soak (§5.4); 0-RTT & coalesced-Initial correctness; Retry handling; renewal loop; multi-hostname-per-backend resolver; metrics (decode hits/misses, TTL-table size, datagram drops).

---

## 9. Open questions
- **noq CID generator:** does our iroh/noq fork expose quinn's `ConnectionIdGenerator` so the backend can emit QUIC-LB CIDs? quinn supports it upstream; confirm it's reachable through our fork without patching iroh internals. **Blocks Stage B.**
- **QUIC-LB algorithm choice:** encrypted-CID (hides topology, one AES op/packet) vs plaintext (cheaper, leaks server count). Lean encrypted; benchmark the per-packet cost at edge.
- **Retry / address validation:** does the edge proxy backend-initiated Retry transparently, or does the backend's anti-amplification get confused behind the edge's source address? Likely transparent (edge preserves client addr semantics via the datagram tag) — verify.
- **0-RTT before server CID adoption:** the random-DCID TTL table must hold long enough for 0-RTT bursts; size/TTL tuning.
- **Datagram-plane CW sharing:** iroh dedups to one connection per peer pair (expose-http §9) — passthrough datagrams may share a CW with unrelated Aster RPC to the same backend. Same starvation surface expose-http §3.4 flags. Possibly wants a dedicated datagram-plane connection. Open there, open here.
- **Do we even build the TCP peeker?** Fronting the edge with stock `sniproxy`/HAProxy-tcp for Stage A may be strictly better than reimplementing ssl_preread. Decide before writing Stage A code.
