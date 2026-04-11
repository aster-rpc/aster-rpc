# Day 0 Launch Playbook

> **Last revised:** 2026-04-11. Replaces the 2026-04-09 draft after the
> 0.1.2 ship and the Mission Control QA passes that validated which
> parts of the story actually land with developers.
>
> **Tone target:** Technical founder who happens to think in categories
> and primitives, writing for other developers. Honest, hands-on, blunt.
> No marketing language. The kind of post that makes a developer think
> "this person built something serious" and an investor think "this is
> the kind of person we cold-email."

---

## The framing

The launch story is layered. Each layer is a different audience-tuned
view of the same product, written so they reinforce each other instead
of competing.

### The thesis (durable)

> **Machines need to authenticate to other machines, often on behalf
> of a user. Aster makes that safe — without a central authority and
> without shared secrets.**

This sentence is permanent. It works whether the machines are AI
agents, IoT devices, multi-tenant microservices, edge nodes, training
pipelines, signed-binary delivery systems, or things that don't have
names yet in 2028. More code now runs on more machines than people,
and the gap is widening. Someone has to authenticate all of those
machine-to-machine calls. Today the answer is "API keys, mTLS
certificates, IAM roles, service mesh sidecars, OAuth proxies" --
five different mechanisms, none of them designed for machines, all
of them bolted onto transport that doesn't care about identity.
Aster's answer is to put identity *in* the connection.

This is the line that goes in the H1 subline. It's the line that
goes in every founder bio. It's the line that closes every HN reply
when the conversation gets philosophical. The example layer below
rotates with the times; the thesis does not.

### The example (2026 vivid)

> **AI agents calling tools on remote machines. No central proxy.
> No API key rotation. Capabilities scoped per method.**

This is the launch moment's most concrete instance of the thesis.
April 2026 is the right window: MCP is the emerging standard for tool
calling, every dev tool company is racing to launch agent integrations,
and the open problem nobody solves cleanly is *"how does my agent on
machine A safely call a tool on machine B without a hosted proxy or
a shared secret."* Aster's answer is the cleanest one because the auth
model was designed for machines from the start, not retrofitted from
a human-login system.

The vivid example does the work the thesis is too abstract for. Use
it everywhere the audience is agent-curious -- HN, r/LocalLLaMA,
r/MachineLearning, the AI Discord channels, dev.to, Twitter. Pair it
with a working demo: an agent on one machine calling a real tool on
another, with a credential that took 10 seconds to mint.

When the cultural attention shifts in 2027 -- to autonomous service
fleets, multi-tenant compute, signed delivery, whatever's next -- swap
the example, keep the thesis. The example is interchangeable. The
thesis isn't.

### The proof (engineering)

> **Capability-based credentials. QUIC endpoint identity. BLAKE3
> contract hashes. Apache Fory cross-language wire format. Four-gate
> authorization. Built on iroh.**

This is the rebuttal layer. The thesis tells someone what to care
about; the example shows them what they can build; the proof tells
them this isn't a wrapper around an LLM call with a UI. Lead with the
proof when:

- A skeptic asks "is this real or is it slideware?"
- An infosec reader asks "what's your threat model?"
- A distributed-systems person asks "what does the auth actually look
  like under the hood?"
- An investor reads the README looking for "is this someone who builds
  hard things?"

Each phrase in the proof line is a credibility marker that VCs and
senior engineers both pattern-match to. None of them are buzzwords;
all of them are what was actually shipped in 0.1.2. The Aster trust
spec, the contract identity spec, and the Fory cross-language test
vectors are public -- link to them when challenged.

### The trajectory (where this goes)

> **Today: typed services, streaming, capabilities, cross-language.
> Next: identity-aware load balancing and self-healing, built from
> the same primitives.**

This is the ambition layer. **Do not claim it as a feature on the
homepage. Do not put it in HN posts.** *Do* mention it when someone
asks "what's the roadmap?" or "how does this compete with Istio +
Consul + Envoy?"

The framing matters enormously. *"We plan to add load balancing and
service discovery"* sounds like a feature roadmap. *"The substrate
keeps growing from one consistent identity model -- load balancing
across endpoints that share an identity, self-healing rooted in the
same trust topology"* sounds like a platform thesis. Same content,
different signal. The platform-thesis framing is what category-defining
founders sound like.

The reason this matters: the "no infrastructure" pitch starts as
*no DNS, no LB, no certs* (three nouns). It grows into *no DNS,
no LB, no certs, no service mesh, no sidecars, no control plane*
(six nouns -- the entire Kubernetes-network-team budget). That's the
long-term competitive surface. Don't promise it now. Don't hide it
either.

---

## Vocabulary discipline

The same words show up across HN, Reddit, Twitter, dev.to, the
homepage, the README, and every founder reply. Repetition is
consistency, not laziness. Every developer who reads two of these
will recognize the third as the same voice. Every investor who scans
them will pattern-match the *category-creation* tone before the
founder name registers.

The phrases below were chosen because they work double-duty: developers
read them as accurate technical descriptions; investors read them as
TAM, defensibility, and seriousness signals. Pick the same words every
time.

**Use:**

| Phrase | Why |
|---|---|
| *machines authenticate to machines, often on behalf of a user* | Durable thesis. Works in any decade. |
| *identity in the connection, not bolted on* | Contrarian framing. Sounds technical, signals first-principles thinking. |
| *the substrate machine fleets need* | Platform vocabulary. "Substrate" beats "framework" -- sounds foundational. |
| *capability-based credentials* | Specific. Signals familiarity with capability-security history (Mark Miller, Capsicum, SPKI). |
| *no central authority, no shared secrets* | Pain catalog. Names two things every infra team has been bitten by. |
| *built on iroh, Apache Fory, BLAKE3* | Credibility-by-association. All respected upstream projects. |
| *machine-to-machine economy* | TAM-defining language. Investors hear "trillion-dollar market." Developers hear "yes, that's the world I'm building in." |
| *the layer between transport and application that we never had a name for* | Category creation. Sounds like a thoughtful description, scans as "this person is naming a new category." |
| *primitives* (not "features") | Platform vocabulary. Suggests composition rather than a fixed feature set. |

**Avoid:**

| Phrase | Why |
|---|---|
| *enterprise-grade*, *mission-critical*, *seamless*, *cutting-edge*, *revolutionary*, *blazing fast* | Repels both developers and investors. Marketing language signals "no engineering inside." |
| *raising soon*, *open to investment*, *investors welcome*, *contact us for funding* | Repels developers (looks desperate) AND investors (looks needy). VCs want to find you, not be sold to. |
| *trusted by N teams* / fake download counts | Honesty matters more than numbers. The first 10 real users beat 1000 imagined ones. |
| *gRPC killer*, *better than X* | Invites immediate dunking. Position yourself in a new category, not as the next version of an old one. |
| *AI-powered*, *AI-first*, *next-gen AI* | Sounds like an LLM wrapper. Aster is *infrastructure for* AI agents, not an AI product. The distinction matters. |

---

## The one-liner

Pick one. The all-purpose:

> **Aster: Identity-first RPC for machines and the AI agents acting
> on their behalf. Built on QUIC. No DNS, no central authority, no
> shared secrets.**

Audience variations (use sparingly, only when the channel demands it):

- **Python devs:** *"P2P RPC with built-in machine identity. `pip
  install aster-rpc`, define a service with a decorator, connect by
  address. No infrastructure."*
- **TypeScript / JS devs:** *"Cross-language RPC where Python and
  TypeScript speak the same wire format natively -- no codegen step.
  `bun add @aster-rpc/aster`."*
- **AI agent / MCP devs:** *"Give your AI agents authenticated access
  to remote tools. Per-method capabilities, no central proxy, no API
  key rotation. MCP-compatible."*
- **Infra / security:** *"Capability-based machine identity over QUIC.
  Four-gate authorization, offline root key, ed25519 endpoint
  identity. The trust model is in the spec."*
- **Distributed systems / platform engineers:** *"The layer between
  transport and application that we never had a name for. Identity
  in the connection, BLAKE3 contract hashes, cross-language wire
  format from Apache Fory."*

---

## Hacker News

### When

Tuesday–Thursday, 8–10am US Eastern. Avoid weekends, Mondays, Fridays.

### Title options

Pick one. Do not edit after posting (kills momentum).

- **"Show HN: Aster -- P2P RPC for AI agents and machines. No DNS, no central server."**
- **"Show HN: Aster -- Identity-first RPC for the machine-to-machine economy"**
- **"Show HN: Aster -- Cross-language P2P RPC with capability-based auth. Built on QUIC."**

The first is the safest 2026 title -- it names the audience (agents +
machines), the property (P2P), and two pain points (DNS, central
server). The second leans harder on the platform thesis. The third
leans on the engineering. Pick based on which audience HN is in the
mood for that week.

### The HN comment (post immediately after submitting)

This is the most important part. HN readers go straight to the
author's comment. Write it like a human, not a press release.

```
Hi HN, I'm Emrul. I've been working on Aster for the past [N months]
full-time.

The problem I kept running into: every time I needed machine A to call
a function on machine B, I had a choice between standing up DNS + a
load balancer + TLS certificates + a CA, or shoving everything through
a hosted proxy. For AI agents calling tools on remote machines it's
even worse -- there's no standard way for an agent to prove who it is
to a service it's never met, without rotating API keys or trusting
some intermediary.

I think the next decade of machine-to-machine isn't a transport
problem. It's an identity problem. So Aster puts identity *in* the
connection rather than bolting it on top. You define a service like
gRPC -- typed methods, four streaming patterns -- but connections are
peer-to-peer over QUIC, the address is the public key, and auth uses
capability-based credentials minted from an offline root key. The
framework enforces access at the connection level before your code
runs.

The cross-language story is the part that surprised me most when I
got it working: a Python service and a TypeScript client speak the
same wire format natively, no codegen step, no IDL file. That's the
same property that made gRPC + protobuf successful, but built around
modern deployment patterns instead of datacenter ones.

Today: typed services, four streaming patterns, capability-based
auth, Python and TypeScript, MCP integration. Built on iroh for the
QUIC transport. Where this is going next: identity-aware load
balancing and self-healing built from the same primitives -- the
substrate machine fleets actually need.

Quickstart is `pip install aster-rpc` or `bun add @aster-rpc/aster`,
about a minute to a working service. Mission Control walkthrough at
[link] is the 30-minute deep dive.

Happy to answer questions about the design -- especially the
four-gate trust model, why I think machine identity needs to be a
transport concern, or the cross-language wire format work.
```

**Why this comment works:**

- *"I've been working on this for N months full-time"* -- time-investment
  marker. Signals seriousness without being a CV.
- *"The problem I kept running into"* -- problem-first framing. HN
  rewards this above all else.
- *"I think the next decade of machine-to-machine isn't a transport
  problem, it's an identity problem"* -- contrarian thesis statement.
  Sounds like a developer insight; scans as category-creation language.
- *"the substrate machine fleets actually need"* -- trajectory hint
  without overclaiming.
- Names the dependencies (iroh, MCP) -- credibility by association.
- Quickstart commands inline -- converts the curious reader.
- Closing question prompts -- invites the conversation in the
  directions you want it to go (trust model, transport-as-identity,
  cross-language).

### Handling criticism

Expect every one of these. Have answers ready.

| Likely comment | Your response |
|---|---|
| *"Just use mTLS + gRPC"* | "That works when you control DNS and have a CA. Aster is for when you don't -- edge devices, AI agents, cross-org calls. Identity is in the address itself, not bolted on via a cert. The QUIC handshake authenticates both ends using ed25519 keys, no separate TLS termination, no certificate rotation." |
| *"Why not libp2p / WireGuard / Tailscale?"* | "Different layer. Tailscale and WireGuard are network-level -- they give you a flat namespace, not authentication semantics for individual method calls. Aster is application-level RPC with per-method authorization, and you could happily run it over Tailscale if you wanted both layers." |
| *"What about NAT traversal?"* | "Built on iroh, which handles hole-punching and relay fallback transparently. Our contribution is the identity and RPC layer on top." |
| *"Show me the threat model"* | Link the trust spec. This crowd respects detailed security docs. The four-gate model is documented end-to-end. |
| *"Why not HTTP?"* | "You wouldn't for web APIs. This is for machine-to-machine where there's no web server, or for AI agents that need to call tools on remote machines without a hosted proxy in between." |
| *"Looks overengineered"* | "Fair question. The quickstart is one decorator and three lines. The trust model is optional -- open-gate mode for dev. The complexity is in what you can opt into, not what you have to use." |
| *"Yet another RPC framework"* | "Fair. Most are protobuf wrappers with a different transport. The bet I'm making with Aster is that the next decade's problem isn't 'how do I serialize a struct' -- it's 'how does this machine prove who it is to that machine.' That's not solved by another protobuf. It's solved by putting identity in the connection." |
| *"AI agent angle is hype"* | "The agent use case is the most concrete 2026 example, but the underlying problem -- machines authenticating to machines on behalf of a user -- exists whether the machines are agents or microservices or IoT devices or anything else. The engineering is identity, capability-based auth, QUIC. None of that depends on there being an LLM in the loop." |

### Do NOT

- Get defensive. Thank people for feedback, even harsh feedback.
- Reply to every comment. Pick the substantive ones. Engage with
  critics most.
- Edit the title after posting.
- Ask friends to upvote (HN detects and penalizes).
- Mention investment, fundraising, or commercial plans anywhere in
  the thread. Repels developers and investors both.

---

## Reddit

### Tier 1 — Post on day 0 or day 1

**r/LocalLLaMA** (~800K, very practical) -- *promoted from tier 2.*

The audience is sitting on a problem Aster solves directly. They run
LLMs locally because they don't want SaaS dependencies, and the open
question is "how do I let my local LLM call tools on other machines I
trust." Usually that becomes a homemade Flask server with an API key.
Show them the agent-on-machine-A-calls-tool-on-machine-B demo with
capability credentials. They will get it immediately.

- **Title:** *"Give your local LLM authenticated access to remote tools -- no central proxy, no API keys"*
- Lead with the demo. Code second.

**r/MachineLearning** (~3M) -- *promoted from tier 2 conditional to tier 1 unconditional.*

Only with a working agent demo. They care less about the implementation
and more about what problem this solves for people building with
agents. The MCP integration is the hook.

- **Title:** *"Aster: authenticated RPC for AI agents -- MCP-compatible, P2P, no central server"*

**r/Python** (~1.5M)

- **Title:** *"Aster: P2P RPC framework with built-in machine identity. `pip install aster-rpc`, no infrastructure required."*
- Lead with the quickstart. They care about pip install experience,
  type hints, async support, code clarity.
- Mention the AI agent angle as one application, not the only one.
  r/Python has agent-builders and non-agent-builders both.

**r/typescript** (~200K)

- **Title:** *"Cross-language P2P RPC: TypeScript and Python on the same wire format. No codegen step."*
- Lead with the cross-language demo. The Fory wire format story is
  the differentiator. Bun support, decorator syntax, type safety.

### Tier 2 — Post within first week

- **r/selfhosted** (~400K) -- angle: "no central server, devices talk
  directly." They love anything that reduces cloud dependency.
- **r/rust** (~300K) -- angle: the iroh + PyO3 + NAPI-RS story. Only
  if you want Rust contributors.
- **r/networking** / **r/netsec** -- angle: capability-based auth, QUIC
  transport, threat model. Smaller, higher signal.

### Tier 3 — Maybe

- r/programming (huge, generic, gets buried fast)
- r/devops (only with a fleet management angle)
- r/homelab (subset of selfhosted)

### Reddit post template

```
# [Title]

**The thesis**: machines need to authenticate to other machines, often
on behalf of a user. Today that means API keys, mTLS certificates,
IAM roles, OAuth proxies, or service mesh sidecars -- five different
mechanisms, all bolted onto transport that doesn't care about identity.
Aster puts identity in the connection itself.

**The example** (2026 version): your AI agent on one machine needs
to call a tool on another. You don't want a central proxy. You don't
want to rotate API keys. You want the agent to prove who it is, scoped
to the methods it's allowed to call. That's exactly what this does.

**Quickstart**:

    pip install aster-rpc

    from aster import service, rpc, AsterServer

    @service(name="Greeter", version=1)
    class Greeter:
        @rpc
        async def hello(self, name: str) -> dict:
            return {"message": f"Hello, {name}!"}

    server = AsterServer(services=[Greeter()])
    await server.start()
    print(server.address)  # aster1...
    await server.serve()

The consumer connects by `aster1...` address -- no DNS, no port
forwarding, no central registry. Auth in dev mode is open-gate; in
production you mint an offline root key, enroll consumers with
capabilities, and the framework enforces access before your code
runs.

**Built on**: iroh (QUIC + NAT traversal), Apache Fory (cross-language
serialization), BLAKE3 (contract identity), capability-based credentials.
The trust model is documented in the spec.

**Cross-language**: Python and TypeScript share the same wire format
natively. No codegen step, no IDL file. Java, .NET, Kotlin, Go in
progress.

**Where this is going**: identity-aware load balancing and self-healing
built from the same primitives -- the substrate machine fleets need.

[GitHub] | [Docs] | [PyPI] | [npm]

Happy to answer questions.
```

### Reddit etiquette

- Don't post to all subreddits on the same day (looks spammy).
- Space them out: day 0, day 2, day 4, day 6.
- Engage in comments for at least 2-3 hours after posting.
- If a post gets traction, cross-link from others.
- Don't delete and repost.

---

## Other channels

### Twitter / X

Thread format. Lead with a 30-second video of the agent-calling-a-tool
demo. Tag @n0computer (iroh team) -- they may retweet. Tag relevant
Anthropic / MCP people *only* if there's a real demo to anchor the
tag, not as cold mentions. Hashtags: #Python #TypeScript #RPC #P2P
#MCP #AIagents.

### Discord / Slack communities

- **Python Discord** -- large, active #showcase channel.
- **iroh Discord** -- natural audience, may get maintainer attention.
  They built the transport you're standing on; show appreciation.
- **MCP / Anthropic Discord** if you have access. The agent crowd is
  tight-knit and validates each other's tools.
- **AI/LLM Discords** -- only with a working agent demo.

### dev.to / Hashnode

Write a "Why I built Aster" post. Architectural decisions, not the
feature list. Title options:

- *"I think machine identity needs to be a transport concern, not an application concern"*
- *"What we're missing in 2026: an RPC framework where identity is in the connection"*
- *"How we got Python and TypeScript to speak the same wire format without codegen"*

Cross-post to Medium only if you have followers there.

### YouTube

A 3-minute terminal demo goes far. *"Connect two machines with
authenticated RPC in 60 seconds."* Don't overproduce -- screen
recording with voiceover is fine. The agent-calling-a-tool demo
works as a separate 2-minute video.

---

## Founder presence

The path a curious investor or potential collaborator follows is
consistent: HN/Reddit post → GitHub repo → README → website →
about page → contact. Make sure that path lands somewhere serious
instead of dead-ending. Without saying anything explicit about being
open to investment.

**What to have:**

- **README** (in the framework repo) that opens with the durable
  thesis line, then a 60-second quickstart, then a one-paragraph
  "where this is going" trajectory note. Same vocabulary as the HN
  comment. Linked from PyPI and npm package pages.
- **Website** with the same H1 subline as the README, the same install
  commands, a CTA to the Mission Control walkthrough, and a small
  *About / Why this exists* page that's a one-paragraph founder note.
  First-person. Time-investment marker. Brief mention of background.
  No CV.
- **GitHub profile** with a real bio and a real link to the website.
  Pin the aster-rpc repo at the top.
- **Twitter / X** with technical posts going back at least a few weeks.
  People check.
- **One real email address** somewhere reachable from the website
  footer. Not a contact form. Not "for investment inquiries." Just an
  email.

**What not to have:**

- An "Investors" page. Repels developers, signals desperation to
  investors.
- A "Backed by" section unless it's true.
- "Currently raising" anywhere on any page.
- A pitch deck linked from anywhere public.

The right move: write for developers honestly. Investors who
pattern-match on the vocabulary, the trajectory, and the engineering
quality will reach out on their own. The first 5 cold emails of the
launch week will tell you whether the calibration is working. If
they're from people building agent infrastructure or distributed
systems infrastructure, the framing landed. If they're from generic
"saw your project" senders, dial it sharper.

---

## Sequencing

| Day | Action |
|---|---|
| Day 0 | PyPI + npm shipped (✅ done). Post "Show HN" 8–10am ET. Post r/Python within 2 hours. |
| Day 1 | Engage HN/Reddit comments. Post Twitter/X thread. LinkedIn (one post, technical, no marketing). |
| Day 2 | Post r/typescript. Share in Python Discord. Share in iroh Discord. |
| Day 3 | Post r/LocalLLaMA. Write the dev.to article. |
| Day 4 | Post r/MachineLearning (with the agent demo as the anchor). |
| Day 5 | Post r/selfhosted. Record and post the 3-min YouTube demo. |
| Day 6–7 | Engage long-tail comments. Reply to anyone who emailed. |
| Week 2 | Follow up on what got traction. Double down on the audience that responded. |

---

## What success looks like

Don't measure by stars or upvotes on day 0. Measure by:

- **5–10 people try the quickstart** (PyPI + npm download spikes are
  the proxy).
- **2–3 substantive GitHub issues** that show someone read the code,
  not just the README.
- **1 person asks "can I use this for X?"** where X is something you
  didn't think of.
- **1–2 cold emails from people who self-describe as building agent
  infrastructure or distributed systems infrastructure.** This is the
  real signal that the framing is landing with the audience that
  matters most -- both for users and for everything that follows.
- **0 security vulnerabilities reported** in the first week.

The first 10 real users matter more than the first 1000 stars. The
first cold email from someone who *gets it* matters more than 100
upvotes.

---

## What NOT to do

- **Do not add features.** The scope is 0.1.2 plus the agent demo.
  Nothing else.
- **Do not refactor for elegance.** If it works, ship it.
- **Do not overclaim the trajectory.** Load balancing and self-healing
  are *next*, not *now*. Mention them only when asked.
- **Do not say anything about investment, fundraising, or commercial
  plans** in any public surface during launch week. Repels developers
  and investors both. The vocabulary discipline does the signaling
  silently; explicit asks break the spell.
- **Do not use marketing language.** Enterprise-grade, mission-critical,
  seamless, cutting-edge, revolutionary, blazing fast -- none of these.
  They signal "no engineering inside."
- **Do not pretend to traction you don't have.** No "trusted by N
  teams." No fake download counts. The first 10 honest users are the
  real currency.
- **Do not get into pissing matches with critics.** The HN crowd
  respects people who engage with substance and disengage from noise.
- **Do not edit the HN title.** Whatever you ship, ship.

---

## The bet

Identity is the moat. The agent example is the lever. The engineering
is the proof. The trajectory is what makes it ambitious enough to be
worth ten years of work.

The thing competitors can't easily copy is the integration: an
identity-aware substrate where the same trust model carries from
connection through method dispatch through (next) load balancing and
self-healing. Each layer reinforces the others. That's a much harder
thing to build than another RPC library, and it's structurally hard
to bolt on after the fact -- because retrofitting identity into a
transport that doesn't have it is exactly the trap most current
systems are stuck in.

The launch is for the first 10 users. Everything in this playbook is
optimized for that.
