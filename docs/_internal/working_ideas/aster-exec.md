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
- **Resource limits via the OS, not custom code**: wrap with
  `systemd-run --scope` (same "the node OS is the kubelet" stance as the
  orchestrator doc). Optional, role-gated.
- **Roles are scoped narrow by default**: exec roles name allowed users,
  path prefixes, pty/shell capability, and signal rights — directory rows,
  evaluated live.

## Calibrating "a little work"

Already exists: transport, identity, NAT traversal, bidi streaming, the PTY
surface, the directory authz model, the CAS. New work is **one crate + one
adapter**: the Exec contract, a job supervisor with bounded buffers
(the `SyncSupervisor` pattern — a job is a run-once workload), deadline +
signal plumbing, role gating, and the MCP server. Honest hard 20%: output
buffer bounds and replay semantics, stdin-wait detection portability, and
Windows (easier than the shell — exec mode needs no PTY).

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
execution with mesh identity, directory authorization, and content-addressed
file transfer — consumable by agents as tools.*
