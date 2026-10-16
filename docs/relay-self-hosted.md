# Self-hosted relay

A relay forwards encrypted traffic between two Aster nodes that can't reach
each other directly, and tells nodes their observed public address over QUIC
(address discovery), which helps hole punching. By default Aster nodes use the
public n0 relays. This guide runs your own relay instead, on Kubernetes, from
the `aster-relay` image that each Aster release publishes.

You may not need one. Inside a single cluster, nodes that dial each other with
a full address (node ID plus pod IP and port) connect directly; set
`RelayMode = "disabled"` and skip the relay.

What's here today: the image, its Kubernetes manifests and the node settings
that point at it. A fully air-gapped install also needs two Aster features that
don't exist yet; see [Not yet supported](#not-yet-supported).

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
   Secret is picked up without a restart. Aster nodes currently accept only
   certificates that chain to the public WebPKI roots; see
   [Not yet supported](#not-yet-supported).

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

## Not yet supported

These are scheduled Aster changes; the relay side above doesn't change when
they land.

- **Private CA trust for relay TLS.** Nodes verify relay certificates against
  the built-in WebPKI roots only, so a relay whose certificate comes from a
  private CA (including the cert-manager example) is rejected, and so is a
  self-signed one. The fix adds a relay CA bundle to `AsterConfig` (a
  `[network]` key with a matching `ASTER_*` variable and builder method; name
  not final), passed to iroh's endpoint TLS roots. Until then, use a
  certificate from a public CA for the relay hostname. That works on an
  isolated network as long as the nodes can resolve the hostname, because
  verification itself needs no network access.
- **Discovery preset without `iroh.link`.** The default address lookup
  queries n0's DNS service at `iroh.link` to find a node by its bare ID. In an
  air-gapped network those lookups fail. A preset that turns this off (or
  points it at your own DNS server) will be another `AsterConfig` setting.
  Until then, dial peers with full addresses (node ID plus relay URL or
  direct IP and port).
- **Node-side network settings** from the same request, also scheduled: a
  fixed IPv6 bind port next to `[network] bind_addr` (which pins IPv4 only
  today) and an advertised external address for nodes behind a NodePort or
  load balancer. They configure nodes, not the relay.
