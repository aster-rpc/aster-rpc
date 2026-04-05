"""
aster_cli.keygen — Key generation commands.

Commands:
  aster keygen root [--out PATH]
      Generate a new ed25519 root key pair.
      Writes a JSON file with ``private_key`` and ``public_key`` (hex).
      Refuses to overwrite existing files.

  aster keygen producer [--out PATH]
      Generate a stable ed25519 producer node key.
      Derives the iroh NodeId and writes a JSON file with
      ``secret_key`` (hex) and ``node_id`` (string).
      Uses a temporary iroh endpoint to derive the NodeId.

No network access required for root keygen.
Producer keygen starts a transient local endpoint to derive the NodeId.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys


def _keygen_root(args) -> int:
    """Generate an ed25519 root key pair and write to disk."""
    from aster_python.aster.trust.signing import generate_root_keypair

    out_path = os.path.expanduser(args.out)

    if os.path.exists(out_path):
        print(f"Error: key file already exists: {out_path}", file=sys.stderr)
        print("Use a different path or remove the existing file.", file=sys.stderr)
        return 1

    priv_raw, pub_raw = generate_root_keypair()
    priv_hex = priv_raw.hex()
    pub_hex = pub_raw.hex()

    out_dir = os.path.dirname(out_path) or "."
    os.makedirs(out_dir, exist_ok=True)

    fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump({"private_key": priv_hex, "public_key": pub_hex}, f, indent=2)
            f.write("\n")
    except BaseException:
        try:
            os.unlink(out_path)
        except OSError:
            pass
        raise

    print(f"Root key pair written to: {out_path}")
    print(f"  public_key : {pub_hex}")
    print("  Keep the private key secret.")
    return 0


async def _derive_node_id(secret_bytes: bytes) -> str:
    """Start a transient iroh endpoint to derive the NodeId from a secret key."""
    from aster_python import create_endpoint_with_config, EndpointConfig

    ep = await create_endpoint_with_config(
        EndpointConfig(secret_key=list(secret_bytes), alpns=[b"_keygen"])
    )
    node_id = ep.endpoint_id()
    await ep.close()
    return node_id


def _keygen_producer(args) -> int:
    """Generate a stable producer node key and derive its iroh NodeId."""
    import secrets as _secrets

    out_path = os.path.expanduser(args.out)

    if os.path.exists(out_path):
        print(f"Error: key file already exists: {out_path}", file=sys.stderr)
        print("Use a different path or remove the existing file.", file=sys.stderr)
        return 1

    secret_bytes = _secrets.token_bytes(32)

    print("Deriving NodeId (starting transient endpoint)...")
    try:
        node_id = asyncio.run(_derive_node_id(secret_bytes))
    except Exception as exc:
        print(f"Error: could not derive NodeId: {exc}", file=sys.stderr)
        return 1

    out_dir = os.path.dirname(out_path) or "."
    os.makedirs(out_dir, exist_ok=True)

    fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump({"secret_key": secret_bytes.hex(), "node_id": node_id}, f, indent=2)
            f.write("\n")
    except BaseException:
        try:
            os.unlink(out_path)
        except OSError:
            pass
        raise

    print(f"Producer key written to: {out_path}")
    print(f"  node_id : {node_id}")
    print()
    print("Next step — sign an enrollment token for this producer:")
    print(f"  aster authorize --root-key <root.key> --producer-id {node_id} --out enrollment.token")
    return 0


def register_keygen_subparser(subparsers) -> None:
    """Register the ``keygen`` subcommand group."""
    kg_parser = subparsers.add_parser("keygen", help="Generate cryptographic keys")
    kg_sub = kg_parser.add_subparsers(dest="keygen_command", help="Key type")

    # aster keygen root
    root_p = kg_sub.add_parser("root", help="Generate a root ed25519 key pair")
    root_p.add_argument(
        "--out",
        default=os.path.expanduser("~/.aster/root.key"),
        metavar="PATH",
        help="Output path for the key JSON (default: ~/.aster/root.key)",
    )

    # aster keygen producer
    prod_p = kg_sub.add_parser("producer", help="Generate a stable producer node key")
    prod_p.add_argument(
        "--out",
        default=os.path.expanduser("~/.aster/node.key"),
        metavar="PATH",
        help="Output path for the key JSON (default: ~/.aster/node.key)",
    )


def run_keygen_command(args) -> int:
    """Dispatch ``keygen`` subcommand."""
    if not hasattr(args, "keygen_command") or args.keygen_command is None:
        print("Usage: aster keygen <root|producer>", file=sys.stderr)
        return 1
    if args.keygen_command == "root":
        return _keygen_root(args)
    elif args.keygen_command == "producer":
        return _keygen_producer(args)
    else:
        print(f"Unknown keygen subcommand: {args.keygen_command}", file=sys.stderr)
        return 1
