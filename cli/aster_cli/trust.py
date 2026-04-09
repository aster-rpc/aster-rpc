"""
aster_cli.trust -- Offline trust management commands.

Commands:
  aster trust keygen --out-key PATH
      Generate a new ed25519 root key pair.
      Writes the 32-byte raw private key seed to PATH (chmod 600).
      Refuses to overwrite existing files.

  aster trust sign \\
      --root-key PATH \\
      --endpoint-id HEX \\
      [--attributes '{"aster.role":"producer"}'] \\
      [--expires 2026-01-01T00:00:00Z] \\
      [--type policy|ott] \\
      --out PATH
      Sign an enrollment credential offline.

No network access required.  The signed credential is serialised as JSON.
"""

from __future__ import annotations

import json
import os
import sys
import time
from datetime import datetime, timezone


def _keygen_command(args) -> int:
    """Generate an ed25519 root key pair and write to disk."""
    from aster.trust.signing import generate_root_keypair

    out_key = os.path.expanduser(args.out_key)

    if os.path.exists(out_key):
        print(f"Error: key file already exists: {out_key}", file=sys.stderr)
        print("Use a different path or remove the existing file.", file=sys.stderr)
        return 1

    priv_raw, pub_raw = generate_root_keypair()

    # Write private key as hex (32 bytes = 64 hex chars)
    priv_hex = priv_raw.hex()
    pub_hex = pub_raw.hex()

    out_dir = os.path.dirname(out_key) or "."
    os.makedirs(out_dir, exist_ok=True)

    # Write private key with restricted permissions (root.key)
    fd = os.open(out_key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump({"private_key": priv_hex, "public_key": pub_hex}, f, indent=2)
            f.write("\n")
    except BaseException:
        os.unlink(out_key)
        raise

    # Write public key separately (root.pub) for safe distribution
    pub_path = out_key.rsplit(".", 1)[0] + ".pub" if "." in out_key else out_key + ".pub"
    if not os.path.exists(pub_path):
        with open(pub_path, "w", encoding="utf-8") as f:
            json.dump({"public_key": pub_hex}, f, indent=2)
            f.write("\n")
        print(f"Root private key written to: {out_key}")
        print(f"Root public key written to:  {pub_path}")
    else:
        print(f"Root key pair written to: {out_key}")
        print(f"  (public key file already exists: {pub_path})")

    print(f"  Public key:  {pub_hex}")
    print("Keep root.key secret. Share root.pub with nodes that need to verify credentials.")
    return 0


def _sign_command(args) -> int:
    """Sign an enrollment credential offline."""
    from aster.trust.credentials import (
        ConsumerEnrollmentCredential,
        EnrollmentCredential,
    )
    from aster.trust.signing import sign_credential

    # Load root private key
    root_key_path = os.path.expanduser(args.root_key)
    try:
        with open(root_key_path) as f:
            key_data = json.load(f)
        priv_raw = bytes.fromhex(key_data["private_key"])
        pub_raw = bytes.fromhex(key_data["public_key"])
    except FileNotFoundError:
        print(f"Error: key file not found: {root_key_path}", file=sys.stderr)
        return 1
    except (json.JSONDecodeError, KeyError, ValueError) as e:
        print(f"Error: invalid key file: {e}", file=sys.stderr)
        return 1

    # Parse attributes
    attributes: dict[str, str] = {}
    if args.attributes:
        try:
            attributes = json.loads(args.attributes)
        except json.JSONDecodeError as e:
            print(f"Error: invalid --attributes JSON: {e}", file=sys.stderr)
            return 1

    # Parse expiry
    if args.expires:
        try:
            dt = datetime.fromisoformat(args.expires.replace("Z", "+00:00"))
            expires_at = int(dt.timestamp())
        except ValueError as e:
            print(f"Error: invalid --expires format: {e}", file=sys.stderr)
            return 1
    else:
        # Default: 30 days
        expires_at = int(time.time()) + 30 * 24 * 3600

    cred_type = getattr(args, "type", "producer")

    if cred_type == "producer":
        if not args.endpoint_id:
            print("Error: --endpoint-id is required for producer credentials", file=sys.stderr)
            return 1
        cred = EnrollmentCredential(
            endpoint_id=args.endpoint_id,
            root_pubkey=pub_raw,
            expires_at=expires_at,
            attributes=attributes,
        )
    elif cred_type in ("policy", "ott"):
        import secrets

        nonce = secrets.token_bytes(32) if cred_type == "ott" else None
        cred = ConsumerEnrollmentCredential(
            credential_type=cred_type,
            root_pubkey=pub_raw,
            expires_at=expires_at,
            attributes=attributes,
            endpoint_id=args.endpoint_id if args.endpoint_id else None,
            nonce=nonce,
        )
    else:
        print(f"Error: unknown credential type {cred_type!r}; use producer, policy, ott", file=sys.stderr)
        return 1

    # Sign
    cred.signature = sign_credential(cred, priv_raw)

    # Serialise to JSON
    if isinstance(cred, EnrollmentCredential):
        doc = {
            "type": "producer",
            "endpoint_id": cred.endpoint_id,
            "root_pubkey": cred.root_pubkey.hex(),
            "expires_at": cred.expires_at,
            "attributes": cred.attributes,
            "signature": cred.signature.hex(),
        }
    else:
        doc = {
            "type": cred.credential_type,
            "root_pubkey": cred.root_pubkey.hex(),
            "expires_at": cred.expires_at,
            "attributes": cred.attributes,
            "endpoint_id": cred.endpoint_id,
            "nonce": cred.nonce.hex() if cred.nonce else None,
            "signature": cred.signature.hex(),
        }

    out_path = os.path.expanduser(args.out)
    out_dir = os.path.dirname(out_path) or "."
    os.makedirs(out_dir, exist_ok=True)

    with open(out_path, "w") as f:
        json.dump(doc, f, indent=2)
        f.write("\n")

    print(f"Signed credential written to: {out_path}")
    print(f"  Type:       {cred_type}")
    print(f"  Expires at: {datetime.fromtimestamp(expires_at, tz=timezone.utc).isoformat()}")
    return 0


def register_trust_subparser(subparsers) -> None:
    """Register the ``trust`` subcommand group onto an existing argument parser."""
    trust_parser = subparsers.add_parser("trust", help="Trust and credential management")
    trust_sub = trust_parser.add_subparsers(dest="trust_command", help="Trust subcommand")

    # aster trust keygen
    kg = trust_sub.add_parser("keygen", help="Generate a new ed25519 root key pair")
    kg.add_argument(
        "--out-key",
        default=os.path.expanduser("~/.aster/root.key"),
        metavar="PATH",
        help="Output path for the key pair JSON (default: ~/.aster/root.key)",
    )

    # aster trust sign
    sg = trust_sub.add_parser("sign", help="Sign an enrollment credential offline")
    sg.add_argument("--root-key", required=True, metavar="PATH", help="Path to root key JSON")
    sg.add_argument("--endpoint-id", default=None, metavar="HEX", help="Endpoint NodeId to bind")
    sg.add_argument(
        "--attributes",
        default=None,
        metavar="JSON",
        help='Attributes JSON, e.g. \'{"aster.role":"producer"}\'',
    )
    sg.add_argument(
        "--expires",
        default=None,
        metavar="ISO8601",
        help="Expiry datetime in ISO 8601, e.g. 2026-01-01T00:00:00Z (default: +30 days)",
    )
    sg.add_argument(
        "--type",
        default="producer",
        choices=["producer", "policy", "ott"],
        help="Credential type (default: producer)",
    )
    sg.add_argument(
        "--out",
        default=".aster/credential.json",
        metavar="PATH",
        help="Output path for the signed credential JSON",
    )

    return trust_parser


def run_trust_command(args) -> int:
    """Dispatch ``trust`` subcommand."""
    if not hasattr(args, "trust_command") or args.trust_command is None:
        print("Usage: aster trust <keygen|sign>", file=sys.stderr)
        return 1
    if args.trust_command == "keygen":
        return _keygen_command(args)
    elif args.trust_command == "sign":
        return _sign_command(args)
    else:
        print(f"Unknown trust subcommand: {args.trust_command}", file=sys.stderr)
        return 1
