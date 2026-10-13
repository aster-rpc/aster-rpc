"""
aster_cli.keygen -- Key generation commands.

Commands:
  aster keygen root [--out PATH]
      Generate a new ed25519 root key pair.
      Writes a JSON file with ``private_key`` and ``public_key`` (hex).
      Refuses to overwrite existing files.

  aster keygen producer [--out PATH]
      Generate a stable producer node key as a **secret key file**: the
      32-byte key as 64 hex characters (mode 0600) -- the one node-key format
      shared by the Rust SDK and every binding. Use it as
      ``AsterConfig(secret_key_file=PATH)`` / ``ASTER_SECRET_KEY_FILE`` /
      ``[network] secret_key_file``. Prints the derived NodeId.

No network access required.
"""

from __future__ import annotations

import json
import os
import sys


def _keygen_root(args) -> int:
    """Generate an ed25519 root key pair.

    Stores the private key in the OS keyring (if available) under the
    active profile, and writes the public key to the profile config.
    Also writes a JSON file to ``--out`` as a backup/export.
    """
    from aster.trust.signing import generate_root_keypair
    from aster_cli.credentials import store_root_privkey, has_keyring, last_keyring_error
    from aster_cli.profile import _load_config, _save_config, _active_profile

    profile_name = getattr(args, "profile", None) or os.environ.get("ASTER_PROFILE")
    config = _load_config()
    if profile_name is None:
        profile_name = _active_profile(config)

    # Ensure profile exists
    profiles = config.setdefault("profiles", {})
    if profile_name not in profiles:
        profiles[profile_name] = {}
        if not config.get("active_profile"):
            config["active_profile"] = profile_name
        print(f"Created profile '{profile_name}'.")

    priv_raw, pub_raw = generate_root_keypair()
    priv_hex = priv_raw.hex()
    pub_hex = pub_raw.hex()

    # Store private key in keyring
    stored_in_keyring = store_root_privkey(profile_name, priv_hex)
    if stored_in_keyring:
        print(f"  Root private key stored in OS keyring (profile: {profile_name})")
    elif not has_keyring():
        print("  WARNING: keyring not available -- private key NOT securely stored.")
        print("  Install keyring: pip install keyring")
    else:
        err = last_keyring_error()
        print(f"  WARNING: keyring write failed -- private key NOT securely stored.")
        print(f"  Reason: {err}")

    # Store public key in profile config
    profiles[profile_name]["root_pubkey"] = pub_hex
    _save_config(config)
    print(f"  Root public key saved to profile '{profile_name}'")

    # Also write the JSON file (export/backup)
    out_path = os.path.expanduser(args.out)
    if not os.path.exists(out_path):
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
        print(f"  Private key written to: {out_path}")

    # Write public key separately (root.pub) for safe distribution
    pub_path = out_path.rsplit(".", 1)[0] + ".pub" if "." in out_path else out_path + ".pub"
    if not os.path.exists(pub_path):
        with open(pub_path, "w", encoding="utf-8") as f:
            json.dump({"public_key": pub_hex}, f, indent=2)
            f.write("\n")
        print(f"  Public key written to:  {pub_path}")

    print()
    print(f"  Profile    : {profile_name}")
    print(f"  public_key : {pub_hex}")
    if stored_in_keyring:
        print("  private_key: **** (in keyring)")
    else:
        print(f"  private_key: {out_path}")
    print()
    print("Keep root.key secret. Share root.pub with nodes that need to verify credentials.")
    return 0


def _keygen_producer(args) -> int:
    """Generate a stable producer node key file and print its NodeId."""
    from aster import SecretKey

    out_path = os.path.expanduser(args.out)

    if os.path.exists(out_path):
        print(f"Error: key file already exists: {out_path}", file=sys.stderr)
        print("Use a different path or remove the existing file.", file=sys.stderr)
        return 1

    key = SecretKey.generate()
    try:
        key.write_file(out_path)
    except ValueError as exc:
        print(f"Error: could not write key file: {exc}", file=sys.stderr)
        return 1
    node_id = str(key.node_id())

    print(f"Producer key written to: {out_path}")
    print(f"  node_id : {node_id}")
    print()
    print("Use it as the node identity:")
    print(f"  ASTER_SECRET_KEY_FILE={out_path}")
    print()
    print("Next step -- issue this node an enrollment credential (Rust aster CLI):")
    print(f"  aster enroll node --subject {node_id} --audience <fleet> --out node.cred")
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
        help="Output path for the key JSON backup (default: ~/.aster/root.key)",
    )
    root_p.add_argument(
        "--profile", "-p",
        default=None,
        metavar="NAME",
        help="Profile to store the key in (default: active profile)",
    )

    # aster keygen producer
    prod_p = kg_sub.add_parser("producer", help="Generate a stable producer node key")
    prod_p.add_argument(
        "--out",
        default=os.path.expanduser("~/.aster/node.key"),
        metavar="PATH",
        help="Output path for the 64-hex secret key file (default: ~/.aster/node.key)",
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
