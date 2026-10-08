"""Offline release fixtures: process concurrency and real HTTP asset transfers."""
from concurrent.futures import ThreadPoolExecutor
from email import policy
from email.parser import BytesParser
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


NPM = module("npm_safety", ROOT / "scripts/ci/npm-publish-all.py")
ASSETS = module("assets_safety", ROOT / "scripts/release/forgejo-release-assets.py")

FAKE_NPM = r'''#!/usr/bin/env python3
import fcntl,hashlib,json,os,pathlib,sys,time
a=sys.argv[1:]; registry=a[a.index('--registry')+1]
root=pathlib.Path(os.environ['FAKE_NPM_ROOT'])
with (root/'state.lock').open('a') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX)
 state=root/'state.json'; all=json.loads(state.read_text()) if state.exists() else {}
 packages=all.setdefault(registry,{})
 with (root/'calls').open('a') as calls: calls.write(json.dumps([registry,*a])+'\n')
 failure=os.environ.get('FAKE_NPM_FAILURE','')
 if a[0]=='pack':
  pkg=json.loads(pathlib.Path('package.json').read_text())
  data=json.dumps({'name':pkg['name'],'version':pkg['version'],'content':pkg.get('fixture_content','exact')},sort_keys=True).encode()
  filename=pkg['name'].replace('/','-')+'-'+pkg['version']+'.tgz'
  (pathlib.Path(a[a.index('--pack-destination')+1])/filename).write_bytes(data)
  print(json.dumps([{'filename':filename,'shasum':hashlib.sha1(data).hexdigest()}]))
 elif a[0]=='view':
  if failure=='view-auth': print(json.dumps({'error':{'code':'E401'}})); sys.exit(1)
  if a[2]=='dist-tags':
   pkg=packages.get(a[1]); value=pkg['tags'] if pkg else None
  else:
   name,version=a[1].rsplit('@',1); value=packages.get(name,{}).get('versions',{}).get(version)
  if value is None:
   print(json.dumps({'error':{'code':'E404'}})); print('launcher: package manager exited 1'); sys.exit(1)
  print(json.dumps(value))
 elif a[0]=='publish':
  assert '--ignore-scripts' in a and a[a.index('--tag')+1]=='aster-publish-pending'
  if failure=='publish-before': sys.exit(1)
  data=pathlib.Path(a[1]).read_bytes(); pkg=json.loads(data)
  p=packages.setdefault(pkg['name'],{'versions':{},'tags':{}})
  if pkg['version'] in p['versions']: sys.exit(1)
  p['versions'][pkg['version']]=hashlib.sha1(data).hexdigest()
  p['tags'][a[a.index('--tag')+1]]=pkg['version']
  state.write_text(json.dumps(all))
  if failure=='publish-after': sys.exit(1)
 elif a[0]=='dist-tag':
  if a[1]=='add':
   name,version=a[2].rsplit('@',1)
   if failure=='tag-before': sys.exit(1)
   # Widen the critical section to expose a broken promotion lock.
   time.sleep(0.1)
   assert version in packages[name]['versions']
   packages[name]['tags'][a[3]]=version
  elif a[1]=='rm': packages[a[2]]['tags'].pop(a[3],None)
  else: sys.exit(2)
 else: sys.exit(2)
 state.write_text(json.dumps(all))
'''


class NpmSafetyTests(unittest.TestCase):
    def setUp(self):
        if not shutil.which("node"):
            self.skipTest("Node is required for package stamping")
        self.temporary = tempfile.TemporaryDirectory(prefix="aster-npm-fixture-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        fake = self.bin / "npm"
        fake.write_text(FAKE_NPM)
        fake.chmod(0o755)
        self.environment = {**os.environ, "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
                            "FAKE_NPM_ROOT": str(self.root), "NODE_AUTH_TOKEN": "offline-secret-token",
                            "ASTER_NPM_LOCK_DIR": str(self.root / "locks")}

    def candidate(self, version, content="exact"):
        directory = self.root / (version + "-" + content)
        directory.mkdir(exist_ok=True)
        (directory / "package.json").write_text(json.dumps({"name": "@aster-rpc/fixture", "version": version, "fixture_content": content}))
        order = directory / "order"
        order.write_text(str(directory) + "\n")
        return order

    def run_candidate(self, order, registry="https://registry.example.invalid/", **extra):
        return subprocess.run(["bash", str(ROOT / "scripts/ci/npm-publish-all.sh"), registry, "latest", str(order)],
                              env={**self.environment, **extra}, capture_output=True, text=True, timeout=20)

    def package(self, registry="https://registry.example.invalid/"):
        return json.loads((self.root / "state.json").read_text())[registry]["@aster-rpc/fixture"]

    def calls(self):
        return [json.loads(line) for line in (self.root / "calls").read_text().splitlines()]

    def test_reversed_versions_preserve_latest_and_resume_both_registries(self):
        newer, older = self.candidate("0.3.901"), self.candidate("0.3.900")
        for registry in ("https://registry.example.invalid/", "https://registry.npmjs.org/"):
            for order in (newer, older, older, newer):
                result = self.run_candidate(order, registry)
                self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.package(registry)["tags"], {"latest": "0.3.901"})
            self.assertEqual(set(self.package(registry)["versions"]), {"0.3.900", "0.3.901"})
        self.assertEqual(len([call for call in self.calls() if call[1] == "publish"]), 4)

    def test_concurrent_promotions_keep_highest_channel(self):
        orders = [self.candidate(version) for version in ("0.3.901", "0.3.900", "0.3.902", "0.3.901")]
        with ThreadPoolExecutor(max_workers=4) as workers:
            results = list(workers.map(self.run_candidate, orders))
        for result in results:
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.package()["tags"], {"latest": "0.3.902"})
        versions = [call[3].rsplit("@", 1)[1] for call in self.calls() if call[1:3] == ["dist-tag", "add"]]
        self.assertEqual(versions, sorted(versions))
        self.assertEqual(len([call for call in self.calls() if call[1] == "publish"]), 3)

    def test_immutable_conflict_never_changes_channel(self):
        self.assertEqual(self.run_candidate(self.candidate("0.3.900")).returncode, 0)
        before = self.package()
        result = self.run_candidate(self.candidate("0.3.900", "conflicting"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("another checksum", result.stderr)
        self.assertEqual(self.package(), before)

    def test_ambiguous_publish_and_failed_channel_are_resumable(self):
        order = self.candidate("0.3.900")
        result = self.run_candidate(order, FAKE_NPM_FAILURE="publish-after")
        self.assertEqual(result.returncode, 0, result.stderr)
        order = self.candidate("0.3.901")
        result = self.run_candidate(order, FAKE_NPM_FAILURE="tag-before")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.package()["tags"], {"latest": "0.3.900"})
        result = self.run_candidate(order)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.package()["tags"], {"latest": "0.3.901"})
        self.assertEqual(len([call for call in self.calls() if call[1] == "publish"]), 2)

    def test_auth_failure_is_not_absence_and_workflow_serializes_all_tags(self):
        result = self.run_candidate(self.candidate("0.3.900"), FAKE_NPM_FAILURE="view-auth")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(call[1] == "publish" for call in self.calls()))
        self.assertNotIn("offline-secret-token", result.stdout + result.stderr)
        import yaml
        workflow = yaml.safe_load((ROOT / ".forgejo/workflows/release.yml").read_text())
        self.assertEqual(workflow["concurrency"]["group"], "forgejo-aster-promotion")
        self.assertFalse(workflow["concurrency"]["cancel-in-progress"])

    def test_semver_ordering_and_invalid_identifiers(self):
        versions = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.0.1", "1.1.0", "2.0.0"]
        self.assertEqual(sorted(versions, key=NPM.Version), versions)
        self.assertEqual(NPM.Version("1.0.0+build"), NPM.Version("1.0.0+other"))
        for invalid in ("01.0.0", "1.0.0-beta.01", "1.0.0+..", "1.0.0-.a", "latest"):
            with self.assertRaises(ValueError):
                NPM.Version(invalid)


class AssetFixture:
    def __init__(self):
        self.assets = {}
        self.calls = []
        self.uploads = []
        self.failure = ""
        self.release_exists = True
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, data, binary=False):
                data = data if binary else json.dumps(data).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                from urllib.parse import parse_qs, urlsplit, unquote
                fixture.calls.append(("GET", self.path))
                if self.path.startswith("/download/"):
                    self.reply(200, fixture.assets[unquote(self.path.removeprefix("/download/"))], True)
                elif "/releases/tags/" in self.path:
                    if fixture.failure == "lookup-auth":
                        self.reply(403, {"message": "denied"})
                    else:
                        self.reply(200 if fixture.release_exists else 404, {"id": 7, "tag_name": "v0.3.900"})
                elif "/assets" in self.path:
                    page = int(parse_qs(urlsplit(self.path).query)["page"][0])
                    listing = [{"id": index + 1, "name": name, "size": len(data), "browser_download_url": fixture.base + "/download/" + __import__("urllib.parse", fromlist=["quote"]).quote(name, safe="")}
                               for index, (name, data) in enumerate(fixture.assets.items())]
                    self.reply(200, listing[(page - 1) * 50:page * 50])
                else:
                    self.reply(404, {})

            def do_POST(self):
                from urllib.parse import parse_qs, urlsplit
                fixture.calls.append(("POST", self.path))
                data = self.rfile.read(int(self.headers["Content-Length"]))
                if self.path.endswith("/releases"):
                    fixture.release_exists = True
                    self.reply(201, {"id": 7, "tag_name": "v0.3.900"})
                    return
                name = parse_qs(urlsplit(self.path).query)["name"][0]
                message = BytesParser(policy=policy.default).parsebytes(("Content-Type: " + self.headers["Content-Type"] + "\r\n\r\n").encode() + data)
                parts = list(message.iter_parts())
                assert len(parts) == 1 and parts[0].get_param("name", header="content-disposition") == "attachment"
                uploaded = parts[0].get_payload(decode=True)
                fixture.uploads.append((name, uploaded))
                if fixture.failure != "upload-before":
                    fixture.assets[name] = uploaded + (b"corrupt" if fixture.failure == "upload-corrupt" else b"")
                self.reply(500 if fixture.failure.startswith("upload-") and fixture.failure != "upload-corrupt" else 201, {"id": 9, "name": name})

            def do_DELETE(self):
                fixture.calls.append(("DELETE", self.path))
                self.reply(500, {"message": "deletion is forbidden"})

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class ReleaseAssetSafetyTests(unittest.TestCase):
    def setUp(self):
        if not shutil.which("curl"):
            self.skipTest("curl is required for real HTTP fixture transfers")
        self.temporary = tempfile.TemporaryDirectory(prefix="aster-assets-fixture-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.fixture = AssetFixture()
        self.addCleanup(self.fixture.close)
        self.environment = {"FORGEJO_SERVER_URL": self.fixture.base, "ASTER_RELEASE_TOKEN": "offline-secret-token"}

    def file(self, name, data):
        path = self.root / name
        path.write_bytes(data)
        return path

    def attach(self, files):
        with patch.dict(os.environ, self.environment):
            ASSETS.attach("v0.3.900", "Fixture", files)

    def test_real_multipart_upload_readback_and_identical_skip(self):
        content = bytes(range(256)) * 2048
        path = self.file("native + file.bin", content)
        self.fixture.release_exists = False
        self.attach([path])
        self.attach([path])
        self.assertEqual(self.fixture.assets[path.name], content)
        self.assertEqual(self.fixture.uploads, [(path.name, content)])
        self.assertFalse(any(method == "DELETE" for method, _ in self.fixture.calls))

    def test_conflict_preflight_writes_nothing_and_preserves_existing(self):
        existing = self.file("existing.bin", b"conflicting!")
        missing = self.file("missing.bin", b"new file")
        self.fixture.assets[existing.name] = b"original file"
        before = dict(self.fixture.assets)
        with self.assertRaisesRegex(ValueError, "another checksum"):
            self.attach([missing, existing])
        self.assertEqual(self.fixture.assets, before)
        self.assertFalse(any(method in ("POST", "DELETE") for method, _ in self.fixture.calls))

    def test_failed_upload_preserves_old_assets_and_resume(self):
        old = self.file("old.bin", b"original")
        new = self.file("new.bin", b"new bytes")
        self.fixture.assets[old.name] = old.read_bytes()
        self.fixture.failure = "upload-before"
        with self.assertRaises(ValueError):
            self.attach([old, new])
        self.assertEqual(self.fixture.assets, {old.name: b"original"})
        self.fixture.failure = ""
        self.attach([old, new])
        self.assertEqual(self.fixture.assets, {old.name: b"original", new.name: b"new bytes"})
        self.assertFalse(any(method == "DELETE" for method, _ in self.fixture.calls))

    def test_lost_upload_ack_reconciles_identical_bytes(self):
        path = self.file("new.bin", b"exact candidate")
        self.fixture.failure = "upload-after"
        self.attach([path])
        self.assertEqual(self.fixture.assets[path.name], path.read_bytes())
        self.assertEqual(len(self.fixture.uploads), 1)
        self.assertFalse(any(method == "DELETE" for method, _ in self.fixture.calls))

    def test_corrupt_uploaded_bytes_fail_readback_without_deleting(self):
        path = self.file("new.bin", b"exact candidate")
        self.fixture.failure = "upload-corrupt"
        with self.assertRaisesRegex(ValueError, "another checksum"):
            self.attach([path])
        self.assertEqual(len(self.fixture.uploads), 1)
        self.assertFalse(any(method == "DELETE" for method, _ in self.fixture.calls))

    def test_paginated_existing_asset_and_auth_failure(self):
        self.fixture.assets = {f"other-{index}": b"x" for index in range(55)}
        path = self.file("last.bin", b"exact")
        self.fixture.assets[path.name] = path.read_bytes()
        self.attach([path])
        self.assertFalse(self.fixture.uploads)
        self.fixture.failure = "lookup-auth"
        with self.assertRaisesRegex(ValueError, "HTTP 403"):
            self.attach([path])
        self.assertFalse(any(method in ("POST", "DELETE") for method, _ in self.fixture.calls))


if __name__ == "__main__":
    unittest.main()
