#!/usr/bin/env python3
"""Stage tested Maven jars/POMs, then deploy exact files without rebuilding."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from urllib.error import HTTPError
from urllib.request import urlopen
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
MODULES = ("aster-annotations", "aster-codegen-core", "aster-codegen-apt", "aster-codegen-ksp", "aster-runtime")
NS = {"p": "http://maven.apache.org/POM/4.0.0"}


def stage(output: Path) -> None:
    version = subprocess.check_output([str(ROOT / "scripts/release/version.sh")], text=True).strip()
    output.mkdir(parents=True)
    packages = []
    for module in ("", *MODULES):
        directory = ROOT / "bindings/java" / module
        pom = ET.parse(directory / "pom.xml").getroot()
        artifact = pom.findtext("p:artifactId", namespaces=NS)
        actual = pom.findtext("p:version", namespaces=NS) or pom.findtext("p:parent/p:version", namespaces=NS)
        if actual != version:
            raise ValueError(f"{artifact}: Maven version {actual}, expected {version}")
        pom_name = f"{artifact}-{version}.pom"
        shutil.copyfile(directory / "pom.xml", output / pom_name)
        packaging = pom.findtext("p:packaging", namespaces=NS) or "jar"
        file = pom_name if packaging == "pom" else f"{artifact}-{version}.jar"
        if packaging != "pom":
            shutil.copyfile(directory / "target" / file, output / file)
        packages.append({"group": "site.aster", "artifact": artifact, "version": version,
                         "packaging": packaging, "pom": pom_name, "file": file})
    (output / "packages.json").write_text(json.dumps(packages, indent=2) + "\n")


def remote_digest(url: str) -> str | None:
    try:
        with urlopen(url, timeout=60) as response:
            return hashlib.file_digest(response, "sha256").hexdigest()
    except HTTPError as error:
        if error.code == 404:
            return None
        raise


def deploy(directory: Path, classifiers: Path) -> None:
    registry = os.environ["FORGEJO_SERVER_URL"].rstrip("/") + "/api/packages/Aster/maven"
    packages = json.loads((directory / "packages.json").read_text())
    runtime = next(p for p in packages if p["artifact"] == "aster-runtime")
    for classifier in ("linux-x86_64", "macos-aarch64"):
        name = f'aster-runtime-{runtime["version"]}-{classifier}.jar'
        shutil.copyfile(classifiers / name, directory / name)
        packages.append({**runtime, "classifier": classifier, "file": name})
    # Preflight all immutable artifacts before deploying any. A registry
    # conflict is never hidden behind deploy-file or --skip-duplicate.
    urls = {}
    for package in packages:
        for name in (package["file"], package["pom"]):
            path = directory / name
            url = f'{registry}/{package["group"].replace(".", "/")}/{package["artifact"]}/{package["version"]}/{name}'
            local = hashlib.sha256(path.read_bytes()).hexdigest()
            remote = remote_digest(url)
            if remote is not None and remote != local:
                raise ValueError(f"immutable Maven file differs: {name}")
            urls[name] = (url, local, remote)
    with tempfile.TemporaryDirectory(prefix="aster-maven-promote-") as temporary:
        settings = Path(temporary) / "settings.xml"
        settings.write_text('''<settings><servers><server><id>forgejo</id><username>agents</username><password>${env.ASTER_PACKAGE_TOKEN}</password></server></servers></settings>''')
        for package in packages:
            url, local, remote = urls[package["file"]]
            if remote == local and urls[package["pom"]][2] == urls[package["pom"]][1]:
                print(f'already published: {package["file"]}')
                continue
            args = ["mvn", "-B", "--no-transfer-progress", "-s", str(settings),
                    "org.apache.maven.plugins:maven-deploy-plugin:3.1.4:deploy-file",
                    f'-Dfile={directory.resolve() / package["file"]}',
                    f'-DpomFile={directory.resolve() / package["pom"]}',
                    "-DgeneratePom=false", f'-DgroupId={package["group"]}',
                    f'-DartifactId={package["artifact"]}', f'-Dversion={package["version"]}',
                    f'-Dpackaging={package["packaging"]}', "-DrepositoryId=forgejo", f"-Durl={registry}"]
            if package.get("classifier"):
                args.append(f'-Dclassifier={package["classifier"]}')
            subprocess.run(args, cwd=temporary, check=True)
        for name, (url, local, _) in urls.items():
            if remote_digest(url) != local:
                raise ValueError(f"Maven publication checksum mismatch: {name}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("stage", "deploy"))
    parser.add_argument("directory", type=Path)
    parser.add_argument("--classifiers", type=Path)
    args = parser.parse_args()
    if args.mode == "stage":
        stage(args.directory)
    else:
        if not args.classifiers:
            parser.error("deploy requires --classifiers")
        deploy(args.directory, args.classifiers)


if __name__ == "__main__":
    main()
