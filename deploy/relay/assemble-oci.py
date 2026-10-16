#!/usr/bin/env python3
"""Assemble the multi-arch aster-relay OCI image without a container daemon.

CI job containers have no Docker/BuildKit daemon, so the release image is
assembled here: the base is the runtime `FROM` of deploy/relay/Dockerfile
(pinned by index digest), plus one layer holding the iroh-relay binary built
by build-binary.sh, plus the same image config the Dockerfile declares.

    assemble-oci.py --binary linux/amd64=out/amd64 --binary linux/arm64=out/arm64 \
        --version 0.3.1012 --output dist/relay

Each --binary directory is a build-binary.sh output (iroh-relay plus
build-info.json). The output directory receives:

  aster-relay-<version>.oci.tar   OCI image layout (index.json + blobs/), tar;
                                  index.json is the multi-arch image index
  aster-relay-<version>.json      index digest, per-platform manifest digests,
                                  base digest, iroh revision and binary hashes

The layout is byte-reproducible for identical inputs: fixed timestamps,
sorted entries, gzip without a name or mtime. Uses only the standard library.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import re
import sys
import tarfile
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DOCKERFILE = Path(__file__).resolve().parent / "Dockerfile"
OCI_INDEX = "application/vnd.oci.image.index.v1+json"
OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
OCI_CONFIG = "application/vnd.oci.image.config.v1+json"
OCI_LAYER_GZIP = "application/vnd.oci.image.layer.v1.tar+gzip"
DOCKER_LIST = "application/vnd.docker.distribution.manifest.list.v2+json"
DOCKER_MANIFEST = "application/vnd.docker.distribution.manifest.v2+json"
DOCKER_LAYER_GZIP = "application/vnd.docker.image.rootfs.diff.tar.gzip"
ACCEPT = ", ".join((OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST))
EPOCH = 0  # 1970-01-01: no wall-clock time leaks into the image
BINARY_PATH = "usr/local/bin/iroh-relay"
UID = GID = 65532

# Must match the runtime stage of deploy/relay/Dockerfile.
EXPOSED = ("8080/tcp", "8443/tcp", "7842/udp", "9090/tcp")
ENTRYPOINT = ["/usr/local/bin/iroh-relay"]
CMD = ["--config-path", "/etc/iroh-relay/config.toml"]
STOP_SIGNAL = "SIGINT"


def fail(message: str) -> None:
    print(f"assemble-oci: error: {message}", file=sys.stderr)
    sys.exit(1)


def sha256(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def runtime_base() -> tuple[str, str, str]:
    """(registry, repository, index digest) from the Dockerfile runtime stage."""
    match = re.search(r"^FROM\s+([^/\s]+)/(\S+?):[^@\s]+@(sha256:[0-9a-f]{64})\s+AS\s+runtime\s*$",
                      DOCKERFILE.read_text(), re.MULTILINE)
    if not match:
        fail("Dockerfile has no digest-pinned `FROM ... AS runtime` line")
    return match.group(1), match.group(2), match.group(3)


class Registry:
    """Anonymous pull client (Docker registry v2 token flow) with digest checks."""

    def __init__(self, host: str, repository: str):
        self.base = f"https://{host}/v2/{repository}"
        self.repository = repository
        self.token: str | None = None

    def _open(self, url: str, accept: str | None = None):
        headers = {"Accept": accept} if accept else {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        try:
            return urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=120)
        except urllib.error.HTTPError as error:
            challenge = error.headers.get("WWW-Authenticate", "")
            if error.code != 401 or self.token or not challenge.startswith("Bearer "):
                raise
            fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
            query = f"service={fields.get('service', '')}&scope=repository:{self.repository}:pull"
            with urllib.request.urlopen(f"{fields['realm']}?{query}", timeout=60) as response:
                body = json.load(response)
            self.token = body.get("token") or body.get("access_token")
            return self._open(url, accept)

    def get(self, kind: str, digest: str, accept: str | None = None) -> bytes:
        with self._open(f"{self.base}/{kind}/{digest}", accept) as response:
            data = response.read()
        if sha256(data) != digest:
            fail(f"{kind} {digest} failed digest verification")
        return data


def binary_layer(binary: Path) -> tuple[bytes, str]:
    """Deterministic gzip layer holding only the relay binary; (blob, diff_id)."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for directory in ("usr", "usr/local", "usr/local/bin"):
            info = tarfile.TarInfo(directory)
            info.type, info.mode, info.mtime = tarfile.DIRTYPE, 0o755, EPOCH
            tar.addfile(info)
        info = tarfile.TarInfo(BINARY_PATH)
        data = binary.read_bytes()
        info.size, info.mode, info.mtime = len(data), 0o555, EPOCH
        tar.addfile(info, io.BytesIO(data))
    uncompressed = raw.getvalue()
    compressed = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=compressed, mtime=EPOCH, compresslevel=9) as gz:
        gz.write(uncompressed)
    return compressed.getvalue(), sha256(uncompressed)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", action="append", required=True, metavar="OS/ARCH=DIR")
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9A-Za-z][0-9A-Za-z.+_-]*", args.version):
        fail(f"invalid version {args.version!r}")

    host, repository, base_digest = runtime_base()
    registry = Registry(host, repository)
    base_index = json.loads(registry.get("manifests", base_digest, ACCEPT))
    if base_index.get("mediaType") not in (OCI_INDEX, DOCKER_LIST):
        fail("runtime base is not a multi-platform index")

    blobs: dict[str, bytes] = {}
    entries = []
    summary = {"version": args.version, "base": f"{host}/{repository}@{base_digest}", "platforms": {}}
    iroh_revs = set()
    for spec in args.binary:
        platform, _, directory = spec.partition("=")
        os_name, _, arch = platform.partition("/")
        source = Path(directory)
        info = json.loads((source / "build-info.json").read_text())
        expected_target = {"amd64": "x86_64-unknown-linux-gnu", "arm64": "aarch64-unknown-linux-gnu"}.get(arch)
        if os_name != "linux" or info.get("target") != expected_target:
            fail(f"{spec}: binary target {info.get('target')} does not match {platform}")
        binary = source / "iroh-relay"
        if sha256(binary.read_bytes()) != "sha256:" + info["binary_sha256"]:
            fail(f"{spec}: binary differs from its build-info.json")
        iroh_revs.add(info["rev"])

        candidates = [m for m in base_index["manifests"]
                      if m.get("platform", {}).get("os") == os_name and m["platform"].get("architecture") == arch]
        if len(candidates) != 1:
            fail(f"base index has {len(candidates)} manifests for {platform}")
        base_manifest = json.loads(registry.get("manifests", candidates[0]["digest"], ACCEPT))
        base_config_bytes = registry.get("blobs", base_manifest["config"]["digest"])
        base_config = json.loads(base_config_bytes)

        layers = []
        for layer in base_manifest["layers"]:
            if layer["mediaType"] not in (OCI_LAYER_GZIP, DOCKER_LAYER_GZIP):
                fail(f"unsupported base layer type {layer['mediaType']}")
            blobs[layer["digest"]] = registry.get("blobs", layer["digest"])
            layers.append({"mediaType": OCI_LAYER_GZIP, "digest": layer["digest"], "size": layer["size"]})
        layer_blob, diff_id = binary_layer(binary)
        layer_digest = sha256(layer_blob)
        blobs[layer_digest] = layer_blob
        layers.append({"mediaType": OCI_LAYER_GZIP, "digest": layer_digest, "size": len(layer_blob)})

        labels = {
            "org.opencontainers.image.title": "aster-relay",
            "org.opencontainers.image.description": "iroh-relay built from the iroh fork commit pinned by Aster",
            "org.opencontainers.image.source": "https://forge.emrul.dev/Aster/iroh",
            "org.opencontainers.image.revision": info["rev"],
            "org.opencontainers.image.version": args.version,
            "org.opencontainers.image.licenses": "MIT OR Apache-2.0",
        }
        config = dict(base_config)
        container = dict(base_config.get("config", {}))
        container.update({
            "User": f"{UID}:{GID}", "WorkingDir": "/", "Entrypoint": ENTRYPOINT, "Cmd": CMD,
            "ExposedPorts": {port: {} for port in EXPOSED}, "StopSignal": STOP_SIGNAL,
            "Labels": {**container.get("Labels", {}), **labels},
        })
        config["config"] = container
        config["created"] = "1970-01-01T00:00:00Z"
        config["rootfs"] = {"type": "layers", "diff_ids": [*base_config["rootfs"]["diff_ids"], diff_id]}
        config["history"] = [*base_config.get("history", []),
                             {"created": "1970-01-01T00:00:00Z",
                              "created_by": f"aster-relay: iroh-relay {info['rev']} ({info['target']})"}]
        config_bytes = json.dumps(config, sort_keys=True, separators=(",", ":")).encode()
        config_digest = sha256(config_bytes)
        blobs[config_digest] = config_bytes

        manifest = {"schemaVersion": 2, "mediaType": OCI_MANIFEST,
                    "config": {"mediaType": OCI_CONFIG, "digest": config_digest, "size": len(config_bytes)},
                    "layers": layers,
                    "annotations": {"org.opencontainers.image.base.name": f"{host}/{repository}",
                                    "org.opencontainers.image.base.digest": candidates[0]["digest"],
                                    "org.opencontainers.image.revision": info["rev"],
                                    "org.opencontainers.image.version": args.version}}
        manifest_bytes = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
        manifest_digest = sha256(manifest_bytes)
        blobs[manifest_digest] = manifest_bytes
        entries.append({"mediaType": OCI_MANIFEST, "digest": manifest_digest, "size": len(manifest_bytes),
                        "platform": {"os": os_name, "architecture": arch}})
        summary["platforms"][platform] = {"manifest": manifest_digest, "binary_sha256": info["binary_sha256"],
                                          "cargo_lock_sha256": info["cargo_lock_sha256"], "rustc": info["rustc"]}

    if len(iroh_revs) != 1:
        fail(f"binaries come from different iroh revisions: {sorted(iroh_revs)}")
    rev = iroh_revs.pop()
    with open(ROOT / "iroh-fork-manifest.toml", "rb") as handle:
        pinned = tomllib.load(handle)["forks"]["iroh"]["commit"]
    if rev != pinned:
        fail(f"binaries were built from {rev}, Aster pins {pinned}")

    entries.sort(key=lambda entry: (entry["platform"]["os"], entry["platform"]["architecture"]))
    index = {"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": entries,
             "annotations": {"org.opencontainers.image.revision": rev,
                             "org.opencontainers.image.version": args.version,
                             "org.opencontainers.image.ref.name": args.version}}
    index_bytes = json.dumps(index, sort_keys=True, separators=(",", ":")).encode()
    index_digest = sha256(index_bytes)
    blobs[index_digest] = index_bytes
    # The layout's index.json is the image index itself, so `crane push --index`
    # publishes exactly index_digest (a wrapper index would get another digest).
    files = {"oci-layout": json.dumps({"imageLayoutVersion": "1.0.0"}).encode(), "index.json": index_bytes}
    for digest, data in blobs.items():
        files["blobs/sha256/" + digest.removeprefix("sha256:")] = data

    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / f"aster-relay-{args.version}.oci.tar"
    with tarfile.open(archive, "w", format=tarfile.PAX_FORMAT) as tar:
        for directory in ("blobs", "blobs/sha256"):
            info = tarfile.TarInfo(directory)
            info.type, info.mode, info.mtime = tarfile.DIRTYPE, 0o755, EPOCH
            tar.addfile(info)
        for name in sorted(files):
            info = tarfile.TarInfo(name)
            info.size, info.mode, info.mtime = len(files[name]), 0o644, EPOCH
            tar.addfile(info, io.BytesIO(files[name]))
    summary.update(index_digest=index_digest, iroh_rev=rev,
                   archive=archive.name, archive_sha256=hashlib.sha256(archive.read_bytes()).hexdigest())
    (args.output / f"aster-relay-{args.version}.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    print(index_digest)


if __name__ == "__main__":
    main()
