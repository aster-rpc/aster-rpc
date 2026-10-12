"""
``python -m aster.codegen client --manifest FILE --out DIR --namespace NAME``

The client-generation handoff the ``aster`` CLI calls for Python. Generates a
typed client package for every service in the manifest file (one manifest or
a list) and writes one JSON object to stdout::

    {"aster_codegen_handoff": 1, "files": [<path>, ...], "usage": "<snippet>"}

Errors go to stderr with exit status 1.
"""

from __future__ import annotations

import argparse
import json
import sys

HANDOFF_VERSION = 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m aster.codegen")
    sub = parser.add_subparsers(dest="command", required=True)
    client = sub.add_parser("client", help="Generate typed clients from a manifest file")
    client.add_argument("--manifest", required=True, metavar="FILE")
    client.add_argument("--out", required=True, metavar="DIR")
    client.add_argument("--namespace", required=True, metavar="NAME")
    client.add_argument("--source", default=None, help="Recorded in file headers (default: --manifest)")
    client.add_argument("--address", default="", help="Shown in the usage snippet")
    args = parser.parse_args(argv)

    from aster.codegen.python import format_usage_snippet, generate_python_clients

    try:
        with open(args.manifest, encoding="utf-8") as f:
            data = json.load(f)
        items = data if isinstance(data, list) else [data]
        manifests = {m.get("service", "UnknownService"): m for m in items}
        files = generate_python_clients(
            manifests,
            out_dir=args.out,
            namespace=args.namespace,
            source=args.source or args.manifest,
        )
        usage = format_usage_snippet(args.out, args.namespace, manifests, address=args.address)
    except Exception as exc:  # noqa: BLE001 - reported to the calling CLI
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        return 1
    json.dump(
        {"aster_codegen_handoff": HANDOFF_VERSION, "files": files, "usage": usage},
        sys.stdout,
        ensure_ascii=False,
    )
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
