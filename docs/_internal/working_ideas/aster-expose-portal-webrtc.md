# aster-expose-portal-webrtc — global signaling front for portal_desktop's WebRTC path

**Status:** Working idea
**Date:** 2026-06-27
**Scope:** Give `portal_desktop`'s WebRTC viewer a public, low-latency, globally-distributed front using `aster-expose`, so a **remote** browser (off-LAN) can reach a private/NAT'd host for **signaling only**. The edge fronts the `/webrtc` viewer + auth + `POST /webrtc/offer`; the actual desktop media rides the WebRTC datachannel **P2P or via TURN, never through the edge**. This is the "B2 public signaling broker + TURN" that portal's own `signaling.rs` already names as future work.

References (portal_desktop, `/Users/emrul/dev/emrul/portal_desktop`):
- Offer handler: `host/src/signaling.rs::handle_offer` (the contract, §1).
- HTTP route: `host/src/salvo_app.rs:169` (`WebRtcOfferHandler`); non-salvo mirror `host/src/lib.rs:1149`.
- One-listener model (page + auth + `/wt` + `/webrtc`): `host/src/lib.rs:371`, `:887`.
- ICE config (STUN only, TURN is B2): `host/src/signaling.rs:62`.
- Body cap 256 KB: `host/src/lib.rs:1229`.

References (aster-rpc, shipped):
- L7 edge: `EdgeRouter`/control plane (`aster-expose/src/control.rs`), `RelayHandler`/`serve_edge` (`aster-expose/src/edge.rs`), `relay_request[_streaming]`, `ExposeNode` (`aster-expose/src/node.rs`).
- Design parents: `aster-expose-http.md` (terminate-and-relay datapath), `aster-expose-passthrough.md` (the zero-trust variant, §6 here).

---

## 1. The real contract (measured, not assumed)

`POST /webrtc/offer` (`signaling.rs::handle_offer`, wired at `salvo_app.rs:169`):

| | |
|---|---|
| **Body in** | raw SDP **offer** as text (`application/sdp`; handler does `from_utf8_lossy`, no content-type check). **≤ 256 KB** (`lib.rs:1229`). Real offers are a few KB. |
| **Auth** | valid **session cookie** `SESSION_COOKIE`, `auth::global().validate(...)`. Missing/invalid → **401** `auth required`. |
| **Body out** | **200 OK**, `Content-Type: application/sdp`, body = SDP **answer**, `Cache-Control: no-store`. |
| **Errors** | **500** + plaintext (bad SDP, server stopped, …). |
| **ICE model** | **non-trickle** — `set_remote_description(offer)` → `create_answer` → gather-to-complete → return one answer with all candidates baked in. STUN `stun.l.google.com:19302`; **no TURN yet** (`signaling.rs:62`). |

**The decisive property:** `handle_offer` doesn't *compute* an answer, it *makes the host the WebRTC peer* — it builds the `RTCPeerConnection`, attaches the `WebRtcCarrier`, and spawns the B3 pump. The answer carries **that PeerConnection's** ephemeral DTLS fingerprint + the host's **live** ICE candidates. So **whoever runs `handle_offer` becomes the media endpoint.** The edge therefore *cannot* run it — it must relay the offer to the host and return the host's answer. (If the edge generated the answer, media would terminate at the edge and we'd be back to a fat media relay — the exact thing this design avoids.) The signaling round-trip is irreducible; it is the price of keeping all media off the edge, and it's one small request at setup.

Two consequences this contract forces:
1. **One-shot, non-trickle ⇒ the trivial stateless-relay path.** No WebSocket, no PATCH session. Each offer is an independent small request/response — any edge instance serves any offer, no affinity.
2. **Cookie auth ⇒ front the whole small control origin,** not literally just `/offer`. The remote browser must load the `/webrtc` viewer *from the edge* (`portal.emrul.dev`) to get a same-origin `SESSION_COOKIE`, then POST the offer same-origin. Page + auth + offer are all tiny HTTP. Only the **media** (datachannel video/audio) is heavy, and it bypasses the edge.

---

## 2. Topology

```
remote browser
  │  GET /webrtc           (viewer page; cacheable at edge)
  │  POST /webauthn/...    (auth → SESSION_COOKIE, same-origin)
  │  POST /webrtc/offer    (SDP offer ──▶ SDP answer)
  ▼
Cloudflare: Worker (TLS, every PoP) ──▶ Container: aster-expose edge
                                           │ warm Aster conn (dialed host node id)
                                           │ relay_request → "portal-webrtc" service
                                           ▼
                                    portal_desktop host (private/NAT'd, runs Aster)
                                           │ salvo listener: page + auth + /webrtc/offer
                                           │ handle_offer → RTCPeerConnection (= media peer)
                                           ▼
                                    SDP answer ──back over Aster──▶ edge ──▶ browser

then WebRTC datachannel: browser ⇄ host DIRECT (srflx hole-punch)  ── or ⇄ TURN ⇄ host
                         ── media NEVER touches the aster-expose edge ──
```

---

## 3. Which expose path: L7 terminate-and-relay (shipped Cut-1)

Offer is HTTP request/response, so this is the **terminate-and-relay** datapath (`aster-expose-http.md`), using the already-built `EdgeRouter` + `RelayHandler` + `serve_edge` + `relay_request`. **No passthrough, no QUIC-LB, no per-hostname cert dance** (§6 covers the zero-trust variant if signaling integrity vs. the edge ever matters). The page (`GET /webrtc`) and auth POSTs relay the same way; the offer is just another relayed request. `RelayHandler` already forwards headers minus hop-by-hop, so the `Cookie` header passes through intact.

---

## 4. The control-plane inversion (the one real adaptation)

Shipped aster-expose is **backend-registers-with-edge** (ngrok shape: backend ephemeral, edge stable). Here it's **reversed**:

- The **edge** is the ephemeral/unknown party — N Cloudflare containers spinning up worldwide, which the host can't enumerate or dial.
- The **host** is the stable identity — a known Aster **node id**, reachable via iroh even behind NAT.

So the edge is configured statically with `{ host_node_id, service_id: "portal-webrtc", host: "portal.emrul.dev" }`, **dials the host** on startup, keeps the Aster connection warm, and relays. No dynamic `request_route` — the route is static config. The shipped data path (`relay_request` over an edge-opened connection) already supports this; only the registration *direction* is new. Portal side: register a `LocalHttpTarget`/`ServiceAcceptor` named `portal-webrtc` whose handler is the host's existing salvo app (page + auth + offer), served over the in-process duplex.

---

## 5. TLS: Cloudflare terminates ⇒ no cert in the container

Put `portal.emrul.dev` on Cloudflare; the Worker + CF managed cert terminate TLS, and the container gets already-decrypted HTTP. **This deletes the entire Let's Encrypt / rate-limit story for this deployment** (no ACME, no `cache_path`, no 50/week). The container just speaks HTTP-relay-over-Aster.

**Trust note:** CF (and the edge container) therefore see the **SDP plaintext** — including the DTLS fingerprint. A malicious edge could swap the fingerprint and MITM the media (become the peer). For portal fronting its *own* CF container this is acceptable (you already trust CF as infra). If you ever don't, that's the only reason to reach for §6.

---

## 6. Optional zero-trust variant (if you don't trust the edge with signaling integrity)

Switch the offer origin to **TCP SNI passthrough** (`aster-expose-passthrough.md` §3.1): the **host** terminates TLS with its own cert for `portal.emrul.dev`, the edge only SNI-forwards. Now the edge/CF can't read or tamper with the SDP — the DTLS fingerprint is integrity-protected end-to-end, killing the MITM vector. Cost: the host needs a public cert for `portal.emrul.dev`. **The §2.1 rate limit is a non-issue here** — it's *one* hostname, and renewals are exempt, so it issues once and renews forever well under any cap. So for portal specifically, zero-trust signaling is cheap and on the table; it's a per-deployment toggle, not a redesign.

---

## 7. TURN is host-side and separate (media plane)

The edge **never** carries WebRTC media — that's what keeps it stateless and cheap. TURN fallback is configured host-side: append your TURN server to `RTCConfiguration.ice_servers` at `signaling.rs:62` so the gathered answer includes **relay** candidates (today there's only Google STUN). Since you're already on Cloudflare, **Cloudflare Realtime/TURN** is the natural co-located relay. iroh relays cannot substitute — they carry iroh/QUIC, not DTLS-SRTP. Net: aster-expose for signaling, CF TURN for media fallback, two independent planes.

---

## 8. Caveats

- **The warm Aster connection is the one piece of state that matters.** Dialing the host per-offer would add an iroh handshake (+ maybe a relay hop) to every connection setup. Keep a long-lived container holding the conn warm — i.e. *not* pure scale-to-zero (CF min-instances / keep-warm), or eat a cold-start penalty on the first offer after idle.
- **Edge→host may itself traverse an iroh relay** if the host is hard-NAT'd — adds latency to the signaling relay, but it's one small round trip at setup, not per-media-packet. Acceptable.
- **Remote ICE needs the host's srflx/relay candidates.** Host LAN candidates are useless to a remote browser; the host must gather srflx (STUN, already there) and, for symmetric-NAT cases, relay (TURN, §7). This is host-side and per-session — reinforces why the offer must hit the host (§1).
- **Cookie domain.** The viewer page and the offer POST must be same-origin (`portal.emrul.dev`) so `SESSION_COOKIE` is sent. Fronting the whole control origin through the edge (page + auth + offer) satisfies this for free.

---

## 9. Staged plan

1. **Portal side — expose the salvo app as an Aster service.** Register `ServiceAcceptor` `portal-webrtc` → existing host salvo router (page + auth + `/webrtc/offer`) over the in-process duplex (`ExposeNode::expose_http`). Host stays private; reached only by its node id.
2. **Edge side — static-route, edge-dials-host variant.** Container runs `serve_edge` with a static route `{ host_node_id, "portal-webrtc", "portal.emrul.dev" }`; dials host on boot, keeps warm, relays. TLS off (CF terminates, §5).
3. **Deploy as a CF container** behind a Worker on `portal.emrul.dev`; min-instances to hold the warm conn.
4. **TURN** — add CF Realtime TURN to `signaling.rs:62` ice_servers; verify a remote (symmetric-NAT) browser falls back to relay and connects.
5. **(Optional) zero-trust toggle** — flip offer origin to TCP passthrough (§6) if signaling integrity vs. CF/edge is required; host gets a single public cert.

## 10. Open questions
- **Edge-dials-host control variant**: shipped control plane is registration-from-backend; confirm `relay_request` over an edge-opened connection with a *static* route needs no new core seam (expected: no — `relay_request` already takes a connection the caller opened). Small wrapper in `aster-expose`.
- **Auth model for remote**: is `SESSION_COOKIE` minted by a flow that works when the whole origin is the edge (WebAuthn/cookie issuance relayed identically)? Likely yes (it's just more relayed requests), but verify the auth flow has no host-absolute-URL or cert-hash dependency (the WT viewer embeds a cert hash at `lib.rs:442`; the **`/webrtc` viewer must not** depend on a reachable WT port — confirm it's WT-independent).
- **Keep-warm economics**: min-instances per region vs. cold-start tolerance for first-offer latency.

---

## 11. RESOLVED — CF Containers are relay-only; direct needs a UDP-native substrate (2026-06-27)

**Finding (research, not yet spiked):** Cloudflare's container/networking model is **HTTP/HTTPS on 80/443 only** — no arbitrary outbound UDP (outbound handlers intercept only HTTP/HTTPS; non-80/443 ports "are never routed"; public UDP unsupported platform-wide, cloudflared #964 open since 2022). No UDP egress ⇒ **no QUIC hole-punch ⇒ the warm Aster conn can never upgrade past Relay on CF.** ([CF outbound docs](https://developers.cloudflare.com/containers/platform-details/outbound-traffic/))

But Aster **does connect** from CF: iroh's relay is DERP-derived **HTTP/TLS→WebSocket over TCP/443** and tunnels the QUIC datagrams inside it (designed for UDP-hostile firewalls; WS-only since 0.91). ([iroh-relay](https://docs.rs/iroh-relay)) So the full offer/answer RPC works — **permanently relayed**. CF egress (TCP/443) → iroh relay → host.

**Cost:** relay adds a one-time triangle hop to the *one-shot* offer round-trip (~tens of ms, relay-placement-dependent); media is P2P/TURN regardless, so steady-state is unaffected. The risk is that a slow relay leg relocates rather than removes the long-haul latency the global edge was meant to cut.

**If direct is required → substrate change.** [Fly.io](https://fly.io/docs/networking/udp-and-tcp/) is UDP-native (inbound anycast + outbound), global, normal netns → iroh goes Direct. Any UDP-capable VM works; Fly preserves the global-low-latency-container property. Hybrid (CF Worker front + Fly Aster node) adds a hop — likely just run the whole edge on Fly.

**Spike to settle it** (`prober` binary, uses `Connection::selected_path()` → `PathRemote::{Direct,Relay}` + RTT): create endpoint → dial a known home host node id → N small (~4 KB, SDP-sized) echo round-trips → log path transitions + RTT percentiles. Built as `aster/examples/probe_{local,edge}.rs`; Fly deploy artifacts in `spikes/connectivity/` (cargo-chef Dockerfile, fly.toml).

**RESULT (2026-06-27, Fly `lhr` London → home-NAT node): DIRECT confirmed.**
- `reached direct : yes, after 1121 ms` — started on relay, **UDP hole-punched to a direct path in ~1.1 s**.
- Direct path `via 87.254.0.165:50391` (home public reflexive addr — a genuine hole-punch, not relayed).
- **Direct app RTT: p50 36 ms** (min 34, max 39; QUIC RTT ~27 ms + ~8 ms stream/echo overhead). **Relay app RTT: 82 ms.** Direct is ~2.3× faster.
- Conclusion: **Fly.io is a viable substrate for a direct, low-latency edge** (UDP-native → iroh hole-punches home). This is exactly what CF Containers *cannot* do (no UDP egress → relay-only). So if the portal edge wants a direct Aster path home, run it on Fly, not CF. For the one-shot WebRTC offer specifically, even CF's relay (~82 ms-class) would be tolerable — but Fly gives direct ~36 ms for free.

---

## 12. Handoff to portal_desktop (build the edge service proper)

Design is de-risked; the substrate unknown is settled. This section is the build brief.

**Substrate decision supersedes the CF assumptions above.** §2/§5/§8 were drafted around Cloudflare. **Read "Fly" for "CF" throughout** (§11). The terminate-and-relay reasoning, the cookie/same-origin requirement, and the "edge owns TLS, container gets HTTP" simplification are all unchanged — only the provider and the *direct-vs-relay capability* differ. On Fly: point `portal.emrul.dev` at the app and `fly certs add portal.emrul.dev` manages the cert (ACME handled by Fly); the container speaks plain HTTP behind it. So the LE rate-limit machinery stays moot, same as the CF story.

**STEP 0 — verify before writing any edge code (the one thing that can invalidate the topology):** confirm the **`/webrtc` viewer + `SESSION_COOKIE` flow works when the whole origin is a remote edge.** Specifically: the `/webrtc` viewer must have **no** dependency on a reachable WebTransport port, an embedded cert hash (the *WT* viewer embeds one at `host/src/lib.rs:442`), or a host-absolute URL. If it does, the remote topology needs a fix first. Cheap to check in portal_desktop; fatal if skipped (open Q #2).

**Cross-repo split:**
- **`aster-rpc-internal` (this repo):** the **edge-dials-host static-route variant** of `aster-expose` (control-plane inversion, §4). Shipped model is backend-registers-with-edge; the edge case needs a thin wrapper that dials a configured `host_node_id` on a static route and relays. Open Q #1 expects no new core seam — `relay_request` over an edge-opened connection already exists; confirm and add the wrapper here.
- **`portal_desktop`:** (a) expose the host salvo app (page + auth + `POST /webrtc/offer`) as an Aster `ServiceAcceptor` named `portal-webrtc` over the in-proc duplex (`ExposeNode::expose_http`); (b) the Fly edge binary (consumes the wrapper above), static route `{host_node_id, "portal-webrtc", "portal.emrul.dev"}`, keep-warm; (c) add a TURN server to `host/src/signaling.rs:62` `ice_servers` for the media fallback.

**Build order = §9 staged plan**, substituting Fly for CF in step 3. Prereqs proven: direct path (§11), real contract (§1), shipped L7 datapath (§3). The `spikes/connectivity/` prober + cargo-chef Dockerfile are a working reference for the Fly deploy shape (keep-warm machine, no inbound services, `fly certs` for the real edge).
