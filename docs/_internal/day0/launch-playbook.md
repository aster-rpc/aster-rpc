# Day 0 Launch Playbook

## The one-liner

**Aster: Identity-first RPC for machines and AI agents. No DNS, no central server. Built on QUIC.**

Variations by audience:
- Python devs: "gRPC-like RPC that works peer-to-peer. `pip install aster-rpc`, define a service, connect by address. No infra."
- AI/agent devs: "Give your AI agents authenticated RPC to remote tools. MCP-compatible, capability-based auth, works across NATs."
- Infra/security crowd: "Machine-to-machine identity without a central authority. Capability-based auth over QUIC, P2P or with relays."

---

## Hacker News

### Should you?

Yes, but with the right framing. HN is brutal on "look at my project" posts but loves "I built X because Y is broken." The key is leading with the problem, not the solution.

### When to post

- Tuesday–Thursday, 8–10am US Eastern (peak HN traffic)
- Avoid weekends, Mondays, Fridays

### Title options (pick one)

Strong:
- **"Show HN: Aster – P2P RPC with built-in machine identity (no DNS, no central server)"**
- **"Show HN: Aster – Identity-first RPC for AI agents and machines over QUIC"**

Avoid:
- "I built a better gRPC" (invites immediate dunking)
- Anything with "revolutionary" or "blazing fast"
- Feature lists in the title

### The HN comment (post immediately after submitting)

This is the most important part. HN readers go straight to the author's comment. Write it like a human, not a press release:

```
Hi HN, I'm Emrul. I've been building Aster for [timeframe].

The problem: when machine A needs to call a function on machine B,
you either set up DNS + load balancers + TLS certs, or you shove
everything through a central server. For AI agents calling remote
tools, it's even worse — there's no standard way for agents to
authenticate to each other.

Aster is an RPC framework where identity and auth are built in from
the start. You define a service (like gRPC), but connections are
peer-to-peer over QUIC. No DNS needed — you connect by a compact
address (aster1...) that encodes the endpoint identity. Auth uses
capability-based credentials — you mint a root key, enroll nodes
with specific roles, and the framework enforces access at the
connection level before your code even runs.

It works with MCP (Model Context Protocol) so AI agents can discover
and call remote tools with proper auth.

Python and TypeScript today, cross-language compatible (same wire
format). Built on iroh (iroh.computer) for the QUIC transport layer.

Happy to answer questions about the design, especially the three-gate
security model and why I think machine identity needs to be a
transport concern, not an application concern.
```

### Handling HN criticism

Expect these comments and have answers ready:

| Likely comment | Your response |
|---|---|
| "Just use mTLS + gRPC" | "That works when you control DNS and have a CA. Aster is for when you don't — edge devices, AI agents, cross-org. Identity is baked into the address, not bolted on via certs." |
| "Why not libp2p / WireGuard / Tailscale?" | "Different layer. Tailscale/WireGuard are network-level. Aster is application-level RPC with per-method authorization. You could run Aster over Tailscale if you wanted." |
| "What about NAT traversal?" | "Built on iroh's QUIC stack which handles NAT traversal via relays and STUN. Relay infrastructure from iroh.computer — we focus on the identity and RPC layer on top." |
| "Show me the threat model" | Link to Aster-trust-spec.md. This crowd respects detailed security docs. |
| "Why would I use this over HTTP?" | "You wouldn't for web APIs. This is for machine-to-machine where you don't have a web server, or for AI agents that need to call tools on remote machines." |
| "Looks overengineered" | "Fair question. The quickstart is 15 lines of Python. The complexity is in the auth layer, which is optional — you can run in open-gate mode for dev." |

### Do NOT

- Get defensive. Thank people for feedback, even harsh feedback.
- Reply to every comment. Pick the substantive ones.
- Edit the title after posting (kills momentum).
- Ask friends to upvote (HN detects and penalizes this).

---

## Reddit

### Subreddits (in priority order)

#### Tier 1 — Post on day 0

**r/Python** (~1.5M members)
- Flair: "Resource" or "Library"
- Title: "Aster: peer-to-peer RPC with built-in auth for Python. Like gRPC but no infrastructure needed."
- They care about: pip install experience, code examples, type hints, async support
- Include: quickstart code snippet (5-10 lines), link to PyPI
- Tone: practical, show-don't-tell

**r/typescript** (~200K)
- Title: "Built a P2P RPC framework with TypeScript + Python cross-language support"
- They care about: type safety, DX, bundle size, bun support
- Include: TypeScript quickstart, decorator syntax

**r/MachineLearning** (~3M) — only if you have an AI agent demo
- Title: "Authenticated RPC for AI agents — MCP-compatible, works P2P"
- They care about: what problem this solves for agents, not the implementation
- Must have: concrete AI agent use case (e.g., "agent on GPU box calls tool on data box")

#### Tier 2 — Post within first week

**r/selfhosted** (~400K)
- Angle: "no central server needed, devices talk directly"
- They love anything that reduces cloud dependency

**r/rust** (~300K)
- Angle: the iroh/QUIC transport layer, PyO3 bindings story
- Only if you want to attract Rust contributors

**r/LocalLLaMA** (~800K)
- Angle: "give your local LLM tools on remote machines"
- Very practical crowd, wants working demos

**r/networking** / **r/netsec**
- Angle: capability-based auth, QUIC transport, threat model
- Much smaller but high-signal audience

#### Tier 3 — Maybe

- r/programming (huge but generic, gets buried fast)
- r/devops (if you have a fleet management angle)
- r/homelab (subset of selfhosted, likes edge/IoT)

### Reddit post template

```
# [Title]

**What it is**: Aster is a peer-to-peer RPC framework with built-in
machine identity. Define services like gRPC, but connect directly
between machines — no DNS, no load balancer, no central server.

**Why I built it**: [1-2 sentences about the pain point]

**Quickstart** (Python):

    pip install aster-rpc

    # server.py
    from aster import service, rpc, AsterServer

    @service(name="Greeter", version=1)
    class Greeter:
        @rpc
        async def hello(self, name: str) -> dict:
            return {"message": f"Hello, {name}!"}

    server = AsterServer(services=[Greeter()])
    await server.start()
    print(server.address)  # aster1abc...
    await server.serve()

**Key features**:
- 4 RPC patterns: unary, server streaming, client streaming, bidi
- Capability-based auth (no central CA needed)
- Cross-language: Python + TypeScript on the same wire format
- MCP integration for AI agent tool calling
- Built on QUIC (iroh.computer) — handles NAT traversal

**Links**: [GitHub] | [Docs] | [PyPI]

Happy to answer questions.
```

### Reddit etiquette

- Don't post to all subreddits on the same day (looks spammy)
- Space them out: day 0, day 2, day 4
- Engage in comments for at least 2-3 hours after posting
- If a post gets traction, cross-link from others ("as discussed in r/Python...")
- Don't delete and repost if it doesn't get traction immediately

---

## Other channels

### Twitter/X
- Thread format works well. Lead with the demo GIF/video.
- Tag @n0computer (iroh team) — they may retweet
- Use #Python #TypeScript #RPC #P2P tags

### Discord / Slack communities
- Python Discord (large, active #showcase channel)
- iroh Discord (natural audience, may get maintainer attention)
- AI/LLM discords (if you have agent demos)

### Dev.to / Hashnode
- Write a "building Aster" post — the architectural decisions, not just features
- "Why I built P2P RPC with capability-based auth" style
- Cross-post to Medium only if you have followers there

### YouTube
- Even a 3-minute terminal demo goes far
- "Connect two machines with authenticated RPC in 60 seconds"
- Don't overproduceit — screen recording with voiceover is fine

---

## Sequencing

| Day | Action |
|-----|--------|
| Day 0 | Publish to PyPI + npm. Post on HN (Show HN). Post on r/Python. |
| Day 1 | Engage HN/Reddit comments. Post on Twitter/X. |
| Day 2 | Post on r/typescript. Share in Python Discord. |
| Day 3 | Post on iroh Discord. Write dev.to article. |
| Day 4 | Post on r/LocalLLaMA or r/MachineLearning (with agent demo). |
| Day 5-7 | Post on r/selfhosted, r/rust. |
| Week 2 | Follow up based on what got traction. Double down on the audience that responded. |

---

## What success looks like

Don't measure by stars or upvotes on day 0. Measure by:

- **5-10 people try the quickstart** (check PyPI download spikes)
- **2-3 substantive GitHub issues** (means someone read the code)
- **1 person asks "can I use this for X?"** where X is something you didn't think of
- **0 security vulnerabilities reported** in the first week

The first 10 users matter more than the first 1000 stars.

---

## The identity angle (long game)

For every post, plant the seed: "the hard part isn't the transport, it's the identity." When people ask "why not just use X?", bring it back to: who is this machine, what is it allowed to do, and how do you prove it without a central authority?

That's the moat. Transport is commoditized. Identity isn't.
