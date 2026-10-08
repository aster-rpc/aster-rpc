#!/usr/bin/env python3
"""Publish immutable npm versions, then advance a serialized monotonic channel.

Forgejo's workflow concurrency group serializes promotions across runners.
The local registry lock also serializes direct invocations on the same host.
"""
import argparse
from contextlib import contextmanager
import fcntl
from functools import total_ordering
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[2]
PENDING_TAG = "aster-publish-pending"


@total_ordering
class Version:
    def __init__(self, value):
        if not isinstance(value, str):
            raise ValueError("semantic version must be a string")
        match = re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?", value)
        if not match:
            raise ValueError(f"invalid semantic version: {value!r}")
        self.core = tuple(int(part) for part in match.groups()[:3])
        self.pre = match[4].split(".") if match[4] else []
        if match[5] and any(not part for part in match[5].split(".")):
            raise ValueError(f"invalid semantic version: {value!r}")
        if any(not part or (part.isdigit() and len(part) > 1 and part[0] == "0") for part in self.pre):
            raise ValueError(f"invalid semantic version: {value!r}")

    def __eq__(self, other):
        return self.core == other.core and self.pre == other.pre

    def __lt__(self, other):
        if self.core != other.core:
            return self.core < other.core
        if not self.pre or not other.pre:
            return bool(self.pre) and not other.pre
        for left, right in zip(self.pre, other.pre):
            if left == right:
                continue
            if left.isdigit() and right.isdigit():
                return int(left) < int(right)
            if left.isdigit() != right.isdigit():
                return left.isdigit()
            return left < right
        return len(self.pre) < len(other.pre)


def budget(name, default, maximum):
    value = float(os.environ.get(name, default))
    if not 0 < value <= maximum:
        raise ValueError(f"{name} must be positive and at most {maximum}s")
    return value


def run(arguments, *, cwd=None, required=True):
    try:
        result = subprocess.run(arguments, cwd=cwd, capture_output=True, text=True,
                                timeout=budget("ASTER_NPM_COMMAND_TIMEOUT", 180, 600))
    except subprocess.TimeoutExpired as error:
        raise ValueError(f"{arguments[0]} command timed out") from error
    if required and result.returncode:
        # npm may include registry diagnostics. Never reproduce the token.
        diagnostic = (result.stderr or result.stdout).replace(os.environ.get("NODE_AUTH_TOKEN", ""), "[redacted]")
        raise ValueError(f"{arguments[0]} command failed: {diagnostic[:2000]}")
    return result


def npm(arguments, registry, *, cwd=None, required=True):
    return run(["npm", *arguments, "--registry", registry, "--fetch-timeout=30000",
                "--fetch-retries=2", "--fetch-retry-mintimeout=1000", "--fetch-retry-maxtimeout=5000"],
               cwd=cwd, required=required)


def decode(output):
    # Some launchers append a diagnostic after the first npm JSON response.
    return json.JSONDecoder().raw_decode(output.lstrip())[0]


def view(spec, field, registry):
    result = npm(["view", spec, field, "--json"], registry, required=False)
    try:
        value = decode(result.stdout)
    except (ValueError, TypeError) as error:
        raise ValueError(f"could not verify {spec} ({field})") from error
    if result.returncode:
        if isinstance(value, dict) and value.get("error", {}).get("code") == "E404":
            return None
        raise ValueError(f"could not verify {spec} ({field}); registry request failed")
    return value


@contextmanager
def registry_lock(registry):
    directory = Path(os.environ.get("ASTER_NPM_LOCK_DIR", str(Path(tempfile.gettempdir()) / f"aster-npm-promotions-{os.getuid()}")))
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    if directory.is_symlink() or directory.stat().st_uid != os.getuid():
        raise ValueError("npm lock directory must be owned by the current user and not a symlink")
    filename = directory / (hashlib.sha256(registry.encode()).hexdigest() + ".lock")
    descriptor = os.open(filename, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        deadline = time.monotonic() + budget("ASTER_NPM_LOCK_TIMEOUT", 300, 3600)
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise ValueError("timed out waiting for npm registry promotion lock")
                time.sleep(0.05)
        yield
    finally:
        os.close(descriptor)


def publish(registry, channel, order_file):
    parsed = urlsplit(registry)
    if parsed.scheme != "https" or not parsed.netloc or parsed.username or parsed.password or parsed.query or parsed.fragment or not registry.endswith("/"):
        raise ValueError("registry must be an https URL ending in / without credentials or query")
    if not re.fullmatch(r"[a-z][a-z0-9-]*", channel) or channel == PENDING_TAG:
        raise ValueError("invalid or reserved dist-tag")
    if not os.environ.get("NODE_AUTH_TOKEN"):
        raise ValueError("NODE_AUTH_TOKEN is required")
    packages = []
    for line in Path(order_file).read_text().splitlines():
        if not line:
            continue
        directory = Path(line).resolve()
        metadata = json.loads((directory / "package.json").read_text())
        name, version = metadata["name"], metadata["version"]
        if not isinstance(name, str) or not re.fullmatch(r"(?:@[a-z0-9._-]+/)?[a-z0-9._-]+", name):
            raise ValueError("invalid package name")
        Version(version)
        packages.append((directory, name, version))
    if not packages or len({version for _, _, version in packages}) != 1 or len({name for _, name, _ in packages}) != len(packages):
        raise ValueError("order must contain distinct packages at one candidate version")

    with registry_lock(registry), tempfile.TemporaryDirectory(prefix="aster-npm-") as temporary:
        temporary = Path(temporary)
        npmrc = temporary / "npmrc"
        npmrc.write_text(f"@aster-rpc:registry={registry}\n{registry.removeprefix('https:')}:_authToken=${{NODE_AUTH_TOKEN}}\n")
        npmrc.chmod(0o600)
        previous_config = os.environ.get("NPM_CONFIG_USERCONFIG")
        os.environ["NPM_CONFIG_USERCONFIG"] = str(npmrc)
        try:
            for directory, name, version in packages:
                run(["node", str(ROOT / "scripts/ci/stamp-npm-package.mjs"), str(directory / "package.json"), version, registry])
                pack = decode(npm(["pack", "--ignore-scripts", "--json", "--pack-destination", str(temporary)], registry, cwd=directory).stdout)[0]
                archive = temporary / pack["filename"]
                if archive.parent != temporary or not archive.is_file():
                    raise ValueError("npm pack returned an invalid archive path")
                value = hashlib.sha1()
                with archive.open("rb") as stream:
                    for block in iter(lambda: stream.read(1024 * 1024), b""):
                        value.update(block)
                digest = value.hexdigest()
                if digest != pack["shasum"]:
                    raise ValueError("npm pack checksum mismatch")
                remote = view(f"{name}@{version}", "dist.shasum", registry)
                if remote is not None and remote != digest:
                    raise ValueError(f"immutable {name}@{version} already exists with another checksum")
                if remote is None:
                    # Publishing must never implicitly change latest. This one
                    # reserved staging tag is removed after immutable verification.
                    result = npm(["publish", str(archive), "--access", "public", "--tag", PENDING_TAG, "--ignore-scripts"], registry, required=False)
                    if view(f"{name}@{version}", "dist.shasum", registry) != digest:
                        raise ValueError(f"publication checksum verification failed for {name}@{version} (exit {result.returncode})")
                print(f"verified immutable {name}@{version}", flush=True)

            # Validate every channel before changing any. Equal and older
            # candidate versions are resumable without moving a channel backward.
            changes = []
            for _, name, version in packages:
                tags = view(name, "dist-tags", registry)
                if not isinstance(tags, dict):
                    raise ValueError(f"missing or invalid dist-tags for {name}")
                current = tags.get(channel)
                if current is not None and not isinstance(current, str):
                    raise ValueError(f"invalid {channel} version for {name}")
                if current is None or Version(current) < Version(version):
                    changes.append((name, version))
                else:
                    print(f"preserved {name} {channel}={current}; candidate={version}", flush=True)
                if tags.get(PENDING_TAG) == version:
                    npm(["dist-tag", "rm", name, PENDING_TAG], registry)
            for name, version in changes:
                npm(["dist-tag", "add", f"{name}@{version}", channel], registry)
                tags = view(name, "dist-tags", registry)
                if not isinstance(tags, dict) or tags.get(channel) != version:
                    raise ValueError(f"channel readback failed for {name} {channel}")
                print(f"advanced {name} {channel}={version}", flush=True)
        finally:
            if previous_config is None:
                os.environ.pop("NPM_CONFIG_USERCONFIG", None)
            else:
                os.environ["NPM_CONFIG_USERCONFIG"] = previous_config


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("registry")
    parser.add_argument("channel")
    parser.add_argument("order_file")
    args = parser.parse_args()
    try:
        publish(args.registry, args.channel, args.order_file)
    except (ValueError, OSError, KeyError) as error:
        parser.exit(1, f"npm-publish-all: {error}\n")


if __name__ == "__main__":
    main()
