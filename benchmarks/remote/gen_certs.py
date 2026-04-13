#!/usr/bin/env python3
"""Generate self-signed TLS certs for gRPC benchmark."""
import subprocess, os

os.makedirs("certs", exist_ok=True)

subprocess.run([
    "openssl", "req", "-x509", "-newkey", "rsa:2048",
    "-keyout", "certs/server.key", "-out", "certs/server.crt",
    "-days", "1", "-nodes", "-batch",
    "-subj", "/CN=bench.local",
    "-addext", "subjectAltName=IP:192.168.1.140,DNS:localhost",
], check=True)

print("Generated certs/server.key and certs/server.crt")
