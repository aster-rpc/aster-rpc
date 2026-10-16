#!/usr/bin/env bash
# Publish the exact candidate relay image (deploy/relay) and record its digest.
#
#   scripts/release/publish-relay-image.sh <dist-relay-dir> <version>
#
# <dist-relay-dir> is the materialized `relay-image` candidate artifact: the
# OCI layout tar, its aster-relay-<version>.json record, Trivy reports and
# CycloneDX SBOMs. Nothing is rebuilt: the layout's index.json is pushed as
# is, and the run fails unless the registry then serves exactly the recorded
# index digest. An existing tag with another digest fails (versions are
# immutable); the same digest is an idempotent retry.
#
# Signing: with ASTER_COSIGN_KEY set (a cosign private key, PEM, password in
# ASTER_COSIGN_PASSWORD) the index is signed and each platform manifest gets
# its SBOM as a CycloneDX attestation. Nothing goes to a public transparency
# log (--tlog-upload=false): air-gapped verifiers could not reach it; they
# verify with the published cosign.pub. Without the key the image is published
# unsigned, with a warning, and the record says "signed": false.
#
# Finally the record, reports, SBOMs, OCI archive (for registry mirroring) and
# a publication record go to the generic package aster-relay/<version>.
#
# Env: ASTER_PACKAGE_TOKEN (required), ASTER_PACKAGE_USER (default agents),
#      ASTER_RELAY_IMAGE (default <forge host>/aster/aster-relay),
#      ASTER_COSIGN_KEY, ASTER_COSIGN_PASSWORD (optional), FORGEJO_SERVER_URL.
# Needs crane and cosign on PATH (deploy/relay/install-image-tools.sh).
set -euo pipefail
IFS=$'\n\t'

log() { printf '%s publish-relay-image %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
die() { log "error: $*"; exit "${2:-1}"; }

[[ $# -eq 2 ]] || die "usage: publish-relay-image.sh <dist-relay-dir> <version>" 64
dist="$1"; version="$2"
[[ "$version" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || die "invalid version '$version'" 64
: "${ASTER_PACKAGE_TOKEN:?ASTER_PACKAGE_TOKEN is required}"
user="${ASTER_PACKAGE_USER:-agents}"
forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"
host="${forge#https://}"; host="${host%%/*}"
image="${ASTER_RELAY_IMAGE:-$host/aster/aster-relay}"
repo_root="$(cd "$(dirname "$0")/../.." && pwd)"

record="$dist/aster-relay-$version.json"
archive="$dist/aster-relay-$version.oci.tar"
[[ -f "$record" && -f "$archive" ]] || die "missing $record or $archive" 66

# field <key>...: one value from the record; a dict value prints its sorted keys.
field() {
  python3 -c '
import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[key]
print("\n".join(sorted(value)) if isinstance(value, dict) else value)
' "$record" "$@"
}
index_digest="$(field index_digest)"
[[ "$index_digest" =~ ^sha256:[0-9a-f]{64}$ ]] || die "invalid index digest in record"
[[ "$(field version)" == "$version" ]] || die "record is for another version"
[[ "$(sha256sum "$archive" | cut -d' ' -f1)" == "$(field archive_sha256)" ]] \
  || die "OCI archive differs from its record"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
export DOCKER_CONFIG="$work/docker"
mkdir -p "$DOCKER_CONFIG" "$work/layout"
tar -xf "$archive" -C "$work/layout"
[[ "sha256:$(sha256sum "$work/layout/index.json" | cut -d' ' -f1)" == "$index_digest" ]] \
  || die "layout index.json is not the recorded index $index_digest"

crane auth login "$host" -u "$user" --password-stdin <<<"$ASTER_PACKAGE_TOKEN" >/dev/null

existing="$(crane digest "$image:$version" 2>/dev/null || true)"
if [[ -n "$existing" && "$existing" != "$index_digest" ]]; then
  die "$image:$version already exists as $existing, not $index_digest" 3
fi
crane push --index "$work/layout" "$image:$version" >/dev/null
served="$(crane digest "$image:$version")"
[[ "$served" == "$index_digest" ]] || die "registry serves $served for $image:$version, expected $index_digest" 4
log "published $image:$version@$index_digest"

signed=false
pubkey_sha=""
publication_files=()
if [[ -n "${ASTER_COSIGN_KEY:-}" ]]; then
  export COSIGN_PASSWORD="${ASTER_COSIGN_PASSWORD:-}"
  cosign public-key --key env://ASTER_COSIGN_KEY > "$work/cosign.pub"
  cosign sign --yes --key env://ASTER_COSIGN_KEY --tlog-upload=false "$image@$index_digest"
  cosign verify --key "$work/cosign.pub" --insecure-ignore-tlog=true "$image@$index_digest" >/dev/null
  for platform in $(field platforms); do
    arch="${platform#linux/}"
    manifest="$(field platforms "$platform" manifest)"
    sbom="$dist/sbom-linux-$arch.cdx.json"
    [[ -f "$sbom" ]] || die "missing SBOM $sbom"
    cosign attest --yes --key env://ASTER_COSIGN_KEY --tlog-upload=false \
      --type cyclonedx --predicate "$sbom" "$image@$manifest"
  done
  signed=true
  pubkey_sha="$(sha256sum "$work/cosign.pub" | cut -d' ' -f1)"
  cp "$work/cosign.pub" "$work/aster-relay-cosign.pub"
  publication_files+=("$work/aster-relay-cosign.pub")
  log "signed $image@$index_digest and attested SBOMs"
else
  log "warning: ASTER_COSIGN_KEY is not configured; $image@$index_digest is published UNSIGNED"
  echo "::warning::relay image published unsigned: configure ASTER_COSIGN_KEY (docs/relay-self-hosted.md)"
fi

python3 - "$record" "$work/aster-relay-$version.publication.json" "$image" "$version" "$signed" "$pubkey_sha" <<'PY'
import json, sys
record, out, image, version, signed, pubkey = sys.argv[1:]
r = json.load(open(record))
json.dump({"image": image, "tag": version, "index_digest": r["index_digest"],
           "reference": f'{image}@{r["index_digest"]}',
           "platforms": {p: v["manifest"] for p, v in r["platforms"].items()},
           "iroh_rev": r["iroh_rev"], "base": r["base"],
           "signed": signed == "true", "tlog": False,
           "cosign_public_key_sha256": pubkey or None},
          open(out, "w"), indent=2, sort_keys=True)
open(out, "a").write("\n")
PY
publication_files+=("$work/aster-relay-$version.publication.json")

"$repo_root/scripts/release/publish-generic-package.sh" aster-relay "$version" \
  "$dist"/* "${publication_files[@]}"
cp "$work/aster-relay-$version.publication.json" "$dist/"
log "recorded $image@$index_digest in aster-relay/$version"
