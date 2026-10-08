#!/usr/bin/env python3
"""Attach immutable, verified files to a private Forgejo release mirror."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import tempfile
import time
from urllib.parse import quote, urlencode, urljoin, urlsplit

JSON_LIMIT = 2 * 1024 * 1024


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


class Forgejo:
    def __init__(self):
        self.forge = os.environ.get("FORGEJO_SERVER_URL", "https://forge.emrul.dev").rstrip("/")
        parsed = urlsplit(self.forge)
        if parsed.username or parsed.password or parsed.query or parsed.fragment or not parsed.netloc:
            raise ValueError("invalid Forgejo server URL")
        if parsed.scheme != "https" and not (parsed.scheme == "http" and parsed.hostname in ("127.0.0.1", "localhost", "::1")):
            raise ValueError("Forgejo must use https (http is allowed only for loopback fixtures)")
        repository = os.environ.get("FORGEJO_REPOSITORY", "Aster/aster-rpc-internal")
        if not re.fullmatch(r"[A-Za-z0-9._-]+/[A-Za-z0-9._-]+", repository):
            raise ValueError("invalid Forgejo repository")
        self.api = f"{self.forge}/api/v1/repos/{repository}"
        self.token = os.environ.get("ASTER_RELEASE_TOKEN", "")
        if not self.token or len(self.token) > 16384 or any(character in self.token for character in "\r\n"):
            raise ValueError("ASTER_RELEASE_TOKEN is required and must be a single line")
        self.protocols = "=https,http" if parsed.scheme == "http" else "=https"

    def response(self, method, url, *, payload=None, file=None, limit=JSON_LIMIT, sink=None):
        timeout = 300 if file else 120 if sink else 30
        arguments = ["curl", "--config", "-", "--silent", "--show-error",
                     "--connect-timeout", "10", "--max-time", str(timeout),
                     "--proto", self.protocols, "--proto-redir", self.protocols,
                     "--max-redirs", "3", "--location", "--request", method,
                     "--write-out", "\n%{http_code}"]
        with tempfile.TemporaryDirectory(prefix="aster-release-request-") as temporary:
            if payload is not None:
                body = Path(temporary) / "body.json"
                body.write_text(json.dumps(payload))
                arguments += ["--header", "Content-Type: application/json", "--data-binary", f"@{body}"]
            if file is not None:
                escaped = str(file).replace("\\", "\\\\").replace('"', '\\"')
                arguments += ["--form", f'attachment=@"{escaped}";type=application/octet-stream']
            arguments.append(url)
            token = self.token.replace("\\", "\\\\").replace('"', '\\"')
            process = subprocess.Popen(arguments, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            selector = selectors.DefaultSelector()
            output = bytearray()
            tail = b""
            count = 0
            try:
                process.stdin.write(f'header = "Authorization: token {token}"\n'.encode())
                process.stdin.close()
                os.set_blocking(process.stdout.fileno(), False)
                selector.register(process.stdout, selectors.EVENT_READ)
                deadline = time.monotonic() + timeout + 5
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise ValueError("Forgejo transport deadline exceeded")
                    for key, _ in selector.select(min(remaining, 1)):
                        block = os.read(key.fd, 64 * 1024)
                        if not block:
                            selector.unregister(key.fileobj)
                            continue
                        count += len(block)
                        if count > limit + 4:
                            raise ValueError("Forgejo response exceeds expected size")
                        block = tail + block
                        tail = block[-4:]
                        data = block[:-4]
                        if sink:
                            sink(data)
                        else:
                            output.extend(data)
                code = process.wait(timeout=max(0.1, deadline - time.monotonic()))
                if code or not re.fullmatch(rb"\n[0-9]{3}", tail):
                    raise ValueError(f"Forgejo {method} transport failed (curl exit {code})")
                return int(tail[1:]), bytes(output)
            finally:
                selector.close()
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
                process.stdout.close()

    def json(self, method, path, *, payload=None, file=None, missing=False):
        status, data = self.response(method, self.api + path, payload=payload, file=file)
        if missing and status == 404:
            return None
        if not 200 <= status < 300:
            raise ValueError(f"Forgejo {method} failed (HTTP {status})")
        try:
            return json.loads(data)
        except ValueError as error:
            raise ValueError("Forgejo returned invalid JSON") from error

    def release(self, tag, title, body):
        path = "/releases/tags/" + quote(tag, safe="")
        release = self.json("GET", path, missing=True)
        if release is None:
            try:
                release = self.json("POST", "/releases", payload={"tag_name": tag, "name": title,
                                    "body": body, "draft": False, "prerelease": False})
            except ValueError:
                # A competing create or lost acknowledgement is recoverable
                # only if the exact tag now resolves. Other lookup errors fail.
                release = self.json("GET", path, missing=True)
        if not isinstance(release, dict) or type(release.get("id")) is not int or release["id"] <= 0 or release.get("tag_name") != tag:
            raise ValueError("could not verify the release identity")
        return release["id"]

    def assets(self, release):
        listing = {}
        for page in range(1, 21):
            batch = self.json("GET", f"/releases/{release}/assets?limit=50&page={page}")
            if not isinstance(batch, list) or len(batch) > 50:
                raise ValueError("invalid Forgejo asset listing")
            for asset in batch:
                if not isinstance(asset, dict) or type(asset.get("id")) is not int or asset["id"] <= 0 or not isinstance(asset.get("name"), str):
                    raise ValueError("invalid Forgejo asset identity")
                if asset["name"] in listing:
                    raise ValueError("duplicate Forgejo release asset names")
                listing[asset["name"]] = asset
            if len(batch) < 50:
                return listing
        raise ValueError("Forgejo release has too many assets to verify safely")

    def verify(self, asset, path, expected):
        size = path.stat().st_size
        if asset.get("size") is not None and asset["size"] != size:
            raise ValueError(f"{path.name} already exists with another checksum/size")
        url = urljoin(self.forge + "/", asset.get("browser_download_url", ""))
        if not asset.get("browser_download_url") or urlsplit(url).netloc != urlsplit(self.forge).netloc or urlsplit(url).scheme != urlsplit(self.forge).scheme:
            raise ValueError("asset download must start at the configured Forgejo origin")
        value = hashlib.sha256()
        status, _ = self.response("GET", url, limit=size, sink=value.update)
        if status != 200 or value.hexdigest() != expected:
            raise ValueError(f"{path.name} already exists with another checksum or cannot be read back")


def attach(tag, title, files, body=None):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", tag):
        raise ValueError("invalid release tag")
    candidates = {}
    for filename in files:
        path = Path(filename).resolve()
        if not path.is_file() or any(character in path.name for character in "\r\n"):
            raise ValueError("release attachments must be regular files with single-line names")
        local = digest(path)
        if path.name in candidates and candidates[path.name][1] != local:
            raise ValueError(f"conflicting local files named {path.name}")
        candidates[path.name] = (path, local)
    api = Forgejo()
    release = api.release(tag, title, body or f"{title}. Verified private mirror of release artifacts.")
    listing = api.assets(release)
    # Check every existing file before writing any missing file.
    for name, (path, local) in candidates.items():
        if name in listing:
            api.verify(listing[name], path, local)
    for name, (path, local) in candidates.items():
        if name in listing:
            print(f"identical private mirror asset {name}; skipping", flush=True)
            continue
        try:
            api.json("POST", f"/releases/{release}/assets?" + urlencode({"name": name}), file=path)
        except ValueError:
            # Never replace/delete an existing asset. A failed upload may
            # have committed; reconcile with exact bytes before accepting it.
            listing = api.assets(release)
            if name not in listing:
                raise
        listing = api.assets(release)
        if name not in listing:
            raise ValueError(f"new mirror asset {name} is missing after upload")
        api.verify(listing[name], path, local)
        print(f"verified private mirror asset {name}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--body")
    parser.add_argument("tag")
    parser.add_argument("title")
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    try:
        attach(args.tag, args.title, args.files, args.body)
    except (ValueError, OSError) as error:
        parser.exit(1, f"attach-forgejo-release: {error}\n")


if __name__ == "__main__":
    main()
