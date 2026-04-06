# Aster Configuration

Aster uses a layered configuration system. You can configure everything from trust policy to network binding in one place, and each layer overrides the previous:

1. **Built-in defaults** -- sensible starting values (ephemeral keys, in-memory storage, all gates open).
2. **TOML config file** -- a structured file you check into your deployment config.
3. **`ASTER_*` environment variables** -- final overrides, useful for container orchestration and CI.

Later layers win. If you set `allow_all_consumers = true` in your TOML file but export `ASTER_ALLOW_ALL_CONSUMERS=false`, the environment variable takes effect.

## Quick start

The fastest way to get a config object:

```python
from aster import AsterConfig

# Reads ASTER_* env vars only (no file needed)
config = AsterConfig.from_env()
```

If you pass no config to `AsterServer`, it calls `AsterConfig.from_env()` automatically:

```python
from aster import AsterServer

# Equivalent to AsterServer(services=[...], config=AsterConfig.from_env())
async with AsterServer(services=[MyService()]) as srv:
    await srv.serve()
```

## Three ways to build an AsterConfig

### 1. From environment variables only

```python
config = AsterConfig.from_env()
```

This reads every `ASTER_*` variable listed below. If none are set, you get the built-in defaults: ephemeral identity, in-memory storage, all admission gates open (dev mode).

### 2. From a TOML file (with env overrides)

```python
config = AsterConfig.from_file("aster.toml")
```

This loads the TOML file first, then applies any `ASTER_*` environment variables on top. This is the recommended approach for production: check in the TOML file with your deployment, and override secrets (like `ASTER_SECRET_KEY`) via environment injection at runtime.

### 3. Inline (testing and scripts)

```python
config = AsterConfig(
    root_pubkey=my_pubkey_bytes,
    secret_key=my_secret_key_bytes,
    allow_all_consumers=False,
    storage_path="/var/lib/aster",
)
```

This bypasses both TOML and environment variables. Every field is set directly. Useful for tests and one-off scripts where you have all values in memory already.

## TOML file format

The configuration file has three sections: `[trust]`, `[network]`, and `[storage]`. All sections and all fields within them are optional.

```toml
[trust]
# Path to a file containing the root public key.
# Accepts either a plain 64-character hex string (32 bytes)
# or a JSON file with a "public_key" field.
root_pubkey_file = "~/.aster/root.key"

# Admission policy.
# allow_all_consumers = false means consumers must present a valid
# enrollment credential before they can call any RPC method.
allow_all_consumers = false

# allow_all_producers = true means any producer can join the mesh
# without presenting a credential. Set false in production.
allow_all_producers = true

[network]
# Base64-encoded 32-byte ed25519 secret key.
# Determines the stable node identity (EndpointId).
# If omitted, a new identity is generated each run.
# secret_key = "base64-encoded-32-bytes-here"

# Relay mode: "default" uses n0's public relay servers.
# Other options: "disabled", or a custom relay URL.
relay_mode = "default"

# Local bind address for the QUIC listener.
bind_addr = "0.0.0.0:9000"

[storage]
# Persistent storage path. If omitted, everything is in-memory
# (fine for dev, bad for production -- blobs and docs vanish on restart).
path = "/var/lib/aster"
```

### TOML field reference

| Section | Field | Type | Default | Description |
|---------|-------|------|---------|-------------|
| `[trust]` | `root_pubkey` | hex string | -- | Inline root public key (32 bytes as 64 hex chars) |
| `[trust]` | `root_pubkey_file` | path | -- | Path to file containing the root public key |
| `[trust]` | `enrollment_credential` | path | -- | Path to a JSON enrollment credential file |
| `[trust]` | `allow_all_consumers` | bool | `false` | Skip consumer admission gate |
| `[trust]` | `allow_all_producers` | bool | `true` | Skip producer admission gate |
| `[network]` | `secret_key` | base64 | -- | 32-byte node identity key |
| `[network]` | `relay_mode` | string | `"default"` | Relay configuration |
| `[network]` | `bind_addr` | string | -- | Local QUIC bind address |
| `[network]` | `enable_monitoring` | bool | `false` | Enable endpoint monitoring |
| `[network]` | `enable_hooks` | bool | `false` | Enable connection hooks (Gate 0) |
| `[network]` | `hook_timeout_ms` | int | `5000` | Timeout for hook decisions |
| `[network]` | `portmapper_config` | string | -- | Port mapper configuration (`"disabled"`, etc.) |
| `[network]` | `proxy_url` | string | -- | HTTPS proxy URL |
| `[network]` | `proxy_from_env` | bool | `false` | Read proxy from `HTTPS_PROXY` env var |
| `[storage]` | `path` | string | -- | Persistent storage directory (omit for in-memory) |

## Environment variables

Every configuration field can be set via an `ASTER_*` environment variable. These always override the TOML file.

### Trust variables

| Variable | Maps to | Example |
|----------|---------|---------|
| `ASTER_ROOT_PUBKEY` | `root_pubkey` | `ASTER_ROOT_PUBKEY=abcdef0123...` (64 hex chars) |
| `ASTER_ROOT_PUBKEY_FILE` | `root_pubkey_file` | `ASTER_ROOT_PUBKEY_FILE=~/.aster/root.key` |
| `ASTER_ENROLLMENT_CREDENTIAL` | `enrollment_credential_file` | `ASTER_ENROLLMENT_CREDENTIAL=/etc/aster/cred.json` |
| `ASTER_ALLOW_ALL_CONSUMERS` | `allow_all_consumers` | `true`, `false`, `1`, `0`, `yes`, `no` |
| `ASTER_ALLOW_ALL_PRODUCERS` | `allow_all_producers` | `true`, `false`, `1`, `0`, `yes`, `no` |

### Network variables

| Variable | Maps to | Example |
|----------|---------|---------|
| `ASTER_SECRET_KEY` | `secret_key` | Base64-encoded 32-byte key |
| `ASTER_RELAY_MODE` | `relay_mode` | `default`, `disabled` |
| `ASTER_BIND_ADDR` | `bind_addr` | `0.0.0.0:9000` |
| `ASTER_ENABLE_MONITORING` | `enable_monitoring` | `true` / `false` |
| `ASTER_ENABLE_HOOKS` | `enable_hooks` | `true` / `false` |
| `ASTER_HOOK_TIMEOUT_MS` | `hook_timeout_ms` | `5000` |
| `ASTER_PORTMAPPER_CONFIG` | `portmapper_config` | `disabled` |
| `ASTER_PROXY_URL` | `proxy_url` | `https://proxy.example.com:8080` |

### Storage variables

| Variable | Maps to | Example |
|----------|---------|---------|
| `ASTER_STORAGE_PATH` | `storage_path` | `/var/lib/aster` |

Boolean variables accept any of: `true`, `false`, `1`, `0`, `yes`, `no`, `on`, `off` (case-insensitive).

## Debugging configuration with `print_config()`

When something isn't working, use `print_config()` to see the resolved configuration and where each value came from:

```python
config = AsterConfig.from_file("aster.toml")
config.resolve_root_pubkey()  # trigger key resolution
config.print_config()
```

Output:

```
  [trust]
    root_pubkey                 : abcdef0123456789...              (aster.toml [trust])
    root_pubkey_file            : ~/.aster/root.key                (aster.toml [trust])
    enrollment_credential_file  : <not set>                        (default)
    allow_all_consumers         : False                            (ASTER_ALLOW_ALL_CONSUMERS)
    allow_all_producers         : True                             (aster.toml [trust])
  [network]
    secret_key                  : ****...a1b2c3d4                  (ASTER_SECRET_KEY)
    relay_mode                  : default                          (aster.toml [network])
    bind_addr                   : 0.0.0.0:9000                    (aster.toml [network])
    enable_monitoring           : False                            (default)
    enable_hooks                : False                            (default)
  [storage]
    path                        : /var/lib/aster                   (aster.toml [storage])
```

The provenance column (in parentheses) tells you exactly which layer set each value: `default`, the TOML file name and section, or the environment variable name.

Sensitive fields (`secret_key`, `enrollment_credential_file`) are masked in the output. The root public key is shown in full -- it is not secret.

For machine-readable output, pass `json=True`:

```python
text = config.print_config(json=True)
```

This returns (and prints) a JSON string with the same information, structured as nested objects with `"value"` and `"source"` keys for each field.

## Dev mode vs. production

### Dev mode (the default)

When you create a config with no trust settings and admission is needed, Aster generates an ephemeral root keypair on the fly:

```python
config = AsterConfig(allow_all_consumers=False)
config.resolve_root_pubkey()
# Logs: "Generated ephemeral root key (set ASTER_ROOT_PUBKEY_FILE for production)"
```

The ephemeral private key exists only in memory for the lifetime of the process. It is never written to disk. This is convenient for local development -- you can run a producer and consumer on your laptop without touching key management -- but it is not suitable for production because credentials cannot survive a restart, and no external tool can mint credentials for the node.

When both `allow_all_consumers` and `allow_all_producers` are `True` (the default), no root key is needed at all. Everything is open:

```python
# This is the default -- no trust, no keys, no admission.
config = AsterConfig()
```

### Production mode

In production, you generate a root keypair once, distribute the public key to all nodes, and sign credentials offline:

```bash
# Generate the root keypair (once, by the operator)
aster keygen root --out ~/.aster/root.key

# The file contains both keys; distribute only the public_key to nodes.
```

Then configure each node to use the root public key file:

```toml
# aster.toml (deployed to each node)
[trust]
root_pubkey_file = "/etc/aster/root_pub.key"
allow_all_consumers = false
allow_all_producers = false
```

Or via environment:

```bash
export ASTER_ROOT_PUBKEY_FILE=/etc/aster/root_pub.key
export ASTER_ALLOW_ALL_CONSUMERS=false
export ASTER_ALLOW_ALL_PRODUCERS=false
```

The root **private** key stays offline -- on the operator's workstation, in a vault, wherever your security policy puts it. It never touches a running Aster node. See the [Trust Model](trust.md) guide for the full picture.

## Passing config to AsterServer

`AsterServer` accepts the config object via its `config` keyword argument. Inline keyword arguments (`root_pubkey`, `allow_all_consumers`, `allow_all_producers`) override the config when both are provided:

```python
config = AsterConfig.from_file("aster.toml")

async with AsterServer(
    services=[MyService()],
    config=config,
    # This overrides whatever allow_all_consumers was in the config:
    allow_all_consumers=True,
) as srv:
    print(srv.endpoint_addr_b64)
    await srv.serve()
```

## EndpointConfig (low-level)

`AsterConfig` wraps the higher-level trust + storage + network concerns. Under the hood, its network fields are forwarded to an `EndpointConfig` via `config.to_endpoint_config()`. You almost never need to build an `EndpointConfig` yourself, but it is available if you need fine-grained control over the iroh endpoint:

```python
from aster import EndpointConfig

ep_config = EndpointConfig(
    alpns=[b"my-custom-protocol"],
    relay_mode="default",
    secret_key=my_key_bytes,
    bind_addr="0.0.0.0:9000",
    enable_hooks=True,
    hook_timeout_ms=3000,
)
```

There is also a standalone `load_endpoint_config()` function that reads `EndpointConfig` from a TOML file and env vars, but for most applications `AsterConfig` is the better choice since it also handles trust and storage.
