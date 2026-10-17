"""Guards for scripts/ci/fork_compat.py's manifest logic (no fork checkouts
needed): which forks a manifest change affects, and what counts as stale."""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("fork_compat", os.path.join(HERE, "fork_compat.py"))
fc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fc)

NOQ = "e" * 40
IROH = "a" * 40
OLD_IROH = "1" * 40
MANIFEST = {
    "forks": {
        "noq": {"commit": NOQ, "tag": "aster-noq-v1", "crates": ["noq", "noq-proto"], "crate_version": "1"},
        "iroh": {"commit": IROH, "tag": "aster-iroh-v1", "crates": ["iroh"],
                 "registry_crates": ["iroh", "iroh-dns"], "crate_version": "1"},
        "iroh-gossip": {"commit": "g" * 40, "tag": "aster-iroh-gossip-v1", "crates": ["iroh-gossip"],
                        "crate_version": "1"},
    }
}
GIT = "https://forge.emrul.dev/Aster"


def cargo_toml(iroh_rev, noq_rev):
    return f"""[package]
name = "iroh-gossip"

[dependencies]
iroh = "1"

[patch.crates-io]
iroh = {{ git = "{GIT}/iroh.git", rev = "{iroh_rev}" }}
noq = {{ git = "{GIT}/noq.git", rev = "{noq_rev}" }}
"""


def cargo_lock(*pkgs):
    out = "version = 4\n"
    for name, version, source in pkgs:
        out += f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
        if source:
            out += f'source = "{source}"\n'
    return out


def git_src(repo, sha):
    return f"git+{GIT}/{repo}.git?rev={sha}#{sha}"


class Stale(unittest.TestCase):
    def findings(self, toml, lock):
        return fc.stale_findings("iroh-gossip", MANIFEST["forks"]["iroh-gossip"], MANIFEST, toml, lock)

    def test_matching_patch_block_and_lock(self):
        lock = cargo_lock(("iroh-gossip", "1", None), ("iroh", "1", git_src("iroh", IROH)),
                          ("noq", "1", git_src("noq", NOQ)), ("serde", "1", "registry+https://x"))
        self.assertEqual(self.findings(cargo_toml(IROH, NOQ), lock), [])

    def test_patch_block_pinned_to_an_older_fork(self):
        lock = cargo_lock(("iroh", "1", git_src("iroh", OLD_IROH)), ("noq", "1", git_src("noq", NOQ)))
        found = self.findings(cargo_toml(OLD_IROH, NOQ), lock)
        self.assertTrue(any(f.startswith("patch iroh ->") for f in found), found)
        self.assertTrue(any(f.startswith("Cargo.lock iroh ") for f in found), found)

    def test_forked_crate_taken_from_crates_io(self):
        # TM-7: iroh-docs' lock took iroh-gossip and iroh-util from crates.io.
        lock = cargo_lock(("iroh", "1", git_src("iroh", IROH)), ("noq", "1", git_src("noq", NOQ)),
                          ("iroh-dns", "1", "registry+https://github.com/rust-lang/crates.io-index"))
        found = self.findings(cargo_toml(IROH, NOQ), lock)
        self.assertEqual(len(found), 1, found)
        self.assertIn("iroh-dns", found[0])

    def test_no_patch_block(self):
        self.assertEqual(fc.patch_block("[package]\nname = 'x'\n"), {})


class Affected(unittest.TestCase):
    def test_changed_forks(self):
        moved = {"forks": {k: dict(v) for k, v in MANIFEST["forks"].items()}}
        moved["forks"]["noq"]["commit"] = "f" * 40
        self.assertEqual(fc.changed_forks(MANIFEST, moved), {"noq"})
        self.assertEqual(fc.changed_forks(MANIFEST, MANIFEST), set())
        self.assertEqual(fc.changed_forks({"forks": {}}, MANIFEST), set(MANIFEST["forks"]))

    def test_dependents_from_the_patch_block(self):
        self.assertEqual(fc.depends_on(cargo_toml(IROH, NOQ), MANIFEST), {"iroh", "noq"})


class Suites(unittest.TestCase):
    def test_every_suite_is_locked(self):
        # A suite that may re-resolve tests a stack nobody pinned.
        for name, cmds in fc.SUITES.items():
            for cmd in cmds:
                self.assertIn("--locked", cmd, name)


if __name__ == "__main__":
    unittest.main()
