#!/usr/bin/env python3
"""Run the bounded Java FFM allocation control after a targeted Maven test build."""
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import xml.etree.ElementTree as ET

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--java-home", required=True, type=Path)
parser.add_argument("--native", required=True, type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parents[3]
report = root / "bindings/java/aster-runtime/target/surefire-reports/TEST-site.aster.handle.NodeEndpointTest.xml"
properties = {p.get("name"): p.get("value") for p in ET.parse(report).findall("./properties/property")}
classpath = properties["java.class.path"]
print("native_sha256=" + hashlib.sha256(args.native.read_bytes()).hexdigest(), flush=True)
with tempfile.TemporaryDirectory(prefix="aster-java-arena-") as directory:
    subprocess.run([
        str(args.java_home / "bin/javac"), "-cp", classpath, "-d", directory,
        str(root / "tests/assurance/native/JavaArenaProbe.java"),
    ], check=True, timeout=30)
    subprocess.run([
        str(args.java_home / "bin/java"), "-XX:NativeMemoryTracking=summary",
        "--enable-native-access=ALL-UNNAMED", "-Daster.ffi.lib=" + str(args.native.resolve()),
        "-cp", directory + os.pathsep + classpath, "JavaArenaProbe",
    ], check=True, timeout=90)
