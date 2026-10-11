"""Offline fixtures only: source repos, filtered clones and a simulated bare public remote."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "public_mirror", Path(__file__).with_name("prepare-public-mirror.py")
)
MIRROR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MIRROR)
POLICY = """docs/_internal/
*.key
*.cred
.aster-identity
AGENTS.md
.publicignore
.github/workflows/
.forgejo/
.githooks/
"""


class PublicMirrorTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="aster-public-fixture-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "private"
        self.source.mkdir()
        MIRROR.git(self.source, "init", "-b", "main")
        MIRROR.git(self.source, "config", "user.name", "Fixture")
        MIRROR.git(self.source, "config", "user.email", "fixture@example.invalid")
        MIRROR.git(self.source, "config", "commit.gpgsign", "false")
        MIRROR.git(self.source, "config", "tag.gpgsign", "false")
        self.write("VERSION", "0.3 1\n")
        self.write(".publicignore", POLICY)
        self.write("README.md", "intended public content\n")
        self.write("bindings/go/go.mod", "module getaster.now/aster-rpc/go\n\ngo 1.25.0\n")
        self.write("bindings/java/Api.java", "public Java fixture\n")
        self.write("bindings/dotnet/Api.cs", "public dotnet fixture\n")
        self.write("ffi/aster_ffi.h", "public native fixture\n")
        self.write("docs/_internal/first.txt", "PRIVATE_SENTINEL first\n")
        self.write(".github/workflows/internal.yml", "PRIVATE_SENTINEL workflow\n")
        self.write(".forgejo/runner.yml", "PRIVATE_SENTINEL runner\n")
        self.write(".githooks/pre-push", "private lint hook fixture\n")
        self.commit("initial public and private content")
        MIRROR.git(self.source, "tag", "-a", "v0.3.0", "-m", "old annotated release")
        MIRROR.git(
            self.source, "config", "core.hooksPath", str(self.source / ".githooks")
        )
        self.write("nested/secret.key", "PRIVATE_SENTINEL nested key\n")
        self.write("root.cred", "PRIVATE_SENTINEL credential\n")
        self.write("deep/.aster-identity", "PRIVATE_SENTINEL identity\n")
        self.write("deep/AGENTS.md", "PRIVATE_SENTINEL context\n")
        self.write("docs/_internal/deleted.txt", "PRIVATE_SENTINEL deleted later\n")
        self.commit("private-only change\n\nCo-Authored-By: Claude <bot@anthropic.com>")
        MIRROR.git(self.source, "checkout", "-b", "side")
        self.write("docs/_internal/side.txt", "PRIVATE_SENTINEL side parent\n")
        self.commit("private side parent")
        MIRROR.git(self.source, "checkout", "main")
        MIRROR.git(
            self.source, "mv", "docs/_internal/first.txt", "docs/_internal/renamed.txt"
        )
        self.write("README.md", "intended public content, retained human work\n")
        self.commit("public change\n\nCo-Authored-By: Human <human@example.invalid>")
        MIRROR.git(self.source, "merge", "--no-ff", "side", "-m", "preserve this merge")
        self.write("bindings/go/go.mod", f"module {MIRROR.GO_MODULE}\n\ngo 1.25.0\n")
        self.commit("approved Go identity")
        self.release_commit = self.head()
        self.release = "v" + MIRROR.derived_version(self.source, self.release_commit)
        MIRROR.git(
            self.source,
            "tag",
            "-a",
            self.release,
            "-m",
            "private signed-origin annotation is not copied",
        )
        MIRROR.git(
            self.source,
            "rm",
            "docs/_internal/deleted.txt",
            "nested/secret.key",
            "root.cred",
        )
        self.commit("delete private files from latest tree")
        self.write("nested/secret.key", "PRIVATE_SENTINEL reintroduced key\n")
        self.write("README.md", "latest intended public content\n")
        self.commit("latest main")
        MIRROR.git(self.source, "tag", "unrelated-private-tag")
        self.write("untracked-private-note", "dirty state must remain untouched\n")

    def write(self, path, text):
        file = self.source / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)

    def commit(self, message):
        MIRROR.git(self.source, "add", "-A")
        MIRROR.git(self.source, "commit", "-m", message)

    def head(self, repo=None):
        return MIRROR.git(repo or self.source, "rev-parse", "HEAD").decode().strip()

    def source_state(self):
        return (
            MIRROR.git(self.source, "show-ref"),
            MIRROR.git(self.source, "status", "--porcelain=v1"),
            (self.source / ".git/config").read_bytes(),
            (self.source / ".githooks/pre-push").read_bytes(),
            (self.source / ".publicignore").read_bytes(),
        )

    def prepare(self, name="public", required=None):
        output = self.root / name
        return output, MIRROR.prepare(self.source, output, required=required)

    def test_full_history_exclusion_tag_mapping_metadata_and_source_untouched(self):
        before = self.source_state()
        # A dirty worktree policy and poisoned build environment must not define a release.
        self.write(".publicignore", "uncommitted wrong policy\n")
        before = self.source_state()
        with patch.dict(os.environ, {"ASTER_BUILD_NUMBER": "999999"}):
            output, report = self.prepare(required=self.release)
        self.assertEqual(self.source_state(), before)
        refs = set(
            MIRROR.git(output, "for-each-ref", "--format=%(refname)")
            .decode()
            .splitlines()
        )
        self.assertEqual(
            refs,
            {
                "refs/heads/main",
                "refs/tags/v0.3.0",
                "refs/tags/" + self.release,
                "refs/tags/bindings/go/" + self.release,
            },
        )
        current = next(r for r in report["releases"] if r["tag"] == self.release)
        for name in (self.release, "bindings/go/" + self.release):
            target = (
                MIRROR.git(output, "rev-parse", "refs/tags/" + name).decode().strip()
            )
            self.assertEqual(target, current["filtered_commit"])
            self.assertNotEqual(target, self.release_commit)
            self.assertEqual(MIRROR.derived_version(output, target), self.release[1:])
            self.assertIn(
                MIRROR.GO_MODULE.encode(),
                MIRROR.committed(output, target, "bindings/go/go.mod"),
            )
        self.assertFalse(
            next(r for r in report["releases"] if r["tag"] == "v0.3.0")["go_eligible"]
        )
        self.assertEqual(MIRROR.git(output, "remote"), b"")
        self.assertEqual(
            MIRROR.git(output, "rev-list", "--count", "main"),
            MIRROR.git(self.source, "rev-list", "--count", "main"),
        )
        mapping = dict(
            line.split()
            for line in (output / ".git/filter-repo/commit-map")
            .read_text()
            .splitlines()[1:]
        )
        for row in (
            MIRROR.git(self.source, "rev-list", "--parents", "main")
            .decode()
            .splitlines()
        ):
            source_commit, *parents = row.split()
            filtered = (
                MIRROR.git(
                    output, "rev-list", "--parents", "-n", "1", mapping[source_commit]
                )
                .decode()
                .split()
            )
            self.assertEqual(
                filtered, [mapping[source_commit], *[mapping[p] for p in parents]]
            )
        rules = MIRROR.policy_rules(POLICY.encode())
        for commit in MIRROR.git(output, "rev-list", "--all").decode().splitlines():
            names = MIRROR.git(
                output, "ls-tree", "-r", "--name-only", "-z", commit
            ).split(b"\0")
            self.assertFalse(
                any(name and MIRROR.excluded(name, rules) for name in names)
            )
        for row in MIRROR.git(output, "rev-list", "--objects", "--all").splitlines():
            oid = row.split(b" ", 1)[0].decode()
            if MIRROR.git(output, "cat-file", "-t", oid).strip() == b"blob":
                self.assertNotIn(
                    b"PRIVATE_SENTINEL", MIRROR.git(output, "cat-file", "blob", oid)
                )
        messages = MIRROR.git(output, "log", "--format=%B", "--all")
        self.assertNotIn(b"Claude", messages)
        self.assertIn(b"Co-Authored-By: Human <human@example.invalid>", messages)
        for public_path in (
            "bindings/java/Api.java",
            "bindings/dotnet/Api.cs",
            "ffi/aster_ffi.h",
        ):
            self.assertTrue((output / public_path).exists())

    def test_repeat_export_and_atomic_public_ref_updates_are_idempotent_and_never_force_tags(
        self,
    ):
        first, report = self.prepare("public-one")
        second, repeated = self.prepare("public-two")
        self.assertEqual(report, repeated)
        remote = self.root / "simulated-public.git"
        MIRROR.git(self.source, "init", "--bare", str(remote))

        def push(repo, expected=""):
            return subprocess.run(
                [
                    "git",
                    "-C",
                    str(repo),
                    "-c",
                    "push.followTags=false",
                    "push",
                    "--atomic",
                    f"--force-with-lease=refs/heads/main:{expected}",
                    str(remote),
                    *report["refspecs"],
                ],
                capture_output=True,
            )

        self.assertEqual(push(first).returncode, 0)
        self.assertEqual(push(second, report["filtered_commit"]).returncode, 0)
        self.assertEqual(
            set(
                MIRROR.git(remote, "for-each-ref", "--format=%(refname)")
                .decode()
                .splitlines()
            ),
            {r.split(":")[1] for r in report["refspecs"]},
        )
        # A conflicting public Go tag must reject the whole atomic update, including main.
        old = MIRROR.git(first, "rev-list", "--max-parents=0", "main").decode().strip()
        MIRROR.git(remote, "update-ref", "refs/heads/main", old)
        self.assertNotEqual(push(second, report["filtered_commit"]).returncode, 0)
        self.assertEqual(
            MIRROR.git(remote, "rev-parse", "refs/heads/main").decode().strip(), old
        )
        self.assertEqual(push(second, old).returncode, 0)
        MIRROR.git(remote, "update-ref", "refs/tags/bindings/go/" + self.release, old)
        MIRROR.git(remote, "update-ref", "refs/heads/main", old)
        failed = push(second, old)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(
            MIRROR.git(remote, "rev-parse", "refs/heads/main").decode().strip(), old
        )
        self.assertEqual(
            MIRROR.git(remote, "rev-parse", "refs/tags/bindings/go/" + self.release)
            .decode()
            .strip(),
            old,
        )

    def test_missing_wrong_module_and_wrong_version_required_releases_fail_before_clone(
        self,
    ):
        for name, reason in (("v0.3.4242", "missing"), ("v0.3.0", "module")):
            with self.assertRaisesRegex(MIRROR.PreparationError, reason):
                self.prepare(name.replace(".", "-"), required=name)
        MIRROR.git(self.source, "tag", "v0.3.999")
        with self.assertRaisesRegex(MIRROR.PreparationError, "differs"):
            self.prepare("wrong-version", required="v0.3.999")

    def test_nonancestor_required_release_is_rejected_and_unrelated_refs_never_survive(
        self,
    ):
        MIRROR.git(self.source, "checkout", "--orphan", "private-orphan")
        MIRROR.git(self.source, "rm", "-rf", ".")
        self.write("VERSION", "9.1 1\n")
        self.commit("private orphan")
        MIRROR.git(self.source, "tag", "v9.1.0")
        MIRROR.git(self.source, "checkout", "main")
        with self.assertRaisesRegex(MIRROR.PreparationError, "not an ancestor"):
            self.prepare("orphan-required", required="v9.1.0")
        output, report = self.prepare()
        self.assertIn("v9.1.0", [r["tag"] for r in report["skipped_releases"]])
        self.assertNotIn(b"private-orphan", MIRROR.git(output, "show-ref"))

    def test_source_shallow_replacements_grafts_and_nested_outputs_are_rejected(self):
        with self.assertRaisesRegex(MIRROR.PreparationError, "outside"):
            MIRROR.prepare(self.source, self.source / "publication")
        linked = self.root / "linked-worktree"
        MIRROR.git(self.source, "worktree", "add", "--detach", str(linked), "main")
        with self.assertRaisesRegex(MIRROR.PreparationError, "outside"):
            MIRROR.prepare(linked, self.source / ".git/publication")
        shallow = self.root / "shallow"
        MIRROR.git(
            self.source, "clone", "--depth=1", self.source.as_uri(), str(shallow)
        )
        with self.assertRaisesRegex(MIRROR.PreparationError, "shallow"):
            MIRROR.prepare(shallow, self.root / "from-shallow")
        parent = MIRROR.git(self.source, "rev-parse", "HEAD^").decode().strip()
        MIRROR.git(self.source, "replace", self.head(), parent)
        with self.assertRaisesRegex(MIRROR.PreparationError, "replacement"):
            self.prepare("from-replace")
        MIRROR.git(self.source, "replace", "-d", self.head())
        (self.source / ".git/info/grafts").write_text(self.head() + "\n")
        with self.assertRaisesRegex(MIRROR.PreparationError, "grafts"):
            self.prepare("from-graft")

    def test_policy_cannot_remove_an_eligible_go_module_or_version(self):
        for name, suffix, reason in (
            ("missing-go", "bindings/go/\n", "Go module"),
            ("missing-version", "VERSION\n", "VERSION"),
        ):
            self.write(".publicignore", POLICY + suffix)
            self.commit("bad public policy " + name)
            with self.assertRaisesRegex(MIRROR.PreparationError, reason):
                self.prepare(name)

    def test_unsafe_or_unsupported_policy_is_rejected(self):
        for rule in (
            "../private",
            "/absolute",
            "!private",
            "a/../../private",
            "a\\private",
            "a//private",
            "secrets*/",
        ):
            with self.assertRaises(MIRROR.PreparationError):
                MIRROR.policy_rules((POLICY + rule + "\n").encode())
        with self.assertRaisesRegex(MIRROR.PreparationError, "must exclude"):
            MIRROR.policy_rules(b"docs/_internal/\n")


if __name__ == "__main__":
    unittest.main()
