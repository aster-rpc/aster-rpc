#!/usr/bin/env python3
"""Isolated publisher fixtures. No real registry access or publication."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


maven = load("maven", "scripts/ci/maven-release.py")
cargo = load("cargo", "scripts/release/cargo-publication-identity.py")


class Response:
    def __enter__(self):
        return self
    def __exit__(self, *args):
        pass
    def read(self):
        return b""


class RegistryIdentityTests(unittest.TestCase):
    def test_maven_clean_interrupted_resume_and_conflict(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp) / "staged"
            classifiers = Path(tmp) / "classifiers"
            directory.mkdir()
            classifiers.mkdir()
            version = "0.0.987654-fixture"
            pom = f"aster-runtime-{version}.pom"
            jar = f"aster-runtime-{version}.jar"
            (directory / pom).write_bytes(b"tested pom")
            (directory / jar).write_bytes(b"tested jar")
            for classifier in ("linux-x86_64", "macos-aarch64"):
                (classifiers / f"aster-runtime-{version}-{classifier}.jar").write_bytes(classifier.encode())
            (directory / "packages.json").write_text(json.dumps([dict(group="site.aster", artifact="aster-runtime", version=version, packaging="jar", pom=pom, file=jar)]))
            remote = {}
            writes = []
            interrupt = True
            def digest(url):
                payload = remote.get(url)
                return None if payload is None else hashlib.sha256(payload).hexdigest()
            def upload(request, timeout):
                nonlocal interrupt
                self.assertEqual(timeout, 60)
                if interrupt and len(writes) == 2:
                    raise TimeoutError("simulated interruption")
                self.assertNotIn(request.full_url, remote, "immutable artifact was resent")
                remote[request.full_url] = request.data
                writes.append(request.full_url)
                return Response()
            with patch.dict(os.environ, FORGEJO_SERVER_URL="https://fixture.invalid", ASTER_PACKAGE_TOKEN="fixture"), patch.object(maven, "remote_digest", digest), patch.object(maven, "urlopen", upload):
                with self.assertRaises(TimeoutError):
                    maven.deploy(directory, classifiers)
                self.assertEqual(len(remote), 2)
                interrupt = False
                maven.deploy(directory, classifiers)
                self.assertEqual(len(remote), 4)
                self.assertEqual(sum(url.endswith(pom) for url in writes), 1)
                maven.deploy(directory, classifiers)
                self.assertEqual(len(writes), 4)
                remote[next(iter(remote))] = b"conflicting bytes"
                with self.assertRaisesRegex(ValueError, "differs"):
                    maven.deploy(directory, classifiers)
                self.assertEqual(len(writes), 4)
                remote.clear()
                writes.clear()
                maven.deploy(directory, classifiers)
                self.assertEqual(len(writes), 4)

    def test_cargo_identity_and_visibility(self):
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "fixture.crate"
            archive.write_bytes(b"qualified archive")
            entry = json.dumps(dict(name="aster", vers="0.0.987654", cksum=hashlib.sha256(archive.read_bytes()).hexdigest(), yanked=False)).encode()
            args = ("https://fixture.invalid", "aster", "0.0.987654", archive)
            with patch.object(cargo, "read", side_effect=[archive.read_bytes(), entry]):
                self.assertTrue(cargo.verify(*args))
            with patch.object(cargo, "read", side_effect=[None, None]):
                self.assertFalse(cargo.verify(*args))
            with patch.object(cargo, "read", side_effect=[b"wrong archive", entry]):
                with self.assertRaisesRegex(ValueError, "archive differs"):
                    cargo.verify(*args)
            wrong_entry = entry.replace(hashlib.sha256(archive.read_bytes()).hexdigest().encode(), b"0" * 64)
            with patch.object(cargo, "read", side_effect=[archive.read_bytes(), wrong_entry]):
                with self.assertRaisesRegex(ValueError, "index differs"):
                    cargo.verify(*args)
            with patch.object(cargo, "read", side_effect=[archive.read_bytes(), None, archive.read_bytes(), entry]), patch.object(cargo.time, "sleep"):
                self.assertTrue(cargo.verify(*args, attempts=2))
            with patch.object(cargo, "read", side_effect=[archive.read_bytes(), None]), patch.object(cargo.time, "sleep"):
                with self.assertRaises(TimeoutError):
                    cargo.verify(*args, attempts=1)
            with patch.object(cargo, "read", side_effect=[None, None, archive.read_bytes(), entry]), patch.object(cargo.time, "sleep"):
                self.assertTrue(cargo.verify(*args, wait=True, attempts=2))


if __name__ == "__main__":
    unittest.main()
