#!/usr/bin/env python3
"""Compile and execute a standalone cgo consumer of an exact final FFI archive.

No SDK checkout or module replacement is used by the consumer. The extracted
header and library are its only Aster inputs. Linux ARM64 runs under QEMU when
requested; other targets execute on their native hosts.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import platform
from pathlib import Path, PurePosixPath
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile

LIBRARIES = {"linux-x64": "libaster_transport_ffi.so", "linux-arm64": "libaster_transport_ffi.so",
             "osx-arm64": "libaster_transport_ffi.dylib", "win-x64": "aster_transport_ffi.dll"}


def verify_host(rid: str, qemu: bool) -> None:
    machine = platform.machine().lower()
    architecture = "arm64" if machine in ("arm64", "aarch64") else "x64" if machine in ("amd64", "x86_64") else machine
    system = {"Darwin": "osx", "Linux": "linux", "Windows": "win"}.get(platform.system())
    expected = "linux-x64" if qemu else rid
    if f"{system}-{architecture}" != expected or (qemu and rid != "linux-arm64"):
        raise ValueError("consumer host does not match its claimed execution target")


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def extract(archive: Path, target: Path, library: str) -> None:
    with tarfile.open(archive, "r:gz") as bundle:
        seen = set()
        total = 0
        for member in bundle.getmembers():
            path = PurePosixPath(member.name)
            total += member.size
            name = str(path)
            if (path.is_absolute() or ".." in path.parts or "\\" in member.name
                    or name in seen or not (member.isfile() or member.isdir()) or total > 2147483648):
                raise ValueError("unsafe or oversized FFI archive")
            seen.add(name)
        files = {str(PurePosixPath(m.name)) for m in bundle.getmembers() if m.isfile()}
        if files != {"aster_ffi.h", library}:
            raise ValueError(f"unexpected FFI archive inventory: {sorted(files)}")
        # Validate inventory ourselves as well as applying Python's data filter.
        bundle.extractall(target, filter="data")


def fetch(destination: Path) -> Path:
    """Reuse the audited exact-run ArtifactService transport, including on Windows."""
    path = Path(__file__).resolve().parents[1] / "ci/candidate-handoff.py"
    spec = importlib.util.spec_from_file_location("handoff", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    token = os.environ["ACTIONS_RUNTIME_TOKEN"]
    ids = module.backend_ids(token)
    if ids["workflow_run_backend_id"] != os.environ["FORGEJO_RUN_ID"]:
        raise ValueError("artifact runtime identity differs from this run")
    base = module.origin(os.environ["ACTIONS_RESULTS_URL"])
    name = "ffi-tarballs-" + os.environ["FORGEJO_RUN_NUMBER"]
    transport = module.Transport(destination / "retrieval.jsonl")
    response = destination / "response.json"
    listing = transport.get(base + module.SERVICE + "ListArtifacts", response, token=token, data=ids)
    matches = [module.wire_fields(e) for e in listing["artifacts"] if e.get("name") == name]
    if len(matches) != 1 or matches[0].get("workflow_run_backend_id") != ids["workflow_run_backend_id"]:
        raise ValueError("missing or ambiguous exact-run FFI artifact")
    entry = matches[0]
    request = {key: entry[key] for key in ids}
    request["name"] = name
    signed = module.wire_fields(transport.get(base + module.SERVICE + "GetSignedArtifactURL", response,
                                             token=token, data=request, artifact=name))
    archive = destination / "artifact.zip"
    transport.get(signed["signed_url"], archive, artifact=name)
    if entry.get("digest") is not None and entry["digest"] != "sha256:" + digest(archive):
        raise ValueError("artifact transport checksum differs")
    target = destination / "archives"
    module.extract(archive, target)
    return target


def run(archive: Path, rid: str, version: str, output: Path, qemu: bool = False) -> dict:
    library = LIBRARIES[rid]
    expected = archive.with_name("FFI-SHA256SUMS").read_text().splitlines()
    matches = [line.split()[0] for line in expected if len(line.split()) == 2 and line.split()[1] == archive.name]
    if matches != [digest(archive)]:
        raise ValueError("FFI archive checksum missing or mismatched")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="aster-ffi-consumer-") as temporary:
        project = Path(temporary).resolve()
        native = project / "native"
        native.mkdir()
        extract(archive, native, library)
        shutil.copy2(Path(__file__).with_suffix(".go"), project / "main.go")
        (project / "go.mod").write_text("module ffi-archive-consumer\n\ngo 1.25.0\n")
        env = dict(os.environ)
        for name in ("CC", "CXX", "CGO_CFLAGS", "CGO_CPPFLAGS", "CGO_CXXFLAGS", "CGO_LDFLAGS", "CPATH", "C_INCLUDE_PATH", "LIBRARY_PATH", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "GOFLAGS", "GOWORK"):
            env.pop(name, None)
        env.update(CGO_ENABLED="1", GOWORK="off", CGO_CFLAGS="-I" + shlex.quote(str(native)),
                   CGO_LDFLAGS="-L" + shlex.quote(str(native)) + " -laster_transport_ffi",
                   GOCACHE=str(project / "go-cache"), GOMODCACHE=str(project / "go-mod-cache"),
                   GOENV="off", GOTOOLCHAIN="local", GOPROXY="off", GOSUMDB="off")
        if rid == "win-x64":
            env["CC"] = "gcc"
        if rid != "win-x64":
            env["CGO_LDFLAGS"] += " -Wl,-rpath," + shlex.quote(str(native))
        if rid.startswith("linux"):
            env["CGO_LDFLAGS"] += " -ldl"
        env["LD_LIBRARY_PATH"] = str(native)
        env["DYLD_LIBRARY_PATH"] = str(native)
        env["PATH"] = str(native) + os.pathsep + env["PATH"]
        if qemu:
            if rid != "linux-arm64":
                raise ValueError("QEMU is only supported for linux-arm64")
            env.update(GOOS="linux", GOARCH="arm64", CC="aarch64-linux-gnu-gcc")
        else:
            env.pop("GOOS", None)
            env.pop("GOARCH", None)
        executable = project / ("consumer.exe" if rid == "win-x64" else "consumer")
        with (output / "build.log").open("w") as log:
            subprocess.run(["go", "build", "-o", str(executable), "."], cwd=project, env=env,
                           stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)
        command = (["qemu-aarch64", "-L", "/usr/aarch64-linux-gnu"] if qemu else []) + [str(executable)]
        result = subprocess.run(command, cwd=project, env=env, text=True, capture_output=True, timeout=60)
        (output / "consumer.log").write_text(result.stdout + result.stderr)
        result.check_returncode()
        receipt = json.loads(result.stdout)
        if receipt["version"] != version or Path(receipt["loaded_image"]).resolve() != (native / library).resolve():
            raise ValueError("consumer loaded a different native image or version")
        receipt.update(rid=rid, execution="qemu-user" if qemu else "native", archive_sha256=digest(archive),
                       header_sha256=digest(native / "aster_ffi.h"), native_sha256=digest(native / library))
        (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        return receipt


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--fetch", action="store_true")
    parser.add_argument("--rid", choices=LIBRARIES, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--qemu", action="store_true")
    args = parser.parse_args()
    if bool(args.archive) == args.fetch:
        parser.error("choose --archive or --fetch")
    verify_host(args.rid, args.qemu)
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.archive or fetch(args.output) / f"libaster_transport_ffi-{args.rid}.tar.gz"
    try:
        # This JSON receipt is retained in the exact-run job logs. Candidate
        # provenance records required task success, rather than embedding it.
        print(json.dumps(run(archive.resolve(), args.rid, args.version, args.output.resolve(), args.qemu)))
    except Exception:
        for name in ("build.log", "consumer.log"):
            path = args.output / name
            if path.is_file():
                print(f"--- {name} ---\n{path.read_text()[-20000:]}", file=sys.stderr)
        raise


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Transport exceptions can contain signed URLs. Keep logs credential-safe.
        print(f"FFI archive consumer failed ({type(error).__name__}); inspect build.log, consumer.log or retrieval.jsonl", file=sys.stderr)
        sys.exit(1)
