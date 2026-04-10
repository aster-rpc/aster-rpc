"""
aster_cli.enroll -- ``aster enroll node`` command.

The main operator workflow: reads the root private key from keyring (or
``--root-key`` file), generates or reuses a node keypair, signs an
enrollment credential, and writes/updates the ``.aster-identity`` file.

Usage::

    # First enrollment (generates node key + producer credential):
    aster enroll node --profile prod --role producer --name billing-producer

    # Add a consumer credential to an existing identity:
    aster enroll node --profile analytics --role consumer --name analytics-consumer \\
        --identity .aster-identity
"""

from __future__ import annotations

import base64
import json
import os
import sys
import time
from pathlib import Path


def _derive_endpoint_id(secret_key_bytes: bytes) -> str:
    """Derive the EndpointId (ed25519 public key hex) from a 32-byte secret key.

    Pure cryptographic operation -- no iroh runtime needed.
    """
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    priv = Ed25519PrivateKey.from_private_bytes(secret_key_bytes)
    pub = priv.public_key()
    pub_raw = pub.public_bytes_raw()
    return pub_raw.hex()


def _parse_expires(expires_str: str | None) -> int:
    """Parse an expiry string to epoch seconds.

    Accepts:
    - Relative: "30d", "24h", "1h30m"
    - Absolute ISO 8601: "2025-12-31T23:59:59"
    - None → default 30 days
    """
    if expires_str is None:
        return int(time.time()) + 30 * 86400  # 30 days

    s = expires_str.strip()

    # Relative: "30d", "7d", "24h"
    if s.endswith("d"):
        return int(time.time()) + int(s[:-1]) * 86400
    if s.endswith("h"):
        return int(time.time()) + int(s[:-1]) * 3600

    # Absolute ISO 8601
    from datetime import datetime, timezone
    try:
        dt = datetime.fromisoformat(s)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return int(dt.timestamp())
    except ValueError:
        print(f"Error: cannot parse expiry '{s}'. Use '30d', '24h', or ISO 8601.", file=sys.stderr)
        sys.exit(1)


def _parse_attributes(attrs_str: str | None) -> dict[str, str]:
    """Parse attributes from 'key1=val1,key2=val2' format."""
    if not attrs_str:
        return {}
    result = {}
    for pair in attrs_str.split(","):
        pair = pair.strip()
        if "=" in pair:
            k, v = pair.split("=", 1)
            result[k.strip()] = v.strip()
    return result


    # _get_root_privkey and _get_root_pubkey removed -- replaced by
    # the CredentialSigner protocol in signer.py.


def _style_helpers(quiet: bool):
    """Return (bold, dim, green, yellow, reset) functions that respect
    NO_COLOR, --quiet, and TTY detection."""
    use_color = (
        not quiet
        and sys.stdout.isatty()
        and os.environ.get("NO_COLOR") is None
        and os.environ.get("TERM") != "dumb"
    )
    if use_color:
        return (
            lambda s: f"\033[1m{s}\033[0m",      # bold
            lambda s: f"\033[2m{s}\033[0m",      # dim
            lambda s: f"\033[32m{s}\033[0m",     # green
            lambda s: f"\033[33m{s}\033[0m",     # yellow
            "",
        )
    return (lambda s: s, lambda s: s, lambda s: s, lambda s: s, "")


def cmd_enroll_node(args) -> int:
    """Execute ``aster enroll node``."""
    from aster_cli.identity import load_identity, save_identity, add_peer
    from aster_cli.signer import resolve_signer

    profile_name = args.profile or _resolve_profile()
    role = args.role
    name = args.name
    identity_path = Path(args.identity or args.out or ".aster-identity")
    quiet = bool(getattr(args, "quiet", False))

    # ── Resolve signer (pluggable -- local keyring/file by default) ───────
    signer = resolve_signer(
        profile_name,
        root_key_file=args.root_key,
        signer_type=getattr(args, "signer", None),
    )
    root_pubkey = signer.root_pubkey

    # ── Load or generate node key ────────────────────────────────────────
    if identity_path.exists():
        identity_data = load_identity(identity_path)
        node = identity_data.get("node", {})
        secret_key_b64 = node.get("secret_key")
        if secret_key_b64:
            secret_key = base64.b64decode(secret_key_b64)
            endpoint_id = node.get("endpoint_id") or _derive_endpoint_id(secret_key)
            if not quiet:
                print(f"Reusing node key from {identity_path} (endpoint_id={endpoint_id[:16]}...)")
        else:
            secret_key, endpoint_id = _generate_node_key(quiet)
    else:
        identity_data = {"node": {}, "peers": []}
        secret_key, endpoint_id = _generate_node_key(quiet)

    # Store node key in identity
    identity_data["node"] = {
        "secret_key": base64.b64encode(secret_key).decode(),
        "endpoint_id": endpoint_id,
    }

    # ── Build + sign credential via the signer ───────────────────────────
    expires_at = _parse_expires(args.expires)
    attributes = _parse_attributes(args.attributes)

    # --capabilities sets aster.role (comma-separated capability list)
    if hasattr(args, 'capabilities') and args.capabilities:
        attributes["aster.role"] = args.capabilities

    if role == "producer":
        from aster.trust.credentials import EnrollmentCredential
        attributes.setdefault("aster.role", "producer")
        cred = EnrollmentCredential(
            endpoint_id=endpoint_id,
            root_pubkey=root_pubkey,
            expires_at=expires_at,
            attributes=attributes,
        )
        cred.signature = signer.sign(cred, root_pubkey)
        cred_type = "policy"
    else:
        from aster.trust.credentials import ConsumerEnrollmentCredential
        attributes.setdefault("aster.role", "consumer")
        cred = ConsumerEnrollmentCredential(
            credential_type=args.type or "policy",
            root_pubkey=root_pubkey,
            expires_at=expires_at,
            attributes=attributes,
            endpoint_id=endpoint_id,
        )
        cred.signature = signer.sign(cred, root_pubkey)
        cred_type = args.type or "policy"

    # ── Build peer entry ─────────────────────────────────────────────────
    peer_entry: dict = {
        "name": name,
        "role": role,
        "type": cred_type,
        "root_pubkey": root_pubkey.hex(),
        "endpoint_id": endpoint_id,
        "expires_at": expires_at,
        "signature": cred.signature.hex(),
        "attributes": dict(attributes),
    }
    if hasattr(cred, "nonce") and cred.nonce:
        peer_entry["nonce"] = cred.nonce.hex()

    add_peer(identity_data, peer_entry)
    save_identity(identity_path, identity_data)

    # ── Print summary ────────────────────────────────────────────────────
    from datetime import datetime, timezone
    expiry_dt = datetime.fromtimestamp(expires_at, tz=timezone.utc)

    if quiet:
        # Single line, parseable: "<path> <endpoint_id> <expires_iso>"
        print(f"{identity_path} {endpoint_id} {expiry_dt.isoformat()}")
        return 0

    bold, dim, green, yellow, _ = _style_helpers(quiet)
    capabilities = attributes.get("aster.role", "(none)")
    abs_path = identity_path.resolve()

    if role == "producer":
        intro = (
            "This file lets you run a server signed by your root key.\n"
            "  When this server starts, it presents this credential and the\n"
            "  Aster mesh recognises it as part of your trust domain."
        )
        next_step = (
            "Use it:\n"
            f"  Set ASTER_IDENTITY_FILE={identity_path} when starting your server,\n"
            "  or place .aster-identity in the working directory."
        )
    else:
        intro = (
            "This file lets a consumer connect to your trusted-mode servers.\n"
            "  It contains a node identity (secret key) AND a signed enrollment\n"
            "  credential. The server validates the credential and grants the\n"
            "  capabilities listed below."
        )
        next_step = (
            "Use it:\n"
            f"  aster shell <peer-addr> --rcan {identity_path}\n"
            f"  aster call <peer-addr> Service.method '<json>' --rcan {identity_path}"
        )

    print("")
    print(f"{bold(green('✓ Enrollment credential created'))}")
    print("")
    print(f"  {bold('File:')}         {abs_path}")
    print(f"  {bold('Format:')}       TOML (.aster-identity) with [node] + [[peers]] sections")
    print("")
    print(f"  {bold('Peer:')}         {name}")
    print(f"  {bold('Role:')}         {role} ({cred_type})")
    print(f"  {bold('Capabilities:')} {capabilities}")
    print(f"  {bold('Endpoint ID:')}  {endpoint_id[:16]}...")
    print(f"  {bold('Trust root:')}   {root_pubkey.hex()[:16]}...")
    print(f"  {bold('Expires:')}      {expiry_dt.isoformat()}")
    print("")
    print(f"  {dim(intro)}")
    print("")
    print(f"  {next_step}")
    print("")
    print(f"  {yellow('⚠  Keep this file secret -- it is both an identity AND a credential.')}")
    print(f"  {dim(f'Anyone with this file can act as ' + repr(name) + ' until ' + str(expiry_dt.date()) + '.')}")
    print("")
    return 0


def _generate_node_key(quiet: bool = False) -> tuple[bytes, str]:
    """Generate a fresh node keypair."""
    from aster.trust.signing import generate_root_keypair
    priv, _pub = generate_root_keypair()
    endpoint_id = _derive_endpoint_id(priv)
    if not quiet:
        print(f"Generated new node key (endpoint_id={endpoint_id[:16]}...)")
    return priv, endpoint_id


def _resolve_profile() -> str:
    """Resolve the active profile name."""
    profile = os.environ.get("ASTER_PROFILE")
    if profile:
        return profile
    from aster_cli.profile import _load_config, _active_profile
    config = _load_config()
    return _active_profile(config)


# ── Argparse registration ────────────────────────────────────────────────


def register_enroll_subparser(subparsers) -> None:
    enroll_parser = subparsers.add_parser("enroll", help="Enroll a node in a mesh")
    enroll_sub = enroll_parser.add_subparsers(dest="enroll_command")

    node_p = enroll_sub.add_parser("node", help="Enroll this node (generate identity + credential)")
    node_p.add_argument("--profile", "-p", default=None, help="Operator profile (default: active)")
    node_p.add_argument("--role", "-r", required=True, choices=["producer", "consumer"],
                        help="Role: producer or consumer")
    node_p.add_argument("--name", "-n", required=True, help="Human label for this peer entry")
    node_p.add_argument("--type", "-t", default="policy", choices=["policy", "ott"],
                        help="Credential type (default: policy)")
    node_p.add_argument("--attributes", "-a", default=None,
                        help='Comma-separated key=value pairs, e.g. "aster.name=billing"')
    node_p.add_argument("--expires", "-e", default=None,
                        help='Expiry: "30d", "24h", or ISO 8601 (default: 30d)')
    node_p.add_argument("--root-key", default=None,
                        help="Path to root key JSON (fallback when keyring unavailable)")
    node_p.add_argument("--signer", default=None,
                        help='Signer type: "local" (default), future: "kms", "remote"')
    node_p.add_argument("--identity", "-i", default=None,
                        help="Existing .aster-identity file to add peer to")
    node_p.add_argument("--capabilities", "-c", default=None,
                        help="Comma-separated capabilities (e.g., ops.status,ops.logs)")
    node_p.add_argument("--out", "-o", default=".aster-identity",
                        help="Output path (default: .aster-identity)")
    node_p.add_argument("--quiet", "-q", action="store_true",
                        help="Suppress educational output. Prints one parseable line: PATH ENDPOINT_ID EXPIRES_ISO")


def run_enroll_command(args) -> int:
    if args.enroll_command == "node":
        return cmd_enroll_node(args)
    print("Usage: aster enroll node [options]", file=sys.stderr)
    return 1
