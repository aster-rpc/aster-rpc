# Linux Aster Exec deployment

`aster-exec-server` hosts the authenticated Iroh RPC service as an unprivileged
API account. `aster-exec-helper` is a separate root-owned Unix socket service;
it starts the fixed `aster-exec-supervisor` in systemd transient units, with a
dedicated non-root payload UID. Only the `open` profile is currently supported:
network and filesystem access inherit the payload account's permissions. Do
not describe this profile as a sandbox.

Build the three binaries on Linux with systemd and unified cgroup v2:

```sh
cargo build --release -p aster-exec --features service --bins
```

Install the binaries under `/usr/local/libexec` as root, mode 0755, with every
ancestor root-owned and not writable by group or others. Create separate
`aster-exec-api` and payload accounts with no login shell. The API account must
have no sudo, polkit, container daemon, or writable executable privileges.
Install both `deploy/systemd/aster-exec-*.service` units into `/etc/systemd/system`.
Use the helper service/configuration supplied with this repository; its
`api_uid`/`api_gid` must match the API account, and its payload UID/GID and
supplementary groups must be explicitly selected by the machine owner. The
helper socket must be accessible only to that API account.

Install `/etc/aster-exec/helper.json` as root mode 0600. Replace numeric IDs
with the actual account IDs from `id`; never use UID zero for either API or
payload. Example (resource limits must match the server configuration):

```json
{
  "socket": "/run/aster-exec/helper.sock",
  "state_root": "/var/lib/aster-exec-helper",
  "supervisor": "/usr/local/libexec/aster-exec-supervisor",
  "systemd_run": "/usr/bin/systemd-run",
  "api_uid": 991, "api_gid": 991,
  "payload_uid": 992, "payload_gid": 992,
  "payload_groups": [],
  "resource_limits": {
    "cpu_millis_per_second": 1000, "memory_bytes": 536870912,
    "processes": 64, "file_bytes": 67108864
  }
}
```

The helper enforces fixed admission ceilings of 32 active jobs and 4 GiB of
reserved retained job state. It expires output after 24 hours and acknowledged
terminal metadata after seven days. Unacknowledged outcomes remain protected;
exhaustion refuses new work. The API unit caps its own memory and task count.

Provision `/var/lib/aster-exec-api/node.key` with the persistent Aster secret key
(mode 0600, owned by the API account). Obtain the corresponding public node ID
before issuing its root-signed enrollment credential and Exec grants. Preserve
this key across API restarts. A ceiling naming a different target ID prevents
startup. Never run the network API as root.

Install `/etc/aster-exec/server.json` and the credential and Docs read-ticket
files as `root:aster-exec-api`, mode 0640. The API refuses symlinks, oversized
files, non-root ownership, and permissions writable by its group. Example
configuration (replace identities, paths, accounts, and receiver):

```json
{
  "state_directory": "/var/lib/aster-exec-api",
  "bind_address": "0.0.0.0:9010",
  "root_public_key": "<64 lowercase hex characters>",
  "policy_namespace": "<64 lowercase hex characters>",
  "policy_replicas": [{"node_id": "<enrolled replica public ID>", "relay_url": null, "direct_addresses": ["192.0.2.1:9010"]}],
  "audience": "production-fleet",
  "policy_read_ticket_file": "/etc/aster-exec/policy.ticket",
  "credential_file": "/etc/aster-exec/enrollment.chain",
  "helper_socket": "/run/aster-exec/helper.sock",
  "ceiling": {
    "version": 1,
    "target_id": "<persistent node public ID>",
    "account_alias": "builder",
    "default_profile": "open",
    "enabled_profiles": ["open"],
    "enabled_operations": ["discover", "submit", "status", "output", "watch", "cancel"],
    "cwd_roots": ["/srv/build"],
    "max_timeout_ms": 1800000,
    "max_output_bytes": 16777216,
    "denied_callers": []
  },
  "resource_limits": {
    "cpu_millis_per_second": 1000,
    "memory_bytes": 536870912,
    "processes": 64,
    "file_bytes": 67108864
  },
  "audit": {
    "directory": "/var/lib/aster-exec-api/audit",
    "destination": {"endpoint": "https://audit.example.net/ingest"},
    "max_retained_bytes": 268435456,
    "max_event_bytes": 65536
  },
  "reconcile_interval_ms": 1000
}
```

Optional keys for root pre-rotation, copied from `aster trust show` on the
root operator machine: `next_root_commitment` (64 lowercase hex), `root_seq`
(rotations already applied to `root_public_key`, default 0) and
`root_rotations` (the applied rotation statements, oldest first). Without
`next_root_commitment` the server refuses every rotation statement.

Issue enrollment credentials on the root operator machine using the
configured Aster CLI profile (repeat for each client and the API node). A
credential is a delegation chain (`aster-chain-v1~...`) and every credential
expires. A long-running API node should be **persistent**: its credential
admits it until the root's next online session publishes its registration
(`aster enroll promote`, or a registration you publish directly), after which
the record is its authority. Its lifetime must cover that gap; the CLI sizes it
from the profile's `root_cadence` and `replication_allowance`:

```sh
aster enroll node --subject NODE_PUBLIC_ID --audience production-fleet \
  --root-key /secure/root.key --persistent --out enrollment.chain
```

Copy the API credential to `/etc/aster-exec/enrollment.chain` and its policy read ticket
to `/etc/aster-exec/policy.ticket`, then set the ownership/modes above. The
root key stays on the operator machine. Publish a complete `ExecGrantV1` JSON file using the selected root profile and
an owner-only namespace secret file. The node is the destination transport
identity; the JSON target is the execution target identity:

```sh
aster --profile ROOT exec grant --file grant.json --namespace NAMESPACE_HEX \
  --namespace-secret-file /secure/policy.namespace.key --audience production-fleet \
  --peer API_NODE_PUBLIC_ID
aster exec check --node API_NODE_PUBLIC_ID --target API_NODE_PUBLIC_ID
```

The selected root profile needs an existing `keys/ROOT.node.key` and its own
enrollment credential in `exec_credential_file`. This transport identity is separate
from the offline root signing key. Supply `--peer` for every current policy
replica used for publication. The command completes reciprocal enrollment and
requires successful native Docs synchronization with those replicas, and the
current root record's content, **before** reading the predecessor revision or
writing a grant. An unavailable replica, failed sync, or root deletion
tombstone stops publication. The publisher keeps no Docs state: the root author
exists only in memory for the run. After writing, it synchronizes with every
replica again and reports `"delivered": true`; if any replica did not
synchronize, the command exits with an error and the record exists nowhere
else, so rerun the same command, which replays it idempotently. Delivery does
not prove target acceptance; inspect remote readiness before submitting.
Replicas fetch the record's content after synchronizing; the publisher keeps
serving for five seconds after delivery but cannot confirm those downloads.
Before relying on a revocation, confirm a replica holds it, for example by
running `exec check` against a target after its next refresh.

Publishers for one root and namespace must be serialized operationally, and
the chosen replicas must hold current root history. Docs does not provide a
cross-machine compare-and-swap for concurrent root publishers. To revoke,
publish a newer `revoked` JSON record with `exec revoke` and the same namespace
and peer arguments. Direct low-level Docs writes bypass the CLI's predecessor
check and are not an approved grant publication path.

Set the grant's version to 1, status to `active`, revision to a monotonically
increasing positive integer, issued_at/not_before/expires_at to Unix seconds,
and account_alias/profile/operations/cwd_roots/resource ceilings explicitly.
Set `cancel_on_revocation` true. Revocation publishes `revoked` at a newer
revision. Subsequent reauthorization must name the denied revision in
`reauthorizes_revision`; restoring an older database is not reauthorization.
Until this operator provisioning and policy replication are complete, the
server is installable but cannot authorize production jobs.

Resource limits must exactly match the helper's owner-installed limits. The
ceiling and root grant must both allow each operation, profile, working
folder, timeout and output size. Publish grants under the exact-root Docs
`aster/exec/v1/grants/` key derived by `aster_exec::policy::grant_key`; use the
`ExecGrantV1` schema. Copying a grant or job ID does not grant authority.

The read ticket must name the pinned namespace. Configure at least one
reachable enrollment-enabled `policy_replicas` entry. Startup first admits a
replica through enrollment, then synchronizes its authenticated read ticket
and waits for the pinned policy replica to become locally available. It
retains its local Docs replica and refuses startup when bootstrap fails. Enrollment
is installed before RPC serving, gates ordinary ALPNs, and handles the
enrollment ALPN separately. Its credential must name this node and the pinned
root/audience. Enrollment proves membership; an Exec grant independently
permits operations. A new caller performs one bounded cold Enrollment and Docs
read; warm RPCs use the local authority snapshot without repeating those reads.
Configure background refresh between 200 ms and 2 seconds. A positive snapshot
expires after 30 seconds and then RPCs fail closed until a successful refresh;
denials are retried after one second. Enrollment revocation notifications
invalidate the snapshot immediately.

Reconciliation runs every ten seconds, and on each revocation, to cancel jobs
whose owner has lost authority and to replay terminal audit outcomes. It reads
each job owner's enrollment and grant afresh and acts on the result:

- A definite denial (a revoked or expired grant, a revoked enrollment) cancels
  the owner's running jobs on that pass.
- A caller with no enrollment session, as after an API restart, cannot make
  calls until it reconnects, and its CLI does this automatically. Its running
  jobs continue while its grant stays valid.
- If policy cannot be read (local storage, a pending Docs record, a timeout),
  calls fail with a retryable error and running jobs continue for up to five
  minutes; after that they are cancelled.

Revoke the Exec grant to stop a caller's work on a target: grant revocation
applies even to callers that have not reconnected since an API restart.
Preserve local denial watermarks when restoring state.

### Grant namespaces

By default grants live in the fleet policy namespace (`policy_namespace`). The
root can instead bind an Exec-grant namespace per role (`aster enroll namespace
bind --purpose exec_grants --read-scope ROLE ...`). A node then reads grants
from the one Exec-grant namespace bound for it; replicas hand its read ticket
only to nodes whose verified scopes include the binding's read scope. The
server refuses to start if more than one Exec-grant namespace is bound for it.
Publish grants into that namespace with `aster exec grant --namespace`.

### Clock synchronization

Enrollment and grants are judged against the local clock. `aster-exec-server`
refuses to start while the kernel reports the clock unsynchronised, and the
unit orders after `time-sync.target`. `aster exec setup` refuses as well and
names the remedy: `systemctl enable --now systemd-timesyncd`, or keep
`chronyd`. Peers whose clocks differ by more than the activation leeway
(60 seconds) are refused with a clock-skew error that reports the offset.

### Root rotation

When the root rotates, the server applies the rotation statement it learns from
an enrollment peer (or from `root_rotations` in its configuration), journals it
beside its state, and exits so systemd restarts it under the new root and fleet
namespace; no grant read under the old root stays in force. Grant state for a
rotated fleet namespace is kept in `grants-<namespace>.redb`. Republish grants
under the new root (`aster enroll republish` carries enrollment records only;
Exec grants are republished with `aster exec grant`).

### Submitting on behalf of a principal

A grant names its principal (`caller`), which may be a person's or an agent's
key. That principal can let another actor submit for it with an action chain:

```sh
aster exec delegate --subject ORCHESTRATOR_ID ...   # principal -> orchestrator
aster exec authorize --subject ACTOR_ID --resource exec:target=TARGET_ID \
  --resource exec:profile=open ...                   # -> actor
aster exec run --on-behalf-of chain.txt --principal PRINCIPAL_ID ...
```

The server verifies the chain against the principal alone, then authorizes the
job as the principal's grant narrowed by the chain's operations and resources
(`exec:target`, `exec:profile`, `exec:cwd_root`; any other resource is
refused). The actor's own grant is never used. A grant with
`require_request_digest: true` also requires the chain to bind the exact
request (`aster exec run --print-request-digest`); set it for agents. The audit
record names the principal, the actor, the chain digest and every credential
id. Revoking the principal's grant cancels the job; revoking a delegate in the
chain (`IssuerStatusV1`) stops new submissions through it.

Place API and helper state on a filesystem with an explicit project quota or
dedicated bounded volume, and alert before exhaustion. Keep enough free space
for terminal outcomes and audit retry. Outbox saturation refuses new admission.
Do not remove unresolved request reservations or unacknowledged outcomes to
recover space: archive operator-approved terminal history using the supported
retention workflow. Back up identity and policy watermarks together.

Before starting services, run the idempotent owner preflight on the Linux target:

```sh
aster exec setup --api-uid 991 --payload-uid 992 \
  --server-config /etc/aster-exec/server.json \
  --helper-config /etc/aster-exec/helper.json \
  --server-bin /usr/local/libexec/aster-exec-server \
  --helper-bin /usr/local/libexec/aster-exec-helper \
  --supervisor-bin /usr/local/libexec/aster-exec-supervisor
```

Run this as root to inspect the installed credentials; the command never invokes
sudo. It validates existing configuration, account separation, binary ownership,
node-key binding, the signed enrollment credential, the pinned read-only Docs
ticket, matching helper/server limits, and systemd/cgroup v2 prerequisites. It
changes no files and starts no services. A successful preflight reports
`configuration_preflight_passed`, with `exec_ready: false`: service startup,
policy acceptance, audit delivery and a diagnostic job remain separate checks.

```sh
systemctl daemon-reload
systemctl enable --now aster-exec-helper.service aster-exec-server.service
journalctl -u aster-exec-server.service -u aster-exec-helper.service
```

API shutdown does not terminate supervisor-owned jobs. Restart the API and
verify status/output and pending outcome delivery before reopening access after
maintenance. Host reboot survival is not promised. Capabilities report helper
unavailability without claiming execution readiness; inspect service logs and
the outbox receiver when admission fails.


## Qualification smoke

On a disposable Linux/systemd host with passwordless sudo, the repository
contains an opt-in real-stack example. It uses generated identities, a local
root-author policy replica and durable audit receiver, an API subprocess as
the invoking unprivileged UID, and the existing nobody UID 65534 for payloads.
It creates temporary helper binaries/state under `/var/lib/aster-exec-e2e-*`
and transient units; no account or permanent service is installed.

Build and run inside a dedicated bounded filesystem (for example an 8 GiB
loopback volume), with a private fixture directory:

```sh
CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  cargo build -p aster-exec --features service --bins \
  -p aster-exec-client --example linux_full_smoke
./target/debug/examples/linux_full_smoke "$(pwd)/target/debug" "$(pwd)/fixture"
```

Use a new fixture directory for every run. The example has a 150-second
execution deadline and a 180-second helper unit lifetime; jobs have explicit
60-second deadlines and 64 KiB output limits. It verifies enrollment, exact
root grant replication, submit/retry/status/output, twenty warm status calls,
actual payload UID, cancellation and durable audit delivery. Normal errors
and the internal timeout clean its tracked units and privileged state. After
an external SIGKILL or host failure, inspect the helper/job units and the
specific temporary `/var/lib/aster-exec-e2e-*` directory recorded in the
system journal before removing that owned state. Remove the private fixture
and bounded build volume after collecting results.

### Provision the CLI caller

The local broker requires an existing caller key at
`$ASTER_HOME/keys/<profile>.node.key` (`~/.aster` by default). Generate it once
with `aster key node generate --out PATH`; `aster exec whoami` prints the same
public node ID without creating a replacement key. Issue its enrollment credential
with `aster enroll node --subject CALLER_ID --audience FLEET --out /absolute/caller.chain ...`.
Keep the key and credential owned by the operator with mode 0600. Configure the selected
profile in `config.toml` before starting the broker:

```toml
[profiles.operator]
root_pubkey = "ROOT_PUBLIC_ID"
exec_credential_file = "/absolute/caller.chain"
exec_policy_namespace = "POLICY_NAMESPACE_PUBLIC_ID"
exec_audience = "FLEET"

# Optional for a target without working address discovery:
[profiles.operator.exec_direct_addresses]
"TARGET_NODE_ID" = ["192.0.2.10:4433"]
```

The credential's subject and the Exec grant caller must both equal `aster exec whoami`'s
node ID. The broker performs a real reciprocal enrollment handshake before
opening Exec RPC. It loads networking and identity from this profile rather
than inheriting `ASTER_*` networking settings from whichever process starts it.
After editing profile settings or rotating credentials, restart it with
`aster exec daemon restart` using the same `--home` and `--profile`.
A missing key fails startup instead of silently changing the caller identity.
The broker exits after ten idle minutes and writes startup errors to
`daemon.log` in its private runtime directory. If a target reports that the
caller has no enrollment session, for example after the target's API restarted,
the broker reconnects, enrolls again and retries once.

MCP exposes Exec observation tools by default. Command execution and cancellation
require separate local opt-ins:

```sh
aster mcp --allow exec-submit --allow exec-cancel
```

These tools use the selected operator identity and can execute commands or
interrupt writes. MCP submission takes an executable, arguments and working
directory; it cannot set environment overrides or stdin. Output reaches the
model as text, with invalid UTF-8 replaced; use the CLI for exact bytes. Prefer
a separate MCP profile, caller key, and narrowly scoped grant. Treat all remote
output as untrusted data, including apparent instructions.
The CLI uses bounded paginated observations; the unused unbounded streaming
`Client::watch` API is not exposed by the public Rust client.

Setup preflight checks the paths permitted by the shipped systemd units:
API state `/var/lib/aster-exec-api`, helper state `/var/lib/aster-exec-helper`,
and the helper socket directly within `/run/aster-exec`. A different layout
requires separately customized and reviewed units. Followed jobs preserve the
remote process exit code (1–255) or signal status (128 + signal). Terminal jobs
without a process status use 124 for timeout, 130 for cancellation, and 125 for
lost or failed execution. These overlap the CLI's own error codes (2–5), so
scripts that must tell a remote exit from a local failure should use `--json`:
a remote exit carries `remote_job_exit_code` in the error details.
