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

    def test_a_lock_only_repin_of_a_sibling_is_not_stale(self):
        # A sibling pinned at an older commit that is the same source (only
        # Cargo.lock and the [patch] block differ) builds the same code.
        lock = cargo_lock(("iroh", "1", git_src("iroh", OLD_IROH)), ("noq", "1", git_src("noq", NOQ)))
        same = lambda other, rev, want: rev == want or (other == "iroh" and rev == OLD_IROH)
        found = fc.stale_findings("iroh-gossip", MANIFEST["forks"]["iroh-gossip"], MANIFEST,
                                  cargo_toml(OLD_IROH, NOQ), lock, equivalent=same)
        self.assertEqual(found, [])

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


class LockOnlyRepin(unittest.TestCase):
    """lock_only_repin over a real (temporary) git repository."""

    def setUp(self):
        import subprocess, tempfile
        self.sp = subprocess
        self.repo = tempfile.mkdtemp(prefix="fork-compat-test-")
        self.run_git("init", "-q")
        self.run_git("config", "user.email", "t@example.com")
        self.run_git("config", "user.name", "t")
        self.run_git("config", "commit.gpgsign", "false")
        self.write("src/lib.rs", "pub fn f() {}\n")
        self.write("Cargo.lock", "# lock v1\n")
        self.write("Cargo.toml", '[package]\nname = "x"\nversion = "1.0.0"\n\n'
                                 '[patch.crates-io]\niroh = { git = "u", rev = "old" }\n')
        self.base = self.commit("base")

    def run_git(self, *args):
        return self.sp.run(["git", "-C", self.repo, *args], check=True, capture_output=True, text=True).stdout

    def write(self, rel, text):
        full = os.path.join(self.repo, rel)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)

    def commit(self, msg):
        self.run_git("add", "-A")
        self.run_git("commit", "-q", "-m", msg)
        return self.run_git("rev-parse", "HEAD").strip()

    def test_lock_and_patch_block_only_is_the_same_source(self):
        self.write("Cargo.lock", "# lock v2\n")
        self.write("Cargo.toml", '[package]\nname = "x"\nversion = "1.0.0"\n\n'
                                 '[patch.crates-io]\niroh = { git = "u", rev = "new" }\n')
        repin = self.commit("re-pin")
        self.assertTrue(fc.lock_only_repin(self.repo, self.base, repin))

    def test_adding_a_patch_block_where_there_was_none_is_the_same_source(self):
        # iroh-util 0.6.3 -> 0.6.3-p1: no [patch] block before, one appended after.
        self.write("Cargo.toml", '[package]\nname = "x"\nversion = "1.0.0"\n')
        bare = self.commit("no patch block")
        self.write("Cargo.toml", '[package]\nname = "x"\nversion = "1.0.0"\n\n'
                                 '[patch.crates-io]\niroh = { git = "u", rev = "new" }\n')
        self.write("Cargo.lock", "# lock v3\n")
        added = self.commit("add patch block")
        self.assertTrue(fc.lock_only_repin(self.repo, bare, added))

    def test_a_source_change_is_not(self):
        self.write("src/lib.rs", "pub fn g() {}\n")
        changed = self.commit("source")
        self.assertFalse(fc.lock_only_repin(self.repo, self.base, changed))

    def test_a_manifest_change_outside_the_patch_block_is_not(self):
        self.write("Cargo.toml", '[package]\nname = "x"\nversion = "1.0.1"\n\n'
                                 '[patch.crates-io]\niroh = { git = "u", rev = "old" }\n')
        bumped = self.commit("version")
        self.assertFalse(fc.lock_only_repin(self.repo, self.base, bumped))

    def test_an_unknown_commit_is_not(self):
        self.assertFalse(fc.lock_only_repin(self.repo, self.base, "f" * 40))


if __name__ == "__main__":
    unittest.main()
