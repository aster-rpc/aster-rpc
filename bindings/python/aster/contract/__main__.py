"""
``python -m aster.contract gen --service MODULE:CLASS [--service ...] [--semver V]``

The manifest handoff the ``aster`` CLI calls for Python services. Writes one
JSON object to stdout::

    {"aster_manifest_handoff": 1, "manifests": [<ContractManifest>, ...]}

Errors go to stderr with exit status 1. Every language binding implements the
same envelope, so the CLI treats them alike.
"""

from __future__ import annotations

import argparse
import json
import sys

HANDOFF_VERSION = 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m aster.contract")
    sub = parser.add_subparsers(dest="command", required=True)
    gen = sub.add_parser("gen", help="Write the manifests of @service classes to stdout")
    gen.add_argument("--service", action="append", required=True, metavar="MODULE:CLASS")
    gen.add_argument("--semver", default=None)
    args = parser.parse_args(argv)

    # The service's own modules are found from the working directory, as the
    # CLI's caller expects.
    if "" not in sys.path and "." not in sys.path:
        sys.path.insert(0, "")

    from aster.contract.generate import build_manifest, import_service

    try:
        manifests = [
            json.loads(build_manifest(import_service(spec), semver=args.semver).to_json())
            for spec in args.service
        ]
    except Exception as exc:  # noqa: BLE001 - reported to the calling CLI
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        return 1
    json.dump(
        {"aster_manifest_handoff": HANDOFF_VERSION, "manifests": manifests},
        sys.stdout,
        ensure_ascii=False,
    )
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
