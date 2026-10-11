#!/usr/bin/env python3
"""Prepare an offline, history-filtered publication clone; never push or alter the source.

Requires git-filter-repo 2.47.0 (CI pins it). Only filtered main and validated,
remapped release refs survive. The .git/public-mirror.json audit and
.git/public-refspecs allowlist are private clone metadata, never committed.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import subprocess
from pathlib import Path


GO_MODULE = "sdk.getaster.now/go"
RELEASE = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")


class PreparationError(Exception):
    pass


def git(repo: Path, *args: str, data: bytes | None = None) -> bytes:
    env = os.environ.copy()
    for name in (
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_REPLACE_REF_BASE",
    ):
        env.pop(name, None)
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        input=data,
        capture_output=True,
        check=False,
        env=env,
    )
    if result.returncode:
        raise PreparationError(
            f"git {args[0]} failed: {result.stderr.decode(errors='replace').strip()}"
        )
    return result.stdout


def committed(repo: Path, commit: str, path: str) -> bytes:
    return git(repo, "show", f"{commit}:{path}")


def policy_rules(data: bytes) -> list[bytes]:
    rules = []
    for line in data.decode("utf-8").splitlines():
        rule = line.strip()
        if not rule or rule.startswith("#"):
            continue
        if (
            rule.startswith(("/", "!", "\\"))
            or "\\" in rule
            or any(part in ("", ".", "..") for part in rule.rstrip("/").split("/"))
            or "\x00" in rule
            or (rule.endswith("/") and any(character in rule for character in "*?["))
        ):
            raise PreparationError(f"unsafe or unsupported public exclusion: {rule!r}")
        rules.append(rule.encode("utf-8"))
    # The publication controls must never become reachable public files.
    for required in (
        b".publicignore",
        b".github/workflows/",
        b".forgejo/",
        b".githooks/",
    ):
        if required not in rules:
            raise PreparationError(f"public policy must exclude {required.decode()}")
    return rules


def excluded(filename: bytes, rules: list[bytes]) -> bool:
    for rule in rules:
        if rule.endswith(b"/"):
            if filename.startswith(rule):
                return True
        elif fnmatch.fnmatchcase(
            filename if b"/" in rule else filename.rsplit(b"/", 1)[-1], rule
        ):
            return True
    return False


def derived_version(source: Path, commit: str) -> str:
    lines = [
        line.strip()
        for line in committed(source, commit, "VERSION").decode("ascii").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if len(lines) != 1 or not re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\s+[0-9]+", lines[0]
    ):
        raise PreparationError("invalid committed VERSION")
    base, offset = lines[0].split()
    count = int(git(source, "rev-list", "--count", commit))
    if count < int(offset):
        raise PreparationError("committed VERSION offset exceeds original commit count")
    return f"{base}.{count - int(offset)}"


def release_inventory(
    source: Path, head: str, required: str | None
) -> tuple[list[dict], list[dict]]:
    names = (
        git(source, "for-each-ref", "--format=%(refname:strip=2)", "refs/tags")
        .decode()
        .splitlines()
    )
    if required is not None and (
        not RELEASE.fullmatch(required) or required not in names
    ):
        raise PreparationError(
            f"required release {required!r} is missing or not a root v<version> tag"
        )
    releases, skipped = [], []
    for name in sorted(names):
        if not RELEASE.fullmatch(name):
            continue
        try:
            commit = (
                git(source, "rev-parse", "--verify", f"refs/tags/{name}^{{commit}}")
                .decode()
                .strip()
            )
            if commit not in git(source, "rev-list", head).decode().splitlines():
                raise PreparationError("release is not an ancestor of main")
            expected = "v" + derived_version(source, commit)
            if name != expected:
                raise PreparationError(
                    f"release name differs from original committed version {expected}"
                )
        except PreparationError as error:
            if name == required:
                raise PreparationError(f"required release {name}: {error}") from error
            skipped.append({"tag": name, "reason": str(error)})
            continue
        go_reason = None
        try:
            mod = committed(source, commit, "bindings/go/go.mod").decode("utf-8")
            declarations = re.findall(r"^\s*module\s+(\S+)\s*$", mod, re.MULTILINE)
            if declarations != [GO_MODULE]:
                go_reason = f"committed Go module is not {GO_MODULE}"
            elif int(RELEASE.fullmatch(name).group(1)) >= 2:
                go_reason = (
                    "major version 2+ requires a version-suffixed Go module path"
                )
        except PreparationError:
            go_reason = "release has no committed bindings/go/go.mod"
        if go_reason and name == required:
            raise PreparationError(f"required release {name}: {go_reason}")
        releases.append(
            {
                "tag": name,
                "source_commit": commit,
                "go_eligible": go_reason is None,
                "go_skip_reason": go_reason,
            }
        )
    return releases, skipped


def prepare(
    source: Path, output: Path, source_ref: str = "main", required: str | None = None
) -> dict:
    source, output = source.resolve(), output.resolve()
    git_dir = Path(
        git(source, "rev-parse", "--absolute-git-dir").decode().strip()
    ).resolve()
    common_dir = Path(
        git(source, "rev-parse", "--path-format=absolute", "--git-common-dir")
        .decode()
        .strip()
    ).resolve()
    if (
        output.is_relative_to(source)
        or output.is_relative_to(git_dir)
        or output.is_relative_to(common_dir)
    ):
        raise PreparationError(
            "output must be outside the source repository and its git directory"
        )
    if output.exists():
        raise PreparationError("output must not exist; use a new disposable directory")
    if git(source, "rev-parse", "--is-shallow-repository").strip() != b"false":
        raise PreparationError("source is shallow; full original history is required")
    if (
        git(source, "for-each-ref", "--format=%(refname)", "refs/replace").strip()
        or (common_dir / "info/grafts").exists()
    ):
        raise PreparationError("source replacement refs or grafts are unsupported")
    head = (
        git(
            source,
            "rev-parse",
            "--verify",
            "--end-of-options",
            f"{source_ref}^{{commit}}",
        )
        .decode()
        .strip()
    )
    rules = policy_rules(committed(source, head, ".publicignore"))
    releases, skipped = release_inventory(source, head, required)
    git(
        source, "filter-repo", "--version"
    )  # Fail before creating a clone if unavailable.
    git(
        source,
        "clone",
        "--no-local",
        "--no-hardlinks",
        "--no-checkout",
        "--",
        str(source),
        str(output),
    )
    # Never leave private branches, tags, replace refs or a source remote to push by mistake.
    for ref in git(output, "for-each-ref", "--format=%(refname)").decode().splitlines():
        git(output, "update-ref", "-d", ref)
    for remote in git(output, "remote").decode().splitlines():
        git(output, "remote", "remove", remote)
    git(output, "update-ref", "refs/heads/main", head)
    git(output, "symbolic-ref", "HEAD", "refs/heads/main")
    filename_callback = f"""
import fnmatch
for rule in {rules!r}:
    if rule.endswith(b'/'):
        if filename.startswith(rule): return None
    elif fnmatch.fnmatchcase(filename if b'/' in rule else filename.rsplit(b'/', 1)[-1], rule):
        return None
return filename
"""
    message_callback = r"""
import re
message = b'\n'.join(line for line in message.split(b'\n') if not re.search(br'co-authored-by:.*claude', line, re.I))
message = re.sub(br'claude Opus[^\s]*', b'[LLM]', message, flags=re.I)
message = re.sub(br'claude', b'[LLM]', message, flags=re.I)
return re.sub(br'anthropic\.com', b'[scrubbed]', message, flags=re.I)
"""
    git(
        output,
        "filter-repo",
        "--force",
        "--refs",
        "refs/heads/main",
        "--prune-empty",
        "never",
        "--prune-degenerate",
        "never",
        "--replace-refs",
        "delete-no-add",
        "--filename-callback",
        filename_callback,
        "--message-callback",
        message_callback,
    )
    mapping = dict(
        line.split()
        for line in (output / ".git/filter-repo/commit-map")
        .read_text()
        .splitlines()[1:]
    )
    source_count = int(git(source, "rev-list", "--count", head))
    if int(git(output, "rev-list", "--count", "refs/heads/main")) != source_count:
        raise PreparationError(
            "filter unexpectedly changed the version-critical ancestor count"
        )
    if derived_version(output, mapping[head]) != derived_version(source, head):
        raise PreparationError("filter changed the committed main version derivation")
    refs = ["refs/heads/main:refs/heads/main"]
    for release in releases:
        target = mapping.get(release["source_commit"])
        if target is None or not target.strip("0"):
            raise PreparationError(
                f"release {release['tag']} has no filtered commit mapping"
            )
        if "v" + derived_version(output, target) != release["tag"]:
            raise PreparationError(f"filter changed release version {release['tag']}")
        if release["go_eligible"]:
            try:
                mod = committed(output, target, "bindings/go/go.mod").decode("utf-8")
            except PreparationError as error:
                raise PreparationError(
                    "filter removed an eligible release's Go module"
                ) from error
            if re.findall(r"^\s*module\s+(\S+)\s*$", mod, re.MULTILINE) != [GO_MODULE]:
                raise PreparationError(
                    "filtered release has the wrong Go module identity"
                )
        release["filtered_commit"] = target
        names = [release["tag"]]
        if release["go_eligible"]:
            names.append("bindings/go/" + release["tag"])
        for name in names:
            git(output, "update-ref", "refs/tags/" + name, target)
            refs.append(f"refs/tags/{name}:refs/tags/{name}")
    expected_refs = {ref.split(":")[0] for ref in refs}
    actual_refs = set(
        git(output, "for-each-ref", "--format=%(refname)").decode().splitlines()
    )
    if actual_refs != expected_refs or git(output, "remote").strip():
        raise PreparationError("publication clone contains unexpected refs or remotes")
    for commit in git(output, "rev-list", "--all").decode().splitlines():
        for path in git(output, "ls-tree", "-r", "--name-only", "-z", commit).split(
            b"\0"
        ):
            if path and excluded(path, rules):
                raise PreparationError(f"excluded historical path survived: {path!r}")
    git(output, "reset", "--hard", "refs/heads/main")
    git(output, "reflog", "expire", "--expire=now", "--all")
    git(output, "gc", "--prune=now")
    report = {
        "source_commit": head,
        "filtered_commit": mapping[head],
        "original_commit_count": source_count,
        "module": GO_MODULE,
        "releases": releases,
        "skipped_releases": skipped,
        "refspecs": refs,
    }
    (output / ".git/public-mirror.json").write_text(json.dumps(report, indent=2) + "\n")
    (output / ".git/public-refspecs").write_text("\n".join(refs) + "\n")
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-ref", default="main")
    parser.add_argument("--required-release")
    args = parser.parse_args()
    try:
        report = prepare(
            args.source, args.output, args.source_ref, args.required_release
        )
    except (PreparationError, UnicodeError) as error:
        parser.exit(1, f"public mirror preparation failed: {error}\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
