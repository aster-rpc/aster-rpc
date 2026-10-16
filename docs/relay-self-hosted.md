# Self-hosted relay

A relay forwards encrypted traffic between two Aster nodes that can't reach
each other directly, and tells nodes their observed public address over QUIC
(address discovery), which helps hole punching. By default Aster nodes use the
public n0 relays. This guide runs your own relay instead, on Kubernetes, from
the `aster-relay` image that each Aster release publishes.

You may not need one. Inside a single cluster, nodes that dial each other with
a full address (node ID plus pod IP and port) connect directly; set
`RelayMode = "disabled"` and skip the relay. See
[Running inside Kubernetes](#running-inside-kubernetes).

What's here: the image, its Kubernetes manifests, and the node settings that
point at it, including trust for a private-CA relay certificate and discovery
that never contacts n0 for air-gapped installs.

## 1. The image

| | |
|---|---|
| Image | `forge.emrul.dev/aster/aster-relay:<aster-version>`, linux/amd64 and linux/arm64 |
| Binary | `iroh-relay` built from the iroh fork commit that the same Aster release pins (`[forks.iroh] commit` in `iroh-fork-manifest.toml`), so relay and nodes run the same transport code |
| Base | `gcr.io/distroless/cc-debian13:nonroot`, pinned by digest: glibc and a CA bundle, no shell, no package manager |
| User | `65532:65532`; runs with a read-only root filesystem and no capabilities |
| Ports | 8080/tcp HTTP (captive-portal probe only), 8443/tcp HTTPS relay, 7842/udp QUIC address discovery, 9090/tcp metrics |
| Command | `iroh-relay --config-path /etc/iroh-relay/config.toml` |
| Stop signal | `SIGINT`; iroh-relay ignores `SIGTERM` when it runs as PID 1 |

Deploy it **by digest**, never by tag. Every release records the digest in
the generic package `aster-relay/<version>`:

| File | Contents |
|---|---|
| `aster-relay-<version>.publication.json` | `reference` (image@digest to deploy), `index_digest`, per-platform manifest digests, iroh revision, base image, `signed` |
| `aster-relay-<version>.json` | build record: binary and `Cargo.lock` hashes per platform, rustc version |
| `aster-relay-<version>.oci.tar` | the image itself as an OCI layout, for loading into a registry without network access |
| `sbom-linux-{amd64,arm64}.cdx.json` | CycloneDX SBOM per platform |
| `trivy-*.json` | the release's vulnerability scans (image and the relay's `Cargo.lock`) |
| `aster-relay-cosign.pub` | public key that signed this release (signed releases only) |

The `publication.json` is also attached to the Aster release on Forgejo.

### Verify the signature

Signed releases carry a cosign signature on the image index and a CycloneDX
SBOM attestation on each platform manifest. They're key-based and are not
logged to a public transparency log, so verification works offline:

```sh
cosign verify --key aster-relay-cosign.pub --insecure-ignore-tlog=true \
  forge.emrul.dev/aster/aster-relay@sha256:<index_digest>
cosign verify-attestation --key aster-relay-cosign.pub --insecure-ignore-tlog=true \
  --type cyclonedx forge.emrul.dev/aster/aster-relay@sha256:<platform-manifest-digest>
```

Check the key's SHA-256 against `cosign_public_key_sha256` in
`publication.json`. If `signed` is `false`, that release was published before
signing was configured; rely on the digest from `publication.json`.

## 2. Mirror the image into your registry

Nothing is pulled from the internet at runtime: the Deployment references the
image by digest in your own registry. Copying keeps the digest unchanged.

With network access to `forge.emrul.dev` (all platforms, plus signatures):

```sh
crane copy forge.emrul.dev/aster/aster-relay@sha256:<index_digest> \
  registry.example.internal/aster/aster-relay:<version>
cosign copy forge.emrul.dev/aster/aster-relay@sha256:<index_digest> \
  registry.example.internal/aster/aster-relay   # signatures and attestations
```

Fully offline, from the release's OCI archive:

```sh
mkdir layout && tar -xf aster-relay-<version>.oci.tar -C layout
crane push --index layout registry.example.internal/aster/aster-relay:<version>
crane digest registry.example.internal/aster/aster-relay:<version>   # must equal index_digest
```

`skopeo copy --all oci:layout docker://registry.example.internal/aster/aster-relay:<version>`
works too.

## 3. Deploy on Kubernetes

The manifests are in [`deploy/relay/k8s/`](../deploy/relay/k8s/), applied with
kustomize:

| File | What it does |
|---|---|
| `namespace.yaml` | `aster-relay` namespace, Pod Security `restricted` enforced |
| `serviceaccount.yaml` | own ServiceAccount, no API token mounted |
| `configmap.yaml` | the relay's `config.toml` (section 4) |
| `deployment.yaml` | one replica, `Recreate`; non-root, read-only root filesystem, all capabilities dropped, seccomp `RuntimeDefault`, resource requests and limits, HTTPS `/healthz` probes |
| `service.yaml` | ClusterIP: 80→8080, 443→8443, 7842/udp, 9090 |
| `networkpolicy.yaml` | ingress only from pods labelled `aster.io/relay-client: "true"` (any namespace) and metrics from namespace `monitoring`; no egress |
| `cert-manager-certificate.example.yaml` | optional cert-manager `Certificate`; not applied by default |

Steps:

1. **TLS Secret.** The relay terminates TLS itself and needs a certificate
   for the hostname the nodes dial. Create a `kubernetes.io/tls` Secret named
   `aster-relay-tls` (`tls.crt` with the leaf first, then intermediates):

   ```sh
   kubectl create namespace aster-relay
   kubectl -n aster-relay create secret tls aster-relay-tls --cert=tls.crt --key=tls.key
   ```

   The config uses `cert_mode = "Reloading"`, so a renewed certificate in the
   Secret is picked up without a restart. The certificate can come from a
   public CA or from your own (cert-manager, an internal PKI); for a private
   CA, give the nodes its certificate with `relay_ca_certs`
   ([section 5](#5-point-aster-nodes-at-the-relay)).

2. **Image.** In `kustomization.yaml`, set `newName` to your registry and
   `digest` to `index_digest` from `publication.json`.

3. **Apply.**

   ```sh
   kubectl apply -k deploy/relay/k8s
   ```

4. **Network.** Label the Aster pods that use the relay
   `aster.io/relay-client: "true"`. Under a default-deny egress policy those
   pods also need egress to the relay pods on TCP 8443 and UDP 7842 (TCP 8080
   for the optional captive-portal probe). NetworkPolicy matches pod ports,
   after Service translation:

   ```yaml
   egress:
     - to:
         - namespaceSelector:
             matchLabels:
               kubernetes.io/metadata.name: aster-relay
           podSelector:
             matchLabels:
               app.kubernetes.io/name: aster-relay
       ports:
         - {port: 8443, protocol: TCP}
         - {port: 7842, protocol: UDP}
   ```

   Nodes resolve the relay hostname, so they also need DNS egress.

5. **Reachability from outside the cluster.** Change the Service to
   `LoadBalancer` (or `NodePort`) with `externalTrafficPolicy: Local`, so
   address discovery reports each client's real source address. The QUIC
   port must stay 7842: nodes dial address discovery on that port.

**One replica per relay URL.** Two nodes reach each other through a relay only
if both are connected to the same relay process, so don't scale the
Deployment out behind one Service. For redundancy, deploy a second copy under
its own hostname and give nodes both URLs.

Check it:

```sh
kubectl -n aster-relay port-forward svc/aster-relay 8443:443 9090:9090
curl -k https://127.0.0.1:8443/healthz     # 200
curl http://127.0.0.1:9090/metrics         # Prometheus metrics
```

## 4. Configuration

`config.toml` is iroh-relay's own format. The shipped ConfigMap sets:

| Key | Value | Notes |
|---|---|---|
| `enable_relay` | `true` | `false` leaves only QUIC address discovery |
| `http_bind_addr` | `0.0.0.0:8080` | serves only `/generate_204` once TLS is on |
| `enable_quic_addr_discovery` | `true` | requires `[tls]` |
| `metrics_bind_addr` | `0.0.0.0:9090` | `enable_metrics = true` |
| `access` | `"everyone"` | the NetworkPolicy limits who can connect; `access.allowlist = ["<node id hex>", …]` admits listed nodes only; `access.http.url` asks your service per connection (needs an egress rule to it) |
| `[tls] https_bind_addr` | `0.0.0.0:8443` | relay endpoint (`/relay`), `/healthz`, `/ping` |
| `[tls] quic_bind_addr` | `0.0.0.0:7842` | UDP |
| `[tls] cert_mode` | `"Reloading"` | reads `manual_cert_path` / `manual_key_path` and re-reads them on change. Don't use `LetsEncrypt` in an air-gapped install |
| `[limits]` | `accept_conn_limit = 200.0`, `accept_conn_burst = 512` | new connections per second; `[limits.client.rx] bytes_per_second` / `max_burst_bytes` rate-limit each client |

The binds are IPv4. On a dual-stack cluster use `[::]:<port>` for each.
iroh-relay also supports `access.shared_token`; Aster nodes don't send a relay
token yet, so don't enable it.

## 5. Point Aster nodes at the relay

Set the relay mode to custom with the relay's HTTPS URL. A URL alone selects
custom mode; address discovery uses UDP 7842 on the same host.

| Path | Setting |
|---|---|
| Rust | `AsterConfig::builder().relay_urls(["https://relay.example.com"])` |
| Environment (all bindings) | `ASTER_RELAY_URLS=https://relay.example.com` (comma-separated for several) |
| `aster.toml` | `[network]` `relay_urls = ["https://relay.example.com"]` |
| Python | `AsterConfig(relay_urls=["https://relay.example.com"])` |
| Go | `cfg.RelayMode = "custom"; cfg.RelayURLs = []string{"https://relay.example.com"}` |
| TypeScript | `relayUrls: ["https://relay.example.com"]` in the `AsterConfig` |
| Java | `AsterConfig.builder().relayUrls(List.of("https://relay.example.com"))` |
| .NET | `new AsterConfig { RelayUrls = ["https://relay.example.com"] }` |

The hostname must match the relay certificate, and each node's home relay is
part of the address it publishes, so peers must be able to reach the same URL.

### A relay certificate from a private CA

Nodes verify the relay's certificate against the built-in WebPKI roots. For a
certificate from your own CA (cert-manager `ClusterIssuer`, an internal PKI),
give the nodes the CA certificate as a PEM file, for example mounted from a
ConfigMap or Secret:

| Path | Setting |
|---|---|
| Rust | `AsterConfig::builder().relay_ca_cert("/etc/aster/relay-ca.pem")` (repeatable), `.relay_ca_only(true)` |
| Environment | `ASTER_RELAY_CA_CERTS=/etc/aster/relay-ca.pem` (comma-separated for several), `ASTER_RELAY_CA_ONLY=true` |
| `aster.toml` | `[network]` `relay_ca_certs = ["/etc/aster/relay-ca.pem"]`, `relay_ca_only = true` |
| Go | `cfg.RelayCACerts = []string{"/etc/aster/relay-ca.pem"}; cfg.RelayCAOnly = true` |
| C ABI (7.3) | `aster_network_config_t.relay_ca_certs`, `.relay_ca_only`, set with `aster_config_set_network` |

The CAs are trusted in addition to the WebPKI roots; `relay_ca_only` trusts
them alone. A file that is missing, unreadable or holds no `CERTIFICATE` block
fails the node's start. Python, TypeScript, Java and .NET read these settings
in a later release.

This is transport security for the hop to the relay. It does not affect peer
identity: traffic between two nodes is end-to-end encrypted and authenticated
by their node keys, so a relay (trusted or not) cannot read it or stand in for
a node. With a relay certificate the node does not trust, the node never gets
a home relay and start-up waits for one.

## 6. Build the image yourself

[`deploy/relay/Dockerfile`](../deploy/relay/Dockerfile) builds the same image
from source with BuildKit, for operators who rebuild inside their own network.
Run it from the repository root:

```sh
rev="$(python3 -c 'import tomllib;print(tomllib.load(open("iroh-fork-manifest.toml","rb"))["forks"]["iroh"]["commit"])')"
docker buildx build -f deploy/relay/Dockerfile --build-arg IROH_REV="$rev" \
  --platform linux/amd64,linux/arm64 -t registry.example.internal/aster/aster-relay:dev .
deploy/relay/smoke-test.sh registry.example.internal/aster/aster-relay:dev
```

The builder needs network access to `forge.emrul.dev` (the iroh and noq
forks), crates.io and Debian mirrors; the resulting image needs none. The
smoke test starts the image with the shipped ConfigMap's `config.toml` and a
throwaway self-signed certificate, checks `/healthz`, `/ping`,
`/generate_204`, metrics, the served certificate, the non-root user and a
prompt stop.

Release images are not built from the Dockerfile: CI job containers have no
container daemon, so the release pipeline assembles the same runtime stage with
`deploy/relay/assemble-oci.py` around binaries from
`deploy/relay/build-binary.sh`. The base image pin lives only in the
Dockerfile; the assembler reads it from there.

## Running inside Kubernetes

Node settings for Aster pods, all on the `[network]` section of `aster.toml`
or the matching `ASTER_*` variables (Rust builder and Go field names in the
[configuration reference](aster-rust-getstarted.md#22-configuration-reference)):

```toml
[network]
bind_addr = "0.0.0.0:7842"        # fixed IPv4 UDP port   (ASTER_BIND_ADDR)
bind_addr_v6 = "[::]:7842"        # fixed IPv6 UDP port   (ASTER_BIND_ADDR_V6), dual-stack only
advertise_addrs = ["203.0.113.10:31842"]  # NodePort / LB (ASTER_ADVERTISE_ADDRS)
discovery = "none"                # never query iroh.link (ASTER_DISCOVERY)
relay_mode = "disabled"           # direct only, or relay_urls = [...] for a private relay
```

- **Bind ports.** `bind_addr` pins the socket of its own address family. On a
  dual-stack cluster, set `bind_addr_v6` too, or the IPv6 socket takes a
  random port that a default-deny NetworkPolicy won't allow. `bind_addr_v6`
  must be an IPv6 address and makes the IPv6 bind required: leave it unset on
  IPv4-only clusters. Allow both UDP ports in the NetworkPolicy.
- **Advertised addresses.** A node can't see the NodePort or load balancer in
  front of it. `advertise_addrs` publishes those `IP:port` addresses with the
  ones it discovers, and peers that find the node through discovery try them.
  They are hints only: a dial checks the node key in the TLS handshake, so an
  address that now belongs to another pod fails rather than reaching it.
  Direct IP transports must be on (`clear_ip_transports` unset).
- **Direct only, no relay.** `relay_mode = "disabled"` (or
  `clear_relay_transports = true`). Start-up doesn't wait for a relay. Peers
  can't find a node by its bare ID through a relay, so dial with a full
  address: node ID plus pod IP and port, for example from your API.
- **Private relay.** `relay_urls = ["https://relay.example.internal"]` with
  `relay_ca_certs` for a private-CA certificate
  ([section 5](#a-relay-certificate-from-a-private-ca)).
- **Discovery.** The default preset publishes to and resolves from n0's DNS
  service at `iroh.link`. In-cluster and air-gapped installs set:
  - `discovery = "none"`: no lookup service at all; dial by full address.
  - `discovery = "local"`: mDNS on the local network only (rarely useful in
    a pod network).
  - `discovery_dns_origin = "dns.example.internal"` and/or
    `discovery_pkarr_relays = ["https://dns.example.internal/pkarr"]`: your own
    iroh-dns-server (this selects `discovery = "custom"`). The pkarr relay's
    TLS uses `relay_ca_certs` too.

  With `none` or `custom` no lookup goes to n0. Relays are a separate setting:
  `relay_mode = "default"` still uses the n0 relays, so an air-gapped node
  also sets `relay_urls` or `relay_mode = "disabled"`.
- **MTU.** Leave `transport_initial_mtu` unset. QUIC starts at 1200 bytes and
  probes upward, so a 1450-byte overlay (flannel VXLAN) works.

Go: `cfg.BindAddr`, `cfg.BindAddrV6`, `cfg.AdvertiseAddrs`, `cfg.Discovery =
aster.DiscoveryNone`, `cfg.RelayMode = "disabled"` (C ABI 7.3). Python,
TypeScript, Java and .NET pick these settings up in a later release.
