from __future__ import annotations

import json
from argparse import Namespace

import pytest

from aster.trust.signing import generate_root_keypair
from aster_cli import aster_service, join, profile, publish


@pytest.fixture
def isolated_profile(tmp_path, monkeypatch):
    aster_dir = tmp_path / ".aster"
    config_path = aster_dir / "config.toml"
    monkeypatch.setattr(profile, "ASTER_DIR", aster_dir)
    monkeypatch.setattr(profile, "CONFIG_PATH", config_path)
    return aster_dir, config_path


def _write_profile(
    *,
    root_pubkey: str = "",
    handle: str = "",
    handle_status: str = "unregistered",
    email: str = "",
    published_services: str = "",
    service_node: str = "",
) -> None:
    profile._save_config(
        {
            "active_profile": "default",
            "aster_service": {
                "enabled": True,
                "node_id": service_node,
                "relay": "",
                "offline_banner": True,
            },
            "profiles": {
                "default": {
                    "root_pubkey": root_pubkey,
                    "handle": handle,
                    "handle_status": handle_status,
                    "handle_claimed_at": "",
                    "email": email,
                    "signer": "local",
                    "published_services": published_services,
                }
            },
        }
    )


def test_parse_duration_seconds():
    assert aster_service.parse_duration_seconds("5m", default=1) == 300
    assert aster_service.parse_duration_seconds("2h", default=1) == 7200
    assert aster_service.parse_duration_seconds("15", default=1) == 15
    assert aster_service.parse_duration_seconds(None, default=7) == 7


def test_build_signed_envelope_from_root_key_file(isolated_profile, tmp_path):
    priv_raw, pub_raw = generate_root_keypair()
    _write_profile(root_pubkey=pub_raw.hex())

    root_key = tmp_path / "root.key"
    root_key.write_text(
        json.dumps(
            {
                "private_key": priv_raw.hex(),
                "public_key": pub_raw.hex(),
            }
        )
    )

    envelope = aster_service.build_signed_envelope(
        {"b": 2, "a": 1},
        root_key_file=str(root_key),
    )

    assert envelope.payload_json == '{"a":1,"b":2}'
    assert envelope.signer_pubkey == pub_raw.hex()
    assert len(envelope.signature) == 128


def test_resolve_aster_service_address_from_config(isolated_profile):
    _write_profile(service_node="aster1example")
    assert aster_service.resolve_aster_service_address() == "aster1example"


def test_cmd_join_real_dispatch(monkeypatch, isolated_profile):
    _write_profile(root_pubkey="ab" * 32)
    monkeypatch.setattr(join, "ensure_root_key_exists", lambda: False)

    captured = {}

    async def fake_join_remote(args, *, handle, email, announcements):
        captured["handle"] = handle
        captured["email"] = email
        captured["announcements"] = announcements
        return 0

    monkeypatch.setattr(join, "_join_remote", fake_join_remote)

    rc = join.cmd_join(
        Namespace(
            handle="alice-test",
            email="alice@example.com",
            announcements=True,
            demo=False,
            aster=None,
            root_key=None,
        )
    )

    assert rc == 0
    assert captured == {
        "handle": "alice-test",
        "email": "alice@example.com",
        "announcements": True,
    }


def test_cmd_publish_real_dispatch(monkeypatch, isolated_profile, tmp_path):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")

    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_text(
        json.dumps(
            {
                "service": "TaskManager",
                "version": 1,
                "contract_id": "c" * 64,
                "methods": [],
            }
        )
    )

    monkeypatch.setattr(publish, "load_local_endpoint_id", lambda identity_path=None: "node123")
    captured = {}

    async def fake_publish_remote(args, *, handle, service_name, manifest_dict, endpoint_id):
        captured["handle"] = handle
        captured["service_name"] = service_name
        captured["manifest_dict"] = manifest_dict
        captured["endpoint_id"] = endpoint_id
        return 0

    monkeypatch.setattr(publish, "_publish_remote", fake_publish_remote)

    rc = publish.cmd_publish(
        Namespace(
            target="TaskManager",
            manifest=str(manifest_path),
            semver=None,
            aster=None,
            root_key=None,
            identity_file=None,
            endpoint_id=None,
            relay="",
            endpoint_ttl="5m",
            description="Task queue",
            status="experimental",
            public=False,
            private=False,
            open=False,
            closed=False,
            token_ttl="5m",
            rate_limit=None,
            role=[],
            demo=False,
        )
    )

    assert rc == 0
    assert captured["handle"] == "alice-test"
    assert captured["service_name"] == "TaskManager"
    assert captured["endpoint_id"] == "node123"
    assert captured["manifest_dict"]["contract_id"] == "c" * 64


def test_cmd_publish_requires_description(monkeypatch, isolated_profile, tmp_path):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")

    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_text(
        json.dumps(
            {
                "service": "TaskManager",
                "version": 1,
                "contract_id": "c" * 64,
                "methods": [],
            }
        )
    )

    rc = publish.cmd_publish(
        Namespace(
            target="TaskManager",
            manifest=str(manifest_path),
            semver=None,
            aster=None,
            root_key=None,
            identity_file=None,
            endpoint_id="node123",
            relay="",
            endpoint_ttl="5m",
            description="",
            status="experimental",
            public=False,
            private=False,
            open=False,
            closed=False,
            token_ttl="5m",
            rate_limit=None,
            role=[],
            demo=False,
        )
    )

    assert rc == 1


def test_cmd_discover_dispatch(monkeypatch):
    captured = {}

    async def fake_discover_remote(args):
        captured["query"] = args.query
        captured["limit"] = args.limit
        captured["offset"] = args.offset
        captured["raw_json"] = args.raw_json
        return 0

    monkeypatch.setattr(publish, "_discover_remote", fake_discover_remote)

    rc = publish.cmd_discover(
        Namespace(
            query="@alice",
            aster=None,
            limit=5,
            offset=10,
            raw_json=True,
        )
    )

    assert rc == 0
    assert captured == {
        "query": "@alice",
        "limit": 5,
        "offset": 10,
        "raw_json": True,
    }


def test_cmd_status_remote_sync(monkeypatch, isolated_profile, capsys):
    _write_profile(root_pubkey="ab" * 32)

    async def fake_fetch_remote_status(args):
        return {
            "handle": "alice-test",
            "status": "verified",
            "email_masked": "a***@example.com",
            "display_name": "Alice",
            "bio": None,
            "url": None,
            "registered_at": "2026-04-09T00:00:00Z",
            "services_published": 1,
            "recovery_codes_remaining": 0,
        }

    monkeypatch.setattr(join, "_fetch_remote_status", fake_fetch_remote_status)

    rc = join.cmd_status(
        Namespace(
            command="status",
            raw_json=False,
            local_only=False,
            aster=None,
            root_key=None,
        )
    )

    out = capsys.readouterr().out
    assert rc == 0
    assert "Handle: @alice-test" in out
    assert "Handle status: verified" in out

    _name, prof, _config = profile.get_active_profile()
    assert prof["handle"] == "alice-test"
    assert prof["handle_status"] == "verified"
